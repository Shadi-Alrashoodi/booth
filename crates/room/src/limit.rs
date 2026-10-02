// Token buckets checked before any key math: one per source, so one address
// cannot keep the host busy with handshakes, and one shared by every source,
// so many addresses cannot either. Cookie replies get a shared bucket of their
// own, so many addresses cannot fill the host's upload with them.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

const PER_SECOND: f64 = 10.0;
const BURST: f64 = 20.0;
const MAX_SOURCES: usize = 4096;
// A sweep walks the whole table, so a flood from new sources gets one per
// interval instead of one per packet.
const SWEEP_EVERY: Duration = Duration::from_millis(100);

// Under load a source must return its cookie before it gets this far, so
// this bounds the key math a flood from many real addresses can cause. An
// initiation costs about 0.1 ms, so 200 a second is 2 percent of one core and
// pings keep their timing. Friends already in the room skip it, so a flood
// holds up new joins but not a rekey.
const ALL_PER_SECOND: f64 = 200.0;
const ALL_BURST: f64 = 50.0;

#[derive(Debug)]
pub(crate) struct RateLimit {
    buckets: HashMap<IpAddr, Bucket>,
    next_sweep: Option<Instant>,
    all: Bucket,
    cookies: Bucket,
    cookies_per_second: f64,
    cookie_burst: f64,
}

// Why an initiation under load gets no cookie reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Source,
    Cap,
}

// Also what the host takes from each friend's chat.
#[derive(Debug)]
pub(crate) struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub(crate) fn full(now: Instant, burst: f64) -> Bucket {
        Bucket {
            tokens: burst,
            last: now,
        }
    }

    pub(crate) fn take(&mut self, now: Instant, per_second: f64, burst: f64) -> bool {
        self.take_many(now, 1.0, per_second, burst)
    }

    // For a bucket of bytes: all of `count` or nothing.
    pub(crate) fn take_many(
        &mut self,
        now: Instant,
        count: f64,
        per_second: f64,
        burst: f64,
    ) -> bool {
        if self.refill(now, per_second, burst) < count {
            return false;
        }
        self.tokens -= count;
        true
    }

    // How long until take_many can take `count`, none if it can now. At
    // least a millisecond otherwise, so rounding never leaves a wait that
    // ends before the tokens are there.
    pub(crate) fn wait_for(
        &mut self,
        now: Instant,
        count: f64,
        per_second: f64,
        burst: f64,
    ) -> Duration {
        let short = count - self.refill(now, per_second, burst);
        if short <= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(short / per_second).max(Duration::from_millis(1))
    }

    fn refill(&mut self, now: Instant, per_second: f64, burst: f64) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_second).min(burst);
        self.last = now;
        self.tokens
    }
}

impl RateLimit {
    pub(crate) fn new(now: Instant, cookies_per_second: u32, cookie_burst: u32) -> RateLimit {
        // Under one, not even a friend's first try would get a reply.
        let cookie_burst = f64::from(cookie_burst.max(1));
        RateLimit {
            buckets: HashMap::new(),
            next_sweep: None,
            all: Bucket {
                tokens: ALL_BURST,
                last: now,
            },
            cookies: Bucket {
                tokens: cookie_burst,
                last: now,
            },
            cookies_per_second: f64::from(cookies_per_second),
            cookie_burst,
        }
    }

    pub(crate) fn allow(&mut self, ip: IpAddr, in_room: bool, now: Instant) -> bool {
        self.take(ip, !in_room, now)
    }

    // A cookie reply costs a hash and no key math, so the key math budget
    // stays out of it: a flood that gets them must not use up the budget a
    // friend returning a cookie needs to get in. The source's own bucket is
    // spent even when the cap then holds the reply back, so a source sending
    // past its rate gets no bigger share of what the cap lets out.
    pub(crate) fn allow_cookie(&mut self, ip: IpAddr, now: Instant) -> Result<(), Refused> {
        if !self.take(ip, false, now) {
            return Err(Refused::Source);
        }
        if self
            .cookies
            .refill(now, self.cookies_per_second, self.cookie_burst)
            < 1.0
        {
            return Err(Refused::Cap);
        }
        self.cookies.tokens -= 1.0;
        Ok(())
    }

