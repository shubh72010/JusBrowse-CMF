//! The watch side of FMC Companion's miniapp RPC: what a watch app sends to ask the phone for its calendar,
//! reminders, weather, a web request and the rest, and how it reads the answers.
//!
//! The contract is `docs/miniapp-rpc.md` in the companion repo (github.com/fmctools/companion); its byte layouts are
//! pinned by `rpc-vectors.json` there, a copy of which `tests/vectors.rs` checks this crate against. `no_std`, no
//! allocation: requests are built into a [`Msg`] (128 bytes on the stack), records decode into owned structs whose
//! text is a [`Text`] of at most 64 bytes.
//!
//! ```text
//! request  "R1" 'Q' id method args                    watch -> phone, one message of 1-128 bytes
//! part     "R1" 'Q' id method index total chunk       a multi-part request (http.request, storage.put, job.set)
//! reply    "R1" 'A' id status index total record      phone -> watch, `total` messages, one record each
//! ```
//!
//! An app sends a request with `api.send(&msg)`, feeds every message `api.recv` gives it to [`Call::take`] and
//! decodes each record with the method's type ([`Event::decode`], [`Reminder::decode`], ...). [`Call`] also keeps
//! the retry timer: the phone may not answer at all (asleep, out of range, the companion not running), so the app
//! shows its cached copy and asks again after 3 s, then less often.

#![no_std]

/// A channel message, either way.
pub const MSG_MAX: usize = 128;
/// One reply record.
pub const RECORD_MAX: usize = 121;
/// The args of a single-message request.
pub const ARGS_MAX: usize = MSG_MAX - 5;
/// One part of a multi-part request.
pub const CHUNK_MAX: usize = 121;
pub const PARTS_MAX: usize = 9;
/// The joined payload of a multi-part request.
pub const JOINED_MAX: usize = PARTS_MAX * CHUNK_MAX;
/// Text fields: the phone cuts them to this at a char boundary; also what one `api.text` call draws.
pub const TEXT_MAX: usize = 64;

// statuses
pub const OK: u8 = 0;
/// Unknown method or bad args; `reminders.complete` for a reminder that is no longer open.
pub const BAD_REQUEST: u8 = 1;
/// The user said no, or the OS permission is off.
pub const DENIED: u8 = 2;
/// No fix, no network, a timeout, or a method this phone can't do (reminders on Android, notifications and media on
/// iOS).
pub const UNAVAILABLE: u8 = 3;
/// The phone asked the user: show "Allow on your phone" and ask again soon.
pub const ASK: u8 = 4;
/// An `http.*` body over 2048 bytes, a `storage.put` over the quota.
pub const TOO_LARGE: u8 = 5;

/// Method ids, the byte after the request id.
pub mod method {
    pub const CAPS: u8 = 0x01;
    pub const CALENDAR_EVENTS: u8 = 0x10;
    pub const REMINDERS: u8 = 0x11;
    pub const REMINDERS_COMPLETE: u8 = 0x12;
    pub const LOCATION: u8 = 0x20;
    pub const WEATHER: u8 = 0x30;
    pub const HTTP_GET: u8 = 0x40;
    pub const HTTP_REQUEST: u8 = 0x41;
    pub const NOTIFICATIONS: u8 = 0x50;
    pub const NOTIFY: u8 = 0x51;
    pub const STORAGE_GET: u8 = 0x60;
    pub const STORAGE_PUT: u8 = 0x61;
    pub const STORAGE_DELETE: u8 = 0x62;
    pub const PHONE_BATTERY: u8 = 0x70;
    pub const MEDIA_NOW_PLAYING: u8 = 0x80;
    pub const MEDIA_CONTROL: u8 = 0x81;
    pub const JOB_SET: u8 = 0x90;
    pub const JOB_DELETE: u8 = 0x91;
}

/// `http.request` methods.
pub mod http {
    pub const GET: u8 = 0;
    pub const POST: u8 = 1;
    pub const PUT: u8 = 2;
    pub const PATCH: u8 = 3;
    pub const DELETE: u8 = 4;
}

