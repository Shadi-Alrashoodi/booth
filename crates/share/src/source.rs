// What a share shows: a monitor through Desktop Duplication, or capture's
// test pattern standing in for one, and the encoder opened on the same
// device.

use std::time::{Duration, Instant};

use capture::{Adapter, Capture, CaptureError, Frame, Monitor, Next, Pattern, Rotation};
use encode::{Codec, EncodeError, Encoder, Fit, Kind, Settings};
use net::Timer;

use crate::Line;
use crate::knob::Knob;

// The source's Direct3D device, which the encoder works through. A macro
// rather than a method, since naming the device's type would take the
// windows crate as a dependency of this one.
macro_rules! device {
    ($source:expr) => {
        match $source {
            Source::Screen(capture) => capture.device(),
            Source::Pattern(pattern) => pattern.device(),
            Source::Busy(busy) => busy.pattern.device(),
        }
    };
}

#[derive(Clone)]
pub enum Choice {
    Screen(Monitor),
    // A monitor of this size, made up: one past 4096 wide or 1440 high is
    // scaled down to fit the way a capture would be. Nothing of the screen
    // is captured. Busy turns it to noise (Busy below).
    Pattern {
        adapter: capture::Adapter,
        width: u32,
        height: u32,
        busy: bool,
    },
}

impl Choice {
    // The GPU the share is captured, converted and encoded on.
    pub fn adapter(&self) -> &Adapter {
        match self {
            Choice::Screen(monitor) => &monitor.adapter,
            Choice::Pattern { adapter, .. } => adapter,
        }
    }
}

// PCI vendor ids.
const NVIDIA: u32 = 0x10de;
const INTEL: u32 = 0x8086;

// How each new encoder's codec is chosen (Setup::codec and
// Setup::takes_hevc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pick {
    // None leaves it to the share: HEVC where it is offered and the viewers
    // take it.
    pub codec: Option<Codec>,
    pub takes_hevc: bool,
}

// Which encoder takes HEVC at one size and rate, or why none does, for the
// log. Asked once for each size and rate, since asking opens an NVENC
// session for a moment (encode::offer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HevcOffer {
    pub size: (u32, u32),
    pub fps: u32,
    pub kind: Option<Kind>,
    pub why_not: String,
}

impl HevcOffer {
    pub(crate) fn is_for(&self, size: (u32, u32), fps: u32) -> bool {
        self.size == size && self.fps == fps
    }
}

// HEVC only from an encoder as good as the one H.264 gets. On an NVIDIA GPU
// that is NVENC: Media Foundation's encoder there answers every loss with
// an IDR where NVENC invalidates, and fewer bits are not worth that.
// Elsewhere the Media Foundation hardware encoder is what both codecs get:
// Booth has no code for AMD's or Intel's own encoders until there is such a
// PC to test it on. `pinned` is the one encoder the share may use.
pub(crate) fn hevc_kind(offered: &[Kind], pinned: Option<Kind>, nvidia: bool) -> Option<Kind> {
    if let Some(kind) = pinned {
        return offered.contains(&kind).then_some(kind);
    }
    let fastest = *offered.first()?;
    (!nvidia || fastest == Kind::Nvenc).then_some(fastest)
}

// Whether a share on the GPU of PCI vendor `vendor` starts on Windows'
// software encoder: on Intel graphics, unless the share was set up for one
// encoder or one codec (Setup::encoder, Setup::codec), as the loopback is to
// try Intel's own. Booth's use of Intel's hardware encoders is not tested:
// on an Iris Xe laptop the HEVC one gave one frame and never asked for
// another, and the H.264 one has never sent a frame through a room. NVENC
// and AMD's encoder keep their place.
pub(crate) fn starts_in_software(vendor: u32, encoder: Option<Kind>, codec: Option<Codec>) -> bool {
    vendor == INTEL && encoder.is_none() && codec.is_none()
}

pub(crate) fn intel_in_software(gpu: &str) -> String {
    format!(
        "the GPU is Intel's ({gpu}), whose hardware encoders Booth does not use yet, so this share uses the software encoder"
    )
}

// What the share says when it runs smaller for the software encoder.
pub(crate) fn runs_smaller(fit: Fit, (width, height): (u32, u32), fps: u32) -> Line {
    Line::Say(format!(
        "the software encoder takes up to 1080p at 60 fps, so this share runs at {}x{} and {} fps instead of {width}x{height} and {fps}",
        fit.width, fit.height, fit.fps
    ))
}

