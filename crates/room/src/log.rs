// The diagnostic log, written only when Booth was started with --log. The
// room's threads format a line and hand it over without waiting; a thread of
// its own owns the file. Nothing secret goes in: no invite codes, ids or
// secrets, no keys (people are named by fingerprint), no message content.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use invite::{Candidate, CandidateKind, Mapping};
use session::InitKind;

use crate::limit;
use crate::view::PathWord;

// Lines waiting for the writer. Past this many a new line is dropped, so a
// slow disk costs lines and never holds up a packet.
const WAITING_LINES: usize = 1024;
const ROTATE_AFTER: u64 = 1024 * 1024;
// leave() runs on the panel's thread and has 200 ms for everything. A writer
// stuck in a write (a stalled USB or network drive, a file held by a virus
// scanner) gets this long in all, then finishes on its own.
const STOP_WAIT: Duration = Duration::from_millis(100);

// Checks the log before anything is formatted, so with logging off a line
// costs one branch and its arguments are never evaluated.
macro_rules! log {
    ($log:expr, $($arg:tt)+) => {
        if $log.is_on() {
            $log.line(format!($($arg)+));
        }
    };
}
pub(crate) use log;

enum ToWriter {
    Line(SystemTime, String),
    Stop,
}

#[derive(Clone, Default)]
pub(crate) struct Log {
    out: Option<Arc<Out>>,
}

struct Out {
    lines: Sender<ToWriter>,
    dropped: Arc<AtomicU64>,
}

impl Log {
    pub(crate) fn off() -> Log {
        Log::default()
    }

    pub(crate) fn is_on(&self) -> bool {
        self.out.is_some()
    }

    pub(crate) fn line(&self, text: String) {
        let Some(out) = &self.out else {
            return;
        };
        if out
            .lines
            .try_send(ToWriter::Line(SystemTime::now(), text))
            .is_err()
        {
            out.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl fmt::Debug for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Log").field("on", &self.is_on()).finish()
    }
}

pub(crate) struct Writer {
    thread: Option<JoinHandle<()>>,
    lines: Sender<ToWriter>,
    // Nothing is ever sent on it. It disconnects when the writer thread
    // returns, which a join cannot wait for with a time limit.
    done: Receiver<()>,
}

impl Writer {
    // Called after the room's own threads have stopped, so their last lines
    // are in the file.
    pub(crate) fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + STOP_WAIT;
        if self.lines.send_deadline(ToWriter::Stop, deadline).is_err() {
            return;
        }
        if let Err(RecvTimeoutError::Disconnected) = self.done.recv_deadline(deadline) {
            let _ = thread.join();
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop();
    }
}

// `role` is the word every line carries, "host" or "client".
pub(crate) fn open(path: Option<&Path>, role: &'static str) -> io::Result<(Log, Option<Writer>)> {
    let Some(path) = path else {
        return Ok((Log::off(), None));
    };
    let file = LogFile::open(path, role, ROTATE_AFTER)?;
    let (lines, waiting) = crossbeam_channel::bounded(WAITING_LINES);
    let (finished, done) = crossbeam_channel::bounded(0);
    let dropped = Arc::new(AtomicU64::new(0));
    let thread = thread::Builder::new().name("room log".into()).spawn({
        let dropped = Arc::clone(&dropped);
        move || {
            write_lines(file, &waiting, &dropped);
            drop(finished);
        }
    })?;
    let log = Log {
        out: Some(Arc::new(Out {
            lines: lines.clone(),
            dropped,
        })),
    };
    let writer = Writer {
        thread: Some(thread),
        lines,
        done,
    };
    Ok((log, Some(writer)))
}

fn write_lines(mut file: LogFile, waiting: &Receiver<ToWriter>, dropped: &AtomicU64) {
    while let Ok(first) = waiting.recv() {
        let mut stop = false;
        for entry in std::iter::once(first).chain(waiting.try_iter()) {
            match entry {
                ToWriter::Line(at, text) => file.write(at, &text),
                ToWriter::Stop => {
                    stop = true;
                    break;
                }
            }
        }
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            file.write(
                SystemTime::now(),
                &format!("{lost} lines left out here, the log writer fell behind"),
            );
        }
        // Once per batch: a crash loses at most the lines of the last one.
        file.flush();
        if stop {
            return;
        }
    }
}