/// `media.control` actions.
pub mod media {
    pub const PLAY: u8 = 0;
    pub const PAUSE: u8 = 1;
    pub const PLAY_PAUSE: u8 = 2;
    pub const NEXT: u8 = 3;
    pub const PREVIOUS: u8 = 4;
}

/// `notify` flags.
pub const NOTIFY_SOUND: u8 = 1;
pub const NOTIFY_FIND_PHONE: u8 = 2;

/// Record flags: bit 2 of calendar, reminder and notification records, the text stayed on the phone ("Show text on
/// the watch" is off for this app, the default).
pub const HIDDEN: u8 = 4;

// ---------------------------------------------------------------------------------------------------------------
// text

/// The longest valid UTF-8 prefix of `b` of at most `max` bytes (the phone's cut, and a guard against bad bytes).
pub fn utf8_cut(b: &[u8], max: usize) -> &str {
    let b = &b[..b.len().min(max)];
    match core::str::from_utf8(b) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// An owned text field of at most [`TEXT_MAX`] bytes of valid UTF-8.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Text {
    buf: [u8; TEXT_MAX],
    len: u8,
}

impl Text {
    pub const EMPTY: Text = Text { buf: [0; TEXT_MAX], len: 0 };

    /// `b` cut with [`utf8_cut`].
    pub fn new(b: &[u8]) -> Text {
        let s = utf8_cut(b, TEXT_MAX).as_bytes();
        let mut t = Text::EMPTY;
        t.buf[..s.len()].copy_from_slice(s);
        t.len = s.len() as u8;
        t
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for Text {
    fn default() -> Text {
        Text::EMPTY
    }
}

impl core::fmt::Debug for Text {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// requests

/// One message to send, `api.send(&msg)`.
#[derive(Clone, Copy)]
pub struct Msg {
    buf: [u8; MSG_MAX],
    len: u8,
}

impl Msg {
    /// `"R1Q" id method` and the pieces of the args; `None` if they don't fit.
    fn build(id: u8, method: u8, args: &[&[u8]]) -> Option<Msg> {
        let mut m = Msg { buf: [0; MSG_MAX], len: 5 };
        m.buf[..5].copy_from_slice(&[b'R', b'1', b'Q', id, method]);
        for a in args {
            let at = m.len as usize;
            m.buf.get_mut(at..at + a.len())?.copy_from_slice(a);
            m.len += a.len() as u8;
        }
        Some(m)
    }

    /// For args that always fit.
    fn small(id: u8, method: u8, args: &[u8]) -> Msg {
        Msg::build(id, method, &[args]).unwrap_or(Msg { buf: [0; MSG_MAX], len: 0 })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
}

impl core::ops::Deref for Msg {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Any single-message request; `None` if `args` is over [`ARGS_MAX`].
pub fn request(id: u8, method: u8, args: &[u8]) -> Option<Msg> {
    Msg::build(id, method, &[args])
}

/// `caps`: one record, the method ids this app may call now.
pub fn caps(id: u8) -> Msg {
    Msg::small(id, method::CAPS, &[])
}

/// `calendar.events`: `days` (1-7) from `from_day` (0 = the phone's today), at most `limit` (1-20) events.
pub fn calendar_events(id: u8, from_day: i8, days: u8, limit: u8) -> Msg {
    Msg::small(id, method::CALENDAR_EVENTS, &[from_day as u8, days, limit])
}

/// `reminders`: at most `limit` (1-20) open reminders, iOS only.
pub fn reminders(id: u8, limit: u8) -> Msg {
    Msg::small(id, method::REMINDERS, &[limit])
}

/// `reminders.complete`: marks reminder [`Reminder::id`] done.
pub fn reminders_complete(id: u8, reminder: u32) -> Msg {
    Msg::small(id, method::REMINDERS_COMPLETE, &reminder.to_le_bytes())
}

pub fn location(id: u8) -> Msg {
    Msg::small(id, method::LOCATION, &[])
}

/// `weather`: the current record, the hourly one for the next `hours` (0-24, over counts as 24), then daily ones
/// for `days` (0-7) days. `hours` 0 asks the way every companion understands; a companion from before hours
/// answers `BAD_REQUEST` (Android) or without the hourly record (iOS) to anything else.
pub fn weather(id: u8, days: u8, hours: u8) -> Msg {
    if hours == 0 {
        return Msg::small(id, method::WEATHER, &[days]);
    }
    Msg::small(id, method::WEATHER, &[days, hours.min(24)])
}

/// `http.get` of an `https://` URL of at most 123 bytes.
pub fn http_get(id: u8, url: &str) -> Option<Msg> {
    request(id, method::HTTP_GET, url.as_bytes())
}

/// `notifications`: at most `limit` (1-10), Android only.
pub fn notifications(id: u8, limit: u8) -> Msg {
    Msg::small(id, method::NOTIFICATIONS, &[limit])
}

/// `notify`: a notification on the phone. `flags`: [`NOTIFY_SOUND`], [`NOTIFY_FIND_PHONE`]. Title ≤ 64 bytes, title
/// and body ≤ 121 together. At most one per 10 s.
pub fn notify(id: u8, flags: u8, title: &str, body: &str) -> Option<Msg> {
    if title.len() > TEXT_MAX {
        return None;
    }
    Msg::build(id, method::NOTIFY, &[&[flags, title.len() as u8], title.as_bytes(), body.as_bytes()])
}

/// A `storage.*` key: 1-16 bytes of `A-Z a-z 0-9 . _ -`.
pub fn storage_key_ok(key: &[u8]) -> bool {
    (1..=16).contains(&key.len()) && key.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// `storage.get`: `OK` with no records for a key with no value, else `length:u16` then the value in chunks.
pub fn storage_get(id: u8, key: &str) -> Option<Msg> {
    storage_key_ok(key.as_bytes()).then(|| Msg::small(id, method::STORAGE_GET, key.as_bytes()))
}

pub fn storage_delete(id: u8, key: &str) -> Option<Msg> {
    storage_key_ok(key.as_bytes()).then(|| Msg::small(id, method::STORAGE_DELETE, key.as_bytes()))
}

pub fn phone_battery(id: u8) -> Msg {
    Msg::small(id, method::PHONE_BATTERY, &[])
}

/// `media.now_playing`, Android only.
pub fn media_now_playing(id: u8) -> Msg {
    Msg::small(id, method::MEDIA_NOW_PLAYING, &[])
}

/// `media.control` with a [`media`] action, Android only.
pub fn media_control(id: u8, action: u8) -> Msg {
    Msg::small(id, method::MEDIA_CONTROL, &[action])
}

pub fn job_delete(id: u8, job: u8) -> Msg {
    Msg::small(id, method::JOB_DELETE, &[job])
}

/// The parts of a multi-part request: `"R1Q" id method index total chunk`, in order. The phone answers after the
/// last one only. The outbox holds 4 messages: on `send` = -11 wait a moment and send the same part again.
#[derive(Clone)]
pub struct Parts<'a> {
    id: u8,
    method: u8,
    payload: &'a [u8],
    index: u8,
    total: u8,
}

impl Parts<'_> {
    pub fn total(&self) -> u8 {
        self.total
    }
}

impl Iterator for Parts<'_> {
    type Item = Msg;
    fn next(&mut self) -> Option<Msg> {
        if self.index >= self.total {
            return None;
        }
        let at = self.index as usize * CHUNK_MAX;
        let chunk = &self.payload[at..(at + CHUNK_MAX).min(self.payload.len())];
        let m = Msg::build(self.id, self.method, &[&[self.index, self.total], chunk]);
        self.index += 1;
        m
    }
}

/// Any multi-part request over a joined `payload` of at most [`JOINED_MAX`] bytes.
pub fn parts(id: u8, method: u8, payload: &[u8]) -> Option<Parts<'_>> {
    if payload.len() > JOINED_MAX {
        return None;
    }
    let total = payload.len().div_ceil(CHUNK_MAX).max(1) as u8;
    Some(Parts { id, method, payload, index: 0, total })
}

/// `method ‖ url_len ‖ url ‖ body` into `out`; its length.
fn http_payload(out: &mut [u8], method: u8, url: &str, body: &[u8]) -> Option<usize> {
    if method > http::DELETE || url.len() > 255 || !url.starts_with("https://") || (method == http::GET && !body.is_empty()) {
        return None;
    }
    let n = 2 + url.len() + body.len();
    let o = out.get_mut(..n)?;
    o[0] = method;
    o[1] = url.len() as u8;
    o[2..2 + url.len()].copy_from_slice(url.as_bytes());
    o[2 + url.len()..].copy_from_slice(body);
    Some(n)
}

/// `http.request`: any [`http`] method with a body; the payload is joined in `out`. Same reply as `http.get`.
pub fn http_request<'a>(id: u8, method: u8, url: &str, body: &[u8], out: &'a mut [u8; JOINED_MAX]) -> Option<Parts<'a>> {
    let n = http_payload(out, method, url, body)?;
    parts(id, method::HTTP_REQUEST, &out[..n])
}

