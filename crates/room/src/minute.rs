// One link's control stream over the last minute, beside its pings. The
// retransmit timer is fed ping round trips; an ack that takes longer than a
// ping does (the peer's own timers, a path that treats the two differently)
// shows up here as retransmits with no loss behind them.

use std::collections::VecDeque;
use std::fmt::Write;
use std::time::{Duration, Instant};

use share::rate::RoundTrip;

use crate::log::counted;

const SPAN: Duration = Duration::from_secs(60);
// Pings go out ten times a second at most, messages far less often.
const MAX_SAMPLES: usize = 1024;

pub(crate) struct Minute {
    acks: VecDeque<(Instant, Duration)>,
    pings: VecDeque<(Instant, Duration)>,
    next_line: Instant,
    // The link's retransmit count when the last line was written.
    retransmits_before: u64,
}

impl Minute {
    pub(crate) fn new(now: Instant) -> Minute {
        Minute {
            acks: VecDeque::new(),
            pings: VecDeque::new(),
            next_line: now + SPAN,
            retransmits_before: 0,
        }
    }

    pub(crate) fn ack(&mut self, now: Instant, delay: Duration) {
        keep(&mut self.acks, now, delay);
    }

    pub(crate) fn ping(&mut self, now: Instant, rtt: Duration) {
        keep(&mut self.pings, now, rtt);
    }

    pub(crate) fn ack_delay_avg(&self, now: Instant) -> Option<Duration> {
        spread(&self.acks, now).map(|(_, avg, _, _)| avg)
    }

    // For the video rate's backoff (share::rate). None without a ping
    // answered in the last second.
    pub(crate) fn round_trip(&self, now: Instant) -> Option<RoundTrip> {
        share::rate::round_trip(&self.pings, now)
    }

    pub(crate) fn next_line(&self) -> Instant {
        self.next_line
    }

    // Once a minute: what the log line says after "reliable to <who>: ".
    // `retransmits` is the link's count so far, `timer` its retransmit
    // timeout now.
    pub(crate) fn line(
        &mut self,
        now: Instant,
        retransmits: u64,
        timer: Option<Duration>,
    ) -> Option<String> {
        if now < self.next_line {
            return None;
        }
        self.next_line = now + SPAN;
        let resent = retransmits.saturating_sub(self.retransmits_before);
        self.retransmits_before = retransmits;
        let mut line = String::new();
        match spread(&self.acks, now) {
            Some((count, avg, min, max)) => {
                let _ = write!(
                    line,
                    "{} acked, ack delay {}/{}/{} ms",
                    counted(count as u64, "message", "messages"),
                    ms(min),
                    ms(avg),
                    ms(max)
                );
            }
            None => line.push_str("0 messages acked, no ack delay"),
        }
        let _ = write!(line, ", {}", counted(resent, "retransmit", "retransmits"));
        match spread(&self.pings, now) {
            Some((_, avg, min, max)) => {
                let _ = write!(line, ", ping {}/{}/{} ms", ms(min), ms(avg), ms(max));
            }
            None => line.push_str(", no ping answered"),
        }
        match timer {
            Some(timer) => {
                let _ = write!(line, ", timer now {} ms", ms(timer));
            }
            None => line.push_str(", timer not set, no round trip yet"),
        }
        Some(line)
    }
}

fn keep(samples: &mut VecDeque<(Instant, Duration)>, now: Instant, value: Duration) {
    while samples
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= SPAN)
        || samples.len() >= MAX_SAMPLES
    {
        samples.pop_front();
    }
    samples.push_back((now, value));
}

// Count, average, min and max of the samples from the last minute.
fn spread(
    samples: &VecDeque<(Instant, Duration)>,
    now: Instant,
) -> Option<(usize, Duration, Duration, Duration)> {
    let recent = || {
        samples
            .iter()
            .filter(move |(at, _)| now.saturating_duration_since(*at) < SPAN)
            .map(|(_, value)| *value)
    };
    let count = recent().count();
    let min = recent().min()?;
    let max = recent().max()?;
    let sum: Duration = recent().sum();
    Some((count, sum / count as u32, min, max))
}

fn ms(value: Duration) -> u128 {
    (value.as_micros() + 500) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn minute_line() {
        let start = Instant::now();
        let mut minute = Minute::new(start);
        for (i, delay) in [18, 30, 140, 20, 32].into_iter().enumerate() {
            let at = start + Duration::from_secs(i as u64 + 1);
            minute.ack(at, ms(delay));
            minute.ping(at, ms([16, 32, 105, 20, 27][i]));
        }
        assert_eq!(minute.ack_delay_avg(start + ms(5000)), Some(ms(48)));
        assert_eq!(minute.line(start + ms(59_999), 1, Some(ms(190))), None);
        assert_eq!(
            minute.line(start + SPAN, 1, Some(ms(190))).as_deref(),
            Some(
                "5 messages acked, ack delay 18/48/140 ms, 1 retransmit, ping 16/40/105 ms, timer now 190 ms"
            )
        );
        // The next minute counts its own retransmits, and the first sample
        // has aged out of it.
        let next = start + SPAN * 2;
        minute.ack(start + SPAN + ms(1), ms(25));
        assert_eq!(
            minute.line(next, 4, None).as_deref(),
            Some(
                "1 message acked, ack delay 25/25/25 ms, 3 retransmits, no ping answered, timer not set, no round trip yet"
            )
        );
        let empty = next + SPAN;
        assert_eq!(
            minute.line(empty, 4, Some(ms(50))).as_deref(),
            Some(
                "0 messages acked, no ack delay, 0 retransmits, no ping answered, timer now 50 ms"
            )
        );
        assert_eq!(minute.ack_delay_avg(empty), None);
    }
}
