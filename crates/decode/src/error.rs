use std::fmt;
use std::path::PathBuf;

use crate::decoder::{Codec, MAX_ACCESS_UNIT};
use crate::library::AVCODEC;

// The size fields.c's limit is named by in the text. The limit counts 16x16
// blocks, of the size rounded up to 128 for HEVC, so another shape of about
// as many pixels passes too.
const LARGEST: (u32, u32) = (4096, 2304);

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DecodeError {
    /// One of FFmpeg's DLLs is not next to the exe.
    Missing {
        file: &'static str,
        folder: PathBuf,
    },
    NoExeFolder {
        detail: String,
    },
    Load {
        file: &'static str,
        folder: PathBuf,
        source: windows::core::Error,
    },
    MissingExport {
        file: &'static str,
        name: &'static str,
    },
    /// (major, minor) of the DLL and of the headers Booth was built with.
    WrongVersion {
        file: &'static str,
        found: (u32, u32),
        needed: (u32, u32),
    },
    /// A Direct3D call failed; `action` says what Booth was doing.
    Direct3D {
        action: &'static str,
        source: windows::core::Error,
    },
    /// The device cannot be used from the decoder's thread and the
    /// viewer's at once. `why` reads after "because".
    NotShareable {
        gpu: String,
        why: &'static str,
    },
    /// The device was removed or its driver reset. Nothing made on it works
    /// again: the viewer needs a new device and a new decoder on it.
    DeviceLost {
        gpu: String,
        source: windows::core::Error,
    },
    /// FFmpeg refused to set up the decoder. `action` reads after "could
    /// not".
    Setup {
        action: &'static str,
        gpu: String,
        detail: String,
    },
    NoDecoder {
        codec: Codec,
    },
    // From here on each is about one access unit, and the decoder stays
    // usable after it.
    Empty,
    TooLarge {
        size: usize,
    },
    /// FFmpeg found the access unit broken.
    Damaged {
        detail: String,
    },
    /// FFmpeg could not set up the GPU's decoder for the stream, and the
    /// device was not lost: the decoder does not take the size or profile,
    /// or video memory ran out. FFmpeg does not say which.
    Unsupported {
        gpu: String,
        codec: Codec,
        width: u32,
        height: u32,
        profile: String,
        level: i32,
    },
    /// Anything but 8-bit 4:2:0, which is all the viewer draws: 10-bit or
    /// 4:4:4, for example.
    WrongFormat {
        codec: Codec,
        profile: String,
    },
    /// Refused before FFmpeg sized anything for it: an H.264 picture in the
    /// format callback (fields.c), an HEVC one before FFmpeg saw its SPS
    /// (src/guard.rs). The size is the coded one.
    Oversized {
        width: u32,
        height: u32,
    },
    /// An HEVC SPS the decoder's own reader could not read, kept from
    /// FFmpeg with the rest of its access unit (src/guard.rs).
    UnreadableSps,
    /// An HEVC access unit with more SPSs than a stream can use, refused
    /// before any was read (src/guard.rs).
    TooManySps {
        count: usize,
    },
    /// The stream made the decoder hold frames back to reorder them. The
    /// decoder was reset, so nothing decodes until the next IDR.
    HeldBack {
        frames: u32,
    },
    /// A frame came back in memory other than a D3D11 texture.
    NotOnGpu,
    /// From [`crate::probe`]: HEVC on Intel graphics, which waits until it
    /// is tested there.
    HevcUntested {
        gpu: String,
    },
    /// From [`crate::probe`]: the GPU's own decoder does not take the codec
    /// at that size. `why` reads after "because"; `next` is a sentence of
    /// what may help.
    NotDecodable {
        gpu: String,
        codec: Codec,
        width: u32,
        height: u32,
        why: &'static str,
        next: &'static str,
    },
}

impl std::error::Error for DecodeError {}

const UNZIP: &str = "Unzip Booth again with all its files";