/// `storage.put`: replaces the key's value (up to 1088 − key length bytes) in the app's store on the phone.
pub fn storage_put<'a>(id: u8, key: &str, value: &[u8], out: &'a mut [u8; JOINED_MAX]) -> Option<Parts<'a>> {
    let k = key.as_bytes();
    let n = 1 + k.len() + value.len();
    if !storage_key_ok(k) || n > JOINED_MAX {
        return None;
    }
    out[0] = k.len() as u8;
    out[1..1 + k.len()].copy_from_slice(k);
    out[1 + k.len()..n].copy_from_slice(value);
    parts(id, method::STORAGE_PUT, &out[..n])
}

/// What a background job asks the phone, as the request of the same name would.
#[derive(Clone, Copy, Debug)]
pub enum Job<'a> {
    Web { method: u8, url: &'a str, body: &'a [u8] },
    Weather { days: u8, hours: u8 },
    Calendar { from_day: i8, days: u8, limit: u8 },
}

/// `job.set` targets: the widget card (web jobs only) and the blob the app reads at start.
pub const JOB_WIDGET: u8 = 0;
pub const JOB_BLOB: u8 = 1;

/// `job.set`: job `job` (0-3) runs every `minutes` (15-1440) while the app is closed and the phone `PUSH`es its
/// result into `target`. Needs stub 8 on the watch; an older stub drops the result.
pub fn job_set<'a>(id: u8, job: u8, minutes: u16, what: Job<'_>, target: u8, out: &'a mut [u8; JOINED_MAX]) -> Option<Parts<'a>> {
    let web = matches!(what, Job::Web { .. });
    if job > 3 || target > JOB_BLOB || (target == JOB_WIDGET && !web) {
        return None;
    }
    out[0] = job;
    out[1..3].copy_from_slice(&minutes.to_le_bytes());
    out[4] = target;
    let n = match what {
        Job::Web { method, url, body } => {
            out[3] = 0;
            5 + http_payload(&mut out[5..], method, url, body)?
        }
        Job::Weather { days, hours } => {
            out[3] = 1;
            out[5] = days;
            out[6] = hours.min(24);
            if hours == 0 { 6 } else { 7 }
        }
        Job::Calendar { from_day, days, limit } => {
            out[3] = 2;
            out[5..8].copy_from_slice(&[from_day as u8, days, limit]);
            8
        }
    };
    parts(id, method::JOB_SET, &out[..n])
}

