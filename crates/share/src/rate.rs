// The rate a share encodes at, and when it runs at 1080p60 instead of the
// size and rate it was asked for. The host's bitrate rule gives the rate
// allowed; this backs off from it when the share's own video fills a queue
// somewhere, which shows as loss past the parity or a round trip that rises
// and stays, and climbs back once the link is clean. The room and the
// loopback run this same code, once a second, and every rule can be tried
// with made-up seconds. The numbers are placeholders, set where they are by
// the first share over the internet on 2026-09-29, for more shares with
// friends to measure.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use crate::recovery::Back;

// 20 percent down per backoff, at most once per 2 s: the software encoder
// takes about 0.27 s to act on a new rate, and a report needs a round trip
// to show what the last cut did.
pub const BACKOFF: f64 = 0.8;
pub const BACKOFF_GAP: Duration = Duration::from_secs(2);
// The backoff stops here: below it even 1080p60 is a smear, and a lower
// rate would not save a link that cannot carry 1 Mbit/s.
pub const RATE_FLOOR_KBPS: u32 = 1_000;

// Only a share that sends near its rate can fill a queue, so what left,
// video and parity over the last NEAR_SECONDS, is held against the rate in
// use. On busy content Booth's encoders make 75 to 85 percent of the rate
// and the parity adds at least a fifth, so a share sends about the rate or
// more (112 percent on the loopback's busy pattern); a still or lightly
// moving screen sends a tenth or less (6 to 9 percent of 15 Mbit/s in the
// first test over the internet, all upload counted, where 12 backoffs took
// the rate down to 1 Mbit/s). Half is far from both. Below it loss and
// delay are taken for the link's own and cause no backoff unless they pass
// the gate below; a lower rate would not change what goes out anyway.
pub const NEAR_SECONDS: usize = 2;
pub const NEAR_SHARE: f64 = 0.5;

// The gate: a link that carries less than half the rate in use, as while
// someone else in the house uploads, fills a queue with a share sending
// between the two, which the rule above never answers. In simulation a
// 5 Mbit/s link at 15 allowed with 7.2 sent lost 91 percent of its frames
// with no backoff. So two signs back off even while the share sends under
// half its rate, signs a link's own noise has not made in any of Booth's
// tests: HEAVY_LOST_PER_HUNDRED or more frames in 100 lost past the parity
// over a full loss window and in HEAVY_SECONDS of its seconds, or a round
// trip past AT_ONCE_MARGINS times the margin for FAR_SECONDS in a row. The
// first test's Wi-Fi at its worst lost 2.4 frames in 100 over 5 s and had a
// median 24 ms over its floor, under twice even the least margin, and
// --video-loss 20 in the loopback lost at worst 5 in 100 past the parity
// over 5 s in H.264 and 11 in HEVC (67 of 600); a queue that overflows loses
// 70 to 100 in 100 and holds the round trip up to the router's 200 ms. The
// loss is every watcher's together, each frame counted once, so lossy
// watchers add up: three losing 11 in 100 each at random lose about 30
// between them. Five seconds as the loss window has, where a Wi-Fi retry or
// scan holds the round trip up for a second at most. Both, since a watcher
// that has no picture yet reports next to no loss: in the loopback through a
// 2 Mbit/s link, 0 frames of 600 were reported lost while 66 percent of the
// packets were, and the round trip made the cut. Neither tells a queue the
// share filled from one it only waits in, as behind a watcher's own
// download. The cut is BACKOFF times what was sent over the last
// NEAR_SECONDS, which is what filled the queue: a fifth off 15 leaves 12,
// and the share sends its 7.2 as before. It stops at RATE_FLOOR_KBPS as
// every backoff does.
pub const HEAVY_LOST_PER_HUNDRED: u32 = 20;
pub const FAR_SECONDS: u32 = 5;

// What got through. Every watcher reports its shard loss over the last 2 s
// once a second for the parity, before its first picture too, when no frame
// it lost is reported any other way. A queue the share overflows loses
// shards and holds the round trip up by its depth, however shallow: behind
// a router queue of 17 ms a light share at 15 Mbit/s over 5.6 lost 37
// percent of its shards every second, each frame's burst and every IDR
// overflowing it, with the median round trip 2 to 6 ms over its floor and
// no picture to report a loss from, so neither rule above said so, in a
// simulated link. Loss on a radio link moves no round trip: 12 percent in
// bursts of 100 ms lost up to 43 percent over 2 s and was past SHARD_QUEUE
// in one report in ten, and random loss of 12 percent read 11 to 13. So
// SHARD_QUEUE or more with the round trip past half its margin backs off,
// as does SHARD_ALONE whatever the round trip does, and SHARD_QUEUE in
// SHARD_SECONDS reports in a row. The cut is BACKOFF times what arrived,
// what was sent less the shards lost, which is what the path carried: a
// share started at 15 Mbit/s on a 2.9 Mbit/s uplink took ten cuts and 20 s
// of a full queue to get under it by fifths, and two cuts this way. Not
// while the round trip falls by more than the margin, the queue draining
// after a cut: the watchers' 2 s still describe the rate before it.
//
// Heavy radio loss, 10 percent or more or long bursts of it, can still pass
// SHARD_ALONE in a single report and cut where nothing queued.
pub const SHARD_QUEUE: f32 = 25.0;
pub const SHARD_SECONDS: u32 = 5;
pub const SHARD_ALONE: f32 = 50.0;