// Why hevc_kind took none of the encoders that offered HEVC.
fn not_that_one(offered: &[Kind], pinned: Option<Kind>) -> String {
    let names: Vec<String> = offered.iter().map(Kind::to_string).collect();
    match pinned {
        Some(kind) => format!(
            "{kind} does not take HEVC here, only {}",
            names.join(" and ")
        ),
        None => format!(
            "only {} takes HEVC here, which answers every loss with an IDR where NVENC invalidates",
            names.join(" and ")
        ),
    }
}

pub(crate) enum Source {
    Screen(Box<Capture>),
    Pattern(Box<Pattern>),
    Busy(Box<Busy>),
}

impl Source {
    pub(crate) fn open(choice: &Choice, options: capture::Options) -> Result<Source, String> {
        let text = |err: CaptureError| err.to_string();
        Ok(match choice {
            Choice::Screen(monitor) => {
                Source::Screen(Box::new(Capture::open(monitor, options).map_err(text)?))
            }
            Choice::Pattern {
                adapter,
                width,
                height,
                busy,
            } => {
                let device = capture::device_on(adapter).map_err(text)?;
                let pattern =
                    Pattern::with_source(&device, *width, *height, Rotation::Identity, options)
                        .map_err(text)?;
                if *busy {
                    Source::Busy(Box::new(Busy::new(pattern, options)?))
                } else {
                    Source::Pattern(Box::new(pattern))
                }
            }
        })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        let plan = match self {
            Source::Screen(capture) => capture.plan(),
            Source::Pattern(pattern) => pattern.plan(),
            Source::Busy(busy) => busy.pattern.plan(),
        };
        (plan.width, plan.height)
    }

    pub(crate) fn next(&mut self) -> Result<Next, String> {
        match self {
            Source::Screen(capture) => capture.next().map_err(|err| err.to_string()),
            Source::Pattern(pattern) => pattern
                .next()
                .map(Next::Frame)
                .map_err(|err| err.to_string()),
            Source::Busy(busy) => busy.next().map(Next::Frame),
        }
    }

    pub(crate) fn encoder(
        &self,
        kind: Option<Kind>,
        codec: Codec,
        (width, height): (u32, u32),
        fps: u32,
        settings: &Settings,
    ) -> Result<Box<dyn Encoder>, EncodeError> {
        let device = device!(self);
        match kind {
            Some(kind) => {
                encode::open_kind_codec(kind, codec, device, width, height, fps, settings)
            }
            None => encode::open_codec(codec, device, width, height, fps, settings),
        }
    }

    // `vendor` is the PCI vendor id of the GPU the share is on.
    pub(crate) fn hevc_offer(
        &self,
        pinned: Option<Kind>,
        vendor: u32,
        size: (u32, u32),
        fps: u32,
    ) -> HevcOffer {
        let offered = encode::offer(Codec::Hevc, device!(self), size.0, size.1, fps);
        let (kind, why_not) = match offered {
            Ok(offer) => match hevc_kind(&offer.kinds, pinned, vendor == NVIDIA) {
                Some(kind) => (Some(kind), String::new()),
                None if offer.kinds.is_empty() => (None, offer.refused.join("; ")),
                None => (None, not_that_one(&offer.kinds, pinned)),
            },
            Err(err) => (None, err.to_string()),
        };
        HevcOffer {
            size,
            fps,
            kind,
            why_not,
        }
    }
}

// The pattern's picture turned to faint noise, new every frame, which no
// encoder can predict from the frame before: a share of it sends as much as
// its rate lets it, as a game does, where the plain pattern sends 2 to 4
// Mbit/s at any rate. It is drawn on the CPU and goes to the GPU whole each
// frame, so it is for small sizes and short runs. Faint, 32 levels around
// grey, so the encoder can still meet a rate as low as the backoff's floor
// by quantizing it away.
pub(crate) struct Busy {
    pattern: Pattern,
    picture: Vec<u8>,
    noise: Knob,
    period: Option<Duration>,
    timer: Timer,
    due: Option<Instant>,
}

const BUSY_GREY: u8 = 112;
const BUSY_LEVELS: u64 = 32;

impl Busy {
    fn new(pattern: Pattern, options: capture::Options) -> Result<Busy, String> {
        let timer = Timer::new().map_err(|err| err.to_string())?;
        // The picture before any scaling, which convert takes.
        let plan = pattern.plan();
        let pixels = plan.source_width as usize * plan.source_height as usize;
        Ok(Busy {
            pattern,
            picture: vec![255; pixels * 4],
            noise: Knob::new(0.0, crate::fresh_seed()),
            period: (options.max_fps > 0).then(|| Duration::from_secs(1) / options.max_fps),
            timer,
            due: None,
        })
    }

