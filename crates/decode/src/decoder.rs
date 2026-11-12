use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::ptr;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_SINGLETHREADED,
    D3D11_FORMAT_SUPPORT_SHADER_SAMPLE, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::{DXGI_ADAPTER_DESC, IDXGIDevice};
use windows::core::Interface;

use crate::error::DecodeError;
use crate::ffi::{
    AVBufferRef, AVCodecContext, AVFrame, AVPacket, Choice, booth_chose_d3d11,
    booth_codec_clear_reorder, booth_codec_forget_format, booth_codec_h264, booth_codec_hevc,
    booth_codec_reorder, booth_codec_setup, booth_codec_stream, booth_device_fill,
    booth_error_again, booth_error_bare, booth_frame_read, booth_hwdevice_d3d11va,
    booth_packet_set, booth_padding, booth_pix_fmt_d3d11, booth_refused_format,
    booth_refused_second_setup, booth_refused_setup, booth_refused_too_large,
};
use crate::guard;
use crate::library::{Library, library};
use crate::timing::{GpuTime, Timing};

/// The largest access unit [`Decoder::decode`] takes. The video packets
/// carry at most 2048 data shards of at most 1344 bytes (crates/channels),
/// 2.75 MB, so anything larger did not come through them.
pub const MAX_ACCESS_UNIT: usize = 3 << 20;

// FFmpeg keeps a unit's SEI for the picture it expects next, and with no
// picture coming it adds the next unit's to it: HEVC after any IDR, H.264
// after a lone field. Only a flush lets go of it. So much of the smallest
// messages it keeps cost about 5 MB and, as it copies its list of them for
// each one, about 5 ms for the last unit. Encoders write a few hundred bytes
// with an IDR and a picture in every unit, so a stream comes this far only
// when it has shown nothing for thousands of frames, and the flush takes
// nothing from it that it still had.
const MOST_SEI_KEPT: usize = 256 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    /// Main profile only: 8-bit 4:2:0.
    Hevc,
}

impl Codec {
    pub(crate) fn id(self) -> c_int {
        match self {
            Codec::H264 => booth_codec_h264,
            Codec::Hevc => booth_codec_hevc,
        }
    }

    // FFmpeg reports the SPS's level_idc as it is: ten times the level for
    // H.264, thirty times for HEVC.
    pub(crate) fn level(self, level_idc: i32) -> Option<String> {
        if level_idc <= 0 {
            return None;
        }
        Some(match self {
            Codec::H264 => format!("{}.{}", level_idc / 10, level_idc % 10),
            Codec::Hevc => format!("{}.{}", level_idc / 30, level_idc % 30 / 3),
        })
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Codec::H264 => f.write_str("H.264"),
            Codec::Hevc => f.write_str("HEVC"),
        }
    }
}

/// One decoded picture, NV12, on the device the decoder was made with.
///
/// The picture stays as it is until the next [`Decoder::decode`] that
/// returns a frame, or until the decoder is dropped: FFmpeg reuses the slice
/// for a later frame after that. The texture reference itself stays valid
/// for as long as it is held.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// An array texture from FFmpeg's pool. Its bind flags include shader
    /// resource whenever the GPU can sample NV12, which every GPU Booth runs
    /// on can, so the viewer draws from it without a copy.
    pub texture: ID3D11Texture2D,
    /// The picture's slice in the array.
    pub index: u32,
    pub width: u32,
    pub height: u32,
    /// Which access unit this decoder made the picture from, counting from
    /// 1; [`GpuTime`] names pictures by it.
    pub unit: u64,
    /// From handing FFmpeg the access unit to having the frame back, wall
    /// clock: the CPU's part only, so not the stats panel's decode ms.
    /// FFmpeg returns once the GPU has the work, and the GPU finishes the
    /// picture later (about 2 ms after the call at 1440p in the round-trip
    /// test); a draw from the texture waits for that on the GPU, not on the
    /// CPU. The GPU's time comes later from [`Decoder::gpu_times`].
    pub submit_time: Duration,
}