// Frames lost past the parity are the share's queue only beside a sign of
// one: the round trip past its margin, or SHARD_FRAMES of shards lost. A
// radio burst of 30 to 100 ms takes out 2 to 12 frames in a row, past any
// parity, without moving the round trip, and jitter of 20 ms or more drops
// frames whose packets came more than a frame interval apart with no packet
// lost at all; at 5 percent loss in 30 ms bursts the loss rule alone cut a
// 60 Mbit/s link's share to the floor, 20 backoffs and nothing queued. The
// full margin, not half: jitter of 60 ms on Wi-Fi holds the median about
// two spreads over the floor, under the margin of four.
pub const SHARD_FRAMES: f32 = 40.0;

// The oldest ping on the share's links has waited this long for its pong,
// right after a second whose round trip was past the margin: the queue grew
// past the last second's pings, which is as far as a round trip can rise,
// and that second counts as past AT_ONCE_MARGINS times the margin, not as
// no news. Behind 295 ms of router buffer a drop to 0.5 Mbit/s queued 2.5 s,
// no pong came back within a second, and the rate climbed to 27 Mbit/s on
// the empty seconds.
pub const UNANSWERED_FAR_MS: f32 = 1000.0;

// The heavy loss counts only when HEAVY_SECONDS or more of the window's
// seconds are heavy on their own: HEAVY_LOST_PER_HUNDRED or more in 100 of
// the frames sent that second, and LOST_AT_LEAST frames at least. A queue
// the share fills loses frames every second it stands, 60 and then about
// 110 of 120 a second in the simulated 5 Mbit/s link above. An outage of a
// second or more loses every frame it covers, and a watcher hears of them
// only from the first frame after it, all in one recover request, so they
// fall in one second: on a lightly moving screen at 60 frames a second, 1 s
// of outage is 60 of the window's 300 frames, 20 in 100, and 2 s are 40,
// which through the gate cut 15 Mbit/s to the floor, 0.8 times the 1.2 the
// screen sent, for a link that was fine before and after. Heavy on its own
// frames and not any loss at all, since the first test's Wi-Fi lost a frame
// every third or fourth second, next to nearly any outage in 5 s; and
// LOST_AT_LEAST, since the IDR ask that can follow an outage's report the
// second after is one frame, which on a still screen's few a second is 20 in
// 100 or more by itself.
//
// That has two costs. One heavy second is an outage's and a queue's alike,
// so a queue that loses every frame from its first second backs off a
// second later than the window alone would have: in the simulated link
// falling at once from 20 Mbit/s to 4 or less under a share sending 7.2, at
// 12 s where it was 11, and each backoff after it a second later too, 120
// more frames lost and a second more of a full router queue. And under 25
// frames a second LOST_AT_LEAST is more than 20 in 100 of a second's
// frames, so a queue losing 4 of 10 every second, 40 in 100, never makes a
// heavy second, and under 5 frames a second only a report of frames from
// more than one second can. Those are left to the round trip, which a queue
// that overflows holds past AT_ONCE_MARGINS times the margin: in the tests,
// 4 of 10 lost backs off after FAR_SECONDS of it, 2 s after the window
// alone would have.
pub const HEAVY_SECONDS: usize = 2;

// Loss past what the parity repaired: frames any watcher reported lost
// (recover requests and IDR asks) over the last LOSS_SECONDS, more than
// LOST_PER_HUNDRED in 100 of the frames sent then, and at least
// LOST_AT_LEAST of them. With nothing queued, the laptop's Wi-Fi in the
// first test lost up to about 2.4 frames in 100 over 5 s, as the log's
// counts place them, and the parity is sized so 5 percent of packets lost at
// random drop under 1 frame in 100; a queue that overflows loses frames in
// runs, 70 to 100 in 100 in the loopback at 5 Mbit/s. Five seconds are 300
// frames at 60 fps, where one frame is a third of a point, and the floor of
// 5 frames keeps a still screen's few from adding up to a share. The window
// starts again at a backoff, so one loss is never cut for twice, and when
// the share comes near its rate, since what it lost below that was the
// link's own.
pub const LOSS_SECONDS: usize = 5;
pub const LOST_PER_HUNDRED: u32 = 4;
pub const LOST_AT_LEAST: u32 = 5;