struct LogFile {
    path: PathBuf,
    old: PathBuf,
    role: &'static str,
    out: BufWriter<File>,
    size: u64,
    rotate_after: u64,
}

impl LogFile {
    fn open(path: &Path, role: &'static str, rotate_after: u64) -> io::Result<LogFile> {
        let file = append(path)?;
        let size = file.metadata().map_or(0, |meta| meta.len());
        let mut old = OsString::from(path);
        old.push(".1");
        Ok(LogFile {
            path: path.to_path_buf(),
            old: PathBuf::from(old),
            role,
            out: BufWriter::new(file),
            size,
            rotate_after,
        })
    }

    fn write(&mut self, at: SystemTime, text: &str) {
        self.put(at, text);
        if self.size > self.rotate_after {
            self.rotate();
        }
    }

    // A write that fails (a full disk) loses that line; there is nowhere
    // better to say so than the file that failed.
    fn put(&mut self, at: SystemTime, text: &str) {
        let line = format!("{} {:<6} {}\r\n", stamp(at), self.role, printable(text));
        if self.out.write_all(line.as_bytes()).is_ok() {
            self.size += line.len() as u64;
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    fn rotate(&mut self) {
        self.flush();
        let path = self.path.display().to_string();
        let old = self.old.display().to_string();
        let note = match fs::rename(&self.path, &self.old) {
            Ok(()) => match append(&self.path) {
                Ok(file) => {
                    self.out = BufWriter::new(file);
                    None
                }
                Err(err) => Some(format!(
                    "moved the log to {old}, could not start a new {path}: {err}; still writing to {old}"
                )),
            },
            Err(moved) => {
                // Not there: deleted by hand while Booth ran, so the lines
                // since then went nowhere. Anything else is most likely the
                // old file held open by another program, and starting this
                // one over still keeps the log from growing without end.
                let gone = moved.kind() == io::ErrorKind::NotFound;
                let fresh = if gone {
                    append(&self.path)
                } else {
                    OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&self.path)
                };
                match fresh {
                    Ok(file) => {
                        self.out = BufWriter::new(file);
                        Some(if gone {
                            format!("{path} was not there to move to {old}; started a new one")
                        } else {
                            format!("could not move the log to {old}: {moved}; started {path} over")
                        })
                    }
                    Err(err) => Some(format!(
                        "could not move the log to {old}: {moved}; could not reopen {path}: {err}; still writing to the old file"
                    )),
                }
            }
        };
        // Counted from zero even when nothing worked, so the next try comes
        // after another rotate_after bytes and not on every line. The note
        // goes in without the size check: a note longer than rotate_after
        // would otherwise rotate again, and fail again, without end.
        self.size = 0;
        if let Some(note) = note {
            self.put(SystemTime::now(), &note);
        }
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

// "2026-09-24T21:12:03.456Z"
fn stamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "{}.{:03}Z",
        utc_seconds(since.as_secs()),
        since.subsec_millis()
    )
}

// "2026-09-24T21:12:03", no zone letter.
fn utc_seconds(unix: u64) -> String {
    let (year, month, day) = civil_from_days((unix / 86_400) as i64);
    let second = unix % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second / 3600,
        second / 60 % 60,
        second % 60
    )
}

// "2026-09-24T21:22:03Z"
pub(crate) fn utc(unix: u64) -> String {
    format!("{}Z", utc_seconds(unix))
}

// Howard Hinnant's days-from-civil run backwards: count in 400-year eras of
// 146097 days, with years starting on 1 March so the leap day is the last
// day of its year.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

pub(crate) fn init_word(kind: InitKind) -> &'static str {
    match kind {
        InitKind::Invite(_) => "invite",
        InitKind::Known => "known",
        InitKind::Rekey => "rekey",
    }
}

pub(crate) fn kind_word(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Lan => "lan",
        CandidateKind::Vpn => "vpn",
        CandidateKind::Ipv6 => "ipv6",
        CandidateKind::Public => "public",
    }
}