// ---------------------------------------------------------------------------------------------------------------
// replies

/// One reply message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reply<'a> {
    pub id: u8,
    pub status: u8,
    pub index: u8,
    pub total: u8,
    pub record: &'a [u8],
}

/// A reply, or `None` for anything else on the channel (the app's own messages, requests, short messages).
pub fn reply(m: &[u8]) -> Option<Reply<'_>> {
    match m {
        [b'R', b'1', b'A', id, status, index, total, record @ ..] => {
            Some(Reply { id: *id, status: *status, index: *index, total: *total, record })
        }
        _ => None,
    }
}

/// What a message meant for a [`Call`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Got<'a> {
    /// Not an answer to the open request: another prefix, an old id, a repeat.
    Nothing,
    /// The whole answer without records: `OK` with an empty result, or an error status.
    Status(u8),
    /// Record `index` of `total` (any order, each once); `last` when every one has come.
    Record { index: u8, total: u8, record: &'a [u8], last: bool },
}

/// First retry when the phone does not answer; it doubles up to the call's longest wait.
pub const RETRY_MS: u32 = 3_000;

/// One request at a time to the phone: the id of the open one, the records that came for it, and when to ask
/// (again). Times are `api.now()` ms. Answers carry no link-level id, so anything for an older id is dropped.
#[derive(Clone, Copy, Debug)]
pub struct Call {
    id: u8,
    open: bool,
    silent: bool,
    next: Option<u32>,
    backoff: u32,
    longest: u32,
    total: u8,
    got: u32,
}