// A queue shows as a round trip that rises and stays. Each second's lower
// quartile round trip (the third quickest of about 10 pings) has risen when
// it is past the lowest of the last ROUND_TRIP_FLOOR_OVER by more than the
// margin, and counts once it has for RISEN_SECONDS in a row: an IDR queues
// for a ping or two, which the quartile passes over, and a Wi-Fi retry or
// scan for a second at most. The quartile and not the median: a queue the
// share built delays every ping waiting behind it, while jitter on a radio
// delays some and leaves others. Jitter of 40 ms that came in episodes,
// with the margin learned in the calm between them at its least, held the
// median past it with nothing queued, and the rule cut 15 Mbit/s to 2.5 and
// took 55 s to climb back, in a simulated link.
// Past AT_ONCE_MARGINS times the margin it counts the first second: a step
// 10 percent past what the link carries queues 100 ms more every second, 2
// more seconds of it fill a home router's queue and a friend hears it in
// voice, and the first test's Wi-Fi never had a median past 24 ms over its
// floor, under twice even the least margin. Waiting the 2 s regardless kept
// 50 ms or more queued 1.5 to 2.1 times as long in a simulated link of 5
// to 12 Mbit/s. A rise that fell by more than the margin since the second
// before is a queue draining after a cut, which the cut answered already:
// in the loopback at 5 Mbit/s, cutting again while the router's full queue
// drained took the rate to 3.1 Mbit/s where 4.3 fit. The rise over the
// floor and not the median itself, since the room gives the worst of
// several watchers' links, a different one some seconds.
//
// The margin follows the link's own spread: SPREADS times how far the
// quickest quarter of the last 30 s of round trips reach above the floor,
// and never under ROUND_TRIP_RISE_MS. A jittery link's median sits about
// two such spreads above its floor by itself, so four leave two for a
// queue: the first test's Wi-Fi (ping 4/16/65 ms) had medians of 10 to
// 27 ms over a floor of 4, which the old fixed 15 ms took for a queue. The
// round trips' own spread rather than the strip's jitter: it comes from the
// same pings as the median and the floor, both ways, where the strip's
// jitter covers only the way toward this PC, which for a sharer is not the
// video's, and a Wi-Fi laptop's delay sits mostly in the way toward it. The
// quickest quarter rather than the spread around the median, and of the
// round trips within MOST_MARGIN_MS of the floor only, since a queue of the
// share's own is no part of the link's spread: in the loopback at 5 Mbit/s,
// 16 s of a full queue took the spread around the median, and with it the
// margin, to 78 ms, and 12 s of it counted in the quickest quarter held the
// margin at 50 ms to the end of the run, past a 35 ms queue that stood
// unseen. Never past MOST_MARGIN_MS either: the first test's Wi-Fi needed
// about 25, and a queue that stands past 50 is one a friend hears in voice.
pub const ROUND_TRIP_RISE_MS: f32 = 15.0;
pub const SPREADS: f32 = 4.0;
pub const MOST_MARGIN_MS: f32 = 50.0;
pub const RISEN_SECONDS: u32 = 2;
pub const AT_ONCE_MARGINS: f32 = 2.0;
pub const ROUND_TRIP_FLOOR_OVER: Duration = Duration::from_secs(30);

// The seconds come when the share's thread gets to them after a frame, a
// frame interval late or more while an encoder opens again, so two of them
// 2 s apart can measure 1.99 s. The gaps above and below are whole seconds,
// and this much short of one still counts.
const TICK_SLACK: Duration = Duration::from_millis(250);

// The climb back, over clean seconds: no sign of a queue, or signs let pass
// because the share sends too little to fill one. From a backoff to a step
// past the rate that queued it is careful, CAREFUL_CLIMB a step once the
// link has been clean for CAREFUL_APART_SECONDS since the backoff or the
// step before, so the queue a step makes has time to show and count before
// the next step adds to it: steps 2 s apart kept 50 ms or more queued 1.3
// to 2.1 times as long in a simulated link of 5 to 12 Mbit/s. The step past
// is careful too: the rate a backoff came at can be one the link carried,
// cut while an older queue drained. Anywhere else it climbs FAST_CLIMB every
// second once CLIMB_AFTER_SECONDS have been clean: 4 Mbit/s back to 15 in
// about 10 s, where 10 percent per 5 s took over a minute. After a backoff
// through the gate the rate that queued is what was sent: the rate in use
// above it never reached the link.
//
// While the share sends too little to count as near its rate, it climbs no
// further than where what it sends would count as near, or back to the rate
// that last queued: a rate it does not use is one the link has not shown it
// carries. Climbing on regardless, a share stepped down for a 5 Mbit/s link
// climbed to 10 at 1080p60 on light content, stepped up into the same queue
// twice in a minute and lost 32 percent of its frames, where capped it lost
// 18, nearly all while it was busy, in a simulated link.
pub const CLIMB_AFTER_SECONDS: u32 = 2;
pub const CAREFUL_CLIMB: f64 = 1.1;
pub const CAREFUL_APART_SECONDS: u32 = 5;
pub const FAST_CLIMB: f64 = 1.15;

// Below 8 Mbit/s for each viewer on an internet path, 1440p120 is not worth
// its frames. Back up at 10, a quarter above, so a rate that hovers around 8
// does not switch back and forth, and never within 10 s of the last step
// either way: each one costs a new encoder and an IDR. Nor while the climb
// is still careful: one backoff from 10 is 8, and a link that carries
// 9.5 Mbit/s queues at 10, so a share that stepped up there backed off under
// 8 and stepped down again 13 s later, 7 steps in 2 minutes in a simulated
// link, where waiting for the careful climb to end it steps down once and
// stays down.
//
// The climb stops at the rate allowed, so when that is between 8 and 10 the
// rate never reaches 10. There it steps back up once the rate is back at
// the rate allowed and the link has been clean for STEP_UP_CLEAN_SECONDS: a
// share that started at that rate would run full size, and a link that
// carries it is not the one that made it back off.
//
// And never before the rate has been clean for STEP_UP_CLEAN_SECONDS in a
// row, as long as a careful step waits. After the three backoffs of an
// overload the careful climb only reaches past the last of them, and the
// fast climb takes the rate past 10 in a second, past what a 10 to 12 Mbit/s
// link carries: stepping up there put the new encoder's IDR into a full
// queue, 9 steps in 200 s and freezes of 1 to 5 s in a simulated link.
pub const STEP_DOWN_BELOW_KBPS: u32 = 8_000;
pub const STEP_UP_FROM_KBPS: u32 = 10_000;
pub const STEPS_APART: Duration = Duration::from_secs(10);
pub const STEP_UP_CLEAN_SECONDS: u32 = 5;

