// The loss knob: drops a share of the video packets between the two sides,
// at random but from a seed, so a run's drops can be had again. SplitMix64
// is plenty for picking packets and needs no crate.

pub struct Knob {
    // Out of 2^53, so the comparison is exact for any percentage given with
    // a few decimals.
    threshold: u64,
    state: u64,
}

impl Knob {
    pub fn new(percent: f64, seed: u64) -> Knob {
        let share = (percent / 100.0).clamp(0.0, 1.0);
        Knob {
            threshold: (share * (1u64 << 53) as f64).round() as u64,
            state: seed,
        }
    }

    pub fn drops(&mut self) -> bool {
        self.threshold > 0 && self.next() >> 11 < self.threshold
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

// A seed nobody picked: the clock and the process id, which differ between
// any two runs.
pub fn fresh_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    let mut knob = Knob::new(0.0, nanos ^ (u64::from(std::process::id()) << 32));
    // Neighbouring seeds give unrelated first numbers, and a short seed is
    // easier to type back in.
    knob.next() % 1_000_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKETS: u32 = 200_000;
    // The binomial spread over 200 000 packets is 0.09 points at 20 percent
    // and 0.11 at 50, so 0.3 is 2.7 of it at the widest. The seeds are
    // fixed, so the test gives the same answer every run.
    const TOLERANCE: f64 = 0.3;

    fn measured(percent: f64, seed: u64) -> f64 {
        let mut knob = Knob::new(percent, seed);
        let dropped = (0..PACKETS).filter(|_| knob.drops()).count();
        dropped as f64 * 100.0 / f64::from(PACKETS)
    }

    #[test]
    fn drops_the_asked_share_within_a_third_of_a_point() {
        for percent in [0.5, 5.0, 20.0, 50.0] {
            for seed in [1, 42, 123_456_789] {
                let got = measured(percent, seed);
                println!("asked {percent}%, seed {seed}: dropped {got:.3}%");
                assert!(
                    (got - percent).abs() <= TOLERANCE,
                    "asked {percent}%, seed {seed}: dropped {got:.3}%"
                );
            }
        }
    }

    #[test]
    fn zero_drops_nothing_and_a_hundred_drops_everything() {
        assert_eq!(measured(0.0, 7), 0.0);
        assert_eq!(measured(100.0, 7), 100.0);
    }

    #[test]
    fn a_seed_gives_the_same_drops_again() {
        let pattern = |seed| {
            let mut knob = Knob::new(20.0, seed);
            (0..1000).map(|_| knob.drops()).collect::<Vec<bool>>()
        };
        assert_eq!(pattern(99), pattern(99));
        assert_ne!(pattern(99), pattern(100));
    }
}