fn reached(now: u32, at: u32) -> bool {
    (now.wrapping_sub(at) as i32) >= 0
}

impl Call {
    /// Nothing scheduled; retries back off up to `longest` ms.
    pub const fn new(longest: u32) -> Call {
        Call { id: 0, open: false, silent: false, next: None, backoff: RETRY_MS, longest, total: 0, got: 0 }
    }

    /// Ask at once, with a fresh back-off (start, `RESUME`, something to send).
    pub fn now(&mut self, now: u32) {
        self.open = false;
        self.backoff = RETRY_MS;
        self.next = Some(now);
    }

    /// Ask again `ms` from `now` (after an answer).
    pub fn after(&mut self, now: u32, ms: u32) {
        self.next = Some(now.wrapping_add(ms));
    }

    /// Nothing more to ask.
    pub fn stop(&mut self) {
        self.open = false;
        self.next = None;
    }

    /// The id to send the request with now, if one is due; a new id from `ids`, the app's counter shared by its
    /// calls. A request still unanswered then makes the phone [`Call::silent`] and doubles the wait.
    pub fn due(&mut self, now: u32, ids: &mut u8) -> Option<u8> {
        if !reached(now, self.next?) {
            return None;
        }
        if self.open {
            self.silent = true;
            self.backoff = (self.backoff * 2).min(self.longest);
        }
        *ids = ids.wrapping_add(1);
        self.id = *ids;
        self.open = true;
        self.got = 0;
        self.total = 0;
        self.next = Some(now.wrapping_add(self.backoff));
        Some(self.id)
    }

    /// Ms until [`Call::due`] has something to do, `u32::MAX` for never.
    pub fn wait(&self, now: u32) -> u32 {
        self.next.map_or(u32::MAX, |at| (at.wrapping_sub(now) as i32).max(0) as u32)
    }

    /// The last request went unanswered; cleared by the next answer.
    pub fn silent(&self) -> bool {
        self.silent
    }

    /// Waiting for an answer.
    pub fn open(&self) -> bool {
        self.open
    }

    /// One message from `api.recv`. On [`Got::Status`] or the last [`Got::Record`] the call is answered and
    /// nothing is scheduled: [`Call::after`] or [`Call::stop`] next. Answers of more than 32 records are dropped.
    pub fn take<'a>(&mut self, m: &'a [u8]) -> Got<'a> {
        let Some(r) = reply(m) else { return Got::Nothing };
        if !self.open || r.id != self.id {
            return Got::Nothing;
        }
        if r.status != OK || r.total == 0 {
            self.answered();
            return Got::Status(r.status);
        }
        let bit = 1u32.checked_shl(r.index as u32).unwrap_or(0);
        if r.index >= r.total || r.total > 32 || (self.got != 0 && r.total != self.total) || self.got & bit != 0 {
            return Got::Nothing;
        }
        self.total = r.total;
        self.got |= bit;
        let last = self.got == u32::MAX >> (32 - r.total as u32);
        if last {
            self.answered();
        }
        Got::Record { index: r.index, total: r.total, record: r.record, last }
    }

    fn answered(&mut self) {
        self.open = false;
        self.silent = false;
        self.backoff = RETRY_MS;
        self.next = None;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// records

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// `len:u8 ‖ text`.
    fn text(&mut self) -> Option<Text> {
        let n = self.u8()? as usize;
        Some(Text::new(self.take(n)?))
    }
}

/// `calendar.events` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Event {
    /// Days from the phone's today at the time of the answer.
    pub day: i8,
    /// Minute of the day, local; 0 for all-day.
    pub start: u16,
    /// Real minutes from start to end; all-day: 1440 × days.
    pub duration: u16,
    /// [`Event::ALL_DAY`], [`Event::TENTATIVE`], [`HIDDEN`].
    pub flags: u8,
    /// The calendar's colour, RGB565.
    pub color: u16,
    /// Empty with [`HIDDEN`].
    pub title: Text,
}