// The encoder is too slow when its median time stays above the frame
// interval for 3 seconds in a row. P1 is the fastest preset already, so the
// share steps down to 1080p60 and stays there until it is started again: a
// GPU that fell behind next to a game would only fall behind again.
pub const SLOW_SECONDS: u32 = 3;

// A recover request after an outage can name many frames; past this many
// the loss rule has long since seen enough.
const MOST_LOST_IN_ONE: u32 = 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoundTrip {
    // The lower quartile over the last second and the lowest over the last
    // ROUND_TRIP_FLOOR_OVER; and over that time too, how far the lower
    // quartile of those within MOST_MARGIN_MS of that lowest is above it.
    pub recent_ms: f32,
    pub floor_ms: f32,
    pub spread_ms: f32,
}

impl RoundTrip {
    pub fn margin_ms(&self) -> f32 {
        (SPREADS * self.spread_ms).clamp(ROUND_TRIP_RISE_MS, MOST_MARGIN_MS)
    }

    pub fn rise_ms(&self) -> f32 {
        self.recent_ms - self.floor_ms
    }

    // How far the quartile is past the floor and the margin: risen above 0.
    pub fn past_margin_ms(&self) -> f32 {
        self.rise_ms() - self.margin_ms()
    }

    pub fn risen(&self) -> bool {
        self.past_margin_ms() > 0.0
    }
}

// The round trip the rules take, from a link's pings: each one's round
// trip with when its answer came, oldest first. None without a ping
// answered in the last second.
pub fn round_trip(pings: &VecDeque<(Instant, Duration)>, now: Instant) -> Option<RoundTrip> {
    let within = |span: Duration| {
        pings
            .iter()
            .rev()
            .take_while(move |(at, _)| now.saturating_duration_since(*at) < span)
            .map(|(_, rtt)| rtt.as_secs_f32() * 1000.0)
    };
    let mut recent: Vec<f32> = within(Duration::from_secs(1)).collect();
    if recent.is_empty() {
        return None;
    }
    let mut span: Vec<f32> = within(ROUND_TRIP_FLOOR_OVER).collect();
    span.sort_by(f32::total_cmp);
    let floor_ms = span[0];
    span.retain(|ms| *ms <= floor_ms + MOST_MARGIN_MS);
    recent.sort_by(f32::total_cmp);
    Some(RoundTrip {
        recent_ms: recent[(recent.len() - 1) / 4],
        floor_ms,
        spread_ms: span[(span.len() - 1) / 4] - floor_ms,
    })
}

// Frames the viewers reported lost past the parity since the rate last
// looked, as a share's audience hears their answers: every frame a recover
// request names, and one for an IDR asked for. After a backoff only frames
// sent after it count. Reports of frames lost before it keep coming for a
// round trip and a router's queue after the cut, and they are about the
// rate before it: in the loopback at 5 Mbit/s they cut the rate once more,
// to half of what the link carried.
#[derive(Debug, Default)]
pub struct Lost {
    count: u32,
    last_sent: Option<u32>,
    from: Option<u32>,
}

impl Lost {
    pub fn sent(&mut self, number: u32) {
        self.last_sent = Some(number);
    }

    pub fn heard(&mut self, back: &Back) {
        self.count = self.count.saturating_add(self.frames(back));
    }

    pub fn take(&mut self) -> u32 {
        std::mem::take(&mut self.count)
    }

    // The rate was cut: from the next frame sent on.
    pub fn backed_off(&mut self) {
        self.from = self.last_sent.map(|number| number.wrapping_add(1));
    }

    fn frames(&self, back: &Back) -> u32 {
        // At or after `from`, where frame numbers wrap.
        let counted = |number: u32| {
            self.from
                .is_none_or(|from| number.wrapping_sub(from) < 1 << 31)
        };
        match *back {
            Back::Recover { first, last } if counted(last) => {
                let first = if counted(first) {
                    first
                } else {
                    self.from.unwrap_or(first)
                };
                last.wrapping_sub(first)
                    .saturating_add(1)
                    .min(MOST_LOST_IN_ONE)
            }
            Back::Idr { seen } if counted(seen) => 1,
            _ => 0,
        }
    }
}

// What one second of the share measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct Second {
    // Frames sent, and frames the watchers reported lost beyond repair.
    pub sent: u32,
    pub lost: u32,
    // The frames' packets, data and parity.
    pub bytes: u64,
    // The round trip measured since the last second, or None. The room
    // measures it on its own clock, and a reading taken twice would count a
    // risen second twice.
    pub round_trip: Option<RoundTrip>,
    // How long the oldest ping on the share's links has waited for its pong,
    // or None with nothing waiting.
    pub unanswered_ms: Option<f32>,
    // The worst watcher's shard loss over its last 2 s, in percent, as last
    // reported, or None without a report lately.
    pub shard_loss: Option<f32>,
    // The median encode time, None when nothing was encoded.
    pub encode_ms: Option<f32>,
    pub interval: Duration,
    // Someone watches over an internet path.
    pub internet: bool,
}

