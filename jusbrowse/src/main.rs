#![cfg_attr(any(target_os = "none", wp2_sim), no_main, cfg_attr(target_os = "none", no_std))]

mod logic;

#[cfg(not(any(target_os = "none", wp2_sim)))]
fn main() {}

#[cfg(any(target_os = "none", wp2_sim))]
mod device {
    use crate::logic;
    use companion_rpc::{self as rpc, Call, HttpHead};
    use cmf_wp2_app::{Api, BUTTON, CROWN, MESSAGE, PAUSE, RESUME, TOUCH_DOWN};

    cmf_wp2_app::entry!(main);
    cmf_wp2_app::app_icon!();

    const BLACK: u16 = 0x0000;
    const WHITE: u16 = 0xFFFF;
    const GREY: u16 = 0x39E7;
    const ACCENT: u16 = 0xFD20;
    const RED: u16 = 0xF800;
    const KEY_BG: u16 = 0x2124;

    const MAX_BODY: usize = 2048;
    const MAX_INPUT: usize = 96;
    const KEY_X: i16 = 83;
    const KEY_Y: i16 = 146;
    const KEY_STEP: i16 = 50;
    const KEY_ROW: i16 = 40;
    const KEYS: [&[u8]; 7] = [b"abcdef", b"ghijkl", b"mnopqr", b"stuvwx", b"yz0123", b"456789", b" /.:DG"];
    const ASK_URL: &str = "https://en.wikipedia.org/w/api.php?action=opensearch&search=watch&limit=1&format=json";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode { Home, Loading, Results, View, Error }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Want { Search, Article, Page }

    struct State {
        body: [u8; MAX_BODY],
        text: [u8; MAX_BODY],
        url: [u8; logic::MAX_URL],
        input: [u8; MAX_INPUT],
        status: [u8; 64],
        results: logic::Results,
        input_len: usize,
        url_len: usize,
        body_len: usize,
        text_len: usize,
        status_len: usize,
        head_code: u16,
        http_call: Call,
        ask_call: Call,
        started: u32,
        last_send: u32,
        ids: u8,
        ask_ids: u8,
        asked: bool,
        selected: usize,
        scroll: usize,
        mode: Mode,
        want: Want,
        back: Mode,
    }

    impl State {
        const fn new() -> Self {
            Self {
                body: [0; MAX_BODY], text: [0; MAX_BODY], url: [0; logic::MAX_URL],
                input: [0; MAX_INPUT], status: [0; 64], results: logic::Results::new(),
                input_len: 0, url_len: 0, body_len: 0, text_len: 0, status_len: 0, head_code: 0,
                http_call: Call::new(8000), ask_call: Call::new(8000), started: 0, last_send: 0,
                ids: 0, ask_ids: 200, asked: false,
                selected: 0, scroll: 0,
                mode: Mode::Home, want: Want::Search, back: Mode::Home,
            }
        }
    }

    static mut STATE: State = State::new();

