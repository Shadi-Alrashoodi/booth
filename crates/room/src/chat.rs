// Chat rides its own reliable stream, next to the control one. A client says
// something to the host, and the host hands it on to everyone else in the
// order it took it, which is the one order the room has. Every byte here came
// from a peer, so decode checks every length, and the text is cleaned again
// on every side that takes it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use channels::reliable::MAX_MESSAGE;

use crate::control::{self, MAX_NAME_BYTES, PERSON_FALLBACK, Reader};
use crate::view::{ChatLine, Numbers};

pub(crate) const MAX_TEXT_BYTES: usize = 900;
pub(crate) const MAX_LINES: usize = 20;
// More than two empty lines in a row only push the rest of the chat away.
const MAX_EMPTY_RUN: usize = 2;
// History is kept in memory only, for the life of the room, so there is
// nothing on disk to leak.
pub(crate) const KEPT: usize = 2000;

const SAY: u8 = 1;
const SAID: u8 = 2;

// Said's one flag.
const ABOUT: u8 = 1;

const SAID_MAX: usize = 1 + 32 + 1 + MAX_NAME_BYTES + 2 + MAX_TEXT_BYTES + 8 + 1;
const _: () = assert!(SAID_MAX <= MAX_MESSAGE);

// A sent_at_host of zero: the host had no clock offset for the author yet, so
// nobody can tell how long the message took. The ping clock counts from 1970
// and is never zero.
const NOT_KNOWN: u64 = 0;

// A delivery time past this, or a send time this far ahead of now, is a clock
// or a peer that is wrong. A message held back by a reconnect still fits.
const LONGEST: Duration = Duration::from_secs(60);
const AHEAD: Duration = Duration::from_secs(1);

// Past this jitter the clock offset can be off by more than the number is
// worth, so the panel says "about".
const ABOUT_JITTER_MS: f32 = 5.0;

const SPAN: Duration = Duration::from_secs(60);
// A friend's program can send as fast as the stream lets it.
const MAX_SAMPLES: usize = 1024;

// What the host takes from one friend. A stream carries at most 64 lines a
// round trip, so a friend's program saying more than that would fill the
// others' streams and they would miss lines. Seven friends at this rate
// still fit a 400 ms round trip, seven bursts at once fit the 1024 lines a
// stream holds, and nobody typing or pasting comes near either.
pub(crate) const SAYS_PER_SECOND: f64 = 20.0;
pub(crate) const SAY_BURST: f64 = 100.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRefused {
    // Nothing left once cleaned.
    Empty,
    TooLong,
    TooManyLines,
    // The host is lost or the room has closed.
    NotLive,
}

impl fmt::Display for ChatRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatRefused::Empty => f.write_str("nothing is left of the message once cleaned"),
            ChatRefused::TooLong => write!(f, "the message is over {MAX_TEXT_BYTES} bytes"),
            ChatRefused::TooManyLines => write!(f, "the message has more than {MAX_LINES} lines"),
            ChatRefused::NotLive => f.write_str("the host cannot be reached"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatMessage {
    // Client to host. `sent_at` is on the sender's ping clock, in
    // microseconds.
    Say {
        text: String,
        sent_at: u64,
    },
    // Host to each client. `sent_at_host` is when the author sent it, on the
    // host's clock; None when the host could not convert it. `about` when
    // the host's clock offset to the author was taken over a jittery link,
    // so a time worked out from it is only about right.
    Said {
        author: [u8; 32],
        name: String,
        text: String,
        sent_at_host: Option<u64>,
        about: bool,
    },
}

impl ChatMessage {
    // `text` is what clean_text gave, so it fits.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        match self {
            ChatMessage::Say { text, sent_at } => {
                out.push(SAY);
                put_long_text(&mut out, text);
                out.extend_from_slice(&sent_at.to_le_bytes());
            }
            ChatMessage::Said {
                author,
                name,
                text,
                sent_at_host,
                about,
            } => {
                out.push(SAID);
                out.extend_from_slice(author);
                control::put_text(&mut out, &control::clean(name, PERSON_FALLBACK));
                put_long_text(&mut out, text);
                let at = sent_at_host.unwrap_or(NOT_KNOWN);
                out.extend_from_slice(&at.to_le_bytes());
                out.push(if *about { ABOUT } else { 0 });
            }
        }
        out
    }

    // The text comes back as it was sent, for clean_text to judge; the
    // author's name comes back cleaned.
    pub(crate) fn decode(buf: &[u8]) -> Option<ChatMessage> {
        let mut r = Reader(buf);
        let message = match r.u8()? {
            SAY => ChatMessage::Say {
                text: long_text(&mut r)?,
                sent_at: u64::from_le_bytes(r.array()?),
            },
            SAID => ChatMessage::Said {
                author: r.array()?,
                name: control::clean(r.text()?, PERSON_FALLBACK),
                text: long_text(&mut r)?,
                sent_at_host: match u64::from_le_bytes(r.array()?) {
                    NOT_KNOWN => None,
                    at => Some(at),
                },
                about: match r.u8()? {
                    0 => false,
                    ABOUT => true,
                    _ => return None,
                },
            },
            _ => return None,
        };
        r.0.is_empty().then_some(message)
    }
}