// Why a share runs at 1080p60.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteppedDown {
    // Under 8 Mbit/s for someone watching over an internet path.
    LowRate,
    // The encoder took longer than a frame interval.
    SlowEncode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Down(SteppedDown),
    Up,
}

// A sign of a queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    Loss,
    RoundTrip,
    // Through the gate, while the share sends under half its rate.
    HeavyLoss,
    FarRoundTrip,
    ShardLoss,
}

impl Sign {
    fn through_gate(self) -> bool {
        matches!(self, Sign::HeavyLoss | Sign::FarRoundTrip | Sign::ShardLoss)
    }
}

impl fmt::Display for Sign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sign::Loss => f.write_str("loss"),
            Sign::RoundTrip => f.write_str("a risen round trip"),
            Sign::HeavyLoss => write!(
                f,
                "{HEAVY_LOST_PER_HUNDRED} or more frames in 100 lost in {HEAVY_SECONDS} s or more of {LOSS_SECONDS}"
            ),
            Sign::FarRoundTrip => write!(
                f,
                "a round trip past {AT_ONCE_MARGINS} times the margin for {FAR_SECONDS} s"
            ),
            Sign::ShardLoss => f.write_str("shards lost to a queue"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    // The rate to give the encoder, when it changed: cut for `backoff`, or
    // a climb.
    pub rate_kbps: Option<u32>,
    pub backoff: Option<Sign>,
    // A sign let pass because the share sends too little to fill a queue.
    pub let_pass: Option<Sign>,
    pub step: Option<Step>,
}

// What the last second was judged on, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Judged {
    // Video and parity sent over the last NEAR_SECONDS, against the rate in
    // use then.
    pub sent_kbps: u32,
    pub rate_kbps: u32,
    pub near: bool,
    // Frames lost and sent over the loss window, its length, and its seconds
    // heavy on their own (HEAVY_SECONDS).
    pub lost: u32,
    pub frames: u32,
    pub loss_seconds: usize,
    pub heavy_seconds: usize,
    pub round_trip: Option<RoundTrip>,
    pub risen_seconds: u32,
    // Seconds in a row risen past AT_ONCE_MARGINS times the margin.
    pub far_seconds: u32,
    pub unanswered_ms: Option<f32>,
    pub shard_loss: Option<f32>,
}

impl fmt::Display for Judged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let percent = if self.rate_kbps > 0 {
            u64::from(self.sent_kbps) * 100 / u64::from(self.rate_kbps)
        } else {
            0
        };
        write!(
            f,
            "sent {} kbit/s, {percent} percent of {}; {} of {} frames lost over {} s",
            self.sent_kbps, self.rate_kbps, self.lost, self.frames, self.loss_seconds
        )?;
        if self.heavy_seconds > 0 {
            write!(
                f,
                ", {} s of it at {LOST_AT_LEAST} frames and {HEAVY_LOST_PER_HUNDRED} in 100 or more",
                self.heavy_seconds
            )?;
        }
        f.write_str("; ")?;
        match self.round_trip {
            Some(rtt) => {
                write!(
                    f,
                    "round trip {:.1} ms against a floor of {:.1} and a margin of {:.1}",
                    rtt.recent_ms,
                    rtt.floor_ms,
                    rtt.margin_ms()
                )?;
                if self.risen_seconds > 0 {
                    write!(f, ", risen {} s", self.risen_seconds)?;
                }
                if self.far_seconds > 0 {
                    write!(f, ", past {AT_ONCE_MARGINS} times the margin")?;
                }
                if self.far_seconds > 1 {
                    write!(f, " for {} s", self.far_seconds)?;
                }
            }
            None => match self.unanswered_ms {
                Some(ms) if ms >= UNANSWERED_FAR_MS => {
                    write!(f, "no new round trip, a ping unanswered for {ms:.0} ms")?;
                    if self.far_seconds > 0 {
                        write!(f, ", past {AT_ONCE_MARGINS} times the margin")?;
                    }
                    if self.far_seconds > 1 {
                        write!(f, " for {} s", self.far_seconds)?;
                    }
                }
                _ => f.write_str("no new round trip")?,
            },
        }
        if let Some(percent) = self.shard_loss {
            write!(f, "; {percent:.1} percent of shards lost")?;
        }
        Ok(())
    }
}

pub struct Rate {
    allowed_kbps: u32,
    rate_kbps: u32,
    last_backoff: Option<Instant>,
    // Clean seconds in a row: no sign of a queue, or one let pass.
    clean: u32,
    // Seconds in a row the round trip has risen, and of those the ones past
    // AT_ONCE_MARGINS times the margin, and how far it was over the floor in
    // the last second that measured it.
    risen: u32,
    far: u32,
    rise_before: Option<f32>,
    // The rate in use when the last backoff came, or what was sent for one
    // through the gate, while the climb back is careful.
    queued_kbps: Option<u32>,
    // Bytes sent per second, newest last, NEAR_SECONDS at most.
    bytes: VecDeque<u64>,
    // Frames sent and lost per second, newest last, LOSS_SECONDS at most.
    loss: VecDeque<(u32, u32)>,
    near: bool,
    // Seconds with a sign let pass: in a row up to this one, and since the
    // count was last taken.
    letting_pass: u32,
    let_pass_seconds: u32,
    judged: Judged,
    small: Option<SteppedDown>,
    last_step: Option<Instant>,
    slow: u32,
    backoffs: u32,
    // The last round trip measured, for a second with none (UNANSWERED_FAR_MS).
    last_round_trip: Option<RoundTrip>,
    // Shard loss reports in a row at SHARD_QUEUE or more.
    shard_high: u32,
}

