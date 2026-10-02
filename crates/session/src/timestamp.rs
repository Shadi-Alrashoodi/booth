use std::time::{SystemTime, UNIX_EPOCH};

// Unix seconds on top of the TAI64 epoch, leap seconds ignored as WireGuard does: only the order
// of our own timestamps matters, never how they line up with real TAI.
const TAI64_EPOCH: u64 = 1 << 62;
const NANOS_PER_SECOND: u32 = 1_000_000_000;

// Big-endian seconds then nanoseconds, so comparing the bytes compares the times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tai64N([u8; 12]);

impl Tai64N {
    pub fn now() -> Tai64N {
        Tai64N::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(time: SystemTime) -> Tai64N {
        let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        Tai64N::from_parts(
            TAI64_EPOCH.saturating_add(since_epoch.as_secs()),
            since_epoch.subsec_nanos(),
        )
    }

    pub const fn from_bytes(bytes: [u8; 12]) -> Tai64N {
        Tai64N(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 12] {
        self.0
    }

    fn from_parts(seconds: u64, nanos: u32) -> Tai64N {
        let mut bytes = [0u8; 12];
        bytes[..8].copy_from_slice(&seconds.to_be_bytes());
        bytes[8..].copy_from_slice(&nanos.to_be_bytes());
        Tai64N(bytes)
    }

    fn parts(self) -> (u64, u32) {
        let [s0, s1, s2, s3, s4, s5, s6, s7, n0, n1, n2, n3] = self.0;
        (
            u64::from_be_bytes([s0, s1, s2, s3, s4, s5, s6, s7]),
            u32::from_be_bytes([n0, n1, n2, n3]),
        )
    }

    fn one_nanosecond_later(self) -> Tai64N {
        let (seconds, nanos) = self.parts();
        if nanos < NANOS_PER_SECOND - 1 {
            Tai64N::from_parts(seconds, nanos + 1)
        } else {
            Tai64N::from_parts(seconds.saturating_add(1), 0)
        }
    }
}

// The host drops any initiation whose timestamp is not newer than the last one it accepted from
// us, so retries and rekeys must never reuse or go back in time, even if the clock does.
#[derive(Debug, Default)]
pub struct TimestampSource {
    last: Option<Tai64N>,
}

impl TimestampSource {
    pub fn new() -> TimestampSource {
        TimestampSource::default()
    }

    pub fn next_stamp(&mut self) -> Tai64N {
        self.next_stamp_at(SystemTime::now())
    }

    pub fn next_stamp_at(&mut self, now: SystemTime) -> Tai64N {
        let clock = Tai64N::from_system_time(now);
        let stamp = match self.last {
            Some(last) if clock <= last => last.one_nanosecond_later(),
            _ => clock,
        };
        self.last = Some(stamp);
        stamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Windows keeps SystemTime in 100 ns steps, so tests stay on multiples of 100 ns.
    fn at(seconds: u64, nanos: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(seconds, nanos)
    }

    #[test]
    fn encoding_is_tai64n() {
        assert_eq!(
            Tai64N::from_system_time(UNIX_EPOCH).to_bytes(),
            [0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            Tai64N::from_system_time(at(1_700_000_000, 500)).to_bytes(),
            [0x40, 0, 0, 0, 0x65, 0x53, 0xf1, 0x00, 0, 0, 0x01, 0xf4]
        );
    }

    #[test]
    fn order_follows_time() {
        let times = [
            at(0, 0),
            at(0, 100),
            at(0, 999_999_900),
            at(1, 0),
            at(1_700_000_000, 0),
            at(1_700_000_000, 100),
            at(4_000_000_000, 0),
        ];
        for pair in times.windows(2) {
            assert!(Tai64N::from_system_time(pair[0]) < Tai64N::from_system_time(pair[1]));
        }
    }

    #[test]
    fn one_nanosecond_later_carries_into_seconds() {
        let seconds = TAI64_EPOCH + 1_700_000_000;
        assert_eq!(
            Tai64N::from_parts(seconds, 999_999_999).one_nanosecond_later(),
            Tai64N::from_parts(seconds + 1, 0)
        );
        assert_eq!(
            Tai64N::from_parts(seconds, 7).one_nanosecond_later(),
            Tai64N::from_parts(seconds, 8)
        );
    }

    #[test]
    fn source_never_repeats_or_goes_back() {
        let mut source = TimestampSource::new();
        let clock = at(1_700_000_000, 999_999_900);
        let first = source.next_stamp_at(clock);
        let same_clock = source.next_stamp_at(clock);
        let clock_went_back = source.next_stamp_at(at(1_699_999_000, 0));
        let clock_moved_on = source.next_stamp_at(at(1_700_000_100, 0));

        assert_eq!(first, Tai64N::from_system_time(clock));
        assert!(first < same_clock);
        assert!(same_clock < clock_went_back);
        assert_eq!(
            clock_moved_on,
            Tai64N::from_system_time(at(1_700_000_100, 0))
        );

        let mut previous = clock_moved_on;
        for _ in 0..1000 {
            let next = source.next_stamp();
            assert!(next > previous);
            previous = next;
        }
    }

    #[test]
    fn round_trips_through_bytes() {
        let stamp = Tai64N::from_system_time(at(1_700_000_000, 123));
        assert_eq!(Tai64N::from_bytes(stamp.to_bytes()), stamp);
    }
}
