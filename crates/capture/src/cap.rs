// The fps cap, as pure logic over present times, so it can be tested with
// made-up ones.
//
// Frames are judged by their present time (DWM's own clock), not by when this
// thread woke up, because present times sit exactly on the monitor's refresh
// grid and wakeups do not. Each frame that goes out books the next slot one
// interval later. While frames keep coming less than an interval apart, the
// slots stay on their own grid, so a frame that lands between two slots does
// not push the next one back and the stream keeps its full rate; after a
// quiet spell the grid starts again from the frame that ended it, so a still
// screen never banks slots for a burst. A frame up to a quarter interval
// before its slot still counts as on time: on a 240 Hz monitor every other
// frame lands exactly on a slot, and without that margin the jitter in DWM's
// timestamps would decide which of two neighbours goes out.
//
// A frame that is early is held. It goes out when its slot comes, but not
// before the next source frame would have arrived had the stream kept going,
// plus a little for delivery. If that next frame does arrive, it is newer
// and on time, and it goes out at once in the held one's place. So a steady
// stream never waits: at 144 Hz one frame in six is dropped and the other
// five go out the moment they arrive, where releasing held frames at their
// slot would send most of them late by up to a whole source frame. The last
// change before the screen goes still still goes out, at most one interval
// and the delivery margin after it was presented.

use std::time::{Duration, Instant};

// How long after its present time a source frame may take to reach the
// capture thread. On my PC it takes 0.03 ms typically and 0.5 ms at worst;
// the example program prints it as "present to acquired".
const DELIVERY: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Send,
    Hold,
}

pub(crate) struct FpsCap {
    interval: Duration,
    margin: Duration,
    // The next slot on the present time axis; None until the first frame.
    slot: Option<Instant>,
    last_present: Option<Instant>,
    held: Option<Held>,
}

#[derive(Clone, Copy)]
struct Held {
    present: Instant,
    streaming: bool,
    until: Instant,
}

impl FpsCap {
    // 0 is no cap: with no interval between slots, no frame is ever early.
    pub(crate) fn new(max_fps: u32) -> FpsCap {
        let interval = match max_fps {
            0 => Duration::ZERO,
            fps => Duration::from_secs(1) / fps,
        };
        FpsCap {
            interval,
            margin: interval / 4,
            slot: None,
            last_present: None,
            held: None,
        }
    }

    // A new source frame. Send means it goes out now, and any held frame is
    // dropped; Hold means it replaces the held frame, if there was one.
    pub(crate) fn arrive(&mut self, present: Instant) -> Decision {
        let gap = self
            .last_present
            .map(|last| present.saturating_duration_since(last))
            .filter(|&gap| gap < self.interval);
        self.last_present = Some(present);
        match self.slot {
            Some(slot) if present + self.margin < slot => {
                let next_source = present + gap.unwrap_or_default();
                self.held = Some(Held {
                    present,
                    streaming: gap.is_some(),
                    until: slot.max(next_source) + DELIVERY,
                });
                Decision::Hold
            }
            _ => {
                self.book(present, gap.is_some());
                Decision::Send
            }
        }
    }

    // When the held frame must go out if nothing newer arrives first.
    pub(crate) fn held_until(&self) -> Option<Instant> {
        self.held.map(|held| held.until)
    }

    // The held frame goes out now.
    pub(crate) fn release(&mut self) {
        if let Some(held) = self.held {
            self.book(held.present, held.streaming);
        }
    }

    // Forget the held frame without sending it, as when the duplication is
    // lost and recreated.
    pub(crate) fn drop_held(&mut self) {
        self.held = None;
    }

