use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::inbound::Inbound;
use crate::trace::{Trace, TraceSample};

pub const LOST_AFTER: Duration = Duration::from_secs(2);

// About 25 s of pings at the 100 ms rate, far past LOST_AFTER. Only reached
// when the caller stops passing time in; it keeps memory bounded if so.
const MAX_IN_FLIGHT: usize = 256;

// Pings counted lost are still timed if their pong comes after all, for this
// long: behind a router queue of 2.5 s every pong is past LOST_AFTER, and
// the round trip it measures is the one thing that says how deep the queue
// is. The loss counts stand.
const LATE_FOR: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkSnapshot {
    pub rtt_ms: Option<f32>,
    pub rtt_avg_ms: Option<f32>,
    pub rtt_min_ms: Option<f32>,
    pub rtt_max_ms: Option<f32>,
    pub rtt_p95_ms: Option<f32>,
    pub jitter_ms: Option<f32>,
    pub loss_pct: Option<f32>,
    pub inbound_loss_pct: Option<f32>,
    pub pings_sent: u64,
    pub pongs_received: u64,
    pub lost: u64,
    pub trace: Vec<TraceSample>,
}

#[derive(Debug, Default)]
pub struct LinkStats {
    // Our pings in the order they were sent. The front is always unanswered;
    // anything answered behind it waits there so the trace stays in order.
    in_flight: VecDeque<Ping>,
    trace: Trace,
    inbound: Inbound,
    next_index: u64,
    latest_rtt: Option<(u64, f32)>,
    newest_lost: Option<u64>,
    pings_sent: u64,
    pongs_received: u64,
    lost: u64,
    // Pings counted lost within LATE_FOR, oldest first: their seq and when
    // they were sent.
    late: VecDeque<(u32, Instant)>,
}

#[derive(Debug)]
struct Ping {
    seq: u32,
    // Send order that does not wrap, unlike seq.
    index: u64,
    sent_at: Instant,
    outcome: Option<TraceSample>,
}

impl LinkStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ping_sent(&mut self, seq: u32, at: Instant) {
        self.expire(at);
        if self.in_flight.iter().any(|p| p.seq == seq) {
            return;
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            if let Some(oldest) = self.in_flight.front_mut()
                && oldest.outcome.is_none()
            {
                oldest.outcome = Some(TraceSample::Lost);
                self.lost += 1;
                self.newest_lost = self.newest_lost.max(Some(oldest.index));
                self.late.push_back((oldest.seq, oldest.sent_at));
            }
            self.finalize();
        }
        self.in_flight.push_back(Ping {
            seq,
            index: self.next_index,
            sent_at: at,
            outcome: None,
        });
        self.next_index += 1;
        self.pings_sent += 1;
    }

    /// `rtt` is worked out by the caller from the four ping timestamps, so the
    /// peer's time between receiving the ping and answering is not in it.
    /// `at` is when the pong arrived. Returns the time from sending the ping
    /// to `at` on our own clock, peer's time included, or None when the pong
    /// answers no ping still waiting for one. A pong for a ping already
    /// counted lost is timed too, and changes no count.
    pub fn pong_received(&mut self, seq: u32, rtt: Duration, at: Instant) -> Option<Duration> {
        self.expire(at);
        let Some(ping) = self
            .in_flight
            .iter_mut()
            .find(|p| p.seq == seq && p.outcome.is_none())
        else {
            let i = self.late.iter().position(|&(late, _)| late == seq)?;
            let (_, sent_at) = self.late.remove(i)?;
            return Some(at.saturating_duration_since(sent_at));
        };
        let elapsed = at.saturating_duration_since(ping.sent_at);
        // The peer reports its own processing time. Lying about it can shrink
        // the number but never push it past what our own clock saw.
        let rtt = rtt.min(elapsed);
        let ms = millis(rtt);
        ping.outcome = Some(TraceSample::Rtt(ms));
        self.pongs_received += 1;
        // The number on the strip follows the newest ping answered, so a late
        // pong for an older one does not overwrite it.
        if self.latest_rtt.is_none_or(|(index, _)| ping.index > index) {
            self.latest_rtt = Some((ping.index, ms));
        }
        self.finalize();
        Some(elapsed)
    }

    /// `peer_sent_us` is the peer's clock and `arrived_us` ours, both in
    /// microseconds. Only differences are used, so the clocks need not agree
    /// and either may wrap. If the peer starts its sequence numbers again,
    /// the count follows it after two pings in a row from the new run.
    pub fn peer_ping_received(&mut self, seq: u32, peer_sent_us: u64, arrived_us: u64) {
        self.inbound.record(seq, peer_sent_us, arrived_us);
    }

    pub fn tick(&mut self, now: Instant) {
        self.expire(now);
    }

    /// How long the oldest ping still waiting for its pong has waited, up to
    /// LOST_AFTER, or None with none waiting.
    pub fn waiting_for(&self, now: Instant) -> Option<Duration> {
        self.in_flight
            .iter()
            .find(|p| p.outcome.is_none())
            .map(|p| now.saturating_duration_since(p.sent_at))
    }

    pub fn snapshot(&self) -> LinkSnapshot {
        let summary = self.trace.rtt_summary();
        LinkSnapshot {
            rtt_ms: self.current_rtt(),
            rtt_avg_ms: summary.avg,
            rtt_min_ms: summary.min,
            rtt_max_ms: summary.max,
            rtt_p95_ms: summary.p95,
            jitter_ms: self.inbound.jitter_ms(),
            loss_pct: self.trace.loss_pct(),
            inbound_loss_pct: self.inbound.loss_pct(),
            pings_sent: self.pings_sent,
            pongs_received: self.pongs_received,
            lost: self.lost,
            trace: self.trace.to_vec(),
        }
    }

    // Once a ping sent after the newest answered one has gone unanswered for
    // LOST_AFTER, that answer no longer describes the link. Showing it would
    // keep a link whose pongs stopped coming back looking healthy.
    fn current_rtt(&self) -> Option<f32> {
        let (index, ms) = self.latest_rtt?;
        match self.newest_lost {
            Some(lost) if lost > index => None,
            _ => Some(ms),
        }
    }

    fn expire(&mut self, now: Instant) {
        for ping in &mut self.in_flight {
            if ping.outcome.is_none() && now.saturating_duration_since(ping.sent_at) >= LOST_AFTER {
                ping.outcome = Some(TraceSample::Lost);
                self.lost += 1;
                self.newest_lost = self.newest_lost.max(Some(ping.index));
                self.late.push_back((ping.seq, ping.sent_at));
            }
        }
        while self
            .late
            .front()
            .is_some_and(|&(_, sent_at)| now.saturating_duration_since(sent_at) >= LATE_FOR)
            || self.late.len() > MAX_IN_FLIGHT
        {
            self.late.pop_front();
        }
        self.finalize();
    }

    fn finalize(&mut self) {
        while let Some(sample) = self.in_flight.front().and_then(|p| p.outcome) {
            self.in_flight.pop_front();
            self.trace.push(sample);
        }
    }
}

fn millis(d: Duration) -> f32 {
    (d.as_nanos() as f64 / 1_000_000.0) as f32
}