impl Rate {
    pub fn new(allowed_kbps: u32) -> Rate {
        Rate {
            allowed_kbps,
            rate_kbps: allowed_kbps,
            last_backoff: None,
            clean: 0,
            risen: 0,
            far: 0,
            rise_before: None,
            queued_kbps: None,
            bytes: VecDeque::with_capacity(NEAR_SECONDS),
            loss: VecDeque::with_capacity(LOSS_SECONDS),
            near: false,
            letting_pass: 0,
            let_pass_seconds: 0,
            judged: Judged::default(),
            small: None,
            last_step: None,
            slow: 0,
            backoffs: 0,
            last_round_trip: None,
            shard_high: 0,
        }
    }

    pub fn rate_kbps(&self) -> u32 {
        self.rate_kbps
    }

    pub fn allowed_kbps(&self) -> u32 {
        self.allowed_kbps
    }

    pub fn small(&self) -> Option<SteppedDown> {
        self.small
    }

    pub fn backoffs(&self) -> u32 {
        self.backoffs
    }

    pub fn judged(&self) -> &Judged {
        &self.judged
    }

    // A second's decision and what it was judged on, in the same words in
    // the room and the loopback.
    pub fn describe(&self, decision: &Decision) -> String {
        let what = match (decision.rate_kbps, decision.backoff, decision.let_pass) {
            (Some(_), Some(Sign::ShardLoss), _) => {
                format!(", backed off for {}, to what arrived", Sign::ShardLoss)
            }
            (Some(_), Some(sign), _) if sign.through_gate() => {
                format!(", backed off for {sign} though not near the rate, from what was sent")
            }
            (Some(_), Some(sign), _) => format!(", backed off for {sign}"),
            (Some(_), None, Some(sign)) => {
                format!(", climbing, not near the rate, {sign} let pass")
            }
            (Some(_), None, None) => String::from(", climbing"),
            (None, _, Some(sign)) => format!(", not near the rate, {sign} let pass"),
            (None, _, None) => String::new(),
        };
        format!(
            "rate {} kbit/s of {} allowed{what}; {}",
            self.rate_kbps, self.allowed_kbps, self.judged
        )
    }

    // The room's log line for a second: when the rate changed, or the first
    // second of signs let pass. The rest of a let pass that stands is a
    // count on the 10 s line (take_let_pass), not a line every second.
    pub fn line(&self, decision: &Decision) -> Option<String> {
        let starts_letting_pass = decision.let_pass.is_some() && self.letting_pass == 1;
        (decision.rate_kbps.is_some() || starts_letting_pass).then(|| self.describe(decision))
    }

    // Seconds with a sign let pass since this was last asked.
    pub fn take_let_pass(&mut self) -> u32 {
        std::mem::take(&mut self.let_pass_seconds)
    }

    // The host's rule changed, as someone started or stopped watching. A
    // lower rate applies at once. A higher one does too when the rate had
    // not backed off; otherwise the climb gets there.
    pub fn allow(&mut self, allowed_kbps: u32) -> Option<u32> {
        let was = self.rate_kbps;
        if allowed_kbps < self.rate_kbps || self.rate_kbps == self.allowed_kbps {
            self.rate_kbps = allowed_kbps;
        }
        self.allowed_kbps = allowed_kbps;
        self.at_allowed();
        (self.rate_kbps != was).then_some(self.rate_kbps)
    }

    // allow, for a share whose upload carries one copy for each watcher,
    // when that went from `before` copies to `after`. The new allowance
    // divides the setting, not what the link showed it carries, so a rate
    // the link held under the setting is shared out too, and the climb from
    // there is careful: on a 12 Mbit/s uplink at 8.7 for one watcher, a
    // second one made 18 Mbit/s of 12, three backoffs, 6.9 s before the
    // newcomer's first picture and a freeze for the first one, in a
    // simulated link.
    pub fn copies(&mut self, allowed_kbps: u32, before: u32, after: u32) -> Option<u32> {
        let was = self.rate_kbps;
        self.allow(allowed_kbps);
        if after > before && before > 0 {
            let shared = u64::from(was) * u64::from(before) / u64::from(after);
            let shared = u32::try_from(shared)
                .unwrap_or(u32::MAX)
                .max(RATE_FLOOR_KBPS.min(self.allowed_kbps));
            if shared < self.rate_kbps {
                self.rate_kbps = shared;
                self.queued_kbps = Some(shared);
            }
        }
        (self.rate_kbps != was).then_some(self.rate_kbps)
    }

