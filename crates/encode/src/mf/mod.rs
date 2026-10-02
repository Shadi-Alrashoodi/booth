// The Media Foundation encoders, the two fallbacks after NVENC: the hardware
// H.264 or HEVC encoder a GPU driver registers with Windows, which is what
// AMD GPUs share through until Booth has AMF, and Windows' own software H.264
// encoder, the last resort, which a share on Intel graphics starts with and
// any share falls back to when its GPU encoder fails. Both answer every loss
// with an IDR.

mod codec;
pub(crate) mod hardware;
mod platform;
pub(crate) mod software;

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, IMFMediaBuffer, IMFMediaType,
    IMFSample, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MFMediaType_Video,
    MFNominalRange_16_235, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High,
    eAVEncH265VProfile_Main_420_8,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use crate::{Codec, EncodeError, Fit, Preset};
use codec::CodecApi;
use platform::Functions;

// What the software encoder is held to: 1080p60. Sizes are counted in
// macroblocks, the way H.264 levels count them, so a share of another shape,
// portrait or ultrawide, gets the same pixel rate.
const SOFTWARE_MAX: (u32, u32, u32) = (1920, 1080, 60);
// Level 4.2, the one 1080p60 needs, allows no side longer than
// sqrt(8 * 8704) = 263 macroblocks (H.264 A.3.1).
const SOFTWARE_MAX_SIDE: u32 = 263 * 16;

// In u64: software_fit is public, and in u32 the product overflows from
// about a million pixels a side.
fn macroblocks(width: u32, height: u32) -> u64 {
    u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16))
}

/// The size and rate the software encoder takes for a share of this size,
/// or None when it takes this one as it is.
pub(crate) fn software_fit(width: u32, height: u32, fps: u32) -> Option<Fit> {
    let (max_width, max_height, max_fps) = SOFTWARE_MAX;
    let fits = |w: u32, h: u32| {
        macroblocks(w, h) <= macroblocks(max_width, max_height) && w.max(h) <= SOFTWARE_MAX_SIDE
    };
    if fits(width, height) && fps <= max_fps {
        return None;
    }
    let (mut fit_width, mut fit_height) = (width, height);
    if !fits(width, height) {
        // The caller asks capture for this fit's width, height and rate
        // (crate::software_fit says why both sides). Capture scales the side
        // further past its limit to it and works the other out, rounded to
        // the nearest even number; this works the width out the same way,
        // from each even height down until the picture fits.
        let width_at = |h: u32| {
            let exact = u64::from(width) * u64::from(h);
            (((exact + u64::from(height)) / (2 * u64::from(height)) * 2) as u32).max(2)
        };
        fit_height = (height & !1).max(2);
        while fit_height > 2 && !fits(width_at(fit_height), fit_height) {
            fit_height -= 2;
        }
        fit_width = width_at(fit_height);
    }
    Some(Fit {
        width: fit_width,
        height: fit_height,
        fps: fps.min(max_fps),
    })
}

fn mf_error(action: &'static str) -> impl Fn(windows::core::Error) -> EncodeError {
    move |source| EncodeError::MediaFoundation { action, source }
}

/// Calls an mfplat.dll function that hands back one new object.
fn create<T: Interface>(
    action: &'static str,
    call: impl FnOnce(*mut *mut c_void) -> windows::core::HRESULT,
) -> Result<T, EncodeError> {
    let mut raw = std::ptr::null_mut();
    call(&mut raw).ok().map_err(mf_error(action))?;
    if raw.is_null() {
        return Err(mf_error(action)(
            windows::Win32::Foundation::E_POINTER.into(),
        ));
    }
    // SAFETY: the call succeeded and handed over one reference to a T.
    Ok(unsafe { T::from_raw(raw) })
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// A video type of the given subtype with what the capture shader produces:
/// progressive, square pixels, BT.709 primaries, transfer and matrix, limited
/// range.
fn video_type(
    fns: &Functions,
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType, EncodeError> {
    // SAFETY: MFCreateMediaType's signature, with a valid out pointer.
    let kind: IMFMediaType = create("create a media type", |out| unsafe {
        (fns.create_media_type)(out)
    })?;
    let set = || -> windows::core::Result<()> {
        // SAFETY: setters on a live attribute store with valid GUID pointers.
        unsafe {
            kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            kind.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            kind.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            kind.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            kind.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            kind.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            kind.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            kind.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            kind.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            kind.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
        }
    };
    set().map_err(mf_error("describe the video format"))?;
    Ok(kind)
}

/// Media Foundation's name for a codec.
fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

/// The encoder's output: the codec at Booth's bitrate, H.264 in High
/// profile for CABAC and 8x8 transforms and HEVC in Main, as with NVENC.
fn output_type(
    fns: &Functions,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
) -> Result<IMFMediaType, EncodeError> {
    let kind = video_type(fns, &subtype(codec), width, height, fps)?;
    let profile = match codec {
        Codec::H264 => eAVEncH264VProfile_High.0,
        Codec::Hevc => eAVEncH265VProfile_Main_420_8.0,
    };
    let set = || -> windows::core::Result<()> {
        // SAFETY: as in video_type.
        unsafe {
            kind.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            kind.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)
        }
    };
    set().map_err(mf_error("describe the video format"))?;
    Ok(kind)
}

/// How an encoder is given its buffer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Buffer {
    /// A VBV of one frame's bits, so no frame bursts past the next.
    OneFrame,
    /// The size the encoder picks itself when its types are set, read back
    /// for the notes. Windows' software encoder picks 0.375 s of the bitrate.
    /// On the test pattern at 8 Mbit/s and 1080p60, a buffer of 1 to 3
    /// frames sent 37 Mbit/s, 4 to 12 frames 2.4 to 4.4, and 30 to 45
    /// frames 9.9; its own 7.7.
    EncodersOwn,
}