impl Event {
    pub const ALL_DAY: u8 = 1;
    pub const TENTATIVE: u8 = 2;
    const HEAD: usize = 8;
    /// The longest record, for [`Event::encode`].
    pub const RECORD_MAX: usize = Event::HEAD + TEXT_MAX;

    pub fn decode(r: &[u8]) -> Option<Event> {
        let mut r = Rd(r);
        Some(Event {
            day: r.u8()? as i8,
            start: r.u16()?,
            duration: r.u16()?,
            flags: r.u8()?,
            color: r.u16()?,
            title: Text::new(r.0),
        })
    }

    /// The record again, for a cache; `out` holds [`Event::RECORD_MAX`]. Its length.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        out[0] = self.day as u8;
        out[1..3].copy_from_slice(&self.start.to_le_bytes());
        out[3..5].copy_from_slice(&self.duration.to_le_bytes());
        out[5] = self.flags;
        out[6..8].copy_from_slice(&self.color.to_le_bytes());
        let t = self.title.as_str().as_bytes();
        out[Event::HEAD..Event::HEAD + t.len()].copy_from_slice(t);
        Event::HEAD + t.len()
    }
}

/// `reminders` record: an open reminder on the iPhone.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Reminder {
    /// What `reminders.complete` takes; stable while the reminder lives.
    pub id: u32,
    /// Due date in days from the phone's today, negative when overdue; 0 without one (see [`Reminder::DUE`]).
    pub day: i8,
    /// Due time, minute of the day, local; 0 without one (see [`Reminder::TIMED`]).
    pub minute: u16,
    /// [`Reminder::DUE`], [`Reminder::TIMED`], [`HIDDEN`].
    pub flags: u8,
    /// The list's colour, RGB565.
    pub color: u16,
    /// Empty with [`HIDDEN`].
    pub title: Text,
}

impl Reminder {
    pub const DUE: u8 = 1;
    pub const TIMED: u8 = 2;
    const HEAD: usize = 10;
    pub const RECORD_MAX: usize = Reminder::HEAD + TEXT_MAX;

    pub fn due(&self) -> bool {
        self.flags & Reminder::DUE != 0
    }

    pub fn timed(&self) -> bool {
        self.flags & Reminder::TIMED != 0
    }

    pub fn decode(r: &[u8]) -> Option<Reminder> {
        let mut r = Rd(r);
        Some(Reminder {
            id: r.u32()?,
            day: r.u8()? as i8,
            minute: r.u16()?,
            flags: r.u8()?,
            color: r.u16()?,
            title: Text::new(r.0),
        })
    }

    /// The record again, for a cache; `out` holds [`Reminder::RECORD_MAX`]. Its length.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        out[..4].copy_from_slice(&self.id.to_le_bytes());
        out[4] = self.day as u8;
        out[5..7].copy_from_slice(&self.minute.to_le_bytes());
        out[7] = self.flags;
        out[8..10].copy_from_slice(&self.color.to_le_bytes());
        let t = self.title.as_str().as_bytes();
        out[Reminder::HEAD..Reminder::HEAD + t.len()].copy_from_slice(t);
        Reminder::HEAD + t.len()
    }
}

/// `location` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Location {
    /// 0.1°.
    pub lat: i16,
    pub lon: i16,
    /// Minutes since the fix, [`Location::OLD`] for older.
    pub age: u16,
    /// The locality, ≤ 32 bytes, may be empty.
    pub place: Text,
}

impl Location {
    pub const OLD: u16 = 0xFFFF;

    pub fn decode(r: &[u8]) -> Option<Location> {
        let mut r = Rd(r);
        Some(Location { lat: r.u16()? as i16, lon: r.u16()? as i16, age: r.u16()?, place: Text::new(r.0) })
    }
}

/// One hour of `Weather::Hours`: `temp` °C, `code` WMO, `precip` probability %, `night` for a moon instead of a sun.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hour {
    pub temp: i8,
    pub code: u8,
    pub precip: u8,
    pub night: bool,
}