    pub fn second(&mut self, now: Instant, second: &Second) -> Decision {
        let was = self.rate_kbps;
        if self.bytes.len() == NEAR_SECONDS {
            self.bytes.pop_front();
        }
        self.bytes.push_back(second.bytes);
        let seconds = self.bytes.len() as u64;
        let sent_kbps = kbps(self.bytes.iter().sum::<u64>() / seconds);
        let near = f64::from(sent_kbps) >= NEAR_SHARE * f64::from(self.rate_kbps);
        if near && !self.near {
            self.loss.clear();
            self.risen = 0;
            self.far = 0;
        }
        self.near = near;

        if self.loss.len() == LOSS_SECONDS {
            self.loss.pop_front();
        }
        self.loss.push_back((second.sent, second.lost));
        let (frames, lost) = self
            .loss
            .iter()
            .fold((0u32, 0u32), |(frames, lost), (s, l)| {
                (frames.saturating_add(*s), lost.saturating_add(*l))
            });
        let lossy = lost >= LOST_AT_LEAST
            && lost.saturating_mul(100) > frames.saturating_mul(LOST_PER_HUNDRED);
        let heavy_seconds = self
            .loss
            .iter()
            .filter(|&&(sent, lost)| heavy_loss(sent, lost))
            .count();
        let heavy = self.loss.len() == LOSS_SECONDS
            && heavy_loss(frames, lost)
            && heavy_seconds >= HEAVY_SECONDS;
        // A second with no new round trip leaves the counts as they were,
        // unless a ping has waited past UNANSWERED_FAR_MS right after a rise
        // past the margin.
        let (mut past, mut far, mut draining) = (false, false, false);
        let rise_was = self.rise_before;
        if let Some(rtt) = second.round_trip {
            let (rise, margin) = (rtt.rise_ms(), rtt.margin_ms());
            draining = self
                .rise_before
                .is_some_and(|before| before - rise > margin);
            self.rise_before = Some(rise);
            self.last_round_trip = Some(rtt);
            past = rise > margin;
            far = past && !draining && rise > AT_ONCE_MARGINS * margin;
            self.risen = if past && !draining { self.risen + 1 } else { 0 };
            self.far = if far { self.far + 1 } else { 0 };
        }
        let gone_far = second.round_trip.is_none()
            && second
                .unanswered_ms
                .is_some_and(|ms| ms >= UNANSWERED_FAR_MS)
            && rise_was
                .zip(self.last_round_trip)
                .is_some_and(|(rise, rtt)| rise > rtt.margin_ms());
        if gone_far {
            past = true;
            far = true;
            self.risen += 1;
            self.far += 1;
        }
        let measured = second.round_trip.is_some() || gone_far;
        let queued = measured && (self.risen >= RISEN_SECONDS || far);
        let standing = measured && self.far >= FAR_SECONDS;
        // The last round trip measured, past half its margin or past all of
        // it, for the shard loss and the frame loss to stand beside.
        let rise_now = second.round_trip.map(|rtt| rtt.rise_ms()).or(rise_was);
        let beside = |part: f32| {
            gone_far
                || rise_now
                    .zip(second.round_trip.or(self.last_round_trip))
                    .is_some_and(|(rise, rtt)| rise > rtt.margin_ms() * part)
        };
        let shard = second.shard_loss.unwrap_or(0.0);
        self.shard_high = if shard >= SHARD_QUEUE {
            self.shard_high + 1
        } else {
            0
        };
        let shard_heavy = !draining
            && (shard >= SHARD_ALONE
                || (beside(0.5) && shard >= SHARD_QUEUE)
                || self.shard_high >= SHARD_SECONDS);
        let corroborated = beside(1.0) || shard >= SHARD_FRAMES;
        let lossy = lossy && corroborated;
        let heavy = heavy && corroborated;
        let sign = if shard_heavy {
            Some(Sign::ShardLoss)
        } else if !near && heavy {
            Some(Sign::HeavyLoss)
        } else if !near && standing {
            Some(Sign::FarRoundTrip)
        } else if lossy {
            Some(Sign::Loss)
        } else if queued {
            Some(Sign::RoundTrip)
        } else {
            None
        };
        self.judged = Judged {
            sent_kbps,
            rate_kbps: self.rate_kbps,
            near,
            lost,
            frames,
            loss_seconds: self.loss.len(),
            heavy_seconds,
            round_trip: second.round_trip,
            risen_seconds: self.risen,
            far_seconds: self.far,
            unanswered_ms: second.unanswered_ms,
            shard_loss: second.shard_loss,
        };

        let mut decision = Decision::default();
        match sign {
            Some(sign) if near || sign.through_gate() => {
                self.clean = 0;
                let from = if sign == Sign::ShardLoss {
                    let arrived =
                        f64::from(sent_kbps) * f64::from(100.0 - shard.min(100.0)) / 100.0;
                    (arrived.round() as u32).min(self.rate_kbps)
                } else if near {
                    self.rate_kbps
                } else {
                    sent_kbps
                };
                if self.back_off(now, from) {
                    decision.backoff = Some(sign);
                    self.shard_high = 0;
                }
            }
            Some(sign) => {
                self.clean += 1;
                decision.let_pass = Some(sign);
            }
            // Risen but not for long enough yet, or draining: no climb on
            // it.
            None if near && past => self.clean = 0,
            None => self.clean += 1,
        }
        if decision.let_pass.is_some() {
            self.letting_pass += 1;
            self.let_pass_seconds += 1;
        } else {
            self.letting_pass = 0;
        }
        let most = if near {
            self.allowed_kbps
        } else {
            self.unused_most(sent_kbps)
        };
        let careful = self.careful();
        let needed = if careful {
            CAREFUL_APART_SECONDS
        } else {
            CLIMB_AFTER_SECONDS
        };
        if self.clean >= needed && self.rate_kbps < most {
            self.climb(most, careful);
        }
        decision.rate_kbps = (self.rate_kbps != was).then_some(self.rate_kbps);
        decision.step = self.step(now, second);
        decision
    }