/// FFmpeg's decoder for one stream, with d3d11va on the caller's device.
/// A picture FFmpeg makes of an access unit comes back from the same call or
/// not at all: frame threads and reordering are off, and a stream that asks
/// for reordering is refused.
pub struct Decoder {
    library: &'static Library,
    codec: Codec,
    gpu: String,
    // Asked whether it was lost when FFmpeg fails, since FFmpeg does not say.
    device: ID3D11Device,
    context: *mut AVCodecContext,
    packet: *mut AVPacket,
    received: *mut AVFrame,
    shown: *mut AVFrame,
    // The format callback in fields.c sets the bit booth_chose_d3d11 in it,
    // and why it turned a stream down. Cleared before every access unit. Boxed
    // so its address stays put for as long as FFmpeg has it.
    choice: Box<Cell<Choice>>,
    // The access unit copied with FFmpeg's zero padding after it, reused.
    buffer: Vec<u8>,
    sent: i64,
    timing: Option<Timing>,
    // Whether an IDR has given a picture since the decoder was made or
    // flushed. A packed unit or a refused IDR clears it too, with no flush.
    // FFmpeg's HEVC decoder with output-corrupt (fields.c) makes a picture out
    // of whatever its pool holds for a P frame with nothing to predict from,
    // where the H.264 decoder gives none, so until then HEVC access units other
    // than IDRs do not reach FFmpeg.
    had_idr: bool,
    // Bytes of SEI handed to FFmpeg since it last showed a picture or was
    // flushed, kept to MOST_SEI_KEPT.
    sei_kept: usize,
    // Times FFmpeg set its video decoder and surface pool up afresh on
    // d3d11va: the first IDR, then each new size or profile. A setup it could
    // not finish does not count (fields.c).
    setups: Cell<u64>,
}