    // At its time, as the pattern's own frames come.
    fn next(&mut self) -> Result<Frame, String> {
        if let Some(period) = self.period {
            let due = self.due.unwrap_or_else(Instant::now);
            self.timer
                .set_at(due)
                .and_then(|()| self.timer.wait())
                .map_err(|err| {
                    format!("could not wait for the busy pattern's next frame: {err}")
                })?;
            self.due = Some((due + period).max(Instant::now()));
        }
        // Two pixels from each draw, blue, green and red, alpha left at 255.
        for pixels in self.picture.chunks_mut(8) {
            let draw = self.noise.next();
            for (index, byte) in pixels.iter_mut().enumerate() {
                if index % 4 != 3 {
                    let level = (draw >> (index * 8)) % BUSY_LEVELS;
                    *byte = BUSY_GREY + level as u8;
                }
            }
        }
        self.pattern
            .convert(&self.picture)
            .map_err(|err| err.to_string())
    }
}

// The encoder goes first when dropped: it may still hold the source's
// textures.
pub(crate) struct Opened {
    pub encoder: Box<dyn Encoder>,
    pub source: Source,
    pub fps: u32,
    // What capture was asked for in the end: the software encoder's fit
    // when the source had to open again for it.
    pub options: capture::Options,
    // What encode::offer said of HEVC at this size and rate, when the pick
    // asked it.
    pub hevc: Option<HevcOffer>,
}

