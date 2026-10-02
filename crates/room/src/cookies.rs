// Under load the host answers an initiation with a cookie tied to the
// sender's address and does no key math for it until the cookie comes back
// in mac2. The cookie, mac2 and the reply packet are the session crate's;
// this is when the host wants one, and how a client keeps what it was sent.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use session::{COOKIE_REPLY_LEN, CookieChecker, SessionError};

use crate::limit;
use crate::reply;

// Load is counted over this.
const WINDOW: Duration = Duration::from_secs(1);
// The host makes a new secret every COOKIE_SECRET_LIFETIME, which a cookie
// made just before does not outlive. WireGuard keeps one 5 s less, for the
// time the reply was on its way.
pub(crate) const COOKIE_KEPT: Duration =
    Duration::from_secs(session::COOKIE_SECRET_LIFETIME.as_secs() - 5);
// A client takes cookies only from addresses it tries, so this is room for
// every address an invite can carry.
const JAR_SIZE: usize = invite::MAX_CANDIDATES;
const LINE_EVERY: Duration = Duration::from_secs(60);
const LINE_SOURCES: usize = 256;
const CAPPED_LINE_EVERY: Duration = Duration::from_secs(1);

// The host's half.
pub(crate) struct Gate {
    checker: CookieChecker,
    load: Load,
    // What the log last said, so going under load and coming out are each
    // written once.
    said_under: bool,
    pub(crate) replies_sent: u64,
    lines: Lines,
    capped: Capped,
}

impl Gate {
    pub(crate) fn new(
        host_public: &[u8; 32],
        initiations: u32,
        calm: Duration,
        now: Instant,
    ) -> Gate {
        Gate {
            checker: CookieChecker::new(host_public, now),
            load: Load::new(initiations, calm),
            said_under: false,
            replies_sent: 0,
            lines: Lines::default(),
            capped: Capped::default(),
        }
    }

    // One hash, so junk made for another host key is dropped before it
    // counts toward load or gets a cookie.
    pub(crate) fn has_valid_mac1(&self, packet: &[u8]) -> bool {
        self.checker.has_valid_mac1(packet)
    }

    // Counts an initiation that passed mac1. True when the host is under
    // load, this one counted.
    pub(crate) fn count(&mut self, now: Instant) -> bool {
        self.load.count(now)
    }

    pub(crate) fn has_valid_mac2(&mut self, packet: &[u8], from: SocketAddr, now: Instant) -> bool {
        self.checker.has_valid_mac2(packet, from, now)
    }

    pub(crate) fn reply(
        &mut self,
        initiation: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Result<[u8; COOKIE_REPLY_LEN], SessionError> {
        self.checker.cookie_reply(initiation, from, now)
    }

    // For the log: Some(true) when the host has just gone under load,
    // Some(false) when it has just come out.
    pub(crate) fn changed(&mut self, now: Instant) -> Option<bool> {
        let under = self.load.is_on(now);
        (under != self.said_under).then(|| {
            self.said_under = under;
            under
        })
    }

    // When the load ends if nothing more comes in, while the log says the
    // host is under it, or when the line about a second of initiations the
    // cap held back is due.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let load = self.load.until.filter(|_| self.said_under);
        let capped = self.capped.since.map(|since| since + CAPPED_LINE_EVERY);
        load.into_iter().chain(capped).min()
    }

    // One line a minute per source about its initiations without a cookie
    // is enough to see who is knocking; the stats panel counts every one.
    pub(crate) fn line_due(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.lines.due(ip, now)
    }

    // An initiation the cap on cookie replies held back. A flood over the
    // cap would be thousands of lines a second, so they are summed up in one
    // line a second at most. Returns the count of the second before this
    // one when that is over, since its line goes first.
    pub(crate) fn capped(&mut self, now: Instant) -> Option<u64> {
        let over = self.capped.take_over(now);
        self.capped.since.get_or_insert(now);
        self.capped.count += 1;
        over
    }

    // The count of a second of initiations the cap held back, once it is
    // over.
    pub(crate) fn capped_over(&mut self, now: Instant) -> Option<u64> {
        self.capped.take_over(now)
    }
}

#[derive(Default)]
struct Capped {
    // When the second being counted began, while there is one.
    since: Option<Instant>,
    count: u64,
}

impl Capped {
    fn take_over(&mut self, now: Instant) -> Option<u64> {
        let since = self.since?;
        (now >= since + CAPPED_LINE_EVERY).then(|| {
            self.since = None;
            std::mem::take(&mut self.count)
        })
    }
}