    fn st() -> &'static mut State {
        unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
    }

    fn set_status(s: &str) {
        let st = st();
        let n = s.len().min(st.status.len());
        st.status[..n].copy_from_slice(&s.as_bytes()[..n]);
        st.status_len = n;
    }

    fn set_status_code_for_title(title: &[u8]) {
        let st = st();
        let n = title.len().min(st.status.len());
        st.status[..n].copy_from_slice(&title[..n]);
        st.status_len = n;
    }

    fn status_str() -> &'static str {
        let st = st();
        core::str::from_utf8(&st.status[..st.status_len]).unwrap_or("")
    }

    /// Centred 16 px text clipped to the round panel.
    fn line(api: &Api, y: i16, bytes: &[u8], color: u16) {
        let d = (y - 233).abs() as i32;
        let r2 = 233 * 233 - d * d;
        if r2 <= 0 {
            return;
        }
        let mut half = 0i32;
        while (half + 1) * (half + 1) <= r2 {
            half += 1;
        }
        let chars = ((half - 24) / 8).clamp(0, 22) as usize;
        let n = bytes.len().min(chars);
        api.text(233 - (n as i16) * 8, y, core::str::from_utf8(&bytes[..n]).unwrap_or(""), color);
    }

    /// Wrap by words at `width` bytes; a `\n` ends the logical line early.
    fn wrap_end(bytes: &[u8], from: usize, width: usize) -> usize {
        let limit = (from + width).min(bytes.len());
        if let Some(p) = bytes[from..limit].iter().position(|&b| b == b'\n') {
            return from + p + 1;
        }
        if limit == bytes.len() {
            return limit;
        }
        for i in (from..limit).rev() {
            if bytes[i] == b' ' && i + 1 > from + width / 2 {
                return i + 1;
            }
        }
        limit
    }

    fn line_count(bytes: &[u8]) -> usize {
        let mut n = 0;
        let mut at = 0;
        while at < bytes.len() {
            at = wrap_end(bytes, at, 16);
            n += 1;
        }
        n
    }

    fn key(row: usize, col: usize) -> u8 {
        KEYS[row][col]
    }

    fn press(row: usize, b: u8, api_now: u32) {
        match (row, b) {
            (6, b'D') => { st().input_len = st().input_len.saturating_sub(1); }
            (6, b'G') => { go(api_now); }
            _ => {
                let st = st();
                if st.input_len < MAX_INPUT {
                    st.input[st.input_len] = b;
                    st.input_len += 1;
                }
            }
        }
    }

    fn start_fetch(api_now: u32, want: Want, back: Mode) {
        let st = st();
        st.mode = Mode::Loading;
        st.want = want;
        st.back = back;
        st.body_len = 0;
        st.text_len = 0;
        st.head_code = 0;
        st.scroll = 0;
        st.http_call.now(api_now);
        st.started = api_now;
        st.last_send = api_now;
        set_status("Fetching...");
    }

    /// A typed https:// URL is opened; anything else is a web search.
    fn go(api_now: u32) {
        let st = st();
        if st.input_len == 0 {
            return;
        }
        if logic::is_url(&st.input[..st.input_len]) {
            let len = st.input_len;
            st.url[..len].copy_from_slice(&st.input[..len]);
            st.url_len = len;
            start_fetch(api_now, Want::Page, Mode::Home);
        } else {
            st.url_len = logic::search_url(&st.input[..st.input_len], &mut st.url);
            start_fetch(api_now, Want::Search, Mode::Home);
        }
    }

    fn open_result(api_now: u32) {
        let st = st();
        let Some(title) = st.results.titles.get(st.selected) else { return; };
        let len = st.results.title_lens[st.selected];
        st.url_len = logic::article_url(&title[..len], &mut st.url);
        start_fetch(api_now, Want::Article, Mode::Results);
    }

    fn set_status_code(code: u16) {
        let st = st();
        st.status[0] = b'H'; st.status[1] = b'T'; st.status[2] = b'T'; st.status[3] = b'P'; st.status[4] = b' ';
        st.status[5] = b'0' + (code / 100) as u8;
        st.status[6] = b'0' + ((code / 10) % 10) as u8;
        st.status[7] = b'0' + (code % 10) as u8;
        st.status_len = 8;
    }

    fn handle_message(m: &[u8]) {
        let st = st();
        match st.http_call.take(m) {
            rpc::Got::Nothing => {}
            rpc::Got::Status(s) => {
                set_status(match s {
                    1 => "Bad URL - phone refused",
                    2 => "Denied on phone",
                    3 => "Unavailable",
                    4 => "Allow in companion",
                    5 => "Too large",
                    _ => "Phone error",
                });
                st.mode = Mode::Error;
            }
            rpc::Got::Record { index, record, last, .. } => {
                if index == 0 {
                    if let Some(h) = HttpHead::decode(record) {
                        st.head_code = h.code;
                    }
                } else {
                    let n = record.len().min(MAX_BODY - st.body_len);
                    st.body[st.body_len..st.body_len + n].copy_from_slice(&record[..n]);
                    st.body_len += n;
                }
                if last {
                    let body_len = st.body_len;
                    if st.head_code >= 400 {
                        set_status_code(st.head_code);
                        st.mode = Mode::Error;
                        return;
                    }
                    match st.want {
                        Want::Search => {
                            logic::parse_results(&st.body[..body_len], &mut st.results);
                            if st.results.count > 0 {
                                let mut buf = [0u8; 24];
                                let n = (b"results ").len();
                                buf[..n].copy_from_slice(b"results ");
                                buf[n] = b'0' + st.results.count as u8;
                                set_status(core::str::from_utf8(&buf[..n + 1]).unwrap_or(""));
                                st.selected = 0;
                                st.mode = Mode::Results;
                            } else {
                                st.text_len = logic::strip_html(&st.body[..body_len], &mut st.text);
                                st.mode = if st.text_len > 0 { Mode::View } else { Mode::Error };
                            }
                        }
                        Want::Article => {
                            st.text_len = logic::format_extract(&st.body[..body_len], &mut st.text);
                            if st.text_len == 0 {
                                st.text_len = logic::strip_html(&st.body[..body_len], &mut st.text);
                            }
                            set_status("Article");
                            st.mode = if st.text_len > 0 { Mode::View } else { Mode::Error };
                        }
                        Want::Page => {
                            st.text_len = logic::strip_html(&st.body[..body_len], &mut st.text);
                            set_status_code(st.head_code);
                            st.mode = if st.text_len > 0 { Mode::View } else { Mode::Error };
                        }
                    }
                    st.scroll = 0;
                }
            }
        }
    }

    fn draw(api: &Api) {
        api.rect(0, 0, api.width, api.height, BLACK);
        let st = st();
        match st.mode {
            Mode::Home => {
                line(api, 40, b"JusBrowse", ACCENT);
                let from = st.input_len.saturating_sub(21);
                line(api, 92, &st.input[from..st.input_len], WHITE);
                line(api, 120, b"URL or search", GREY);
                for row in 0..KEYS.len() {
                    for col in 0..6 {
                        let b = key(row, col);
                        let label: &[u8] = match (row, b) {
                            (6, b'D') => b"DEL",
                            (6, b'G') => b"GO",
                            (6, b' ') => b"SPC",
                            _ => &KEYS[row][col..col + 1],
                        };
                        let x = KEY_X + col as i16 * KEY_STEP;
                        let y = KEY_Y + row as i16 * KEY_ROW;
                        api.rect(x + 1, y + 1, 46, 34, KEY_BG);
                        api.text(x + (50 - label.len() as i16 * 16) / 2, y + 8,
                            core::str::from_utf8(label).unwrap_or(""),
                            if row == 6 { ACCENT } else { WHITE });
                    }
                }
                line(api, 428, b"BTN=exit", GREY);
            }
            Mode::Loading => {
                line(api, 80, b"Loading...", WHITE);
                let secs = api.now().wrapping_sub(st.started) / 1000;
                let mut wait_buf = [0u8; 16];
                wait_buf[..10].copy_from_slice(b"Fetching ");
                wait_buf[10] = b'0' + (secs % 10) as u8;
                line(api, 110, &wait_buf[..11], GREY);
                line(api, 428, b"BTN=cancel", GREY);
            }
            Mode::Results => {
                line(api, 30, status_str().as_bytes(), GREY);
                for i in 0..st.results.count.min(10) {
                    let len = st.results.title_lens[i];
                    let mut buf = [0u8; 66];
                    buf[0] = if i == st.selected { b'>' } else { b' ' };
                    buf[1] = b' ';
                    let n = len.min(64);
                    buf[2..2 + n].copy_from_slice(&st.results.titles[i][..n]);
                    line(api, 56 + i as i16 * 22, &buf[..2 + n],
                        if i == st.selected { ACCENT } else { WHITE });
                }
                line(api, 428, b"Tap/Crown=open BTN=back", GREY);
            }
            Mode::View | Mode::Error => {
                line(api, 28, status_str().as_bytes(), if st.mode == Mode::Error { RED } else { GREY });
                let mut pos = 0;
                let mut row = 0;
                let mut y = 52i16;
                while pos < st.text_len && y < 408 {
                    let end = wrap_end(&st.text[..st.text_len], pos, 16);
                    if row >= st.scroll {
                        let mut bytes = &st.text[pos..end];
                        let heading = bytes.first() == Some(&1);
                        if heading {
                            bytes = &bytes[1..];
                        }
                        if !bytes.is_empty() && bytes[0] != b'\n' {
                            line(api, y, bytes, if heading { ACCENT } else { WHITE });
                        }
                        y += 20;
                    }
                    pos = end;
                    row += 1;
                }
                line(api, 428, if st.mode == Mode::Error {
                    b"Tap=retry BTN=back"
                } else if st.back == Mode::Results {
                    b"Tap=full Crown=scroll BTN=back"
                } else {
                    b"Crown=scroll BTN=back"
                }, GREY);
            }
        }
        api.show();
    }

    fn main(api: &'static Api) -> i32 {
        api.own_back(true);
        draw(api);
        let mut paused = false;
        let mut msg = [0u8; 128];
        // A tiny request at launch makes the companion post its "JusBrowse wants..." prompt; a miniapp only
        // appears in Permissions after it has actually asked for a capability.
        st().asked = true;
        st().ask_call.now(api.now());
        loop {
            let mut timed_out = false;
            let mut bad_url = false;
            if !paused && st().mode == Mode::Loading {
                let now = api.now();
                let st = st();
                // The companion's retry backoff is 3/6/8 s; force a fresh send every second instead.
                if now.wrapping_sub(st.last_send) >= 1000 {
                    st.http_call.now(now);
                }
                if now.wrapping_sub(st.started) >= 6000 {
                    st.http_call.stop();
                    st.mode = Mode::Error;
                    timed_out = true;
                } else if let Some(id) = st.http_call.due(now, &mut st.ids) {
                    let url = core::str::from_utf8(&st.url[..st.url_len]).unwrap_or("");
                    if let Some(m) = rpc::http_get(id, url) {
                        let _ = api.send(&m);
                        st.last_send = now;
                    } else {
                        st.http_call.stop();
                        st.mode = Mode::Error;
                        bad_url = true;
                    }
                }
            }
            if timed_out {
                set_status("No answer - phone?");
                draw(api);
            } else if bad_url {
                set_status("URL too long");
                draw(api);
            }
            if !paused {
                let now = api.now();
                let st = st();
                if let Some(id) = st.ask_call.due(now, &mut st.ask_ids) {
                    if let Some(m) = rpc::http_get(id, ASK_URL) {
                        let _ = api.send(&m);
                    }
                    st.ask_call.stop();  // one ask is enough; retries would compete with real requests
                }
            }
            let wait = if paused {
                u32::MAX
            } else if st().mode == Mode::Loading {
                st().http_call.wait(api.now()).min(50)
            } else {
                200u32
            };
            let Some(e) = api.event(wait) else { continue; };
            match e.kind {
                BUTTON => {
                    let mode = st().mode;
                    match mode {
                        Mode::Home => return 0,
                        Mode::Loading => {
                            st().http_call.stop();
                            st().mode = Mode::Home;
                        }
                        Mode::Results | Mode::Error => st().mode = Mode::Home,
                        Mode::View => { st().mode = st().back; }
                    }
                    draw(api);
                }
                CROWN => {
                    let st = st();
                    match st.mode {
                        Mode::Results => {
                            if e.delta > 0 && st.selected + 1 < st.results.count {
                                st.selected += 1;
                            } else if e.delta < 0 && st.selected > 0 {
                                st.selected -= 1;
                            }
                        }
                        Mode::View => {
                            let max = line_count(&st.text[..st.text_len]).saturating_sub(18);
                            if e.delta > 0 {
                                st.scroll = (st.scroll + 1).min(max);
                            } else if e.delta < 0 {
                                st.scroll = st.scroll.saturating_sub(1);
                            }
                        }
                        _ => {}
                    }
                    draw(api);
                }
                TOUCH_DOWN => {
                    let mode = st().mode;
                    if mode == Mode::Home {
                        let col = (e.x - KEY_X) / KEY_STEP;
                        let row = (e.y - KEY_Y) / KEY_ROW;
                        if col >= 0 && col < 6 && row >= 0 && (row as usize) < KEYS.len() {
                            let now = api.now();
                            press(row as usize, key(row as usize, col as usize), now);
                        }
                    } else if mode == Mode::Results {
                        let i = ((e.y - 56) / 22) as usize;
                        if i < st().results.count {
                            let st = st();
                            st.selected = i;
                            let n = st.results.body_lens[i];
                            st.text[..n].copy_from_slice(&st.results.bodies[i][..n]);
                            st.text_len = n;
                            st.scroll = 0;
                            st.back = Mode::Results;
                            st.mode = Mode::View;
                            let len = st.results.title_lens[i];
                            set_status_code_for_title(&st.results.titles[i][..len]);
                        }
                    } else if mode == Mode::Error {
                        let now = api.now();
                        st().http_call.stop();
                        go(now);
                    } else if mode == Mode::View && st().back == Mode::Results {
                        // Second tap on a preview fetches the full article.
                        open_result(api.now());
                    }
                    draw(api);
                }
                PAUSE => { paused = true; }
                RESUME => { paused = false; }
                MESSAGE => {
                    while let Some(m) = api.recv(&mut msg) {
                        let _ = st().ask_call.take(m);
                        handle_message(m);
                    }
                    draw(api);
                }
                _ => {}
            }
        }
    }
}