pub(crate) fn mapping_word(mapping: Mapping) -> &'static str {
    match mapping {
        Mapping::Easy => "easy",
        Mapping::Hard => "hard",
        Mapping::Unknown => "unknown",
    }
}

pub(crate) fn path_text(path: PathWord) -> &'static str {
    match path {
        PathWord::Lan => "lan",
        PathWord::Direct => "direct",
    }
}

pub(crate) fn candidates(candidates: &[Candidate]) -> String {
    let items: Vec<String> = candidates
        .iter()
        .map(|c| format!("{} {}", kind_word(c.kind), c.addr))
        .collect();
    items.join(", ")
}

pub(crate) fn yes_no(yes: bool) -> &'static str {
    if yes { "yes" } else { "no" }
}

// "1 address", "2 addresses".
pub(crate) fn counted(count: u64, one: &str, many: &str) -> String {
    let word = if count == 1 { one } else { many };
    format!("{count} {word}")
}

pub(crate) fn secs(span: Duration) -> String {
    format!("{:.1} s", span.as_secs_f32())
}

// How long a chat message took, with "about" when the link's jitter makes
// the clock offset behind it uncertain. Never the text.
pub(crate) fn delivery(delivery: Option<(f32, bool)>) -> String {
    match delivery {
        Some((ms, false)) => format!("delivery {ms:.1} ms"),
        Some((ms, true)) => format!("delivery about {ms:.1} ms"),
        None => String::from("delivery not measured"),
    }
}

pub(crate) fn list<T: fmt::Display>(items: &[T]) -> String {
    let items: Vec<String> = items.iter().map(T::to_string).collect();
    items.join(", ")
}

// Names come from other people and adapter names from Windows in the
// user's language; the file stays plain ASCII.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

// A name in quotes. A friend picks their own name, and a quote in it must
// not close the quotes and make the rest read as another event.
pub(crate) fn quoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    printable(&out)
}

const MINUTE: Duration = Duration::from_secs(60);

// A count kept elsewhere that only goes up, written down as it grows but at
// most once a minute, so a stream of whatever it counts cannot push the rest
// of the log out of the file.
#[derive(Default)]
pub(crate) struct Tally {
    written: u64,
    quiet_until: Option<Instant>,
}

impl Tally {
    // How much it grew since the last line, when a line is due now.
    pub(crate) fn due(&mut self, total: u64, now: Instant) -> Option<u64> {
        if total <= self.written || self.quiet_until.is_some_and(|until| now < until) {
            return None;
        }
        self.quiet_until = Some(now + MINUTE);
        Some(self.take(total))
    }

    // Whatever grew since the last line, due or not: the room is closing.
    pub(crate) fn rest(&mut self, total: u64) -> Option<u64> {
        (total > self.written).then(|| self.take(total))
    }

    fn take(&mut self, total: u64) -> u64 {
        let grown = total - self.written;
        self.written = total;
        grown
    }
}

const LINES_PER_MINUTE: u32 = 20;
// For all sources together. Past it a flood from many addresses, spoofed ones
// too, is only counted, so the lines the log is for (the bind, STUN, the
// invite, a friend's first try) are still in the file when someone reads it.
const ALL_LINES_PER_MINUTE: u32 = 100;
// Only windows whose minute has ended make way. A new source that finds the
// table full of live ones is counted with the sources over the shared limit;
// giving it the seat of one still inside its minute would hand that one a
// fresh allowance when it comes back.
const MAX_SOURCES: usize = 256;

// At most LINES_PER_MINUTE lines a minute about the packets of one source,
// and ALL_LINES_PER_MINUTE about all of them; the rest are counted and summed
// up in one line when their minute ends. IPv6 sources are counted by /64, as
// the handshake rate limit does.
pub(crate) struct PerSource {
    windows: HashMap<IpAddr, Window>,
    all: Window,
    // When the first minute with packets left out ends.
    due: Option<Instant>,
    // No window in the table ends its minute before this, so a full table is
    // not walked again for one that has.
    next_sweep: Option<Instant>,
}

