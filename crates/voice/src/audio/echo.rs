// What Windows does to a microphone before Booth gets it, from the effects it
// lists for the open stream (Windows 11 and later; Windows 10 lists none),
// and whether echo is cancelled on a stream that asked for it.

use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub automatic_gain: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    EchoCancellation,
    NoiseSuppression,
    AutomaticGain,
    Other,
}

impl Effects {
    // Only effects that are on count.
    pub(crate) fn from_listed(listed: impl IntoIterator<Item = (Effect, bool)>) -> Effects {
        let mut effects = Effects::default();
        for (effect, on) in listed {
            match effect {
                Effect::EchoCancellation => effects.echo_cancellation |= on,
                Effect::NoiseSuppression => effects.noise_suppression |= on,
                Effect::AutomaticGain => effects.automatic_gain |= on,
                Effect::Other => {}
            }
        }
        effects
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Echo {
    // Not asked for: the switch is off, or this Windows cannot do it.
    #[default]
    NotAsked,
    // The microphone or its driver cancels echo itself, as a Bluetooth
    // headset's hands-free microphone does.
    Device,
    // Windows cancels it, on a stream opened in the communications category,
    // listening to `reference`, or with None to the speakers Windows picks,
    // which is its default output. `ducking_off` when Windows was told not to
    // lower other programs' sound for it.
    Windows {
        reference: Option<String>,
        ducking_off: bool,
    },
    // Asked for, and this microphone has none, so it was opened as before.
    Missing,
}

impl Echo {
    pub fn cancelled(&self) -> bool {
        matches!(self, Echo::Device | Echo::Windows { .. })
    }
}

// Whether a canceller Windows turned on can listen to the right speakers:
// the reference can be pointed, or Booth plays to Windows default, which
// is what the canceller listens to unless told otherwise.
pub(crate) fn reference_fits(pointable: bool, reference: Option<&str>) -> bool {
    pointable || reference.is_none()
}

// For the log line.
impl fmt::Display for Echo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Echo::NotAsked => f.write_str("not asked for"),
            Echo::Device => f.write_str("on, the microphone's own"),
            Echo::Windows {
                reference,
                ducking_off,
            } => write!(
                f,
                "on in Windows, listening to {}, other sound {}",
                reference.as_deref().unwrap_or("Windows default"),
                if *ducking_off {
                    "left alone"
                } else {
                    "may be lowered"
                }
            ),
            Echo::Missing => f.write_str("asked for, this microphone has none"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_effects_that_are_on_count() {
        let effects = Effects::from_listed([
            (Effect::EchoCancellation, true),
            (Effect::NoiseSuppression, false),
            (Effect::Other, true),
            (Effect::AutomaticGain, true),
        ]);
        assert_eq!(
            effects,
            Effects {
                echo_cancellation: true,
                noise_suppression: false,
                automatic_gain: true,
            }
        );
        assert_eq!(Effects::from_listed([]), Effects::default());
    }

    // A canceller that cannot be pointed is kept only while Booth plays to
    // Windows default.
    #[test]
    fn the_reference_has_to_fit() {
        assert!(reference_fits(true, Some("{0.0.0}.{speakers}")));
        assert!(reference_fits(true, None));
        assert!(reference_fits(false, None));
        assert!(!reference_fits(false, Some("{0.0.0}.{speakers}")));
    }
}