/// The settings every encoder in Booth runs with (low latency, CBR, no
/// scheduled IDRs, no B frames), as far as this one takes them. Set before
/// the media types, since some encoders read them only there, and checked
/// again after them with CodecApi::reapply.
fn configure(api: &mut CodecApi, fps: u32, bitrate: u32, preset: Preset, buffer: Buffer) {
    api.switch_on("low latency mode", &CODECAPI_AVLowLatencyMode);
    api.setting(
        "CBR".to_string(),
        &CODECAPI_AVEncCommonRateControlMode,
        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
    );
    api.setting(
        format!("{:.1} Mbit/s", f64::from(bitrate) / 1e6),
        &CODECAPI_AVEncCommonMeanBitRate,
        VARIANT::from(bitrate),
    );
    // In bits, which neither the header nor the documentation says. The
    // same frame counted in bytes, an eighth of the number, made NVIDIA's
    // encoder run over at 17.2 Mbit/s against 15; in bits it stays under.
    // The software encoder's own is read back after the types are set.
    if buffer == Buffer::OneFrame {
        api.setting(
            "VBV of one frame".to_string(),
            &CODECAPI_AVEncCommonBufferSize,
            VARIANT::from(bitrate / fps),
        );
    }
    // The largest GOP there is. 0 would mean "no scheduled IDR" by the
    // property's definition, but Windows' software encoder reads it as one
    // IDR a second; neither encoder here reports a range to pick from.
    api.setting(
        "no scheduled IDRs".to_string(),
        &CODECAPI_AVEncMPVGOPSize,
        VARIANT::from(u32::MAX),
    );
    api.setting(
        "no B frames".to_string(),
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        VARIANT::from(0u32),
    );
    // 0 is fastest, 100 best quality.
    let speed = match preset {
        Preset::P1 => 0u32,
        Preset::P2 => 20,
        Preset::P3 => 40,
        Preset::P4 => 60,
    };
    api.setting(
        format!("quality against speed {speed}"),
        &CODECAPI_AVEncCommonQualityVsSpeed,
        VARIANT::from(speed),
    );
}

/// A new bitrate from the next frame on, with a buffer of `buffer_bits`
/// where the encoder takes one.
fn change_bitrate(
    api: &CodecApi,
    bitrate: u32,
    buffer_bits: Option<u32>,
) -> windows::core::Result<()> {
    api.set(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate))?;
    if let Some(bits) = buffer_bits
        && api.supports(&CODECAPI_AVEncCommonBufferSize)
    {
        api.set(&CODECAPI_AVEncCommonBufferSize, &VARIANT::from(bits))?;
    }
    Ok(())
}

/// Notes the buffer the encoder picked for itself, read back once its types
/// are set.
fn note_own_buffer(api: &mut CodecApi, bitrate: u32) {
    let size = match api.value(&CODECAPI_AVEncCommonBufferSize) {
        Some(bits) => format!(
            "{bits} bits, {:.2} s at {:.1} Mbit/s",
            f64::from(bits) / f64::from(bitrate),
            f64::from(bitrate) / 1e6
        ),
        None => "size, which it does not report".to_string(),
    };
    api.note(format!(
        "VBV left at the encoder's own {size}, since a smaller one throws its rate control off"
    ));
}

