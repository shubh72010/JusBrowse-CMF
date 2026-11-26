//! Pure parsing for JusBrowse: URL building, Wikipedia's compact search API, HTML/JSON text extraction.
//! No allocation, so `make test APP=jusbrowse` covers it on the host.

pub const MAX_URL: usize = 123;
pub const TEXT: usize = 64;
pub const MAX_RESULTS: usize = 8;

pub struct Results {
    pub titles: [[u8; TEXT]; MAX_RESULTS],
    pub title_lens: [usize; MAX_RESULTS],
    pub bodies: [[u8; 320]; MAX_RESULTS],
    pub body_lens: [usize; MAX_RESULTS],
    pub count: usize,
}

impl Results {
    pub const fn new() -> Self {
        Self {
            titles: [[0; TEXT]; MAX_RESULTS],
            title_lens: [0; MAX_RESULTS],
            bodies: [[0; 320]; MAX_RESULTS],
            body_lens: [0; MAX_RESULTS],
            count: 0,
        }
    }
}

fn put(dst: &mut [u8], at: &mut usize, bytes: &[u8]) {
    let n = bytes.len().min(dst.len().saturating_sub(*at));
    dst[*at..*at + n].copy_from_slice(&bytes[..n]);
    *at += n;
}

fn encode(dst: &mut [u8], at: &mut usize, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in bytes {
        if dst.len().saturating_sub(*at) < 3 {
            break;
        }
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            dst[*at] = b;
            *at += 1;
        } else if b == b' ' {
            dst[*at] = b'+';
            *at += 1;
        } else {
            dst[*at] = b'%';
            dst[*at + 1] = HEX[(b >> 4) as usize];
            dst[*at + 2] = HEX[(b & 15) as usize];
            *at += 3;
        }
    }
}

/// Wikipedia's opensearch is the only general search endpoint that fits the companion's 2048-byte body cap
/// (Bing/Mojeek/DDG RSS are 4-6 KiB and are rejected as TOO_LARGE).
pub fn search_url(query: &[u8], out: &mut [u8; MAX_URL]) -> usize {
    let mut n = 0;
    // One request returns titles AND excerpts: tapping a result needs no second round trip.
    put(out, &mut n, b"https://en.wikipedia.org/w/rest.php/v1/search/page?q=");
    encode(out, &mut n, query);
    put(out, &mut n, b"&limit=2");
    n
}

pub fn article_url(title: &[u8], out: &mut [u8; MAX_URL]) -> usize {
    let mut n = 0;
    // The REST summary is only the first sentence (useless for a disambiguation page); the query API with
    // redirects=1 returns up to 1400 chars of plain text, still under the companion's 2048-byte cap.
    put(out, &mut n, b"https://en.wikipedia.org/w/api.php?action=query&prop=extracts&explaintext=1&exchars=1400&redirects=1&format=json&titles=");
    encode(out, &mut n, title);
    n
}

pub fn is_url(s: &[u8]) -> bool {
    s.starts_with(b"https://")
}

/// JSON string arrays: skip the query echo (array 1), read the titles (array 2).
fn json_string(body: &[u8], from: usize, out: &mut [u8]) -> Option<usize> {
    let mut i = from;
    if body.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    let mut n = 0;
    while i < body.len() {
        let b = body[i];
        if b == b'"' {
            return Some(n);
        }
        if b == b'\\' && i + 1 < body.len() {
            let e = body[i + 1];
            let c = match e {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                _ => e,
            };
            if n >= out.len() {
                return None;
            }
            out[n] = c;
            n += 1;
            i += 2;
            continue;
        }
        if n >= out.len() {
            return None;
        }
        out[n] = b;
        n += 1;
        i += 1;
    }
    None
}