    // Below a step past the rate that last queued.
    fn careful(&self) -> bool {
        self.queued_kbps
            .is_some_and(|queued| f64::from(self.rate_kbps) <= f64::from(queued) * CAREFUL_CLIMB)
    }

    // How far a share that is not near its rate may climb: to where what it
    // sends would be near, or back to the rate that last queued.
    fn unused_most(&self, sent_kbps: u32) -> u32 {
        let near_at = (f64::from(sent_kbps) / NEAR_SHARE).round() as u32;
        near_at
            .max(self.queued_kbps.unwrap_or(0))
            .min(self.allowed_kbps)
    }

    // A cut to BACKOFF times `from_kbps`, the rate in use or, through the
    // gate, what was sent, which is under half of it. True when the rate
    // came down.
    fn back_off(&mut self, now: Instant, from_kbps: u32) -> bool {
        let floor = RATE_FLOOR_KBPS.min(self.allowed_kbps);
        if !apart(self.last_backoff, now, BACKOFF_GAP) || self.rate_kbps <= floor {
            return false;
        }
        self.queued_kbps = Some(from_kbps);
        let cut = (f64::from(from_kbps) * BACKOFF).round() as u32;
        self.rate_kbps = cut.clamp(floor, self.rate_kbps);
        self.last_backoff = Some(now);
        self.backoffs += 1;
        self.loss.clear();
        self.risen = 0;
        self.far = 0;
        true
    }

    // Up to `most`, which is above the rate.
    fn climb(&mut self, most: u32, careful: bool) {
        let factor = if careful {
            // Clean again for as long before the next step.
            self.clean = 0;
            CAREFUL_CLIMB
        } else {
            self.queued_kbps = None;
            FAST_CLIMB
        };
        let climbed = (f64::from(self.rate_kbps) * factor).round() as u32;
        self.rate_kbps = climbed.max(self.rate_kbps + 1).min(most);
        self.at_allowed();
    }

    // Back at the rate allowed, however it got there, there is nothing left
    // to climb carefully to.
    fn at_allowed(&mut self) {
        if self.rate_kbps >= self.allowed_kbps {
            self.queued_kbps = None;
        }
    }

    fn step(&mut self, now: Instant, second: &Second) -> Option<Step> {
        let interval_ms = second.interval.as_secs_f32() * 1000.0;
        if second.encode_ms.is_some_and(|ms| ms > interval_ms) {
            self.slow += 1;
        } else {
            self.slow = 0;
        }
        let apart = apart(self.last_step, now, STEPS_APART);
        let slow = self.slow >= SLOW_SECONDS;
        let step = match self.small {
            None if slow => Some(Step::Down(SteppedDown::SlowEncode)),
            None if apart && second.internet && self.rate_kbps < STEP_DOWN_BELOW_KBPS => {
                Some(Step::Down(SteppedDown::LowRate))
            }
            // Small already: from now on it stays small.
            Some(SteppedDown::LowRate) if slow => {
                self.small = Some(SteppedDown::SlowEncode);
                None
            }
            Some(SteppedDown::LowRate) if apart && (!second.internet || self.full_size_again()) => {
                Some(Step::Up)
            }
            _ => None,
        };
        if let Some(step) = step {
            self.small = match step {
                Step::Down(why) => Some(why),
                Step::Up => None,
            };
            self.last_step = Some(now);
            // A new encoder, and a new frame interval to measure it against.
            self.slow = 0;
        }
        step
    }

    // Small for the rate, and the rate is back up (STEP_UP_FROM_KBPS), past
    // the careful climb, and has held clean there (STEP_UP_CLEAN_SECONDS).
    fn full_size_again(&self) -> bool {
        self.clean >= STEP_UP_CLEAN_SECONDS
            && self.queued_kbps.is_none()
            && (self.rate_kbps >= STEP_UP_FROM_KBPS
                || (self.allowed_kbps >= STEP_DOWN_BELOW_KBPS
                    && self.rate_kbps >= self.allowed_kbps))
    }
}

// HEAVY_LOST_PER_HUNDRED or more in 100 of `frames`, and never fewer than
// LOST_AT_LEAST, which also makes it loss by the ordinary rule.
fn heavy_loss(frames: u32, lost: u32) -> bool {
    lost >= LOST_AT_LEAST
        && lost.saturating_mul(100) >= frames.saturating_mul(HEAVY_LOST_PER_HUNDRED)
}

// At least `gap` since `last`, less TICK_SLACK, or nothing yet.
fn apart(last: Option<Instant>, now: Instant, gap: Duration) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) + TICK_SLACK >= gap)
}

fn kbps(bytes: u64) -> u32 {
    u32::try_from(bytes.saturating_mul(8) / 1000).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