    fn book(&mut self, present: Instant, streaming: bool) {
        self.held = None;
        self.slot = Some(match self.slot {
            // The grid may trail this frame by at most an interval, more than
            // a stream ever builds up, so one late frame cannot open a burst.
            Some(slot) if streaming => {
                let floor = present.checked_sub(self.interval).unwrap_or(present);
                slot.max(floor) + self.interval
            }
            _ => present + self.interval,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Runs a stream through the cap the way the capture loop does: a frame
    // arrives a little after its present time, and before it is looked at,
    // a held frame whose time has come goes out. Returns (present, sent at)
    // for every frame that went out while the stream ran; a frame still held
    // at the end is left held.
    fn run(cap: &mut FpsCap, presents: &[Instant], delivery: Duration) -> Vec<(Instant, Instant)> {
        let mut sent = Vec::new();
        let mut held: Option<Instant> = None;
        for &present in presents {
            let arrival = present + delivery;
            if let (Some(until), Some(held_present)) = (cap.held_until(), held)
                && until <= arrival
            {
                cap.release();
                sent.push((held_present, until));
            }
            match cap.arrive(present) {
                Decision::Send => {
                    sent.push((present, arrival));
                    held = None;
                }
                Decision::Hold => held = Some(present),
            }
        }
        sent
    }

    fn stream(start: Instant, hz: f64, frames: usize) -> Vec<Instant> {
        (0..frames)
            .map(|n| start + Duration::from_secs_f64(n as f64 / hz))
            .collect()
    }

    const ARRIVAL: Duration = Duration::from_micros(300);

    #[test]
    fn at_240_hz_exactly_every_other_frame_goes_out_on_arrival() {
        let start = Instant::now();
        let presents = stream(start, 240.0, 480);
        let sent = run(&mut FpsCap::new(120), &presents, ARRIVAL);
        let expected: Vec<Instant> = presents.iter().copied().step_by(2).collect();
        let got: Vec<Instant> = sent.iter().map(|(present, _)| *present).collect();
        assert_eq!(got, expected);
        for (present, at) in &sent {
            assert_eq!(*at - *present, ARRIVAL, "a frame waited");
        }
    }

    #[test]
    fn at_240_hz_with_jittery_timestamps() {
        let start = Instant::now();
        // DWM's timestamps wobble by a few tens of microseconds.
        let presents: Vec<Instant> = stream(start, 240.0, 480)
            .into_iter()
            .enumerate()
            .map(|(n, present)| present + Duration::from_micros([0, 40, 15, 70, 5][n % 5]))
            .collect();
        let sent = run(&mut FpsCap::new(120), &presents, ARRIVAL);
        assert_eq!(sent.len(), 240);
        for pair in sent.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(
                gap > Duration::from_millis(8) && gap < Duration::from_micros(8_500),
                "gap of {gap:?}"
            );
        }
    }

    #[test]
    fn at_144_hz_the_average_is_120() {
        let start = Instant::now();
        let presents = stream(start, 144.0, 1440);
        let sent = run(&mut FpsCap::new(120), &presents, ARRIVAL);
        let seconds = 1440.0 / 144.0;
        let fps = sent.len() as f64 / seconds;
        assert!((fps - 120.0).abs() <= 0.5, "{fps} fps");
        let source_frame = Duration::from_secs_f64(1.0 / 144.0);
        for pair in sent.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(
                gap <= source_frame * 2 + Duration::from_micros(1),
                "gap of {gap:?}"
            );
        }
        let late = sent
            .iter()
            .filter(|(present, at)| *at - *present > ARRIVAL)
            .count();
        assert_eq!(late, 0, "{late} frames waited");
    }

    #[test]
    fn between_120_and_240_hz_the_cap_holds_without_losing_rate() {
        for hz in [130.0, 144.0, 165.0, 170.0, 200.0, 239.76, 240.0] {
            let start = Instant::now();
            let presents = stream(start, hz, (hz * 5.0) as usize);
            let sent = run(&mut FpsCap::new(120), &presents, ARRIVAL);
            let fps = sent.len() as f64 / 5.0;
            assert!((119.0..=120.5).contains(&fps), "{fps} fps from {hz} Hz");
            // No second of the run holds more than 121.
            for (n, (first, _)) in sent.iter().enumerate() {
                let within = sent[n..]
                    .iter()
                    .take_while(|(present, _)| *present - *first < Duration::from_secs(1))
                    .count();
                assert!(within <= 121, "{within} frames in a second from {hz} Hz");
            }
        }
    }

    #[test]
    fn a_240_hz_stream_after_a_still_spell() {
        let start = Instant::now();
        let mut cap = FpsCap::new(120);
        assert_eq!(cap.arrive(start), Decision::Send);
        let resumed = stream(start + Duration::from_millis(700), 240.0, 40);
        let sent = run(&mut cap, &resumed, ARRIVAL);
        let expected: Vec<Instant> = resumed.iter().copied().step_by(2).collect();
        let got: Vec<Instant> = sent.iter().map(|(present, _)| *present).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn at_120_hz_and_below_every_frame_goes_out() {
        for hz in [120.0, 119.88, 100.0, 75.0, 60.0, 30.0] {
            let start = Instant::now();
            let presents = stream(start, hz, 600);
            let sent = run(&mut FpsCap::new(120), &presents, ARRIVAL);
            assert_eq!(sent.len(), presents.len(), "at {hz} Hz");
            for (present, at) in &sent {
                assert_eq!(*at - *present, ARRIVAL, "a frame waited at {hz} Hz");
            }
        }
    }

    #[test]
    fn with_no_cap_every_frame_goes_out() {
        let start = Instant::now();
        for hz in [240.0, 360.0, 60.0] {
            let presents = stream(start, hz, 600);
            let sent = run(&mut FpsCap::new(0), &presents, ARRIVAL);
            assert_eq!(sent.len(), presents.len(), "at {hz} Hz");
            for (present, at) in &sent {
                assert_eq!(*at - *present, ARRIVAL, "a frame waited at {hz} Hz");
            }
        }
        // Two presents at the same instant, as after a mode change.
        let mut cap = FpsCap::new(0);
        assert_eq!(cap.arrive(start), Decision::Send);
        assert_eq!(cap.arrive(start), Decision::Send);
    }

    #[test]
    fn a_single_frame_after_a_still_period_goes_out_at_once() {
        let start = Instant::now();
        let mut cap = FpsCap::new(120);
        let mut presents = stream(start, 240.0, 20);
        let after = *presents.last().unwrap() + Duration::from_millis(500);
        presents.push(after);
        let sent = run(&mut cap, &presents, ARRIVAL);
        assert_eq!(sent.last().unwrap(), &(after, after + ARRIVAL));
        // And straight after another short burst.
        assert_eq!(cap.arrive(after + Duration::from_millis(9)), Decision::Send);
    }

    #[test]
    fn a_held_frame_goes_out_when_its_slot_comes() {
        let start = Instant::now();
        let mut cap = FpsCap::new(120);
        assert_eq!(cap.arrive(start), Decision::Send);
        // Two quick changes, then nothing: the second must not be lost.
        let second = start + Duration::from_millis(2);
        assert_eq!(cap.arrive(second), Decision::Hold);
        let slot = start + Duration::from_secs(1) / 120;
        assert_eq!(cap.held_until(), Some(slot + DELIVERY));
        cap.release();
        assert_eq!(cap.held_until(), None);
        // The next slot is one interval after the one the held frame used.
        let early = slot + Duration::from_millis(5);
        assert_eq!(cap.arrive(early), Decision::Hold);
        assert_eq!(
            cap.held_until(),
            Some(slot + Duration::from_secs(1) / 120 + DELIVERY)
        );
    }

    #[test]
    fn a_held_frame_waits_for_the_next_source_frame() {
        let start = Instant::now();
        let mut cap = FpsCap::new(120);
        let frame = Duration::from_secs_f64(1.0 / 144.0);
        assert_eq!(cap.arrive(start), Decision::Send);
        assert_eq!(cap.arrive(start + frame), Decision::Send);
        let third = start + frame * 2;
        assert_eq!(cap.arrive(third), Decision::Hold);
        assert_eq!(cap.held_until(), Some(third + frame + DELIVERY));
        // The fourth arrives in time and goes out; the third is dropped.
        assert_eq!(cap.arrive(start + frame * 3), Decision::Send);
        assert_eq!(cap.held_until(), None);
    }

    #[test]
    fn a_newer_early_frame_replaces_the_held_one() {
        let start = Instant::now();
        let mut cap = FpsCap::new(120);
        cap.arrive(start);
        assert_eq!(cap.arrive(start + Duration::from_millis(1)), Decision::Hold);
        let first_until = cap.held_until().unwrap();
        assert_eq!(cap.arrive(start + Duration::from_millis(3)), Decision::Hold);
        assert!(cap.held_until().unwrap() >= first_until);
        cap.release();
        assert_eq!(cap.held_until(), None);
    }
}