/// `weather` records: the current one first, then the hourly one if hours were asked, then one per day.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weather {
    /// `temp` 0.1 °C, `code` WMO, `wind` m/s, `humidity` %, `age` minutes since fetched (≤ 255).
    Now { temp: i16, code: u8, wind: u8, humidity: u8, age: u8 },
    /// `day` 0 = today, `min`/`max` °C, `precip` probability %.
    Day { day: i8, min: i8, max: i8, code: u8, precip: u8 },
    /// `n` (≤ 24) consecutive hours from `hour`, the local hour (0-23) that contains now: read them with `hour(i)`.
    Hours { hour: u8, n: u8, data: [u8; 72] },
}

impl Weather {
    pub fn decode(r: &[u8]) -> Option<Weather> {
        let mut r = Rd(r);
        match r.u8()? {
            0 => Some(Weather::Now { temp: r.u16()? as i16, code: r.u8()?, wind: r.u8()?, humidity: r.u8()?, age: r.u8()? }),
            1 => Some(Weather::Day { day: r.u8()? as i8, min: r.u8()? as i8, max: r.u8()? as i8, code: r.u8()?, precip: r.u8()? }),
            2 => {
                let hour = r.u8()?;
                let n = (r.0.len() / 3).min(24); // a cut entry is dropped
                let mut data = [0; 72];
                data[..n * 3].copy_from_slice(&r.0[..n * 3]);
                Some(Weather::Hours { hour, n: n as u8, data })
            }
            _ => None,
        }
    }

    /// Hour `i` of an hourly record, 0 the one that contains now.
    pub fn hour(&self, i: usize) -> Option<Hour> {
        match self {
            Weather::Hours { n, data, .. } if i < *n as usize => {
                let e = &data[i * 3..];
                Some(Hour { temp: e[0] as i8, code: e[1], precip: e[2] & 0x7F, night: e[2] & 0x80 != 0 })
            }
            _ => None,
        }
    }
}

/// The first record of `http.get` and `http.request`; the body follows in records of 121 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HttpHead {
    pub code: u16,
    pub length: u16,
}

impl HttpHead {
    pub fn decode(r: &[u8]) -> Option<HttpHead> {
        let mut r = Rd(r);
        Some(HttpHead { code: r.u16()?, length: r.u16()? })
    }
}

/// The first record of `storage.get`: the value's length; the value follows in records of 121 bytes.
pub fn storage_length(r: &[u8]) -> Option<u16> {
    Rd(r).u16()
}

/// `notifications` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Notification {
    /// Minutes since posted.
    pub age: u16,
    /// [`HIDDEN`].
    pub flags: u8,
    /// The app's label, ≤ 24 bytes.
    pub app: Text,
    /// "title: text", empty with [`HIDDEN`].
    pub text: Text,
}

impl Notification {
    pub fn decode(r: &[u8]) -> Option<Notification> {
        let mut r = Rd(r);
        Some(Notification { age: r.u16()?, flags: r.u8()?, app: r.text()?, text: Text::new(r.0) })
    }
}

/// `phone.battery` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    /// 0-100 %.
    pub level: u8,
    /// Charging or full.
    pub plugged: bool,
}

impl Battery {
    pub fn decode(r: &[u8]) -> Option<Battery> {
        let mut r = Rd(r);
        Some(Battery { level: r.u8()?, plugged: r.u8()? & 1 != 0 })
    }
}

/// `media.now_playing` record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NowPlaying {
    pub playing: bool,
    /// Seconds, [`NowPlaying::UNKNOWN`] for a live stream.
    pub position: u16,
    pub duration: u16,
    /// The player's label, ≤ 24 bytes.
    pub app: Text,
    /// ≤ 48 bytes.
    pub title: Text,
    /// ≤ 32 bytes.
    pub artist: Text,
}

impl NowPlaying {
    pub const UNKNOWN: u16 = 0xFFFF;