// Initiations that passed mac1 within the last second, from anyone. At the
// threshold the host is under load, and it stays so until the count has been
// under the threshold for `calm`.
struct Load {
    threshold: usize,
    calm: Duration,
    // When the last of them came in, oldest first: never more than a
    // second's worth, and never more than the threshold.
    recent: VecDeque<Instant>,
    until: Option<Instant>,
}

impl Load {
    fn new(threshold: u32, calm: Duration) -> Load {
        Load {
            threshold: usize::try_from(threshold).unwrap_or(usize::MAX).max(1),
            calm,
            recent: VecDeque::new(),
            until: None,
        }
    }

    fn count(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) >= WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= self.threshold {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        if self.recent.len() >= self.threshold
            && let Some(&oldest) = self.recent.front()
        {
            // The count is at the threshold until the oldest of these is a
            // second old, and the calm starts then.
            let until = oldest + WINDOW + self.calm;
            self.until = Some(self.until.map_or(until, |at| at.max(until)));
        }
        self.is_on(now)
    }

    fn is_on(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

#[derive(Default)]
struct Lines {
    // When a line about each source was last written. IPv6 sources are
    // counted by /64, as the rate limit does.
    at: HashMap<IpAddr, Instant>,
    // No entry runs out before this, so a full table is not walked again for
    // every newcomer in a flood from many addresses.
    next_sweep: Option<Instant>,
}

impl Lines {
    fn due(&mut self, ip: IpAddr, now: Instant) -> bool {
        let source = limit::source(ip);
        let recent = |at: &Instant| now.saturating_duration_since(*at) < LINE_EVERY;
        if self.at.get(&source).is_some_and(recent) {
            return false;
        }
        if !self.at.contains_key(&source) && self.at.len() >= LINE_SOURCES {
            if self.next_sweep.is_some_and(|at| now < at) {
                return false;
            }
            self.at.retain(|_, at| recent(at));
            self.next_sweep = self.at.values().min().map(|&at| at + LINE_EVERY);
            if self.at.len() >= LINE_SOURCES {
                return false;
            }
        }
        self.at.insert(source, now);
        true
    }
}

// The client's half: the cookies a host under load sent, each good only for
// the address it came from, since the host made it for the address this PC
// has on that path.
#[derive(Default)]
pub(crate) struct Jar {
    kept: Vec<Kept>,
}

struct Kept {
    from: SocketAddr,
    cookie: [u8; 16],
    until: Instant,
}

impl Jar {
    pub(crate) fn keep(&mut self, from: SocketAddr, cookie: [u8; 16], now: Instant) {
        self.kept
            .retain(|kept| now < kept.until && !reply::same_address(kept.from, from));
        if self.kept.len() >= JAR_SIZE {
            self.kept.remove(0);
        }
        self.kept.push(Kept {
            from,
            cookie,
            until: now + COOKIE_KEPT,
        });
    }

    // The cookie for the address that answered last, which is where the
    // next answer most likely comes from, or else the newest one held for
    // any of `targets`.
    pub(crate) fn pick(
        &self,
        answered: Option<SocketAddr>,
        targets: &[SocketAddr],
        now: Instant,
    ) -> Option<[u8; 16]> {
        answered.and_then(|addr| self.get(addr, now)).or_else(|| {
            self.kept
                .iter()
                .rev()
                .filter(|kept| now < kept.until)
                .find(|kept| targets.iter().any(|&to| reply::same_address(kept.from, to)))
                .map(|kept| kept.cookie)
        })
    }

    fn get(&self, to: SocketAddr, now: Instant) -> Option<[u8; 16]> {
        self.kept
            .iter()
            .find(|kept| now < kept.until && reply::same_address(kept.from, to))
            .map(|kept| kept.cookie)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const CALM: Duration = Duration::from_secs(5);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_threshold_within_one_second_is_load() {
        let start = Instant::now();
        let mut load = Load::new(32, CALM);
        for n in 0..31 {
            assert!(!load.count(start + ms(n * 30)), "initiation {n}");
        }
        assert!(load.count(start + ms(930)));
    }

    #[test]
    fn just_under_the_threshold_is_never_load() {
        let start = Instant::now();
        let mut load = Load::new(32, CALM);
        // 31 a second for a minute: every 32nd is a second after the first
        // of the 31 before it.
        for n in 0..31 * 60 {
            let at = start + Duration::from_secs(n / 31) + ms(n % 31 * 32);
            assert!(!load.count(at), "initiation {n}");
        }
        assert!(load.until.is_none());
        assert!(load.recent.len() <= 31);
    }

    #[test]
    fn load_lasts_through_the_calm() {
        let start = Instant::now();
        let mut load = Load::new(32, CALM);
        for n in 0..100 {
            load.count(start + ms(n * 10));
        }
        // The last 32 came in from 680 ms on, so the count is under the
        // threshold from 1680 ms and the calm is over 5 s after that.
        let over = start + ms(680) + WINDOW + CALM;
        assert_eq!(load.until, Some(over));
        assert!(load.is_on(over - ms(1)));
        assert!(!load.is_on(over));

        // A few more meanwhile do not start it over.
        for n in 0..10 {
            assert!(load.count(start + Duration::from_secs(3) + ms(n)));
        }
        assert_eq!(load.until, Some(over));
        assert!(!load.count(over));
    }

    #[test]
    fn high_threshold_keeps_one_second() {
        let start = Instant::now();
        let mut load = Load::new(u32::MAX, CALM);
        for n in 0..5000 {
            assert!(!load.count(start + ms(n)));
        }
        assert_eq!(load.recent.len(), 1000);
    }

    #[test]
    fn a_line_a_minute_per_source() {
        let start = Instant::now();
        let mut lines = Lines::default();
        let one = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let other = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));
        assert!(lines.due(one, start));
        assert!(!lines.due(one, start + ms(10)));
        assert!(lines.due(other, start + ms(10)));
        assert!(!lines.due(one, start + LINE_EVERY - ms(1)));
        assert!(lines.due(one, start + LINE_EVERY));
    }

    #[test]
    fn full_line_table_waits_a_minute() {
        let start = Instant::now();
        let mut lines = Lines::default();
        for n in 0..LINE_SOURCES as u32 {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0A00_0000 + n));
            assert!(lines.due(ip, start + ms(u64::from(n))));
        }
        let newcomer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!(!lines.due(newcomer, start + ms(500)));
        assert_eq!(lines.next_sweep, Some(start + LINE_EVERY));
        assert!(!lines.due(newcomer, start + ms(600)));
        assert!(lines.due(newcomer, start + LINE_EVERY));
    }

    #[test]
    fn cookie_per_address_for_115_s() {
        let start = Instant::now();
        let mut jar = Jar::default();
        let a: SocketAddr = "192.168.1.20:41000".parse().unwrap();
        let b: SocketAddr = "203.0.113.9:41000".parse().unwrap();
        jar.keep(a, [1; 16], start);
        assert_eq!(jar.get(a, start), Some([1; 16]));
        assert_eq!(jar.get(b, start), None);
        assert_eq!(COOKIE_KEPT, Duration::from_secs(115));
        assert_eq!(jar.get(a, start + COOKIE_KEPT - ms(1)), Some([1; 16]));
        assert_eq!(jar.get(a, start + COOKIE_KEPT), None);

        // A newer one for the same address takes its place.
        jar.keep(a, [2; 16], start + ms(5));
        jar.keep(b, [3; 16], start + ms(5));
        assert_eq!(jar.get(a, start + ms(5)), Some([2; 16]));
        assert_eq!(jar.get(b, start + ms(5)), Some([3; 16]));
        assert_eq!(jar.kept.len(), 2);
    }

    #[test]
    fn pick_prefers_answering_address() {
        let start = Instant::now();
        let mut jar = Jar::default();
        let lan: SocketAddr = "192.168.1.20:45000".parse().unwrap();
        let public: SocketAddr = "203.0.113.9:45000".parse().unwrap();
        let dropped: SocketAddr = "198.51.100.7:45000".parse().unwrap();
        let targets = [lan, public];
        assert_eq!(jar.pick(None, &targets, start), None);

        jar.keep(lan, [1; 16], start);
        jar.keep(public, [2; 16], start + ms(5));
        // From an address the tries no longer go to.
        jar.keep(dropped, [3; 16], start + ms(10));
        let now = start + ms(10);
        assert_eq!(jar.pick(None, &targets, now), Some([2; 16]));
        assert_eq!(jar.pick(Some(lan), &targets, now), Some([1; 16]));

        // The one for the address that answered has run out.
        let later = start + COOKIE_KEPT;
        assert_eq!(jar.pick(Some(lan), &targets, later), Some([2; 16]));
        assert_eq!(jar.pick(Some(lan), &targets, later + ms(5)), None);
    }

    #[test]
    fn jar_holds_one_per_candidate() {
        let start = Instant::now();
        let mut jar = Jar::default();
        for port in 0..40u16 {
            jar.keep(
                SocketAddr::from(([127, 0, 0, 1], 1000 + port)),
                [0; 16],
                start,
            );
        }
        assert_eq!(jar.kept.len(), JAR_SIZE);
        assert_eq!(
            jar.get(SocketAddr::from(([127, 0, 0, 1], 1000)), start),
            None
        );
        assert!(
            jar.get(SocketAddr::from(([127, 0, 0, 1], 1039)), start)
                .is_some()
        );
    }
}
