use std::time::{Duration, Instant};

use channels::video::VideoNumbers;
use encode::Codec;
use net::PaceNumbers;
use stats::Level;
use viewer::PresentPath;

// The strip's thresholds: capture to display sage under 20 ms on LAN and
// warn to 35, both later by the measured one-way network time over the
// internet; encode and decode sage under one frame interval.
const END_TO_END_GOOD_BELOW_MS: f32 = 20.0;
const END_TO_END_WARN_UP_TO_MS: f32 = 35.0;

// `one_way_ms` is 0 on LAN and in the loopback.
pub fn end_to_end_level(ms: f32, one_way_ms: f32) -> Level {
    let shift = if one_way_ms.is_finite() {
        one_way_ms.max(0.0)
    } else {
        0.0
    };
    if ms < END_TO_END_GOOD_BELOW_MS + shift {
        Level::Good
    } else if ms <= END_TO_END_WARN_UP_TO_MS + shift {
        Level::Warn
    } else {
        Level::Bad
    }
}

pub fn stage_level(ms: f32, interval: Duration) -> Level {
    if ms < interval.as_secs_f32() * 1000.0 {
        Level::Good
    } else {
        Level::Warn
    }
}

pub fn ms(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

// Median and 95th percentile by nearest rank: the smallest value with at
// least that share of the values at or below it. Sorts `values`.
pub fn spread(values: &mut [f32]) -> Option<(f32, f32)> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let at = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    Some((at(50), at(95)))
}

pub fn spread_text(values: &mut [f32]) -> String {
    match spread(values) {
        Some((median, p95)) => format!("median {median:.2} p95 {p95:.2}"),
        None => String::from("none"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharerNumbers {
    // Pictures the source made; for a screen, every one DWM put on the
    // monitor, including those the fps cap dropped.
    pub captured: u64,
    // Sent late, when their slot came, rather than the moment they arrived.
    pub held: u64,
    pub skipped: u64,
    pub encoded: u64,
    pub idrs: u64,
    pub bytes: u64,
    // Recover requests and frames lost here, and how they were answered:
    // by invalidation, by an IDR at once or after the floor, or not at all
    // since an earlier answer covers them. A request answered both ways
    // counts under both.
    pub recoveries: u64,
    pub invalidated: u64,
    pub idr_answers: u64,
    pub covered: u64,
    // IDRs forced because a viewer asked: it had none since the start, or
    // frames went on failing to decode after they were reported.
    pub idr_asks: u64,
    // Of the IDR answers and asks above, those that waited for the last IDR
    // to use a quarter of the upload setting (recovery::IDR_FLOOR_SHARE).
    pub floor_waits: u64,
    // Recover requests and IDR asks naming a frame not sent yet, left
    // unanswered: only a broken or hostile viewer sends one.
    pub unsent: u64,
    // Frames viewers reported lost that never left this PC: the pacer let
    // them go or they were too big, and the sharer recovered them already.
    // A busy PC, not the network; the room's backoff leaves them out.
    pub reported_lost_here: u64,
    pub too_big: u64,
    // IDRs the pacer let go, each made again at once: `idrs` counts both.
    pub idrs_let_go: u64,
    // New encoders for a share left to pick its codec, as its viewers came
    // and went (sharer::SWITCH_GAP).
    pub codec_changes: u64,
    pub pace: PaceNumbers,
    // Every frame's, only with Setup::keep_times.
    pub encode_ms: Vec<f32>,
    // From the first frame asked for to the last.
    pub ran: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct ViewerNumbers {
    pub reassembly: VideoNumbers,
    // Frames that came whole before the first IDR, and so could not decode.
    pub before_first_idr: u64,
    // Frames lost before the first picture: the one whose first packets
    // went out before this viewer was added, most often. The reassembler's
    // dropped count has them too; nothing asks the sharer to recover them.
    pub lost_before_first_idr: u64,
    pub decoded: u64,
    // Frames that came whole and did not decode, those in a codec the
    // viewer had no decoder for included: a new codec's frames before its
    // IDR, and HEVC on a viewer that does not take it once it has shown a
    // picture. Before that such frames count in before_first_idr.
    pub decode_failed: u64,
    pub presented: u64,
    // The codec of the last picture decoded, and how often a new codec's
    // IDR brought a new decoder in place of one that had given pictures:
    // the share's first HEVC IDR is not a change.
    pub codec: Option<Codec>,
    pub codec_changes: u64,
    pub first_present: Option<Instant>,
    pub last_present: Option<Instant>,
    pub path: Option<PresentPath>,
    // The three below have every frame's time, only with Watch::keep_times
    // and only in the numbers Screen::run returns.
    //
    // On the GPU, from just before FFmpeg got the frame to the picture
    // being done (decode::GpuTime).
    pub decode_ms: Vec<f32>,
    // FFmpeg's call alone, on the CPU.
    pub decode_call_ms: Vec<f32>,
    // Capture to display: to the present returning, or to the picture
    // being done on the GPU when that came later.
    pub end_to_end_ms: Vec<f32>,
    // From the window and decoder being ready to the end of the run.
    pub ran: Duration,
}

impl ViewerNumbers {
    pub fn fps(&self) -> Option<f64> {
        let (first, last) = (self.first_present?, self.last_present?);
        let seconds = last.saturating_duration_since(first).as_secs_f64();
        (self.presented > 1 && seconds > 0.0).then(|| (self.presented - 1) as f64 / seconds)
    }
}

pub fn path_word(path: Option<PresentPath>) -> &'static str {
    match path {
        Some(PresentPath::Flip) => "flip",
        Some(PresentPath::Composed) => "composed",
        None => "present path not known",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_median_and_95th_by_nearest_rank() {
        let mut values: Vec<f32> = (1..=100).rev().map(|v| v as f32).collect();
        assert_eq!(spread(&mut values), Some((50.0, 95.0)));
        assert_eq!(spread(&mut [4.0, 1.0, 3.0, 2.0]), Some((2.0, 4.0)));
        let mut twenty: Vec<f32> = (1..=20).map(|v| v as f32).collect();
        assert_eq!(spread(&mut twenty), Some((10.0, 19.0)));
        assert_eq!(spread(&mut [3.0]), Some((3.0, 3.0)));
        assert_eq!(spread(&mut []), None);
    }

    #[test]
    fn levels_at_the_thresholds() {
        let interval = Duration::from_secs(1) / 120;
        assert_eq!(stage_level(2.5, interval), Level::Good);
        assert_eq!(stage_level(8.4, interval), Level::Warn);
        assert_eq!(end_to_end_level(19.9, 0.0), Level::Good);
        assert_eq!(end_to_end_level(35.0, 0.0), Level::Warn);
        assert_eq!(end_to_end_level(35.1, 0.0), Level::Bad);
    }

    // A viewer 25 ms away over the internet: 44 ms of capture to display is
    // sage there, as 19 is on LAN.
    #[test]
    fn capture_to_display_thresholds_shift_by_the_one_way_time() {
        assert_eq!(end_to_end_level(44.0, 25.0), Level::Good);
        assert_eq!(end_to_end_level(45.0, 25.0), Level::Warn);
        assert_eq!(end_to_end_level(60.0, 25.0), Level::Warn);
        assert_eq!(end_to_end_level(60.1, 25.0), Level::Bad);
        assert_eq!(end_to_end_level(19.9, f32::NAN), Level::Good);
        assert_eq!(end_to_end_level(20.0, -3.0), Level::Warn);
    }
}