    fn take(&mut self, ip: IpAddr, shared: bool, now: Instant) -> bool {
        let source = source(ip);
        if !self.buckets.contains_key(&source)
            && self.buckets.len() >= MAX_SOURCES
            && !self.sweep(now)
        {
            return false;
        }
        let bucket = self.buckets.entry(source).or_insert(Bucket {
            tokens: BURST,
            last: now,
        });
        if bucket.refill(now, PER_SECOND, BURST) < 1.0 {
            return false;
        }
        if shared && self.all.refill(now, ALL_PER_SECOND, ALL_BURST) < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        if shared {
            self.all.tokens -= 1.0;
        }
        true
    }

    // A full bucket is a source that has been quiet long enough to forget
    // nothing by being dropped. True when that made room.
    fn sweep(&mut self, now: Instant) -> bool {
        if self.next_sweep.is_some_and(|at| now < at) {
            return false;
        }
        self.next_sweep = Some(now + SWEEP_EVERY);
        self.buckets
            .retain(|_, bucket| bucket.refill(now, PER_SECOND, BURST) < BURST);
        self.buckets.len() < MAX_SOURCES
    }
}

// One home connection gets a whole /64, so counting single IPv6 addresses
// would give an attacker 2^64 buckets.
pub(crate) fn source(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let [a, b, c, d, ..] = v6.segments();
            IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timers;
    use std::net::Ipv4Addr;

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(0x0A00_0000 + n))
    }

    fn with_defaults(now: Instant) -> RateLimit {
        let timers = Timers::default();
        RateLimit::new(
            now,
            timers.cookie_replies_per_second,
            timers.cookie_reply_burst,
        )
    }

    #[test]
    fn burst_then_rate() {
        let start = Instant::now();
        let mut limit = with_defaults(start);
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        for i in 0..20 {
            assert!(limit.allow(ip, false, start), "attempt {i}");
        }
        assert!(!limit.allow(ip, false, start));
        assert!(!limit.allow(ip, false, start + Duration::from_millis(50)));
        assert!(limit.allow(ip, false, start + Duration::from_millis(150)));
        assert!(!limit.allow(ip, false, start + Duration::from_millis(150)));

        let other = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        assert!(limit.allow(other, false, start));
    }

    #[test]
    fn a_byte_bucket_says_when_it_has_enough() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start, 1000.0);
        assert_eq!(
            bucket.wait_for(start, 1000.0, 100.0, 1000.0),
            Duration::ZERO
        );
        assert!(bucket.take_many(start, 700.0, 100.0, 1000.0));
        assert!(!bucket.take_many(start, 500.0, 100.0, 1000.0));
        let wait = bucket.wait_for(start, 500.0, 100.0, 1000.0);
        assert_eq!(wait, Duration::from_secs(2));
        let halfway = start + Duration::from_secs(1);
        assert_eq!(
            bucket.wait_for(halfway, 500.0, 100.0, 1000.0),
            Duration::from_secs(1)
        );
        assert!(bucket.take_many(start + wait, 500.0, 100.0, 1000.0));
        // Never less than a millisecond while short.
        assert_eq!(
            bucket.wait_for(start + wait, 0.01, 100.0, 1000.0),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn one_ipv6_network_shares_a_bucket() {
        let now = Instant::now();
        let mut limit = with_defaults(now);
        for last in 1..=20u16 {
            let ip = IpAddr::V6(Ipv6Addr::new(0x2a02, 0x8071, 1, 2, 0, 0, 0, last));
            assert!(limit.allow(ip, false, now));
        }
        let ip = IpAddr::V6(Ipv6Addr::new(0x2a02, 0x8071, 1, 2, 9, 9, 9, 9));
        assert!(!limit.allow(ip, false, now));
        let elsewhere = IpAddr::V6(Ipv6Addr::new(0x2a02, 0x8071, 1, 3, 0, 0, 0, 1));
        assert!(limit.allow(elsewhere, false, now));
    }

    #[test]
    fn many_sources_share_one_budget() {
        let start = Instant::now();
        let mut limit = with_defaults(start);
        let allowed = (0..1000)
            .filter(|&n| limit.allow(ip(n), false, start))
            .count();
        assert_eq!(allowed, ALL_BURST as usize);
        assert!(!limit.allow(ip(1000), false, start));
        // Someone already in the room is held back only by their own bucket.
        assert!(limit.allow(ip(2000), true, start));

        let later = start + Duration::from_millis(100);
        let allowed = (3000..4000)
            .filter(|&n| limit.allow(ip(n), false, later))
            .count();
        assert_eq!(allowed, (ALL_PER_SECOND / 10.0) as usize);
    }

    #[test]
    fn cookie_replies_per_source() {
        let start = Instant::now();
        // A cap nobody reaches here; the next test is about the cap.
        let mut limit = RateLimit::new(start, 10_000, 10_000);
        let noisy = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let replies = (0..50)
            .filter(|_| limit.allow_cookie(noisy, start).is_ok())
            .count();
        assert_eq!(replies, BURST as usize);
        assert_eq!(limit.allow_cookie(noisy, start), Err(Refused::Source));
        // The same source's cookie reply and key math come out of one bucket.
        assert!(!limit.allow(noisy, false, start));

        let replied = (0..1000)
            .filter(|&n| limit.allow_cookie(ip(n), start).is_ok())
            .count();
        assert_eq!(replied, 1000);
        let allowed = (1000..2000)
            .filter(|&n| limit.allow(ip(n), false, start))
            .count();
        assert_eq!(allowed, ALL_BURST as usize);
    }

    #[test]
    fn cookie_cap_for_all_sources() {
        let start = Instant::now();
        let mut limit = RateLimit::new(start, 100, 10);
        let replied = (0..50)
            .filter(|&n| limit.allow_cookie(ip(n), start).is_ok())
            .count();
        assert_eq!(replied, 10);

        // Held back by the cap, a source still spends its own tokens, and
        // is held back by those once they run out.
        let noisy = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let held: Vec<Result<(), Refused>> =
            (0..25).map(|_| limit.allow_cookie(noisy, start)).collect();
        let (capped, over) = held.split_at(BURST as usize);
        assert!(capped.iter().all(|r| *r == Err(Refused::Cap)), "{held:?}");
        assert!(over.iter().all(|r| *r == Err(Refused::Source)), "{held:?}");

        // The cap fills at its rate, and the key math budget is its own. Half
        // the burst, so a faster rate would show.
        let later = start + Duration::from_millis(50);
        let replied = (100..200)
            .filter(|&n| limit.allow_cookie(ip(n), later).is_ok())
            .count();
        assert_eq!(replied, 5);
        let allowed = (200..300)
            .filter(|&n| limit.allow(ip(n), false, later))
            .count();
        assert_eq!(allowed, ALL_BURST as usize);
    }

    #[test]
    fn full_table_drops_quiet_sources() {
        let start = Instant::now();
        let mut limit = with_defaults(start);
        for n in 0..MAX_SOURCES as u32 {
            assert!(limit.allow(ip(n), true, start));
        }
        let newcomer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        assert!(!limit.allow(newcomer, false, start));
        assert!(limit.allow(newcomer, false, start + Duration::from_secs(1)));
    }

    #[test]
    fn full_table_swept_once_an_interval() {
        let start = Instant::now();
        let mut limit = with_defaults(start);
        for n in 0..MAX_SOURCES as u32 {
            assert!(limit.allow(ip(n), true, start));
        }
        let newcomer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        // Every bucket is one token short and fills again after 100 ms.
        let swept = start + Duration::from_millis(99);
        assert!(!limit.allow(newcomer, false, swept));
        // Quiet sources are there to drop now, but the last sweep was too
        // recent, so the newcomer is refused without walking the table.
        assert!(!limit.allow(newcomer, false, swept + Duration::from_millis(50)));
        assert!(limit.allow(newcomer, false, swept + SWEEP_EVERY));
    }
}