struct Window {
    started: Instant,
    lines: u32,
    left_out: u64,
}

impl Window {
    fn new(now: Instant) -> Window {
        Window {
            started: now,
            lines: 0,
            left_out: 0,
        }
    }

    fn ended(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= MINUTE
    }
}

impl PerSource {
    pub(crate) fn new(now: Instant) -> PerSource {
        PerSource {
            windows: HashMap::new(),
            all: Window::new(now),
            due: None,
            next_sweep: None,
        }
    }

    // True when a line about this packet may be written. Past the shared
    // limit this is a compare and an add: no table, no formatting.
    pub(crate) fn allow(&mut self, ip: IpAddr, now: Instant, log: &Log) -> bool {
        if self.all.ended(now) {
            self.end_all(now, log);
        }
        if self.all.lines >= ALL_LINES_PER_MINUTE {
            self.leave_out_all();
            return false;
        }
        let source = limit::source(ip);
        if !self.windows.contains_key(&source)
            && self.windows.len() >= MAX_SOURCES
            && !self.sweep(now, log)
        {
            self.leave_out_all();
            return false;
        }
        let window = self
            .windows
            .entry(source)
            .or_insert_with(|| Window::new(now));
        if window.ended(now) {
            left_out(log, source, window.left_out);
            *window = Window::new(now);
        }
        if window.lines < LINES_PER_MINUTE {
            window.lines += 1;
            self.all.lines += 1;
            return true;
        }
        window.left_out += 1;
        if window.left_out == 1 {
            let ends = window.started + MINUTE;
            self.due_by(ends);
        }
        false
    }

    pub(crate) fn tick(&mut self, now: Instant, log: &Log) {
        if self.due.is_none_or(|due| now < due) {
            return;
        }
        if self.all.ended(now) {
            self.end_all(now, log);
        }
        self.drop_ended(now, log);
        self.due = self
            .windows
            .values()
            .chain([&self.all])
            .filter(|window| window.left_out > 0)
            .map(|window| window.started + MINUTE)
            .min();
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.due
    }

    // The room is closing: counts whose minute has not ended yet would
    // otherwise never be written.
    pub(crate) fn flush(&mut self, now: Instant, log: &Log) {
        for (source, window) in &mut self.windows {
            left_out(log, *source, window.left_out);
            window.left_out = 0;
        }
        self.end_all(now, log);
        self.due = None;
    }

    fn leave_out_all(&mut self) {
        self.all.left_out += 1;
        if self.all.left_out == 1 {
            self.due_by(self.all.started + MINUTE);
        }
    }

    fn due_by(&mut self, at: Instant) {
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }

    fn end_all(&mut self, now: Instant, log: &Log) {
        let count = self.all.left_out;
        if count > 0 {
            log!(
                log,
                "{count} more packets in the last minute, past the {ALL_LINES_PER_MINUTE} lines a minute kept for all sources together"
            );
        }
        self.all = Window::new(now);
    }

    // True when that made room.
    fn sweep(&mut self, now: Instant, log: &Log) -> bool {
        if self.next_sweep.is_some_and(|at| now < at) {
            return false;
        }
        self.drop_ended(now, log);
        self.next_sweep = self
            .windows
            .values()
            .map(|window| window.started + MINUTE)
            .min();
        self.windows.len() < MAX_SOURCES
    }

    fn drop_ended(&mut self, now: Instant, log: &Log) {
        self.windows.retain(|source, window| {
            let ended = window.ended(now);
            if ended {
                left_out(log, *source, window.left_out);
            }
            !ended
        });
    }
}

fn left_out(log: &Log, source: IpAddr, count: u64) {
    if count == 0 {
        return;
    }
    match source {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            log!(log, "{count} more packets from {v6}/64 in the last minute");
        }
        ip => log!(log, "{count} more packets from {ip} in the last minute"),
    }
}

#[cfg(test)]
pub(crate) struct Captured(Receiver<ToWriter>);

#[cfg(test)]
impl Captured {
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0
            .try_iter()
            .filter_map(|entry| match entry {
                ToWriter::Line(_, text) => Some(text),
                ToWriter::Stop => None,
            })
            .collect()
    }
}