    pub fn decode(r: &[u8]) -> Option<NowPlaying> {
        let mut r = Rd(r);
        Some(NowPlaying {
            playing: r.u8()? & 1 != 0,
            position: r.u16()?,
            duration: r.u16()?,
            app: r.text()?,
            title: r.text()?,
            artist: Text::new(r.0),
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    fn msg(id: u8, status: u8, index: u8, total: u8, record: &[u8]) -> [u8; 9] {
        let mut m = [b'R', b'1', b'A', id, status, index, total, 0, 0];
        m[7..7 + record.len()].copy_from_slice(record);
        m
    }

    #[test]
    fn call_collects_any_order_and_drops_stale_and_repeats() {
        let (mut c, mut ids) = (Call::new(60_000), 0u8);
        assert_eq!(c.due(0, &mut ids), None); // nothing scheduled
        c.now(0);
        assert_eq!(c.due(0, &mut ids), Some(1));
        assert_eq!(c.take(&msg(0, OK, 0, 0, &[])), Got::Nothing); // older id
        assert_eq!(c.take(b"hi"), Got::Nothing);
        assert!(matches!(c.take(&msg(1, OK, 1, 2, &[7, 0])), Got::Record { index: 1, last: false, .. }));
        assert_eq!(c.take(&msg(1, OK, 1, 2, &[7, 0])), Got::Nothing); // a repeat
        assert_eq!(c.take(&msg(1, OK, 2, 2, &[7, 0])), Got::Nothing); // index past total
        assert!(matches!(c.take(&msg(1, OK, 0, 2, &[7, 0])), Got::Record { index: 0, last: true, .. }));
        assert!(!c.open() && c.wait(5) == u32::MAX);
        assert_eq!(c.take(&msg(1, OK, 0, 0, &[])), Got::Nothing); // answered already
    }

    #[test]
    fn call_backs_off_while_silent() {
        let (mut c, mut ids) = (Call::new(10_000), 250u8);
        c.now(0);
        assert_eq!(c.due(0, &mut ids), Some(251));
        assert_eq!(c.due(RETRY_MS - 1, &mut ids), None);
        assert_eq!(c.due(RETRY_MS, &mut ids), Some(252));
        assert!(c.silent());
        assert_eq!(c.wait(RETRY_MS), 2 * RETRY_MS);
        c.due(3 * RETRY_MS, &mut ids);
        assert_eq!(c.wait(3 * RETRY_MS), 10_000); // capped
        for _ in 0..5 {
            c.due(u32::MAX / 2, &mut ids); // ids wrap
        }
        assert_eq!(c.take(&msg(ids, ASK, 0, 0, &[])), Got::Status(ASK));
        assert!(!c.silent());
        c.after(100, 10_000);
        assert_eq!(c.wait(100), 10_000);
    }

    #[test]
    fn requests_that_dont_fit_are_refused() {
        let long = String::from("https://e.com/") + &"a".repeat(ARGS_MAX - 14);
        assert_eq!(http_get(1, &long).unwrap().len(), MSG_MAX);
        assert!(http_get(1, &(long + "a")).is_none());
        assert!(notify(1, 0, &"x".repeat(65), "").is_none());
        assert!(storage_get(1, "a/b").is_none());
        let mut out = [0u8; JOINED_MAX];
        assert!(http_request(1, http::GET, "https://e.com", b"x", &mut out).is_none());
        assert!(http_request(1, 5, "https://e.com", b"", &mut out).is_none());
        assert!(http_request(1, http::GET, "http://e.com", b"", &mut out).is_none());
        assert!(storage_put(1, "k", &[0; JOINED_MAX - 2], &mut out).is_some_and(|p| p.total() == 9));
        assert!(storage_put(1, "k", &[0; JOINED_MAX - 1], &mut out).is_none());
        assert!(job_set(1, 0, 30, Job::Weather { days: 3, hours: 0 }, JOB_WIDGET, &mut out).is_none());
        assert!(job_set(1, 4, 30, Job::Weather { days: 3, hours: 0 }, JOB_BLOB, &mut out).is_none());
    }

    #[test]
    fn utf8_cut_keeps_whole_chars() {
        assert_eq!(utf8_cut("ab€".as_bytes(), 4), "ab");
        assert_eq!(utf8_cut(b"ok\xff!", 64), "ok");
        assert_eq!(Text::new("Привет".as_bytes()).as_str(), "Привет");
    }
}