// Windows' own sentence for a failed load ends in a full stop and may carry
// a %1 for the file name.
fn system_text(source: &windows::core::Error, file: &str) -> String {
    source
        .message()
        .trim()
        .trim_end_matches('.')
        .replace("%1", file)
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Missing { file, folder } => write!(
                f,
                "the video decoder is missing: {file} was not found next to booth.exe in {}\\. {UNZIP}",
                folder.display()
            ),
            DecodeError::NoExeFolder { detail } => write!(
                f,
                "could not find the folder booth.exe runs from, where the video decoder's files are: {detail}"
            ),
            DecodeError::Load {
                file,
                folder,
                source,
            } => write!(
                f,
                "could not load {file} from {}\\: {}. {UNZIP}",
                folder.display(),
                system_text(source, file)
            ),
            DecodeError::MissingExport { file, name } => write!(
                f,
                "{file} has no {name}, so it is not the FFmpeg build Booth comes with. {UNZIP}"
            ),
            DecodeError::WrongVersion {
                file,
                found,
                needed,
            } => {
                if found.0 != needed.0 {
                    write!(
                        f,
                        "{file} is version {}, Booth needs {}. {UNZIP}",
                        found.0, needed.0
                    )
                } else {
                    write!(
                        f,
                        "{file} is version {}.{}, Booth needs {}.{} or newer. {UNZIP}",
                        found.0, found.1, needed.0, needed.1
                    )
                }
            }
            DecodeError::Direct3D { action, source } => {
                write!(f, "could not {action}: Direct3D said: {source}")
            }
            DecodeError::NotShareable { gpu, why } => write!(
                f,
                "the video decoder cannot share the viewer's Direct3D device on the {gpu} because {why}"
            ),
            DecodeError::DeviceLost { gpu, source } => write!(
                f,
                "the {gpu} was reset or removed, and the video decoder with it: Direct3D said: {source}. Open the screen share again"
            ),
            DecodeError::Setup {
                action,
                gpu,
                detail,
            } => write!(f, "could not {action} on the {gpu}: FFmpeg said: {detail}"),
            DecodeError::NoDecoder { codec } => write!(
                f,
                "{AVCODEC} has no {codec} decoder, so it is not the FFmpeg build Booth comes with. {UNZIP}"
            ),
            DecodeError::Empty => write!(f, "the frame to decode has no bytes"),
            DecodeError::TooLarge { size } => write!(
                f,
                "a frame of {size} bytes is larger than the {MAX_ACCESS_UNIT} Booth decodes, so it was dropped"
            ),
            DecodeError::Damaged { detail } => write!(
                f,
                "a video frame arrived damaged and did not decode: FFmpeg said: {detail}"
            ),
            DecodeError::Unsupported {
                gpu,
                codec,
                width,
                height,
                profile,
                level,
            } => {
                let level = codec
                    .level(*level)
                    .map(|level| format!(" at level {level}"))
                    .unwrap_or_default();
                write!(
                    f,
                    "the {gpu} could not set up its video decoder for this {width}x{height} {codec} {profile} screen share{level}: the decoder does not take that size or profile, or video memory is full. The sharer can share at a smaller size, which also needs less video memory"
                )
            }
            DecodeError::WrongFormat { codec, profile } => write!(
                f,
                "this screen share is {codec} {profile} video, which Booth does not show. Booth never shares video like that, so the sharer's Booth may be damaged: they can unzip it again"
            ),
            DecodeError::Oversized { width, height } => {
                let (most_width, most_height) = LARGEST;
                write!(
                    f,
                    "the screen share is {width}x{height}, larger than the {most_width}x{most_height} Booth shows at most. The sharer can share at a smaller size"
                )
            }
            DecodeError::UnreadableSps => write!(
                f,
                "a video frame carries an HEVC sequence header (SPS) Booth cannot read, so it was dropped before decoding. Booth never sends one like that, so the sharer's Booth may be damaged: they can unzip it again"
            ),
            DecodeError::TooManySps { count } => write!(
                f,
                "a video frame carries {count} HEVC sequence headers (SPS) where a Booth stream carries one, so it was dropped before decoding. The sharer's Booth may be damaged: they can unzip it again"
            ),
            DecodeError::HeldBack { frames } => {
                let held = if *frames == 1 {
                    "1 frame".to_string()
                } else {
                    format!("{frames} frames")
                };
                write!(
                    f,
                    "the video stream asked the decoder to hold {held} back for reordering, which a Booth stream never does. The picture waits for the next full frame"
                )
            }
            DecodeError::NotOnGpu => write!(
                f,
                "FFmpeg decoded a frame outside the graphics card, which the viewer cannot show"
            ),
            DecodeError::HevcUntested { gpu } => write!(
                f,
                "HEVC stays off on Intel graphics until it is tested there, after an Iris Xe failed on it a few seconds into every watch, so the {gpu} gets H.264"
            ),
            DecodeError::NotDecodable {
                gpu,
                codec,
                width,
                height,
                why,
                next,
            } => write!(
                f,
                "the {gpu} does not decode {codec} at {width}x{height} because {why}. {next}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::booth_max_macroblocks;

    #[test]
    fn largest_size_is_fields_c_limit() {
        let (width, height) = LARGEST;
        assert_eq!((width / 16 * (height / 16)) as i32, booth_max_macroblocks);
        let text = DecodeError::Oversized {
            width: 8192,
            height: 4608,
        }
        .to_string();
        assert_eq!(
            text,
            "the screen share is 8192x4608, larger than the 4096x2304 Booth shows at most. The sharer can share at a smaller size"
        );
    }

    #[test]
    fn level_numbers() {
        let unsupported = |codec, level| {
            DecodeError::Unsupported {
                gpu: "GPU".to_string(),
                codec,
                width: 8192,
                height: 64,
                profile: "Main".to_string(),
                level,
            }
            .to_string()
        };
        let h264 = unsupported(Codec::H264, 52);
        assert!(
            h264.contains("H.264 Main screen share at level 5.2:"),
            "{h264}"
        );
        let hevc = unsupported(Codec::Hevc, 153);
        assert!(
            hevc.contains("HEVC Main screen share at level 5.1:"),
            "{hevc}"
        );
        let hevc = unsupported(Codec::Hevc, 120);
        assert!(hevc.contains("at level 4.0:"), "{hevc}");
        // FFmpeg's "unknown" is -99.
        let unknown = unsupported(Codec::Hevc, -99);
        assert!(unknown.contains("HEVC Main screen share:"), "{unknown}");
    }

    #[test]
    fn one_frame_held_back_is_singular() {
        let one = DecodeError::HeldBack { frames: 1 }.to_string();
        assert!(one.contains("hold 1 frame back"), "{one}");
        let two = DecodeError::HeldBack { frames: 2 }.to_string();
        assert!(two.contains("hold 2 frames back"), "{two}");
    }
}