/// Every loss is answered with an IDR on these encoders, so one that cannot
/// make an IDR on request could never recover a share and is refused at
/// open, which lets open_codec() move on to the next encoder. `problem` is
/// the sentence the error gives.
fn check_idr_on_request(api: &mut CodecApi, problem: &'static str) -> Result<(), EncodeError> {
    if !api.supports(&CODECAPI_AVEncVideoForceKeyFrame) {
        return Err(EncodeError::EncoderLacks { problem });
    }
    api.note("IDR on request available".to_string());
    Ok(())
}

/// The calls Output makes of an encoder, apart so that a scripted one can
/// stand in for a real one in the tests below.
trait Outputs {
    fn stream_info(&self) -> windows::core::Result<MFT_OUTPUT_STREAM_INFO>;
    fn process_output(
        &self,
        buffers: &mut [MFT_OUTPUT_DATA_BUFFER; 1],
        status: &mut u32,
    ) -> windows::core::Result<()>;
    /// Takes the output format the encoder offers first, after it said its
    /// format changed.
    fn take_new_format(&self) -> Result<(), EncodeError>;
}

impl Outputs for IMFTransform {
    fn stream_info(&self) -> windows::core::Result<MFT_OUTPUT_STREAM_INFO> {
        // SAFETY: a getter on a live transform with its types set.
        unsafe { self.GetOutputStreamInfo(0) }
    }

    fn process_output(
        &self,
        buffers: &mut [MFT_OUTPUT_DATA_BUFFER; 1],
        status: &mut u32,
    ) -> windows::core::Result<()> {
        // SAFETY: one output buffer for the encoder's one stream, with a
        // sample of the size it asked for when it does not bring its own.
        unsafe { self.ProcessOutput(0, buffers, status) }
    }

    fn take_new_format(&self) -> Result<(), EncodeError> {
        // SAFETY: the encoder offers its new output type first.
        unsafe {
            let kind = self
                .GetOutputAvailableType(0, 0)
                .map_err(mf_error("read the encoder's new output format"))?;
            self.SetOutputType(0, &kind, 0)
                .map_err(mf_error("take the encoder's new output format"))
        }
    }
}

/// Where an encoder's bitstream comes out: in samples it brings itself, or
/// in one of ours that is used again frame after frame, since the bytes are
/// copied out before the next call.
struct Output {
    provides_samples: bool,
    size: u32,
    own: Option<IMFSample>,
    // An asynchronous encoder hands its output over only against a
    // METransformHaveOutput event, one ProcessOutput each. Asked without
    // one, NVIDIA's encoders answer E_UNEXPECTED, "Catastrophic failure",
    // every time (the tests in hardware.rs check it), so after a new output
    // format the frame comes with the next event. A synchronous encoder has
    // it at once.
    asynchronous: bool,
    format_changed: bool,
}

impl Output {
    fn of(transform: &impl Outputs, asynchronous: bool) -> Result<Output, EncodeError> {
        let info = transform
            .stream_info()
            .map_err(mf_error("read the encoder's output stream"))?;
        let flags =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        Ok(Output {
            provides_samples: info.dwFlags & flags != 0,
            size: info.cbSize.max(1 << 20),
            own: None,
            asynchronous,
            format_changed: false,
        })
    }

    /// Whether the encoder changed its output format since the last call.
    fn format_changed(&mut self) -> bool {
        std::mem::take(&mut self.format_changed)
    }

    /// One ProcessOutput, two for a synchronous encoder whose output format
    /// changed. None when the encoder has nothing to give yet.
    fn take(
        &mut self,
        transform: &impl Outputs,
        fns: &Functions,
    ) -> Result<Option<Vec<u8>>, EncodeError> {
        for _ in 0..2 {
            let own = match (self.provides_samples, self.own.take()) {
                (true, _) => None,
                (false, Some(sample)) => Some(sample),
                (false, None) => Some(output_sample(fns, self.size)?),
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = transform.process_output(&mut buffers, &mut status);
            // Whatever the call left in the buffer is ours to release.
            let sample = ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pSample,
                ManuallyDrop::new(None),
            ));
            drop(ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pEvents,
                ManuallyDrop::new(None),
            )));
            match result {
                Ok(()) => {
                    let Some(sample) = sample else {
                        return Ok(None);
                    };
                    let bytes = sample_bytes(&sample)?;
                    if !self.provides_samples {
                        self.own = Some(sample);
                    }
                    return Ok(Some(bytes));
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    if !self.provides_samples {
                        self.own = sample;
                    }
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    transform.take_new_format()?;
                    *self = Output::of(transform, self.asynchronous)?;
                    self.format_changed = true;
                    if self.asynchronous {
                        return Ok(None);
                    }
                }
                Err(source) => {
                    return Err(EncodeError::MediaFoundation {
                        action: "take the encoded frame from the encoder",
                        source,
                    });
                }
            }
        }
        Ok(None)
    }
}