// The source and an encoder for it, in the codec `pick` gives. When only
// the software encoder is left and the share is larger than it takes, the
// source opens again at the size and rate it takes (encode::open_codec
// says why). `known` is what the share was told of HEVC before, as for
// codec_for.
pub(crate) fn open(
    choice: &Choice,
    kind: Option<Kind>,
    pick: Pick,
    settings: &Settings,
    mut options: capture::Options,
    mut known: Option<HevcOffer>,
    say: &mut dyn FnMut(Line),
) -> Result<Opened, String> {
    let mut fitted = false;
    loop {
        let source = Source::open(choice, options)?;
        let size = source.size();
        let fps = options.max_fps;
        let mut chosen = codec_for(&source, choice, kind, pick, known.take(), size, fps, say);
        let mut opened = source.encoder(chosen.kind, chosen.codec, size, fps, settings);
        if let (Err(err), Codec::Hevc, None) = (&opened, chosen.codec, pick.codec) {
            say(Line::Log(format!(
                "HEVC was offered at {}x{} and {fps} fps and did not open: {err}; going on in H.264",
                size.0, size.1
            )));
            if let Some(offer) = &mut chosen.hevc {
                offer.kind = None;
                offer.why_not = err.to_string();
            }
            chosen.codec = Codec::H264;
            chosen.kind = kind;
            opened = source.encoder(kind, Codec::H264, size, fps, settings);
        }
        match opened {
            Ok(encoder) => {
                return Ok(Opened {
                    encoder,
                    source,
                    fps,
                    options,
                    hevc: chosen.hevc,
                });
            }
            Err(
                EncodeError::NoEncoderForGpu { fit: Some(fit), .. }
                | EncodeError::SoftwareLimit { fit, .. },
            ) if !fitted => {
                say(runs_smaller(fit, size, options.max_fps));
                // Windows allows one duplication of a monitor per process,
                // so the old one goes before the new one opens.
                drop(source);
                known = chosen.hevc;
                // Both sides: the fit's width is worked out from the share's
                // size and capture's from the monitor's, and the two can
                // round apart.
                options = capture::Options {
                    max_width: fit.width,
                    max_height: fit.height,
                    max_fps: fit.fps,
                };
                fitted = true;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

// Whether `offer` refuses HEVC for a reason the log has not had from
// `known`, the offer asked before it.
fn new_reason(known: Option<&HevcOffer>, offer: &HevcOffer) -> bool {
    offer.kind.is_none()
        && known.is_none_or(|known| known.kind.is_some() || known.why_not != offer.why_not)
}

// The codec for a new encoder, the encoder to ask for it, and what the
// offer said of HEVC if it was asked.
pub(crate) struct Chosen {
    pub codec: Codec,
    pub kind: Option<Kind>,
    pub hevc: Option<HevcOffer>,
}

// What `pick` gives at `size` and `fps`. `known` is an offer asked before,
// used again when it is for the same size and rate. An HEVC share that is
// not offered says why in the log, once for each reason: an offer at a new
// size or rate refused for the reason `known` gave says nothing again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn codec_for(
    source: &Source,
    choice: &Choice,
    kind: Option<Kind>,
    pick: Pick,
    known: Option<HevcOffer>,
    size: (u32, u32),
    fps: u32,
    say: &mut dyn FnMut(Line),
) -> Chosen {
    if let Some(codec) = pick.codec {
        return Chosen {
            codec,
            kind,
            hevc: known,
        };
    }
    if !pick.takes_hevc {
        return Chosen {
            codec: Codec::H264,
            kind,
            hevc: known,
        };
    }
    let offer = match known {
        Some(offer) if offer.is_for(size, fps) => offer,
        known => {
            let offer = source.hevc_offer(kind, choice.adapter().vendor_id, size, fps);
            if new_reason(known.as_ref(), &offer) {
                say(Line::Log(format!(
                    "no HEVC at {}x{} and {fps} fps, so H.264: {}",
                    size.0, size.1, offer.why_not
                )));
            }
            offer
        }
    };
    match offer.kind {
        Some(hevc_kind) => Chosen {
            codec: Codec::Hevc,
            kind: Some(hevc_kind),
            hevc: Some(offer),
        },
        None => Chosen {
            codec: Codec::H264,
            kind,
            hevc: Some(offer),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_comes_only_from_an_encoder_as_good_as_the_h264_one() {
        let both = [Kind::Nvenc, Kind::MfHardware];
        let hardware = [Kind::MfHardware];
        // NVIDIA: NVENC, never Media Foundation's encoder in its place.
        assert_eq!(hevc_kind(&both, None, true), Some(Kind::Nvenc));
        assert_eq!(hevc_kind(&hardware, None, true), None);
        // AMD and Intel: the Media Foundation hardware encoder, as for H.264.
        assert_eq!(hevc_kind(&hardware, None, false), Some(Kind::MfHardware));
        assert_eq!(hevc_kind(&[], None, false), None);
        // One encoder asked for, and no other.
        assert_eq!(
            hevc_kind(&both, Some(Kind::MfHardware), true),
            Some(Kind::MfHardware)
        );
        assert_eq!(hevc_kind(&hardware, Some(Kind::Nvenc), true), None);
        assert_eq!(hevc_kind(&both, Some(Kind::MfSoftware), false), None);
        assert_eq!(
            not_that_one(&hardware, None),
            "only the Media Foundation hardware encoder takes HEVC here, which answers every loss with an IDR where NVENC invalidates"
        );
        assert_eq!(
            not_that_one(&both, Some(Kind::MfSoftware)),
            "the Media Foundation software encoder does not take HEVC here, only NVENC and the Media Foundation hardware encoder"
        );
    }

    #[test]
    fn intel_starts_in_software_unless_pinned() {
        const AMD: u32 = 0x1002;
        // A room's share asks for neither an encoder nor a codec.
        assert!(starts_in_software(INTEL, None, None));
        // The loopback's --encoder and --codec open what they ask for.
        for kind in Kind::ALL {
            assert!(!starts_in_software(INTEL, Some(kind), None), "{kind}");
        }
        for codec in Codec::ALL {
            assert!(!starts_in_software(INTEL, None, Some(codec)), "{codec}");
        }
        assert!(!starts_in_software(
            INTEL,
            Some(Kind::MfHardware),
            Some(Codec::Hevc)
        ));
        // NVENC and AMD's encoder keep their order.
        for vendor in [NVIDIA, AMD, 0] {
            assert!(!starts_in_software(vendor, None, None), "{vendor:#x}");
        }
        assert_eq!(
            intel_in_software("Intel(R) Iris(R) Xe Graphics"),
            "the GPU is Intel's (Intel(R) Iris(R) Xe Graphics), whose hardware encoders Booth does not use yet, so this share uses the software encoder"
        );
        let fit = Fit {
            width: 1920,
            height: 1080,
            fps: 60,
        };
        assert_eq!(
            runs_smaller(fit, (2560, 1440), 120),
            Line::Say(String::from(
                "the software encoder takes up to 1080p at 60 fps, so this share runs at 1920x1080 and 60 fps instead of 2560x1440 and 120"
            ))
        );
    }

    // What the log says of HEVC, once a share: a step down to 1080p60 asks
    // again and gets the same answer.
    #[test]
    fn a_reason_for_no_hevc_is_logged_once() {
        let refused = |size, why: &str| HevcOffer {
            size,
            fps: 60,
            kind: None,
            why_not: why.to_string(),
        };
        let why = "only the Media Foundation hardware encoder takes HEVC here";
        let first = refused((1920, 1200), why);
        let stepped_down = refused((1728, 1080), why);
        assert!(new_reason(None, &first));
        assert!(!new_reason(Some(&first), &stepped_down));
        assert!(new_reason(
            Some(&first),
            &refused((3840, 2400), "past every level")
        ));
        let offered = HevcOffer {
            kind: Some(Kind::Nvenc),
            ..refused((1920, 1200), "")
        };
        assert!(new_reason(Some(&offered), &first));
        assert!(!new_reason(Some(&first), &offered));
    }
}
