// Whether a microphone makes voice worse before Booth gets it, from what
// Windows says about the device: the rate its engine runs at, and whether it
// is a Bluetooth headset in hands-free mode. Settings warns under the input
// device, and the stats panel shows it as the "Microphone" line.

use super::format::Format;

// Below this a microphone is narrowband, telephone sound: at 8 kHz nothing
// above 4 kHz is left, which is where s, f and th differ.
const WIDEBAND_RATE: u32 = 16_000;

// Windows' Bluetooth hands-free driver, bthhfenum.sys, enumerates the
// hands-free endpoints, microphone and speaker alike. A2DP headphones come
// from BTHENUM, USB devices from USB, onboard sound from HDAUDIO.
const HANDS_FREE_ENUMERATOR: &str = "BTHHFENUM";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Microphone {
    // What the Windows audio engine runs the device at, before any
    // resampling to 48 kHz.
    pub rate: u32,
    pub hands_free: bool,
}

impl Microphone {
    pub fn new(engine: &Format, hands_free: bool) -> Microphone {
        Microphone {
            rate: engine.rate,
            hands_free,
        }
    }

    pub fn narrowband(&self) -> bool {
        self.rate < WIDEBAND_RATE
    }

    pub fn warns(&self) -> bool {
        self.narrowband() || self.hands_free
    }
}

// From two of the endpoint's properties. The enumerator decides whenever
// Windows gives one. The name is only a fallback: Windows 10 ends it with
// "Hands-Free", but Windows 11 calls the AirPods microphone "Headset (...)"
// with no sign of it, and older Bluetooth stacks write "Handsfree".
pub fn hands_free(enumerator: Option<&str>, name: Option<&str>) -> bool {
    match enumerator.map(str::trim).filter(|text| !text.is_empty()) {
        Some(enumerator) => enumerator.eq_ignore_ascii_case(HANDS_FREE_ENUMERATOR),
        None => name.is_some_and(|name| {
            let name = name.to_lowercase();
            name.contains("hands-free") || name.contains("handsfree")
        }),
    }
}