fn output_sample(fns: &Functions, size: u32) -> Result<IMFSample, EncodeError> {
    // SAFETY: MFCreateMemoryBuffer's and MFCreateSample's signatures.
    let buffer: IMFMediaBuffer = create("create an output buffer", |out| unsafe {
        (fns.create_memory_buffer)(size, out)
    })?;
    let sample: IMFSample = create("create an output sample", |out| unsafe {
        (fns.create_sample)(out)
    })?;
    // SAFETY: both are live.
    unsafe { sample.AddBuffer(&buffer) }.map_err(mf_error("create an output sample"))?;
    Ok(sample)
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, EncodeError> {
    let error = mf_error("read the encoded frame");
    // SAFETY: plain calls on a live sample; while locked, `data` points at
    // `length` readable bytes, which are copied before the unlock.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(&error)?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer
            .Lock(&mut data, None, Some(&mut length))
            .map_err(&error)?;
        let bytes = if data.is_null() || length == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        };
        buffer.Unlock().map_err(&error)?;
        Ok(bytes)
    }
}

/// Some encoders hand the parameter sets out in a sample of their own before
/// the frame, so an access unit is only whole once it holds a slice.
fn has_slice(codec: Codec, data: &[u8]) -> bool {
    crate::annexb::nal_units(data).any(|nal| nal.is_slice_in(codec.into()))
}

/// Whether an access unit holds an IDR slice, read from the bitstream
/// itself rather than from what the encoder says about the sample.
fn has_idr(codec: Codec, data: &[u8]) -> bool {
    crate::annexb::nal_units(data).any(|nal| nal.is_idr_in(codec.into()))
}

/// Whether an access unit is a picture a decoder could start from without
/// being an IDR: an HEVC CRA or BLA. An HEVC encoder may answer a request
/// for a keyframe with one, and a viewer starts and recovers only at an IDR.
fn keyframe_without_idr(codec: Codec, data: &[u8]) -> bool {
    codec == Codec::Hevc
        && !has_idr(codec, data)
        && crate::annexb::nal_units(data).any(|nal| crate::annexb::hevc::is_irap(&nal))
}

/// Asks for the next frame to be an IDR. An encoder that cannot is refused
/// only once it matters: its first frame is an IDR anyway.
fn request_idr(api: &CodecApi, first_frame: bool) -> Result<(), EncodeError> {
    match api.set(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) {
        Err(_) if first_frame => Ok(()),
        result => result.map_err(mf_error("make an IDR on request")),
    }
}

