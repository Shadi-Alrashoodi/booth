use std::collections::VecDeque;

/// One pixel of the strip's trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TraceSample {
    Rtt(f32),
    Lost,
}

/// Samples kept, one per pixel of the strip's 120 px trace.
pub const TRACE_LEN: usize = 120;

/// Finalized pings the loss percentage is taken over. At 100, one lost ping
/// reads 1 percent, which is warn, and the second turns bad.
pub const LOSS_WINDOW: usize = 100;

const _: () = assert!(
    LOSS_WINDOW <= TRACE_LEN,
    "the loss percentage must only count pings the trace still draws"
);

#[derive(Debug, Default)]
pub(crate) struct Trace {
    samples: VecDeque<TraceSample>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct RttSummary {
    pub avg: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub p95: Option<f32>,
}

impl Trace {
    pub fn push(&mut self, sample: TraceSample) {
        if self.samples.len() == TRACE_LEN {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn to_vec(&self) -> Vec<TraceSample> {
        self.samples.iter().copied().collect()
    }

    pub fn loss_pct(&self) -> Option<f32> {
        let recent = self.samples.iter().rev().take(LOSS_WINDOW);
        let (total, lost) = recent.fold((0u32, 0u32), |(total, lost), s| match s {
            TraceSample::Lost => (total + 1, lost + 1),
            TraceSample::Rtt(_) => (total + 1, lost),
        });
        if total == 0 {
            None
        } else {
            Some(lost as f32 * 100.0 / total as f32)
        }
    }

    pub fn rtt_summary(&self) -> RttSummary {
        let mut values: Vec<f32> = self
            .samples
            .iter()
            .filter_map(|s| match s {
                TraceSample::Rtt(ms) => Some(*ms),
                TraceSample::Lost => None,
            })
            .collect();
        if values.is_empty() {
            return RttSummary::default();
        }
        values.sort_by(f32::total_cmp);

        let sum: f64 = values.iter().map(|v| f64::from(*v)).sum();
        let avg = (sum / values.len() as f64) as f32;

        // Nearest rank: the smallest value with at least 95 percent of the
        // samples at or below it. It is always a real sample, never a blend
        // of two, so the panel never shows a round trip that did not happen.
        let rank = (values.len() * 95).div_ceil(100).max(1);

        RttSummary {
            avg: Some(avg),
            min: values.first().copied(),
            max: values.last().copied(),
            p95: values.get(rank - 1).copied(),
        }
    }
}