#[cfg(test)]
impl Log {
    // A log whose lines the test reads back instead of a file.
    pub(crate) fn capture(capacity: usize) -> (Log, Captured) {
        let (lines, waiting) = crossbeam_channel::bounded(capacity);
        let log = Log {
            out: Some(Arc::new(Out {
                lines,
                dropped: Arc::new(AtomicU64::new(0)),
            })),
        };
        (log, Captured(waiting))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const AT: Duration = Duration::from_millis(1_790_284_323_456);

    fn v4(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0xCB00_7100 + n))
    }

    fn fresh_dir(what: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("booth-log-test-{}-{what}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn days_turn_into_dates() {
        for (days, date) in [
            (0, (1970, 1, 1)),
            (-1, (1969, 12, 31)),
            (11_016, (2000, 2, 29)),
            (11_017, (2000, 3, 1)),
            (19_782, (2024, 2, 29)),
            (19_783, (2024, 3, 1)),
            // 2100 is not a leap year.
            (47_540, (2100, 2, 28)),
            (47_541, (2100, 3, 1)),
        ] {
            assert_eq!(civil_from_days(days), date, "day {days}");
        }
    }

    #[test]
    fn stamps_are_utc_with_milliseconds() {
        assert_eq!(stamp(UNIX_EPOCH + AT), "2026-09-24T21:12:03.456Z");
        assert_eq!(stamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(1_709_164_800 + 86_399), "2024-02-29T23:59:59Z");
    }

    #[test]
    fn lines_are_plain_ascii() {
        assert_eq!(printable("Wi-Fi 2"), "Wi-Fi 2");
        assert_eq!(printable("\u{634}\u{627}\u{62f}\u{64a}"), "????");
        assert_eq!(printable("a\r\nb\tc"), "a??b?c");
    }

    #[test]
    fn a_name_cannot_close_its_quotes() {
        assert_eq!(quoted("Ana"), r#""Ana""#);
        assert_eq!(quoted(r#"x": left, said bye"#), r#""x\": left, said bye""#);
        assert_eq!(quoted(r"a\b"), r#""a\\b""#);
        assert_eq!(quoted("\u{634}\n"), r#""??""#);
    }

    #[test]
    fn a_log_that_is_off_formats_nothing() {
        let off = Log::off();
        let mut evaluated = 0;
        log!(off, "{}", {
            evaluated += 1;
            evaluated
        });
        assert_eq!(evaluated, 0);
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        let (log, captured) = Log::capture(2);
        for n in 0..5 {
            log!(log, "line {n}");
        }
        let dropped = log.out.as_ref().unwrap().dropped.load(Ordering::Relaxed);
        assert_eq!(dropped, 3);
        assert_eq!(captured.lines(), ["line 0", "line 1"]);
    }

    #[test]
    fn a_tally_is_written_at_most_once_a_minute() {
        let mut tally = Tally::default();
        let start = Instant::now();
        assert_eq!(tally.due(0, start), None);
        assert_eq!(tally.due(1, start), Some(1));
        for n in 2..2000 {
            let now = start + Duration::from_millis(n);
            assert_eq!(tally.due(n, now), None, "at {n}");
        }
        assert_eq!(tally.due(2000, start + MINUTE), Some(1999));
        // Nothing new is nothing to say, and the next growth goes in at once.
        assert_eq!(tally.due(2000, start + MINUTE * 3), None);
        assert_eq!(tally.due(2005, start + MINUTE * 3), Some(5));
        assert_eq!(tally.due(2006, start + MINUTE * 3), None);
        assert_eq!(tally.rest(2006), Some(1));
        assert_eq!(tally.rest(2006), None);
    }

    #[test]
    fn one_source_gets_twenty_lines_a_minute() {
        let (log, captured) = Log::capture(64);
        let start = Instant::now();
        let mut sources = PerSource::new(start);
        let noisy = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let quiet = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));
        let allowed = (0..50)
            .filter(|_| sources.allow(noisy, start, &log))
            .count();
        assert_eq!(allowed, 20);
        assert!(sources.allow(quiet, start, &log));
        assert_eq!(sources.next_deadline(), Some(start + MINUTE));

        sources.tick(start + Duration::from_secs(59), &log);
        assert!(captured.lines().is_empty());
        sources.tick(start + MINUTE, &log);
        assert_eq!(
            captured.lines(),
            ["30 more packets from 203.0.113.7 in the last minute"]
        );
        assert_eq!(sources.next_deadline(), None);
        assert!(sources.allow(noisy, start + MINUTE, &log));
    }

    #[test]
    fn one_ipv6_network_counts_as_one_source() {
        let (log, captured) = Log::capture(64);
        let start = Instant::now();
        let mut sources = PerSource::new(start);
        let allowed = (1..=30u16)
            .filter(|&last| {
                let ip = IpAddr::V6(Ipv6Addr::new(0x2a02, 0x8071, 1, 2, 0, 0, last, last));
                sources.allow(ip, start, &log)
            })
            .count();
        assert_eq!(allowed, 20);
        sources.tick(start + MINUTE, &log);
        assert_eq!(
            captured.lines(),
            ["10 more packets from 2a02:8071:1:2::/64 in the last minute"]
        );
    }

    // What a flood from many addresses, or spoofed ones, gets: the lines a
    // minute kept for all sources together, then one line with the count.
    #[test]
    fn rotating_sources_share_a_limit() {
        let (log, captured) = Log::capture(16_384);
        let start = Instant::now();
        let mut sources = PerSource::new(start);
        let mut allowed = 0;
        for round in 0..30 {
            let now = start + Duration::from_secs(round);
            for n in 0..300 {
                if sources.allow(v4(n), now, &log) {
                    allowed += 1;
                }
            }
        }
        assert_eq!(allowed, ALL_LINES_PER_MINUTE);
        assert!(sources.windows.len() <= MAX_SOURCES);
        assert_eq!(sources.next_deadline(), Some(start + MINUTE));
        sources.tick(start + MINUTE, &log);
        assert_eq!(
            captured.lines(),
            [
                "8900 more packets in the last minute, past the 100 lines a minute kept for all sources together"
            ]
        );

        // The next minute has lines of its own and no more.
        let later = start + MINUTE + Duration::from_secs(1);
        let allowed = (0..300)
            .filter(|&n| sources.allow(v4(n), later, &log))
            .count();
        assert_eq!(allowed, ALL_LINES_PER_MINUTE as usize);
    }

    #[test]
    fn full_table_counts_newcomers() {
        let (log, captured) = Log::capture(64);
        let start = Instant::now();
        let mut sources = PerSource::new(start);
        for n in 0..MAX_SOURCES as u32 {
            sources.windows.insert(v4(n), Window::new(start));
        }
        let newcomer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let soon = start + Duration::from_secs(1);
        assert!(!sources.allow(newcomer, soon, &log));
        assert!(!sources.allow(newcomer, soon, &log));
        assert_eq!(sources.windows.len(), MAX_SOURCES);
        assert!(!sources.windows.contains_key(&newcomer));
        assert_eq!(sources.all.left_out, 2);
        // The next walk of the table waits for the first minute to end.
        assert_eq!(sources.next_sweep, Some(start + MINUTE));

        let later = start + MINUTE + Duration::from_secs(1);
        assert!(sources.allow(newcomer, later, &log));
        assert_eq!(sources.windows.len(), 1);
        assert_eq!(
            captured.lines(),
            [
                "2 more packets in the last minute, past the 100 lines a minute kept for all sources together"
            ]
        );
    }

    #[test]
    fn closing_writes_the_counts_still_open() {
        let (log, captured) = Log::capture(64);
        let start = Instant::now();
        let mut sources = PerSource::new(start);
        let friend = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 4));
        for _ in 0..23 {
            sources.allow(friend, start, &log);
        }
        sources.flush(start + Duration::from_secs(3), &log);
        assert_eq!(
            captured.lines(),
            ["3 more packets from 198.51.100.4 in the last minute"]
        );
        assert_eq!(sources.next_deadline(), None);
    }

    #[test]
    fn the_file_moves_aside_past_its_size() {
        let dir = fresh_dir("size");
        let path = dir.join("booth.log");
        let mut file = LogFile::open(&path, "host", 200).unwrap();
        for n in 0..6 {
            file.write(UNIX_EPOCH + AT, &format!("line {n} \u{e9}"));
        }
        file.flush();
        drop(file);

        let old = fs::read_to_string(dir.join("booth.log.1")).unwrap();
        let new = fs::read_to_string(&path).unwrap();
        assert!(
            old.starts_with("2026-09-24T21:12:03.456Z host   line 0 ?\r\n"),
            "{old}"
        );
        assert!(old.len() > 200 && new.len() <= 200, "{old}\n{new}");
        assert!(new.ends_with("line 5 ?\r\n"), "{new}");
        fs::remove_dir_all(&dir).unwrap();
    }

    // Explorer or Remove-Item can delete it while Booth runs, since Rust
    // opens files with delete sharing.
    #[test]
    fn a_log_deleted_while_open_comes_back() {
        let dir = fresh_dir("deleted");
        let path = dir.join("booth.log");
        let mut file = LogFile::open(&path, "host", ROTATE_AFTER).unwrap();
        file.write(UNIX_EPOCH + AT, "before");
        file.flush();
        fs::remove_file(&path).unwrap();
        file.write(UNIX_EPOCH + AT, "lost with the deleted file");
        file.rotate();
        file.write(UNIX_EPOCH + AT, "after");
        file.flush();
        drop(file);

        let new = fs::read_to_string(&path).expect("booth.log is back");
        assert!(new.contains("was not there to move to"), "{new}");
        assert!(new.ends_with("host   after\r\n"), "{new}");
        assert!(!dir.join("booth.log.1").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    // Another program holds booth.log.1 without delete sharing, so it cannot
    // be replaced. The note about that is longer than rotate_after here, and
    // must not set off another rotation from inside this one.
    #[cfg(windows)]
    #[test]
    fn held_old_file_starts_log_over() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = fresh_dir("held");
        let path = dir.join("booth.log");
        let old = dir.join("booth.log.1");
        fs::write(&old, "kept\r\n").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&old)
            .unwrap();
        let mut file = LogFile::open(&path, "host", 50).unwrap();
        for n in 0..3 {
            file.write(
                UNIX_EPOCH + AT,
                &format!("line {n}, long enough to pass the limit"),
            );
        }
        file.flush();
        drop(file);
        drop(held);

        let new = fs::read_to_string(&path).unwrap();
        assert!(new.contains("could not move the log to "), "{new}");
        assert!(new.ends_with(" over\r\n"), "{new}");
        assert!(!new.contains("line 0"), "{new}");
        assert_eq!(fs::read_to_string(&old).unwrap(), "kept\r\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stopping_writes_every_line_waiting() {
        let dir = fresh_dir("stop");
        let path = dir.join("booth.log");
        let (log, writer) = open(Some(&path), "client").unwrap();
        let mut writer = writer.expect("a writer");
        for n in 0..500 {
            log!(log, "line {n}");
        }
        let started = Instant::now();
        writer.stop();
        let took = started.elapsed();
        assert!(took < STOP_WAIT, "stop took {took:?}");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("client line 499\r\n"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    // The writer stands still, as it would inside a write to a stalled
    // drive: once with room left in the queue, once with the queue full.
    #[test]
    fn a_stuck_writer_does_not_hold_up_leave() {
        for capacity in [8, 1] {
            let (lines, waiting) = crossbeam_channel::bounded(capacity);
            let (finished, done) = crossbeam_channel::bounded::<()>(0);
            lines
                .send(ToWriter::Line(SystemTime::now(), String::from("waiting")))
                .unwrap();
            let thread = thread::spawn(move || {
                thread::sleep(Duration::from_secs(3));
                drop((waiting, finished));
            });
            let mut writer = Writer {
                thread: Some(thread),
                lines,
                done,
            };
            let started = Instant::now();
            writer.stop();
            let took = started.elapsed();
            assert!(
                took < Duration::from_millis(200),
                "queue of {capacity}: stop took {took:?}"
            );
        }
    }
}