fn sample_time(index: u64, fps: u32) -> i64 {
    // In 100 ns units.
    (u128::from(index) * 10_000_000 / u128::from(fps)) as i64
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use windows::Win32::Foundation::E_UNEXPECTED;

    use super::*;

    #[test]
    fn software_fit_keeps_1080p60() {
        assert_eq!(software_fit(1920, 1080, 60), None);
        assert_eq!(software_fit(1280, 720, 30), None);
        // Same macroblocks as 1080p, another shape.
        assert_eq!(software_fit(1080, 1920, 60), None);
    }

    #[test]
    fn software_fit_matches_capture() {
        let fit = |w, h, fps| software_fit(w, h, fps).map(|f| (f.width, f.height, f.fps));
        assert_eq!(fit(2560, 1440, 120), Some((1920, 1080, 60)));
        assert_eq!(fit(1920, 1080, 120), Some((1920, 1080, 60)));
        // Other shapes get as many macroblocks as 1080p, not a picture
        // squeezed into a 1920x1080 box.
        assert_eq!(fit(3440, 1440, 60), Some((2216, 928, 60)));
        assert_eq!(fit(2304, 1440, 60), Some((1818, 1136, 60)));
        assert_eq!(fit(5120, 1440, 60), Some((2716, 764, 60)));
        // Capture's own rounding of 5120x2160 to 1440 high, then again to
        // the fit's height, lands on the same width.
        assert_eq!(fit(3414, 1440, 60), Some((2204, 930, 60)));
    }

    #[test]
    fn software_fit_portrait() {
        let fit = |w, h, fps| software_fit(w, h, fps).map(|f| (f.width, f.height, f.fps));
        // Capture makes 1080x1920 from a 1440x2560 monitor turned on its
        // side with max_height 1920, and that is as many macroblocks as
        // 1080p.
        assert_eq!(fit(1440, 2560, 60), Some((1080, 1920, 60)));
        assert_eq!(fit(2160, 3840, 120), Some((1080, 1920, 60)));
        assert_eq!(fit(1440, 5120, 60), Some((766, 2720, 60)));
    }

    #[test]
    fn cra_is_keyframe_without_idr() {
        // HEVC's two header bytes, the type in bits 1 to 6 of the first.
        let unit = |kind: u8| [0, 0, 0, 1, kind << 1, 1, 0xaf];
        let cra = unit(crate::annexb::hevc::CRA);
        let idr = unit(crate::annexb::hevc::IDR_W_RADL);
        let trail = unit(crate::annexb::hevc::TRAIL_R);
        let vps_and_cra = [&unit(crate::annexb::hevc::VPS)[..], &cra].concat();
        assert!(keyframe_without_idr(Codec::Hevc, &cra));
        assert!(keyframe_without_idr(Codec::Hevc, &vps_and_cra));
        assert!(!keyframe_without_idr(Codec::Hevc, &idr));
        assert!(!keyframe_without_idr(Codec::Hevc, &trail));
        // H.264 has no such pictures, whatever the bytes.
        assert!(!keyframe_without_idr(Codec::H264, &cra));
    }

    // An encoder whose first ProcessOutput says its output format changed.
    // Asked again, an asynchronous one has had no METransformHaveOutput
    // since and answers as NVIDIA's do; a synchronous one has nothing yet.
    struct Changing {
        asynchronous: bool,
        calls: Cell<u32>,
        formats_taken: Cell<u32>,
    }

    impl Outputs for Changing {
        fn stream_info(&self) -> windows::core::Result<MFT_OUTPUT_STREAM_INFO> {
            Ok(MFT_OUTPUT_STREAM_INFO {
                dwFlags: MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32,
                cbSize: 0,
                cbAlignment: 0,
            })
        }

        fn process_output(
            &self,
            _: &mut [MFT_OUTPUT_DATA_BUFFER; 1],
            _: &mut u32,
        ) -> windows::core::Result<()> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            Err(match call {
                0 => MF_E_TRANSFORM_STREAM_CHANGE,
                _ if self.asynchronous => E_UNEXPECTED,
                _ => MF_E_TRANSFORM_NEED_MORE_INPUT,
            }
            .into())
        }

        fn take_new_format(&self) -> Result<(), EncodeError> {
            self.formats_taken.set(self.formats_taken.get() + 1);
            Ok(())
        }
    }

    fn changing(asynchronous: bool) -> (Changing, Output, platform::Mf) {
        let encoder = Changing {
            asynchronous,
            calls: Cell::new(0),
            formats_taken: Cell::new(0),
        };
        let output = Output::of(&encoder, asynchronous).unwrap_or_else(|e| panic!("{e}"));
        let mf = platform::Mf::start().unwrap_or_else(|e| panic!("{e}"));
        (encoder, output, mf)
    }

    // What Intel's H.264 encoder may well have met on its first frame, after
    // the HEVC one stopped: asked again at once, an asynchronous encoder
    // says "Catastrophic failure" and the share ended on it.
    #[test]
    fn new_format_async() {
        let (encoder, mut output, mf) = changing(true);
        let taken = output.take(&encoder, &mf.fns);
        assert!(matches!(taken, Ok(None)), "{:?}", taken.err());
        assert_eq!(encoder.calls.get(), 1, "one ProcessOutput per event");
        assert_eq!(encoder.formats_taken.get(), 1);
        assert!(output.format_changed());
        assert!(!output.format_changed(), "said once");
    }

    #[test]
    fn new_format_sync() {
        let (encoder, mut output, mf) = changing(false);
        let taken = output.take(&encoder, &mf.fns);
        assert!(matches!(taken, Ok(None)), "{:?}", taken.err());
        assert_eq!(encoder.calls.get(), 2);
        assert_eq!(encoder.formats_taken.get(), 1);
        assert!(output.format_changed());
    }

    #[test]
    fn sample_times() {
        assert_eq!(sample_time(0, 120), 0);
        assert_eq!(sample_time(120, 120), 10_000_000);
        assert_eq!(sample_time(1, 60), 166_666);
    }
}