// SAFETY: with one thread FFmpeg's decoder has no thread of its own and no
// tie to the thread that made it. new() refuses a single-threaded device and
// checks that multithread protection is on, so the device may be called from
// the thread the decoder moves to, and its immediate context may be used
// from there while another thread uses it too. The GPU timing's queries and
// texture are the device's own objects, used only through that context.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Loads FFmpeg on first use; a failed load is remembered, so Booth
    /// needs a restart after the files are put back. `device` should be made
    /// with D3D11_CREATE_DEVICE_VIDEO_SUPPORT and must not be made with
    /// D3D11_CREATE_DEVICE_SINGLETHREADED. This turns on ID3D11Multithread
    /// protection for it, as FFmpeg does for devices it makes itself,
    /// because the decoder uses the device's immediate context from inside
    /// FFmpeg while the viewer presents through the same context.
    pub fn new(device: &ID3D11Device, codec: Codec) -> Result<Decoder, DecodeError> {
        let library = library()?;
        let gpu = adapter_name(device);
        // Multithread protection covers only the immediate context. On a
        // single-threaded device the device's own calls, such as FFmpeg
        // making its surfaces while the viewer makes a view, are not safe
        // from two threads either.
        // SAFETY: a getter on a live device.
        let flags = unsafe { device.GetCreationFlags() };
        if flags & D3D11_CREATE_DEVICE_SINGLETHREADED.0 != 0 {
            return Err(DecodeError::NotShareable {
                gpu,
                why: "it was made single-threaded",
            });
        }
        // SAFETY: a getter on a live device; it returns an owned reference.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|source| DecodeError::Direct3D {
                action: "get the Direct3D device's context for the video decoder",
                source,
            })?;
        let multithread: ID3D11Multithread =
            context.cast().map_err(|source| DecodeError::Direct3D {
                action: "share the Direct3D device with the video decoder",
                source,
            })?;
        // SAFETY: a setter and a getter on a live interface; the previous
        // setting the setter returns does not matter.
        let protected = unsafe {
            let _ = multithread.SetMultithreadProtected(true);
            multithread.GetMultithreadProtected()
        };
        if !protected.as_bool() {
            return Err(DecodeError::NotShareable {
                gpu,
                why: "Direct3D did not turn on multithread protection for it",
            });
        }

        let mut decoder = Decoder {
            library,
            codec,
            gpu,
            device: device.clone(),
            context: ptr::null_mut(),
            packet: ptr::null_mut(),
            received: ptr::null_mut(),
            shown: ptr::null_mut(),
            choice: Box::new(Cell::default()),
            buffer: Vec::new(),
            sent: 0,
            timing: Timing::new(device, &context, &multithread),
            had_idr: false,
            sei_kept: 0,
            setups: Cell::new(0),
        };

        // SAFETY: returns a new reference, or null when out of memory.
        let hw = HwDevice {
            library,
            buffer: unsafe { (library.av_hwdevice_ctx_alloc)(booth_hwdevice_d3d11va) },
        };
        if hw.buffer.is_null() {
            return Err(decoder.setup_error("start hardware video decoding", "out of memory"));
        }
        // SAFETY: a D3D11VA device context fresh from alloc. It takes the two
        // references handed over here and releases them when it goes, even
        // if init fails.
        unsafe {
            booth_device_fill(
                hw.buffer,
                device.clone().into_raw(),
                context.into_raw(),
                sampling_bind_flags(device),
            );
        }
        // SAFETY: filled in above.
        let status = unsafe { (library.av_hwdevice_ctx_init)(hw.buffer) };
        if status < 0 {
            let detail = decoder.text(status);
            return Err(decoder.setup_error("start Direct3D 11 video decoding", &detail));
        }

        // SAFETY: a lookup by a codec id from FFmpeg's own header.
        let found = unsafe { (library.avcodec_find_decoder)(codec.id()) };
        if found.is_null() {
            return Err(DecodeError::NoDecoder { codec });
        }
        // SAFETY: `found` is a decoder FFmpeg returned.
        decoder.context = unsafe { (library.avcodec_alloc_context3)(found) };
        if decoder.context.is_null() {
            return Err(decoder.setup_error("make the video decoder", "out of memory"));
        }
        // SAFETY: an unopened context. It takes over the device reference,
        // and `choice` is boxed and outlives the context, which drop frees
        // first.
        unsafe { booth_codec_setup(decoder.context, hw.into_raw(), decoder.choice.as_ptr()) };
        // SAFETY: the context was made for this decoder; no options.
        let status = unsafe { (library.avcodec_open2)(decoder.context, found, ptr::null_mut()) };
        if status < 0 {
            let detail = decoder.text(status);
            return Err(decoder.setup_error("open the video decoder", &detail));
        }

        // SAFETY: plain allocations, null when out of memory.
        unsafe {
            decoder.packet = (library.av_packet_alloc)();
            decoder.received = (library.av_frame_alloc)();
            decoder.shown = (library.av_frame_alloc)();
        }
        if decoder.packet.is_null() || decoder.received.is_null() || decoder.shown.is_null() {
            return Err(decoder.setup_error("make the video decoder", "out of memory"));
        }
        Ok(decoder)
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// How many times FFmpeg has set its decoder and surfaces up for this
    /// stream: once for the first IDR, and again for each new size or
    /// profile, which costs what a new decoder does.
    pub fn setups(&self) -> u64 {
        self.setups.get()
    }

    /// Decodes one access unit (Annex B, the whole frame) and returns its
    /// picture at once. An error is about this access unit only and the
    /// decoder goes on with the next, though in HEVC the frames after a refused
    /// unit wait for the next IDR. None means no picture came out and FFmpeg
    /// gave no reason, as when every slice refers to a parameter set that was
    /// lost, or HEVC is waiting for an IDR (FFmpeg's H.264 decoder shows nothing
    /// there either). It also answers a picture thrown away: a P frame FFmpeg
    /// returned from a unit that read as an IDR. Booth's encoders put a picture
    /// in every access unit, so for them None is a lost frame just like an
    /// error.
    ///
    /// A unit packed past what an encoder writes never reaches FFmpeg
    /// ([`DecodeError::Packed`], [`DecodeError::ManyPictures`]). Every SPS in
    /// an HEVC access unit is read in Rust first, and a unit with one past
    /// the size Booth shows ([`DecodeError::Oversized`]) or one that does not
    /// read ([`DecodeError::UnreadableSps`]) never reaches FFmpeg either. One
    /// with more than 16 is refused before any is read
    /// ([`DecodeError::TooManySps`]).
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Decoded>, DecodeError> {
        if access_unit.is_empty() {
            return Err(DecodeError::Empty);
        }
        if access_unit.len() > MAX_ACCESS_UNIT {
            return Err(DecodeError::TooLarge {
                size: access_unit.len(),
            });
        }
        // First, as what comes after it walks from start code to start code
        // and this stops at the first too many. In HEVC the frames after a
        // refused unit may predict from it, so they wait for the next IDR.
        guard::check_unit(self.codec, access_unit).inspect_err(|_| {
            self.had_idr = false;
        })?;
        let idr = self.codec == Codec::Hevc && hevc_idr(access_unit);
        if self.codec == Codec::Hevc && !idr && !self.had_idr {
            return Ok(None);
        }
        if self.codec == Codec::Hevc
            && let Err(refused) = guard::check_hevc(access_unit)
        {
            // As when FFmpeg refuses an IDR: the frames after it predict
            // from it, so they wait for the next one.
            if idr {
                self.had_idr = false;
            }
            return Err(refused);
        }
        // FFmpeg's bitstream readers may read up to the padding past the end
        // of the data.
        let size = access_unit.len();
        self.buffer.clear();
        self.buffer.reserve(size + booth_padding as usize);
        self.buffer.extend_from_slice(access_unit);
        self.buffer.resize(size + booth_padding as usize, 0);
        // FFmpeg's HEVC decoder keeps the stand-ins it makes for lost
        // references (output-corrupt, fields.c) past an IDR, and one with
        // the POC the IDR brings made it drop the IDR as a duplicate, so a
        // damaged frame could spoil the recovery from it. Nothing before an
        // IDR is needed after it.
        if idr {
            // SAFETY: an open context.
            unsafe { (self.library.avcodec_flush_buffers)(self.context) };
            self.sei_kept = 0;
        }
        self.sent += 1;
        self.choice.set(Choice::default());
        let unit = self.sent as u64;
        if let Some(timing) = &mut self.timing {
            timing.begin(unit);
        }
        let result = self.send_and_receive(size, unit, idr);
        if idr {
            self.had_idr = matches!(result, Ok(Some(_)));
        }
        self.sei_kept = match result {
            Ok(Some(_)) | Err(DecodeError::HeldBack { .. }) => 0,
            _ => self.sei_kept + guard::sei_bytes(self.codec, access_unit),
        };
        if self.sei_kept > MOST_SEI_KEPT {
            // SAFETY: an open context.
            unsafe { (self.library.avcodec_flush_buffers)(self.context) };
            self.sei_kept = 0;
            self.had_idr = false;
        }
        if let Some(timing) = &mut self.timing {
            match &result {
                Ok(Some(decoded)) => timing.finish(&decoded.texture, decoded.index),
                _ => timing.abandon(),
            }
        }
        result
    }

    /// GPU times of pictures decoded before, oldest first, each named by
    /// [`Decoded::unit`]. They are read without waiting for the GPU, so one
    /// comes a call or two after its picture; ask again after the next
    /// decode or after a while. A measurement not in after eight more
    /// decodes is dropped, and so is one during which the GPU's clock
    /// changed speed. None at all come when Direct3D would not make the
    /// timers.
    pub fn gpu_times(&mut self, into: &mut Vec<GpuTime>) {
        if let Some(timing) = &mut self.timing {
            timing.take(into);
        }
    }

    pub fn gauge(&self) {
        stats::gauge!("decode.decoder.buffer", self.buffer);
        if let Some(timing) = &self.timing {
            timing.gauge();
        }
    }

    // The access unit is in `buffer`, `size` bytes and the padding. `idr`: its
    // first picture reads as an IDR.
    fn send_and_receive(
        &mut self,
        size: usize,
        unit: u64,
        idr: bool,
    ) -> Result<Option<Decoded>, DecodeError> {
        let library = self.library;
        let started = Instant::now();
        // SAFETY: the packet points at `buffer`, size bytes and the padding.
        // It has no buffer reference of its own, so FFmpeg copies the data
        // rather than keep the pointer (avcodec.h, avcodec_send_packet), and
        // the pointer is taken out again before `buffer` can change.
        let status = unsafe {
            booth_packet_set(
                self.packet,
                self.buffer.as_mut_ptr(),
                size as c_int,
                self.sent,
            );
            let status = (library.avcodec_send_packet)(self.context, self.packet);
            booth_packet_set(self.packet, ptr::null_mut(), 0, 0);
            status
        };
        if status < 0 {
            // FFmpeg's HEVC decoder queues a picture for output as its
            // decode starts, so a frame that fails after that leaves its
            // picture behind for the next access unit to hand back first
            // (FFmpeg 8.1.3, tests/failed.rs). Nothing of a failed access
            // unit is shown.
            let failed = self.stream_error(status);
            self.drain();
            return Err(failed);
        }

        // One access unit is one picture, but anything FFmpeg has is taken,
        // so a frame it kept from before cannot come out with the next one.
        let mut newest = None;
        let mut stale = 0u32;
        let mut failed = None;
        loop {
            // SAFETY: an open context and a frame of ours; receive unrefs
            // the frame before it fills it.
            let status = unsafe { (library.avcodec_receive_frame)(self.context, self.received) };
            if status == booth_error_again {
                break;
            }
            if status < 0 {
                failed = Some(status);
                break;
            }
            // SAFETY: a frame FFmpeg just filled.
            let frame = unsafe { FrameFacts::read(self.received) };
            if frame.format != booth_pix_fmt_d3d11 {
                // SAFETY: a frame of ours.
                unsafe { (library.av_frame_unref)(self.received) };
                return Err(DecodeError::NotOnGpu);
            }
            // The search for the first picture reads a header byte and no
            // more, and FFmpeg skips a slice it cannot use and decodes the
            // next, so a P frame behind a bad IDR comes back as if it were
            // one, predicted from whatever its pool holds. It takes nothing's
            // place: the picture shown before it stays as it is.
            if frame.pts == self.sent && idr && !frame.key {
                // SAFETY: a frame of ours.
                unsafe { (library.av_frame_unref)(self.received) };
            } else if frame.pts == self.sent {
                // SAFETY: two frames of ours; `shown` is empty after unref,
                // which move_ref needs.
                unsafe {
                    (library.av_frame_unref)(self.shown);
                    (library.av_frame_move_ref)(self.shown, self.received);
                }
                newest = Some(frame);
            } else {
                // SAFETY: a frame of ours.
                unsafe { (library.av_frame_unref)(self.received) };
                stale += 1;
            }
        }
        let submit_time = started.elapsed();

        if let Some(refused) = self.refusal() {
            return Err(refused);
        }
        // SAFETY: an open context.
        let reorder = unsafe { booth_codec_reorder(self.context) };
        if reorder > 0 || (newest.is_none() && stale > 0) {
            // Only a flush empties what FFmpeg holds; it also drops the
            // references, so the stream picks up again at its next IDR.
            // SAFETY: an open context.
            unsafe {
                (library.avcodec_flush_buffers)(self.context);
                booth_codec_clear_reorder(self.context);
            }
            self.had_idr = false;
            return Err(DecodeError::HeldBack {
                frames: (reorder.max(0) as u32).max(stale),
            });
        }
        match (newest, failed) {
            (Some(frame), _) => frame.decoded(unit, submit_time).map(Some),
            (None, Some(status)) => {
                let failed = self.stream_error(status);
                self.drain();
                Err(failed)
            }
            (None, None) => Ok(None),
        }
    }

    // Lets go of every picture FFmpeg still holds for output.
    fn drain(&mut self) {
        loop {
            // SAFETY: an open context and a frame of ours.
            let status =
                unsafe { (self.library.avcodec_receive_frame)(self.context, self.received) };
            if status < 0 {
                break;
            }
            // SAFETY: a frame of ours.
            unsafe { (self.library.av_frame_unref)(self.received) };
        }
    }

    fn text(&self, status: c_int) -> String {
        if status == booth_error_bare {
            return "it failed without a reason (error -1)".to_string();
        }
        let mut text = [0 as c_char; 128];
        // SAFETY: the buffer and its length; av_strerror always writes a
        // NUL-terminated string into it.
        unsafe { (self.library.av_strerror)(status, text.as_mut_ptr(), text.len()) };
        // SAFETY: NUL-terminated, see above.
        unsafe { CStr::from_ptr(text.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    fn setup_error(&self, action: &'static str, detail: &str) -> DecodeError {
        self.device_lost().unwrap_or_else(|| DecodeError::Setup {
            action,
            gpu: self.gpu.clone(),
            detail: detail.to_string(),
        })
    }

    // Once the driver resets, every access unit fails, and FFmpeg only says
    // that it did not decode.
    fn stream_error(&self, status: c_int) -> DecodeError {
        self.refusal()
            .or_else(|| self.device_lost())
            .unwrap_or_else(|| DecodeError::Damaged {
                detail: self.text(status),
            })
    }

    fn device_lost(&self) -> Option<DecodeError> {
        // SAFETY: a getter on a live device.
        let removed = unsafe { self.device.GetDeviceRemovedReason() };
        removed.err().map(|source| DecodeError::DeviceLost {
            gpu: self.gpu.clone(),
            source,
        })
    }

    // Why the format callback turned the stream down during this call, if
    // it did. FFmpeg may carry on past a refused slice without an error of
    // its own, so this is asked after every access unit.
    fn refusal(&self) -> Option<DecodeError> {
        let choice = self.choice.take();
        if choice.bits & booth_chose_d3d11 != 0 {
            self.setups.set(self.setups.get() + 1);
        }
        let why = choice.bits & !booth_chose_d3d11;
        if why == 0 {
            return None;
        }
        let (mut width, mut height, mut profile, mut level) = (0, 0, 0, 0);
        // SAFETY: an open context and four live out parameters. FFmpeg is
        // not inside a call on the context, so the format may change.
        unsafe {
            booth_codec_stream(
                self.context,
                &mut width,
                &mut height,
                &mut profile,
                &mut level,
            );
            booth_codec_forget_format(self.context);
        };
        let (width, height) = (width.max(0) as u32, height.max(0) as u32);
        if why == booth_refused_too_large {
            return Some(DecodeError::Oversized {
                width: choice.width.max(0) as u32,
                height: choice.height.max(0) as u32,
            });
        }
        if why == booth_refused_second_setup {
            return Some(DecodeError::ManyPictures);
        }
        // SAFETY: a lookup in FFmpeg's static tables; null for a profile it
        // does not know.
        let name = unsafe { (self.library.avcodec_profile_name)(self.codec.id(), profile) };
        let profile = if name.is_null() {
            format!("profile {profile}")
        } else {
            // SAFETY: a NUL-terminated string in FFmpeg's static tables.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        };
        if why == booth_refused_format {
            return Some(DecodeError::WrongFormat {
                codec: self.codec,
                profile,
            });
        }
        debug_assert_eq!(why, booth_refused_setup);
        if let Some(lost) = self.device_lost() {
            return Some(lost);
        }
        Some(DecodeError::Unsupported {
            gpu: self.gpu.clone(),
            codec: self.codec,
            width,
            height,
            profile,
            level,
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let library = self.library;
        // SAFETY: each is null or ours, and FFmpeg's free functions take a
        // null. The frames go first: they hold slices of the pool the
        // context owns.
        unsafe {
            (library.av_frame_free)(&mut self.shown);
            (library.av_frame_free)(&mut self.received);
            (library.av_packet_free)(&mut self.packet);
            (library.avcodec_free_context)(&mut self.context);
        }
    }
}

struct FrameFacts {
    format: c_int,
    texture: *mut c_void,
    index: isize,
    width: c_int,
    height: c_int,
    pts: i64,
    key: bool,
}

impl FrameFacts {
    // SAFETY: `frame` must be a frame FFmpeg filled.
    unsafe fn read(frame: *const AVFrame) -> FrameFacts {
        let mut facts = FrameFacts {
            format: 0,
            texture: ptr::null_mut(),
            index: 0,
            width: 0,
            height: 0,
            pts: 0,
            key: false,
        };
        let mut key = 0;
        // SAFETY: the caller's frame and live out parameters.
        facts.format = unsafe {
            booth_frame_read(
                frame,
                &mut facts.texture,
                &mut facts.index,
                &mut facts.width,
                &mut facts.height,
                &mut facts.pts,
                &mut key,
            )
        };
        facts.key = key != 0;
        facts
    }

    fn decoded(self, unit: u64, submit_time: Duration) -> Result<Decoded, DecodeError> {
        // SAFETY: a D3D11 frame's texture, borrowed from the frame `shown`
        // holds; the clone takes a reference of its own.
        let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&self.texture) }
            .cloned()
            .ok_or(DecodeError::NotOnGpu)?;
        let index = u32::try_from(self.index).map_err(|_| DecodeError::NotOnGpu)?;
        Ok(Decoded {
            texture,
            index,
            width: self.width.max(0) as u32,
            height: self.height.max(0) as u32,
            unit,
            submit_time,
        })
    }
}

// The device context buffer until the codec context takes it.
struct HwDevice {
    library: &'static Library,
    buffer: *mut AVBufferRef,
}

impl HwDevice {
    fn into_raw(self) -> *mut AVBufferRef {
        let buffer = self.buffer;
        std::mem::forget(self);
        buffer
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: null or a reference of ours.
        unsafe { (self.library.av_buffer_unref)(&mut self.buffer) };
    }
}

// Shader resource binding on the pool lets the viewer draw straight from
// the decoded texture, with no copy on the way.
fn sampling_bind_flags(device: &ID3D11Device) -> u32 {
    // SAFETY: a query on a live device.
    let support = unsafe { device.CheckFormatSupport(DXGI_FORMAT_NV12) }.unwrap_or(0);
    if support & D3D11_FORMAT_SUPPORT_SHADER_SAMPLE.0 as u32 != 0 {
        D3D11_BIND_SHADER_RESOURCE.0 as u32
    } else {
        0
    }
}

fn adapter_desc(device: &ID3D11Device) -> Option<DXGI_ADAPTER_DESC> {
    let desc = device.cast::<IDXGIDevice>().and_then(|dxgi| {
        // SAFETY: getters on live interfaces.
        unsafe { dxgi.GetAdapter().and_then(|adapter| adapter.GetDesc()) }
    });
    desc.ok()
}

// The PCI vendor of the GPU the device is on, 0 when DXGI does not say.
pub(crate) fn adapter_vendor(device: &ID3D11Device) -> u32 {
    adapter_desc(device).map_or(0, |desc| desc.VendorId)
}

pub(crate) fn adapter_name(device: &ID3D11Device) -> String {
    let Some(desc) = adapter_desc(device) else {
        return "graphics card".to_string();
    };
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
        .trim()
        .to_string()
}

pub(crate) fn hevc_idr(access_unit: &[u8]) -> bool {
    matches!(
        annexb::hevc::first_picture(access_unit),
        Some(annexb::hevc::IDR_W_RADL | annexb::hevc::IDR_N_LP)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
    use windows::Win32::Graphics::Direct3D11::{D3D11_SDK_VERSION, D3D11CreateDevice};
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    #[test]
    fn single_threaded_device_refused() {
        // WARP draws on the CPU, so no GPU is touched.
        let mut device = None;
        // SAFETY: a live out parameter; no adapter, context or level wanted.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_SINGLETHREADED,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .expect("a WARP device");
        let device = device.expect("a WARP device");
        let err = Decoder::new(&device, Codec::H264)
            .err()
            .expect("a single-threaded device must be refused");
        let text = err.to_string();
        println!("{text}");
        assert!(matches!(err, DecodeError::NotShareable { .. }), "{err:?}");
        assert!(
            text.ends_with("because it was made single-threaded"),
            "{text}"
        );
    }

    // A NAL unit of `kind` with a four-byte start code and a little payload.
    fn nal(kind: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, kind << 1, 1, 0xaf, 0x09]
    }

    #[test]
    fn hevc_idr_behind_parameter_sets() {
        let idr: Vec<u8> = [32, 33, 34, 39, 19].into_iter().flat_map(nal).collect();
        assert!(hevc_idr(&idr));
        assert!(hevc_idr(&nal(20)));
        // A picture that is not an IDR ends the search, even with an IDR
        // after it.
        let trail: Vec<u8> = [33, 1, 19].into_iter().flat_map(nal).collect();
        assert!(!hevc_idr(&trail));
        assert!(!hevc_idr(&nal(21)), "a CRA is not an IDR");
        assert!(!hevc_idr(&[33, 1, 19]), "no start code");
        assert!(!hevc_idr(&[0, 0, 1]), "a start code at the very end");
    }

    // Start codes mixed into the noise, so the headers after them are read.
    fn noise() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            Just(vec![0u8, 0, 1]),
            prop::collection::vec(any::<u8>(), 0..8)
        ];
        prop::collection::vec(piece, 0..64).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #[test]
        fn any_bytes_give_an_answer(bytes in noise()) {
            hevc_idr(&bytes);
        }
    }

    // Start code and header, then ones and zeros with the stop bit and
    // emulation prevention added.
    fn h264_nal(header: u8, bits: &str) -> Vec<u8> {
        let bits: Vec<u8> = bits
            .bytes()
            .chain(*b"1")
            .map(|bit| bit - b'0')
            .chain([0; 7])
            .collect();
        let mut out = vec![0, 0, 1, header];
        let mut zeros = 0;
        for byte in bits.as_chunks::<8>().0 {
            let byte = byte.iter().fold(0, |acc, bit| acc << 1 | bit);
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    fn ue(value: u32) -> String {
        let coded = value + 1;
        let len = 32 - coded.leading_zeros() as usize;
        format!("{:0>width$b}", coded, width = 2 * len - 1)
    }

    // An SPS for 640x368 that asks for two frames of reordering, its PPS, and an
    // IDR slice of made-up picture data, which only the GPU reads.
    fn asks_for_reordering() -> Vec<u8> {
        let sps = [
            format!("{:08b}{:08b}{:08b}", 100, 0, 51),
            ue(0),                  // sps_id
            ue(1),                  // chroma_format_idc: 4:2:0
            ue(0),                  // bit_depth_luma_minus8
            ue(0),                  // bit_depth_chroma_minus8
            "00".to_string(),       // qpprime_y_zero_transform_bypass, seq_scaling_matrix_present
            ue(0),                  // log2_max_frame_num_minus4
            ue(2),                  // pic_order_cnt_type
            ue(2),                  // max_num_ref_frames
            "0".to_string(),        // gaps_in_frame_num_allowed
            ue(39),                 // width in macroblocks less 1
            ue(22),                 // height in macroblocks less 1
            "1".to_string(),        // frame_mbs_only
            "1".to_string(),        // direct_8x8_inference
            "0".to_string(),        // frame_cropping
            "1".to_string(),        // vui_parameters_present
            "00000000".to_string(), // everything in the VUI up to the next flag
            "11".to_string(),       // bitstream_restriction, motion_vectors_over_pic_boundaries
            ue(0),                  // max_bytes_per_pic_denom
            ue(0),                  // max_bits_per_mb_denom
            ue(16),                 // log2_max_mv_length_horizontal
            ue(16),                 // log2_max_mv_length_vertical
            ue(2),                  // max_num_reorder_frames
            ue(2),                  // max_dec_frame_buffering
        ]
        .concat();
        let pps = [
            ue(0),             // pps_id
            ue(0),             // sps_id
            "00".to_string(),  // entropy_coding_mode, bottom_field_pic_order_in_frame_present
            ue(0),             // num_slice_groups_minus1
            ue(0),             // num_ref_idx_l0_default_active_minus1
            ue(0),             // num_ref_idx_l1_default_active_minus1
            "000".to_string(), // weighted_pred, weighted_bipred_idc
            ue(0),             // pic_init_qp_minus26
            ue(0),             // pic_init_qs_minus26
            ue(0),             // chroma_qp_index_offset
            "000".to_string(), // deblocking_filter_control, constrained_intra_pred, redundant_pic_cnt
        ]
        .concat();
        let slice = [
            ue(0),              // first_mb_in_slice
            ue(7),              // slice_type: all I
            ue(0),              // pps_id
            "0000".to_string(), // frame_num
            ue(0),              // idr_pic_id
            "00".to_string(),   // no_output_of_prior_pics, long_term_reference
            ue(0),              // slice_qp_delta
            "10101100".repeat(64),
        ]
        .concat();
        [
            h264_nal(0x67, &sps),
            h264_nal(0x68, &pps),
            h264_nal(0x65, &slice),
        ]
        .concat()
    }

    // A flush lets go of what FFmpeg kept, so what was counted goes with it, or
    // the next SEI would flush again for nothing. By then the references have
    // gone anyway, so nothing outside shows that flush and this reads the count.
    #[test]
    fn a_flush_for_reordering_starts_the_sei_count_again() {
        let adapters = capture::adapters().unwrap_or_else(|e| panic!("{e}"));
        let Some(adapter) = adapters.iter().find(|a| a.vendor_id == 0x10de) else {
            println!("skipped: no NVIDIA GPU on this PC");
            return;
        };
        let device = capture::device_on(adapter).unwrap_or_else(|e| panic!("{e}"));
        let mut decoder = match Decoder::new(&device, Codec::H264) {
            Ok(decoder) => decoder,
            Err(err @ (DecodeError::Missing { .. } | DecodeError::Unsupported { .. })) => {
                println!("skipped: {err}");
                return;
            }
            Err(err) => panic!("{err}"),
        };
        let sei = [&[0u8, 0, 1, 6, 5, 16][..], &[0x11; 16], &[0x80]].concat();
        let _ = decoder.decode(&sei);
        assert_eq!(decoder.sei_kept, sei.len() - 3);
        let answer = decoder.decode(&asks_for_reordering());
        assert!(
            matches!(answer, Err(DecodeError::HeldBack { .. })),
            "{answer:?}"
        );
        assert_eq!(decoder.sei_kept, 0);
    }

    // With no picture coming, only this count's flush lets go of what FFmpeg
    // kept of a peer's SEI, so it is held here to 256 KB: the leak test
    // (tests/leaks.rs) sees that the flush comes, not how late.
    #[test]
    fn the_sei_count_flushes_past_256_kb() {
        let adapters = capture::adapters().unwrap_or_else(|e| panic!("{e}"));
        let Some(adapter) = adapters.iter().find(|a| a.vendor_id == 0x10de) else {
            println!("skipped: no NVIDIA GPU on this PC");
            return;
        };
        let device = capture::device_on(adapter).unwrap_or_else(|e| panic!("{e}"));
        let mut decoder = match Decoder::new(&device, Codec::H264) {
            Ok(decoder) => decoder,
            Err(err @ (DecodeError::Missing { .. } | DecodeError::Unsupported { .. })) => {
                println!("skipped: {err}");
                return;
            }
            Err(err) => panic!("{err}"),
        };
        let sei = [
            &[0u8, 0, 1, 6, 5, 0xff, 0xff, 0xff, 0][..],
            &[0x11; 765],
            &[0x80],
        ]
        .concat();
        let mut kept = 0;
        for unit in 1.. {
            let _ = decoder.decode(&sei);
            kept += sei.len() - 3;
            if kept > 256 << 10 {
                assert_eq!(decoder.sei_kept, 0, "unit {unit}");
                break;
            }
            assert_eq!(decoder.sei_kept, kept, "unit {unit}");
        }
    }
}
