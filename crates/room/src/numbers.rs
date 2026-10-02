// Turning one link's stats into the strip and the stats panel numbers.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use net::addrs::Path;
use stats::{LinkSnapshot, Thresholds, TraceSample};

use crate::peer::{Link, Sessions, Traffic};
use crate::talk::OnLink;
use crate::view::{Level, LinkState, Numbers, PathWord, Source, Strip, TracePoint};

pub(crate) fn level(level: stats::Level) -> Level {
    match level {
        stats::Level::Good => Level::Good,
        stats::Level::Warn => Level::Warn,
        stats::Level::Bad => Level::Bad,
    }
}

pub(crate) fn rtt_level(rtt_ms: Option<f32>, thresholds: &Thresholds) -> Level {
    rtt_ms.map_or(Level::Good, |ms| level(thresholds.rtt_level(ms)))
}

pub(crate) fn path_word(path: Path) -> PathWord {
    match path {
        Path::Lan => PathWord::Lan,
        Path::Direct => PathWord::Direct,
    }
}

pub(crate) fn millis(d: Duration) -> f32 {
    (d.as_nanos() as f64 / 1_000_000.0) as f32
}

// A link that went quiet keeps its last round trip on show for the panel to
// grey out. The stats stop reporting it once a later ping is lost, and by
// the time a link counts as reconnecting one always has been.
pub(crate) fn shown_rtt(reconnecting: bool, snapshot: &LinkSnapshot) -> Option<f32> {
    if !reconnecting || snapshot.rtt_ms.is_some() {
        return snapshot.rtt_ms;
    }
    snapshot.trace.iter().rev().find_map(|sample| match sample {
        TraceSample::Rtt(ms) => Some(*ms),
        TraceSample::Lost => None,
    })
}

// While the person at the other end of the link was heard in the last 2 s,
// the jitter and loss come from their voice, since 200 packets a second say
// more about the path than ten pings do. While they share and do not talk,
// from their video: each frame's first packet, 120 a second.
// Otherwise, and for the jitter until the voice has a capture time that can
// be read, from the pings. The round trip always comes from the pings:
// neither voice nor video carries a reply.
struct JitterAndLoss {
    jitter_ms: Option<f32>,
    jitter_from: Source,
    loss_pct: Option<f32>,
    loss_from: Source,
}

fn jitter_and_loss(
    snapshot: &LinkSnapshot,
    voice: Option<OnLink>,
    video: Option<OnLink>,
) -> JitterAndLoss {
    let video = video.filter(|_| voice.is_none());
    let (jitter_ms, jitter_from) = match (
        voice.and_then(|voice| voice.jitter_ms),
        video.and_then(|video| video.jitter_ms),
    ) {
        (Some(ms), _) => (Some(ms), Source::Voice),
        (None, Some(ms)) => (Some(ms), Source::Video),
        (None, None) => (snapshot.jitter_ms, Source::Pings),
    };
    let (loss_pct, loss_from) = match (voice, video) {
        (Some(voice), _) => (Some(voice.loss_pct), Source::Voice),
        (None, Some(video)) => (Some(video.loss_pct), Source::Video),
        (None, None) => (snapshot.loss_pct, Source::Pings),
    };
    JitterAndLoss {
        jitter_ms,
        jitter_from,
        loss_pct,
        loss_from,
    }
}

pub(crate) fn strip(
    state: LinkState,
    snapshot: &LinkSnapshot,
    voice: Option<OnLink>,
    video: Option<OnLink>,
    path: Option<PathWord>,
    thresholds: &Thresholds,
) -> Strip {
    let rtt_ms = shown_rtt(state == LinkState::Reconnecting, snapshot);
    let shown = jitter_and_loss(snapshot, voice, video);
    Strip {
        state,
        rtt_ms,
        rtt_level: rtt_level(rtt_ms, thresholds),
        jitter_ms: shown.jitter_ms,
        jitter_level: shown
            .jitter_ms
            .map_or(Level::Good, |ms| level(thresholds.jitter_level(ms))),
        jitter_from: shown.jitter_from,
        loss_pct: shown.loss_pct,
        loss_level: shown
            .loss_pct
            .map_or(Level::Good, |pct| level(thresholds.loss_level(pct))),
        loss_from: shown.loss_from,
        path,
        trace: snapshot
            .trace
            .iter()
            .map(|sample| match sample {
                TraceSample::Rtt(ms) => TracePoint::Rtt(*ms),
                TraceSample::Lost => TracePoint::Lost,
            })
            .collect(),
    }
}

// Everything the stats panel shows about one link. The caller adds what only
// its role knows.
pub(crate) struct LinkNumbers<'a> {
    pub name: Option<String>,
    pub reconnecting: bool,
    pub snapshot: &'a LinkSnapshot,
    pub voice: Option<OnLink>,
    pub video: Option<OnLink>,
    pub link: &'a Link,
    pub sessions: &'a Sessions,
    pub traffic: &'a Traffic,
    pub path: Option<PathWord>,
    pub peer_addr: Option<SocketAddr>,
    pub rekeys: u32,
    pub ping_interval: Duration,
}

impl LinkNumbers<'_> {
    pub(crate) fn fill(&self, numbers: &mut Numbers, now: Instant) {
        let s = self.snapshot;
        numbers.link_name = self.name.clone();
        numbers.rtt_ms = shown_rtt(self.reconnecting, s);
        numbers.rtt_avg_ms = s.rtt_avg_ms;
        numbers.rtt_min_ms = s.rtt_min_ms;
        numbers.rtt_max_ms = s.rtt_max_ms;
        numbers.rtt_p95_ms = s.rtt_p95_ms;
        let shown = jitter_and_loss(s, self.voice, self.video);
        numbers.jitter_ms = shown.jitter_ms;
        numbers.jitter_from = shown.jitter_from;
        numbers.loss_pct = shown.loss_pct;
        numbers.loss_from = shown.loss_from;
        numbers.inbound_loss_pct = s.inbound_loss_pct;
        numbers.ping_interval = self.ping_interval;
        numbers.path = self.path;
        numbers.peer_addr = self.peer_addr;
        numbers.clock_offset_ms = self.link.clock_offset_ms();
        numbers.session_age = self
            .sessions
            .current
            .as_ref()
            .map(|session| now.saturating_duration_since(session.created()));
        numbers.rekeys = self.rekeys;
        numbers.packets_sent = self.traffic.packets_sent;
        numbers.packets_received = self.traffic.packets_received;
        numbers.bytes_sent = self.traffic.bytes_sent;
        numbers.bytes_received = self.traffic.bytes_received;
        numbers.ack_delay_ms = self.link.ack_delay_ms(now);
        numbers.retransmits = self.traffic.earlier_retransmits + self.link.retransmissions();
    }
}