pub fn parse_search(body: &[u8], out: &mut Results) {
    out.count = 0;
    // The response is ["query",["titles"],["urls"],["descriptions"]]; find array 2 by counting '[' outside strings.
    let mut d = 0usize;
    let mut arrays = 0u8;
    let mut i = 0usize;
    while i < body.len() {
        match body[i] {
            b'"' => {
                let mut j = i + 1;
                while j < body.len() {
                    if body[j] == b'\\' {
                        j += 2;
                    } else if body[j] == b'"' {
                        break;
                    } else {
                        j += 1;
                    }
                }
                i = j + 1;
                continue;
            }
            b'[' => {
                arrays += 1;
                if arrays == 2 {
                    i += 1;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    while i < body.len() && out.count < MAX_RESULTS {
        if body[i] == b']' {
            break;
        }
        if body[i] == b'"' {
            if let Some(len) = json_string(body, i, &mut out.titles[out.count]) {
                out.title_lens[out.count] = len;
                out.count += 1;
                i += 1;
                while i < body.len() && body[i] != b',' && body[i] != b']' {
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
}

fn find(body: &[u8], key: &[u8], from: usize) -> Option<usize> {
    if from >= body.len() {
        return None;
    }
    body[from..].windows(key.len()).position(|w| w == key).map(|p| from + p)
}

/// REST search gives each result's exact title and an HTML excerpt in one response.
pub fn parse_results(body: &[u8], out: &mut Results) {
    out.count = 0;
    let mut i = 0;
    while out.count < MAX_RESULTS {
        let Some(t) = find(body, b"\"title\":\"", i) else { break };
        let Some(tlen) = json_string(body, t + 8, &mut out.titles[out.count]) else { break };
        out.title_lens[out.count] = tlen;
        let Some(e) = find(body, b"\"excerpt\":\"", t + 9) else { break };
        let mut raw = [0u8; 512];
        let Some(n) = json_string(body, e + 10, &mut raw) else { break };
        out.body_lens[out.count] = strip_html(&raw[..n], &mut out.bodies[out.count]);
        out.count += 1;
        i = e + 11;
    }
}

/// The Wikipedia summary API returns JSON; take the "extract" field. For ordinary pages use `strip_html`.
pub fn parse_summary(body: &[u8], out: &mut [u8]) -> usize {
    let key = b"\"extract\":\"";
    let Some(at) = body.windows(key.len()).position(|w| w == key) else {
        return 0;
    };
    let mut total = 0;
    let mut i = at + key.len();
    while i < body.len() && body[i] != b'"' {
        let b = body[i];
        if b == b'\\' && i + 1 < body.len() {
            let e = body[i + 1];
            let c = match e {
                b'n' => b'\n',
                b't' => b'\t',
                _ => e,
            };
            if total >= out.len() {
                break;
            }
            out[total] = c;
            total += 1;
            i += 2;
            continue;
        }
        if total >= out.len() {
            break;
        }
        out[total] = b;
        total += 1;
        i += 1;
    }
    total
}

/// Turn a Wikipedia extract into screen lines: markers for headings (`0x01`), no `==` decoration or blank runs.
/// `\n` separates logical lines; the device wraps each one by words.
pub fn format_extract(body: &[u8], out: &mut [u8]) -> usize {
    let mut raw = [0u8; 2048];
    let n = parse_summary(body, &mut raw);
    let mut w = 0;
    for line in raw[..n].split(|&b| b == b'\n') {
        let mut s = line;
        let heading = s.starts_with(b"==");
        if heading {
            while s.first() == Some(&b'=') {
                s = &s[1..];
            }
            while s.first() == Some(&b' ') {
                s = &s[1..];
            }
            while s.last() == Some(&b'=') {
                s = &s[..s.len() - 1];
            }
            while s.last() == Some(&b' ') {
                s = &s[..s.len() - 1];
            }
        }
        if s.is_empty() {
            continue;
        }
        if w + s.len() + 2 > out.len() {
            break;
        }
        if heading {
            out[w] = 1;
            w += 1;
        }
        out[w..w + s.len()].copy_from_slice(s);
        w += s.len();
        out[w] = b'\n';
        w += 1;
    }
    w
}

/// Tags and `<script>/<style>` bodies are dropped; the five common HTML entities become one byte.
pub fn strip_html(body: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    let mut skip = false;
    while i < body.len() {
        if body[i] == b'<' {
            let end = body[i..].iter().position(|&c| c == b'>').map(|p| i + p + 1).unwrap_or(body.len());
            let tag = &body[i + 1..end.saturating_sub(1)];
            skip = (tag.starts_with(b"script") || tag.starts_with(b"style"))
                && !tag.starts_with(b"/script")
                && !tag.starts_with(b"/style");
            if !skip {
                n = push_space(out, n);
            }
            i = end;
            continue;
        }
        if skip {
            i += 1;
            continue;
        }
        if body[i] == b'&' {
            for (ent, ch) in [
                (b"&amp;".as_slice(), b'&'),
                (b"&lt;".as_slice(), b'<'),
                (b"&gt;".as_slice(), b'>'),
                (b"&quot;".as_slice(), b'"'),
                (b"&#39;".as_slice(), b'\''),
                (b"&nbsp;".as_slice(), b' '),
            ] {
                if body[i..].starts_with(ent) {
                    if ch == b' ' {
                        n = push_space(out, n);
                    } else if n < out.len() {
                        out[n] = ch;
                        n += 1;
                    }
                    i += ent.len();
                    break;
                }
            }
            continue;
        }
        let c = body[i];
        if c.is_ascii_whitespace() {
            n = push_space(out, n);
        } else if n < out.len() {
            out[n] = c;
            n += 1;
        }
        i += 1;
    }
    while n > 0 && out[n - 1] == b' ' {
        n -= 1;
    }
    n
}

fn push_space(out: &mut [u8], n: usize) -> usize {
    if n > 0 && n < out.len() && out[n - 1] != b' ' {
        out[n] = b' ';
        return n + 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_fits_the_companions_limit() {
        let mut url = [0u8; MAX_URL];
        let n = search_url(b"hello world", &mut url);
        let s = core::str::from_utf8(&url[..n]).unwrap();
        assert!(s.starts_with("https://en.wikipedia.org/w/rest.php/v1/search/page?q="));
        assert!(s.contains("hello+world"));
        assert!(n <= MAX_URL);
        let n = search_url(&[b'a'; 200], &mut url);
        assert!(n <= MAX_URL);
    }

    #[test]
    fn search_results_are_parsed_and_decoded() {
        let body = br#"["rust",["Rust","Cargo (& tools)","Rusty & co"],["u1","u2","u3"],["d1","d2","d3"]]"#;
        let mut r = Results::new();
        parse_search(body, &mut r);
        assert_eq!(r.count, 3);
        assert_eq!(&r.titles[0][..r.title_lens[0]], b"Rust");
        assert_eq!(&r.titles[1][..r.title_lens[1]], b"Cargo (& tools)");
    }

    #[test]
    fn summary_extract_is_taken() {
        let body = br#"{"type":"standard","extract":"Hello\nworld & friends","extract_html":"<p>"}"#;
        let mut out = [0u8; 256];
        let n = parse_summary(body, &mut out);
        assert_eq!(&out[..n], b"Hello\nworld & friends");
    }

    #[test]
    fn rest_search_results_carry_clean_excerpts() {
        let body = br#"{"pages":[{"title":"HI","excerpt":"<span class=searchmatch>HI</span> or Hi may refer to: &quot;song&quot;"},{"title":"Hi-de-Hi!","excerpt":"A BBC sitcom"}]}"#;
        let mut r = Results::new();
        parse_results(body, &mut r);
        assert_eq!(r.count, 2);
        assert_eq!(&r.titles[0][..r.title_lens[0]], b"HI");
        assert_eq!(&r.bodies[0][..r.body_lens[0]], b"HI or Hi may refer to: \"song\"");
    }

    #[test]
    fn extracts_are_formatted_into_headings_and_lines() {
        let body = br#"{"extract":"HI or Hi may refer to:\n\n\n== Arts and entertainment ==\n\n\n\"Hi\", a song"}"#;
        let mut out = [0u8; 512];
        let n = format_extract(body, &mut out);
        assert_eq!(&out[..n], b"HI or Hi may refer to:\n\x01Arts and entertainment\n\"Hi\", a song\n");
    }

    #[test]
    fn html_is_stripped_and_entities_decoded() {
        let body = b"<html><script>bad()</script><body>Hello &amp; <b>bye</b>&nbsp;now</body></html>";
        let mut out = [0u8; 256];
        let n = strip_html(body, &mut out);
        assert_eq!(&out[..n], b"Hello & bye now");
    }

    #[test]
    fn direct_urls_are_recognised() {
        assert!(is_url(b"https://example.com"));
        assert!(!is_url(b"example.com"));
    }
}