fn put_long_text(out: &mut Vec<u8>, text: &str) {
    // clean_text keeps it under this; the cut is for safety only, and lands
    // between characters so what goes out still reads as text.
    let mut end = text.len().min(MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let bytes = &text.as_bytes()[..end];
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn long_text(r: &mut Reader) -> Option<String> {
    let len = usize::from(u16::from_le_bytes(r.array()?));
    let text = std::str::from_utf8(r.bytes(len)?).ok()?;
    Some(text.to_owned())
}

// The rules a message keeps, on the sender and again on the host before it
// hands it on: control characters go except line breaks, and so do the
// characters that draw nothing or turn text around (the ones names lose);
// more than two empty lines in a row become two; the ends are trimmed. What
// is left must be something, at most 900 bytes and at most 20 lines.
pub(crate) fn clean_text(text: &str) -> Result<String, ChatRefused> {
    let mut out = String::with_capacity(text.len());
    let mut empty_run = 0;
    let mut first = true;
    // Windows writes a line break as \r\n, and a lone \r is one too.
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for part in line.split('\r') {
            let kept: String = part.chars().filter(|c| !control::is_hidden(*c)).collect();
            let kept = control::drop_stacked_marks(&kept);
            let kept = if kept.trim().is_empty() {
                empty_run += 1;
                if empty_run > MAX_EMPTY_RUN {
                    continue;
                }
                String::new()
            } else {
                empty_run = 0;
                kept
            };
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(&kept);
        }
    }
    let out = out.trim();
    if out.is_empty() {
        Err(ChatRefused::Empty)
    } else if out.len() > MAX_TEXT_BYTES {
        Err(ChatRefused::TooLong)
    } else if out.split('\n').count() > MAX_LINES {
        Err(ChatRefused::TooManyLines)
    } else {
        Ok(out.to_owned())
    }
}

// A time on a peer's ping clock, on this PC's. `offset_us` is the peer's
// clock minus this PC's, as OffsetEstimator gives it.
pub(crate) fn to_our_clock(at: u64, offset_us: i64) -> u64 {
    at.wrapping_sub(offset_us as u64)
}

// How long a message took to get here, from when it was sent on a peer's
// clock. A clock offset a little off can make a fast message look as if it
// arrived before it left, and that reads as zero.
pub(crate) fn delivery_ms(sent_at: u64, offset_us: i64, now_us: u64) -> Option<f32> {
    let took = now_us.wrapping_sub(to_our_clock(sent_at, offset_us)) as i64;
    let longest = LONGEST.as_micros() as i64;
    let ahead = AHEAD.as_micros() as i64;
    if took > longest || took < -ahead {
        return None;
    }
    Some(took.max(0) as f32 / 1000.0)
}

pub(crate) fn is_about(jitter_ms: Option<f32>) -> bool {
    jitter_ms.is_some_and(|ms| ms > ABOUT_JITTER_MS)
}

// The chat of this room, newest last. Views get a clone of the Arc, so a
// publish copies nothing. The last view published always holds one, so
// every line added copies the list; each line sits behind an Arc of its
// own, so that copy is 2000 pointers, not 2000 texts.
#[derive(Default)]
pub(crate) struct History(Arc<Vec<Arc<ChatLine>>>);

impl History {
    pub(crate) fn push(&mut self, line: ChatLine) {
        let lines = Arc::make_mut(&mut self.0);
        if lines.len() >= KEPT {
            let over = lines.len() + 1 - KEPT;
            lines.drain(..over);
        }
        lines.push(Arc::new(line));
    }

    pub(crate) fn shared(&self) -> Arc<Vec<Arc<ChatLine>>> {
        Arc::clone(&self.0)
    }
}

// The delivery times of the last minute, for the stats panel.
#[derive(Default)]
pub(crate) struct Delivery {
    samples: VecDeque<Sample>,
    last: Option<Sample>,
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    ms: f32,
    about: bool,
}

impl Delivery {
    pub(crate) fn record(&mut self, now: Instant, ms: f32, about: bool) {
        while self
            .samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) >= SPAN)
            || self.samples.len() >= MAX_SAMPLES
        {
            self.samples.pop_front();
        }
        let sample = Sample { at: now, ms, about };
        self.samples.push_back(sample);
        self.last = Some(sample);
    }

    // The last time, the average over the last minute, and "about" when the
    // last one or any in that minute was taken over a jittery link.
    pub(crate) fn fill(&self, numbers: &mut Numbers, now: Instant) {
        let Some(last) = self.last else {
            return;
        };
        let recent: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) < SPAN)
            .collect();
        numbers.chat_delivery_last_ms = Some(last.ms);
        numbers.chat_delivery_avg_ms = (!recent.is_empty())
            .then(|| recent.iter().map(|sample| sample.ms).sum::<f32>() / recent.len() as f32);
        numbers.chat_delivery_about = last.about || recent.iter().any(|sample| sample.about);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn say(text: &str) -> ChatMessage {
        ChatMessage::Say {
            text: text.to_owned(),
            sent_at: 1_790_284_323_456_789,
        }
    }

    fn said(name: &str, text: &str, sent_at_host: Option<u64>, about: bool) -> ChatMessage {
        ChatMessage::Said {
            author: [7; 32],
            name: name.to_owned(),
            text: text.to_owned(),
            sent_at_host,
            about,
        }
    }

    #[test]
    fn every_message_round_trips() {
        let longest = "\u{e9}".repeat(MAX_TEXT_BYTES / 2);
        for message in [
            say("anyone got the key for the east door"),
            say("two\nlines"),
            say(&longest),
            said("Mara", "on my way", Some(1_790_284_323_456_789), false),
            said("Mara", "on my way", Some(1_790_284_323_456_789), true),
            said(
                "\u{645}\u{631}\u{627}\u{645}",
                "\u{633}\u{644}\u{627}\u{645}",
                None,
                false,
            ),
            said("Jonas", &longest, Some(1), false),
        ] {
            assert_eq!(
                ChatMessage::decode(&message.encode()),
                Some(message.clone())
            );
        }
    }

    #[test]
    fn the_largest_said_fits_one_message() {
        let name = "\u{e9}".repeat(40);
        let text = "a".repeat(MAX_TEXT_BYTES);
        let encoded = said(&name, &text, Some(u64::MAX), true).encode();
        assert_eq!(encoded.len(), SAID_MAX);
        let mut stream = channels::Reliable::new();
        stream.send(&encoded).expect("one reliable message");

        // Past the limit the cut falls between characters, never in one.
        let over = format!("a{}", "\u{e9}".repeat(MAX_TEXT_BYTES / 2));
        let Some(ChatMessage::Say { text, .. }) = ChatMessage::decode(&say(&over).encode()) else {
            panic!("a cut text still decodes");
        };
        assert_eq!(text.len(), MAX_TEXT_BYTES - 1);
    }

    #[test]
    fn bad_shapes_are_refused() {
        let good_say = say("hello").encode();
        let good_said = said("Mara", "hello", Some(5), false).encode();
        for good in [&good_say, &good_said] {
            for cut in 0..good.len() {
                assert_eq!(ChatMessage::decode(&good[..cut]), None, "cut at {cut}");
            }
            let mut trailing = good.clone();
            trailing.push(0);
            assert_eq!(ChatMessage::decode(&trailing), None);
            for kind in [0, 3, 0xFF] {
                let mut unknown = good.clone();
                unknown[0] = kind;
                assert_eq!(ChatMessage::decode(&unknown), None, "kind {kind}");
            }
        }
        // A lone continuation byte, and a byte that never appears in UTF-8.
        let mut bad_text = good_say.clone();
        bad_text[3] = 0x80;
        let mut bad_name = good_said.clone();
        bad_name[1 + 32 + 1] = 0xFF;
        // A text length that runs past the end.
        let mut too_long = good_say.clone();
        too_long[1..3].copy_from_slice(&600u16.to_le_bytes());
        let mut bads = vec![bad_text, bad_name, too_long];
        // A flag nobody defined.
        for flags in [2, 3, 0x80, 0xFF] {
            let mut bad_flags = good_said.clone();
            *bad_flags.last_mut().expect("not empty") = flags;
            bads.push(bad_flags);
        }
        for bad in bads {
            assert_eq!(ChatMessage::decode(&bad), None, "{bad:?}");
        }
    }

    // A name is shown next to what they said, so it is cleaned as a
    // roster's names are.
    #[test]
    fn a_received_name_is_cleaned() {
        let mut raw = vec![SAID];
        raw.extend_from_slice(&[1; 32]);
        let name = format!(" ev\u{202E}il{} ", "x".repeat(50));
        raw.push(name.len() as u8);
        raw.extend_from_slice(name.as_bytes());
        put_long_text(&mut raw, "hi");
        raw.extend_from_slice(&9u64.to_le_bytes());
        raw.push(0);
        let Some(ChatMessage::Said { name, .. }) = ChatMessage::decode(&raw) else {
            panic!("did not decode");
        };
        assert!(name.starts_with("evilxxx"), "{name}");
        assert_eq!(name.chars().count(), control::MAX_NAME_CHARS);
    }

    #[test]
    fn text_keeps_the_rules() {
        let cases: &[(&str, Result<&str, ChatRefused>)] = &[
            ("hello", Ok("hello")),
            ("  hello  ", Ok("hello")),
            ("\n\nhello\n\n", Ok("hello")),
            ("one\r\ntwo\rthree\nfour", Ok("one\ntwo\nthree\nfour")),
            (
                "tab\tbell\u{7}nul\u{0}esc\u{1b}[31m",
                Ok("tabbellnulesc[31m"),
            ),
            ("ev\u{202E}il", Ok("evil")),
            ("a\u{200B}b\u{2066}c\u{2069}\u{FEFF}", Ok("abc")),
            ("next\u{85}line", Ok("nextline")),
            ("para\u{2029}graph", Ok("paragraph")),
            ("a\n\nb", Ok("a\n\nb")),
            ("a\n\n\nb", Ok("a\n\n\nb")),
            ("a\n\n\n\n\n\nb", Ok("a\n\n\nb")),
            ("a\n  \n\t\n \u{200B} \n\nb", Ok("a\n\n\nb")),
            ("  indented\n  kept", Ok("indented\n  kept")),
            ("", Err(ChatRefused::Empty)),
            ("   \n\t\r\n  ", Err(ChatRefused::Empty)),
            ("\u{0}\u{1}\u{7}\u{1b}\u{7f}", Err(ChatRefused::Empty)),
            ("\u{202E}\u{200B}\u{2060}\u{FEFF}", Err(ChatRefused::Empty)),
        ];
        for (raw, want) in cases {
            assert_eq!(clean_text(raw).as_deref().map_err(|e| *e), *want, "{raw:?}");
        }
    }

    #[test]
    fn size_and_lines_have_limits_of_their_own() {
        let most = "a".repeat(MAX_TEXT_BYTES);
        assert_eq!(clean_text(&most).as_deref(), Ok(most.as_str()));
        let over = "a".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(clean_text(&over), Err(ChatRefused::TooLong));
        // Counted after cleaning: what goes is not counted.
        let padded = format!("{most}\u{200B}\u{200B}  \n\n\n\n");
        assert_eq!(clean_text(&padded).as_deref(), Ok(most.as_str()));
        // Two bytes a character.
        let wide = "\u{e9}".repeat(MAX_TEXT_BYTES / 2 + 1);
        assert_eq!(clean_text(&wide), Err(ChatRefused::TooLong));

        let twenty = ["line"; MAX_LINES].join("\n");
        assert_eq!(clean_text(&twenty).as_deref(), Ok(twenty.as_str()));
        let more = ["line"; MAX_LINES + 1].join("\n");
        assert_eq!(clean_text(&more), Err(ChatRefused::TooManyLines));
        // Empty lines count.
        let spaced = ["line"; 11].join("\n\n");
        assert_eq!(clean_text(&spaced), Err(ChatRefused::TooManyLines));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        // Whatever goes in, what comes out keeps every rule, and cleaning it
        // again changes nothing, so the host never refuses what a sender
        // cleaned.
        #[test]
        fn cleaned_text_keeps_the_rules(text in "(\\PC|\\pC|[\n\r \t]){0,400}") {
            if let Ok(clean) = clean_text(&text) {
                prop_assert!(!clean.is_empty() && clean.len() <= MAX_TEXT_BYTES);
                prop_assert!(clean.split('\n').count() <= MAX_LINES);
                prop_assert_eq!(clean.trim(), clean.as_str());
                prop_assert!(!clean.contains("\n\n\n\n"));
                prop_assert!(clean.chars().all(|c| c == '\n' || !control::is_hidden(c)));
                prop_assert_eq!(clean_text(&clean), Ok(clean.clone()));
            }
        }
    }

    // xorshift64*, seeded, so a failure shows up the same way every run.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn byte(&mut self) -> u8 {
            self.next() as u8
        }
    }

    // Random bodies behind each real kind byte, so the parser gets past the
    // first check most of the time.
    #[test]
    fn random_bytes_never_panic() {
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for _ in 0..50_000 {
            let len = random.below(1200);
            let mut buf: Vec<u8> = (0..len).map(|_| random.byte()).collect();
            if let Some(first) = buf.first_mut() {
                *first = match random.below(3) {
                    0 => SAY,
                    1 => SAID,
                    _ => *first,
                };
            }
            if let Some(ChatMessage::Said { name, .. }) = ChatMessage::decode(&buf) {
                assert!(name.len() <= MAX_NAME_BYTES);
            }
        }
    }

    #[test]
    fn edited_messages_never_panic() {
        let mut random = Random(0xD1B5_4A32_D192_ED03);
        let seeds = [
            say("hello").encode(),
            say("\u{645}\u{631}\u{62d}\u{628}\u{627}\nsecond line").encode(),
            said("Mara", "on my way", Some(42), true).encode(),
            said("\u{1F600}\u{1F600}", "x", None, false).encode(),
        ];
        for _ in 0..50_000 {
            let mut buf = seeds[random.below(seeds.len())].clone();
            for _ in 0..1 + random.below(6) {
                let at = random.below(buf.len() + 1);
                match random.below(3) {
                    0 if at < buf.len() => buf[at] = random.byte(),
                    1 => buf.insert(at, random.byte()),
                    _ if at < buf.len() => {
                        buf.remove(at);
                    }
                    _ => {}
                }
            }
            match ChatMessage::decode(&buf) {
                Some(ChatMessage::Said { name, text, .. }) => {
                    assert!(!name.chars().any(control::is_hidden));
                    let _ = clean_text(&text);
                }
                Some(ChatMessage::Say { text, .. }) => {
                    let _ = clean_text(&text);
                }
                None => {}
            }
        }
    }

    fn line(n: usize) -> ChatLine {
        ChatLine {
            author: [1; 32],
            name: String::from("Ana"),
            text: n.to_string(),
            at_unix_ms: n as u64,
            mine: false,
            kind: crate::view::LineKind::Said,
        }
    }

    #[test]
    fn history_keeps_the_newest_lines() {
        let mut history = History::default();
        for n in 0..KEPT + 5 {
            history.push(line(n));
        }
        let lines = history.shared();
        assert_eq!(lines.len(), KEPT);
        assert_eq!(lines[0].text, "5");
        assert_eq!(lines[KEPT - 1].text, (KEPT + 4).to_string());
    }

    // A view holds the list it was given, whatever comes after, and the
    // lines in both are the same lines, not copies.
    #[test]
    fn a_view_keeps_what_it_was_given() {
        let mut history = History::default();
        history.push(line(0));
        let shown = history.shared();
        history.push(line(1));
        assert_eq!(shown.len(), 1);
        let now = history.shared();
        assert_eq!(now.len(), 2);
        assert!(Arc::ptr_eq(&shown[0], &now[0]));
    }

    #[test]
    fn delivery_is_taken_on_this_pcs_clock() {
        // The sender's clock runs 2 ms ahead of this one.
        let offset = 2_000;
        let sent = 1_000_000_000;
        assert_eq!(to_our_clock(sent, offset), sent - 2_000);
        assert_eq!(delivery_ms(sent, offset, sent + 1_500), Some(3.5));
        // Behind, the other way.
        assert_eq!(delivery_ms(sent, -2_000, sent + 3_500), Some(1.5));
        // A slightly wrong offset reads as zero, a wrong clock as nothing.
        assert_eq!(delivery_ms(sent, 0, sent - 300), Some(0.0));
        assert_eq!(delivery_ms(sent, 0, sent - 2_000_000), None);
        assert_eq!(delivery_ms(sent, 0, sent + 61_000_000), None);
        assert_eq!(delivery_ms(u64::MAX, i64::MIN, 0), None);
    }

    #[test]
    fn delivery_last_and_minute_average() {
        let start = Instant::now();
        let mut delivery = Delivery::default();
        let mut numbers = Numbers::default();
        delivery.fill(&mut numbers, start);
        assert_eq!(numbers.chat_delivery_last_ms, None);
        assert_eq!(numbers.chat_delivery_avg_ms, None);

        delivery.record(start, 4.0, false);
        delivery.record(start + Duration::from_secs(1), 2.0, false);
        delivery.fill(&mut numbers, start + Duration::from_secs(2));
        assert_eq!(numbers.chat_delivery_last_ms, Some(2.0));
        assert_eq!(numbers.chat_delivery_avg_ms, Some(3.0));
        assert!(!numbers.chat_delivery_about);

        delivery.record(start + Duration::from_secs(30), 9.0, true);
        delivery.fill(&mut numbers, start + Duration::from_millis(60_500));
        assert_eq!(numbers.chat_delivery_last_ms, Some(9.0));
        assert_eq!(numbers.chat_delivery_avg_ms, Some(5.5));
        assert!(numbers.chat_delivery_about);

        // A minute on, the average is gone and the last one stays.
        let mut later = Numbers::default();
        delivery.fill(&mut later, start + Duration::from_secs(120));
        assert_eq!(later.chat_delivery_last_ms, Some(9.0));
        assert_eq!(later.chat_delivery_avg_ms, None);
        assert!(later.chat_delivery_about);
    }

    #[test]
    fn about_follows_the_jitter() {
        assert!(!is_about(None));
        assert!(!is_about(Some(5.0)));
        assert!(is_about(Some(5.1)));
    }
}
