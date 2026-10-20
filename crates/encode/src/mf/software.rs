// Fallback 2: Windows' own software H.264 encoder, on every Windows 10 and
// 11. It is synchronous and reads system memory, so this is the one place a
// frame leaves the GPU: copied into a staging texture and mapped. The
// frame is the sharer's own screen going to the sharer's own encoder, as
// with the GPU encoders; only its bitstream leaves the PC.

use std::collections::VecDeque;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, CODECAPI_AVEncVideoLTRBufferControl, CODECAPI_AVEncVideoUseLTRFrame,
    IMFMediaBuffer, IMFSample, IMFTransform, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use super::codec::CodecApi;
use super::platform::{self, Functions, Mf};
use super::{
    Buffer, Output, change_bitrate, check_idr_on_request, configure, create, has_idr, has_slice,
    mf_error, note_own_buffer, request_idr, sample_time,
};
use crate::{
    AccessUnit, Codec, EncodeError, Encoder, Frame, Kind, Recovery, Request, Settings, gpu,
};

/// Whether this encoder takes the request as it is; an error
/// message when not.
pub(crate) fn offers(request: &Request) -> Result<(), String> {
    let Request {
        codec,
        width,
        height,
        fps,
    } = *request;
    if codec != Codec::H264 {
        return Err(EncodeError::NoSoftwareHevc.to_string());
    }
    match super::software_fit(width, height, fps) {
        None => Ok(()),
        Some(fit) => Err(EncodeError::SoftwareLimit {
            width,
            height,
            fps,
            fit,
        }
        .to_string()),
    }
}

pub(crate) struct Software {
    transform: IMFTransform,
    api: CodecApi,
    fns: Functions,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    name: String,
    notes: String,
    width: u32,
    height: u32,
    fps: u32,
    // In bits a second: the setting, or set_bitrate's since.
    bitrate: u32,
    // The last frame's system memory, written again once nothing else
    // holds it: a new 3 MB buffer every frame is 3 MB of fresh pages to
    // fault in.
    frame_buffer: Option<IMFMediaBuffer>,
    output: Output,
    drain_every_frame: bool,
    idr_next: bool,
    last_index: Option<u64>,
    // Frames still to give back before one is refused: crate::fault's, None
    // outside a test.
    #[cfg(feature = "fault")]
    fail_in: Option<u64>,
    // None when the encoder did not take a long-term reference, or once it
    // did not do what was asked of it.
    long_term: Option<LongTerm>,
    // What the last frame's header says (Encoder::invalidates), worked out
    // once as it came out.
    survives: bool,
    // Last, so Media Foundation stops only after the encoder is gone.
    _mf: Mf,
}

// With one long-term reference, which the encoder fills with each IDR itself,
// the frame after a loss predicts from the last IDR alone and the frames
// after it never go back past it, so the viewer goes on from that frame
// without waiting for an IDR, as NVENC's invalidation lets it. Measured at
// 1080p60, 8 Mbit/s, 10 percent loss and a 40 ms round trip: recovery frames
// of 23 KB on the still test pattern, whose IDRs are 498 KB, and 59.5 frames
// shown a second against 46.0.
//
// Only the IDR is ever predicted from. A reference marked on a later frame
// would have to be known to have arrived: a viewer that missed it would
// predict from the picture it held before, and nothing would show the
// difference. A viewer that missed the IDR holds every frame until the next.
//
// On a picture that changed everywhere since the IDR a recovery frame costs
// as much as an IDR, and a friend's PC that reports every frame lost would
// ask for one each frame: 73 Mbit/s at a setting of 8. So, as for an IDR
// (share's recovery.rs), the next one waits until the last has used at most
// a quarter of the setting, its bits spread over the time since. Until then
// a loss is answered with an IDR, and the frames' headers tell viewers to
// hold for it rather than show frames that predict from the lost one. A loss
// of one of those frames reported after the wait is answered with an IDR too,
// since its viewer holds for one.
//
// The encoder says in each frame's output what it marked and what it
// predicted from, and the first IDR or recovery frame that is not as asked
// turns this off for the rest of the share; after such a recovery frame the
// next frame is an IDR.
struct LongTerm {
    // The last IDR, once the encoder said it marked it.
    anchor: Option<u64>,
    // The next frame predicts from the anchor alone.
    recovering: bool,
    // The last recovery frame's size and when it came out.
    spent: Option<(usize, Instant)>,
    // The frames whose headers told viewers to hold for an IDR after a loss.
    holding: VecDeque<u64>,
}

// Enough for a wait of 16 s at 60 fps: a recovery frame of 500 KB at the
// rate control's floor of 1 Mbit/s.
const HOLDING_KEPT: usize = 1024;

impl LongTerm {
    fn waits(&self, bitrate: u32, now: Instant) -> bool {
        self.spent.is_some_and(|(bytes, at)| {
            let floor = bytes as f64 * 8.0 / (RECOVERY_SHARE * f64::from(bitrate.max(1)));
            now.saturating_duration_since(at).as_secs_f64() < floor
        })
    }
}

const RECOVERY_SHARE: f64 = 0.25;

// LTR buffer control: one long-term reference the encoder may use until it
// is told otherwise ("trust until", the only mode there is).
const ONE_LONG_TERM: u32 = 1 | (1 << 16);
// UseLTRFrame: long-term reference 0 only, and no older short-term
// reference for the frames after this one.
const FROM_THE_IDR: u32 = 1 | (1 << 16);
// What the output says: the reference a frame was marked as in the low 16
// bits, 0xFFFF for none; the ones it predicted from in the high 16.
const NOT_MARKED: u32 = 0xFFFF;

// SAFETY: as for Hardware: free-threaded Media Foundation objects, COM
// joined at every entry point, &mut self for every call. The device context
// is used only inside encode(), on the thread the caller encodes on, which
// open_codec() in lib.rs ties to the thread that renders the frame.
unsafe impl Send for Software {}

impl Software {
    pub(crate) fn open(
        device: &ID3D11Device,
        request: &Request,
        settings: &Settings,
        passed_over: String,
    ) -> Result<Software, EncodeError> {
        let Request {
            codec,
            width,
            height,
            fps,
        } = *request;
        // Windows has a software HEVC encoder only in an extension from the
        // Store, which Booth does not ask anyone to install.
        if codec != Codec::H264 {
            return Err(EncodeError::NoSoftwareHevc);
        }
        if let Some(fit) = super::software_fit(width, height, fps) {
            return Err(EncodeError::SoftwareLimit {
                width,
                height,
                fps,
                fit,
            });
        }
        let mf = Mf::start()?;
        let fns = mf.fns;
        #[cfg(feature = "fault")]
        if crate::fault::take_software_refusal() {
            return Err(mf_error("start Windows' software H.264 encoder")(
                windows::Win32::Foundation::E_FAIL.into(),
            ));
        }
        // SAFETY: plain COM creation of an in-process class.
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(mf_error("start Windows' software H.264 encoder"))?;

        let mut api = CodecApi::of(&transform);
        check_idr_on_request(
            &mut api,
            "Windows' software H.264 encoder cannot make an IDR on request, which Booth needs to recover from a lost frame: update Windows",
        )?;
        configure(
            &mut api,
            fps,
            settings.bitrate,
            settings.preset,
            Buffer::EncodersOwn,
        );
        let long_term = api
            .setting(
                "one long-term reference".to_string(),
                &CODECAPI_AVEncVideoLTRBufferControl,
                VARIANT::from(ONE_LONG_TERM),
            )
            .then_some(LongTerm {
                anchor: None,
                recovering: false,
                spent: None,
                holding: VecDeque::new(),
            });
        let h264 = super::output_type(&fns, codec, width, height, fps, settings.bitrate)?;
        let nv12 = super::video_type(&fns, &MFVideoFormat_NV12, width, height, fps)?;
        // SAFETY: live media types, output first as encoders want it.
        unsafe {
            transform
                .SetOutputType(0, &h264, 0)
                .map_err(mf_error("set the software encoder's H.264 output"))?;
            transform
                .SetInputType(0, &nv12, 0)
                .map_err(mf_error("set the software encoder's NV12 input"))?;
        }
        api.reapply();
        note_own_buffer(&mut api, settings.bitrate);
        let output = Output::of(&transform, false)?;
        // SAFETY: the types are set, so the encoder may start.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(mf_error("start the software encoder's stream"))?;
        }

        let staging = staging(device, width, height)?;
        // SAFETY: a getter on a live device.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|source| EncodeError::Direct3D {
                action: "reach the device context for the software encoder",
                source,
            })?;
        let notes = [passed_over, api.report()]
            .into_iter()
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join(". ");
        Ok(Software {
            transform,
            api,
            fns,
            device: device.clone(),
            context,
            staging,
            name: "Media Foundation H.264, software, 1080p60".to_string(),
            notes,
            width,
            height,
            fps,
            bitrate: settings.bitrate,
            frame_buffer: None,
            output,
            drain_every_frame: false,
            idr_next: true,
            last_index: None,
            #[cfg(feature = "fault")]
            fail_in: crate::fault::take_software_failure(),
            survives: long_term.is_some(),
            long_term,
            _mf: mf,
        })
    }

    /// The frame, copied from the GPU into a sample in system memory.
    fn sample(&mut self, texture: &ID3D11Texture2D, index: u64) -> Result<IMFSample, EncodeError> {
        let (w, h) = (self.width as usize, self.height as usize);
        let size = w * h * 3 / 2;
        let fns = self.fns;
        let buffer = match self.frame_buffer.take() {
            Some(buffer) if only_holder(&buffer) => buffer,
            // SAFETY: MFCreateMemoryBuffer's signature.
            _ => create("create a frame buffer", |out| unsafe {
                (fns.create_memory_buffer)(size as u32, out)
            })?,
        };
        self.frame_buffer = Some(buffer.clone());
        let map_error = |source| EncodeError::Direct3D {
            action: "copy the frame to system memory for the software encoder",
            source,
        };
        let mf_err = mf_error("copy the frame into a Media Foundation buffer");
        // SAFETY: the staging texture matches the frame (checked by the
        // caller), and Map waits for the copy. While mapped, an NV12 texture
        // is `h` rows of luma then h / 2 rows of chroma, RowPitch apart; the
        // buffer holds `size` bytes while locked. Both are released on
        // every path before this block ends.
        unsafe {
            self.context.CopyResource(&self.staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(map_error)?;
            let pitch = mapped.RowPitch as usize;
            let mut data = std::ptr::null_mut();
            let mut max = 0;
            if let Err(e) = buffer.Lock(&mut data, Some(&mut max), None) {
                self.context.Unmap(&self.staging, 0);
                return Err(mf_err(e));
            }
            if (max as usize) < size || data.is_null() {
                let _ = buffer.Unlock();
                self.context.Unmap(&self.staging, 0);
                return Err(mf_err(windows::Win32::Foundation::E_POINTER.into()));
            }
            let source = mapped.pData as *const u8;
            for row in 0..h * 3 / 2 {
                std::ptr::copy_nonoverlapping(source.add(row * pitch), data.add(row * w), w);
            }
            let _ = buffer.Unlock();
            self.context.Unmap(&self.staging, 0);
            buffer.SetCurrentLength(size as u32).map_err(&mf_err)?;
        }
        // SAFETY: MFCreateSample's signature.
        let sample: IMFSample =
            create("create a sample", |out| unsafe { (fns.create_sample)(out) })?;
        // SAFETY: plain calls on live objects.
        unsafe {
            sample.AddBuffer(&buffer).map_err(&mf_err)?;
            sample
                .SetSampleTime(sample_time(index, self.fps))
                .map_err(&mf_err)?;
            sample
                .SetSampleDuration(sample_time(1, self.fps))
                .map_err(&mf_err)?;
        }
        Ok(sample)
    }

    /// The output for the frame just fed in, None when the encoder wants
    /// more input first. It may come in more than one sample: see has_slice.
    fn take_output(&mut self) -> Result<Option<Vec<u8>>, EncodeError> {
        let mut data = Vec::new();
        while !has_slice(Codec::H264, &data) {
            match self.output.take(&self.transform, &self.fns)? {
                Some(bytes) => data.extend_from_slice(&bytes),
                None if data.is_empty() => return Ok(None),
                None => break,
            }
        }
        Ok(has_slice(Codec::H264, &data).then_some(data))
    }

    // From here on every loss is answered with an IDR, and the frames' headers
    // say so.
    fn long_term_failed(&mut self, what: &str) {
        self.long_term = None;
        self.notes.push_str(&format!(
            ". The encoder {what}, so losses are answered with an IDR from then on"
        ));
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        // SAFETY: a plain message to a streaming encoder.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(mf_error("drain the software encoder"))
    }
}

impl Encoder for Software {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> Kind {
        Kind::MfSoftware
    }

    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn notes(&self) -> &str {
        &self.notes
    }

    fn gauge(&self) {
        stats::gauge!("encode.software.notes", self.notes);
    }

    fn encode(&mut self, frame: &Frame<'_>) -> Result<AccessUnit, EncodeError> {
        platform::com()?;
        if let Some(previous) = self.last_index
            && frame.index <= previous
        {
            return Err(EncodeError::FrameOutOfOrder {
                index: frame.index,
                previous,
            });
        }
        gpu::check_texture(&self.device, frame.texture, self.width, self.height)?;
        let mut idr_asked = self.idr_next || frame.force_idr;
        let mut recovering = false;
        if !idr_asked && self.long_term.as_ref().is_some_and(|l| l.recovering) {
            recovering = self
                .api
                .set(
                    &CODECAPI_AVEncVideoUseLTRFrame,
                    &VARIANT::from(FROM_THE_IDR),
                )
                .is_ok();
            if !recovering {
                self.long_term_failed("refused to predict from its last IDR");
                idr_asked = true;
            }
        }
        if idr_asked {
            request_idr(&self.api, self.last_index.is_none())?;
        }
        self.output.long_term = None;

        // The copy out of the GPU counts as encode time: every other encoder
        // reads the texture where it is.
        let submitted = Instant::now();
        let sample = self.sample(frame.texture, frame.index)?;
        #[cfg(feature = "fault")]
        match self.fail_in {
            Some(0) => {
                self.fail_in = None;
                self.idr_next = true;
                return Err(mf_error("feed a frame to the software encoder")(
                    windows::Win32::Foundation::E_FAIL.into(),
                ));
            }
            Some(frames) => self.fail_in = Some(frames - 1),
            None => {}
        }
        // SAFETY: a live sample of the input type set at open.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.map_err(|source| {
            self.idr_next = true;
            EncodeError::MediaFoundation {
                action: "feed a frame to the software encoder",
                source,
            }
        })?;
        if self.drain_every_frame {
            self.drain().inspect_err(|_| self.idr_next = true)?;
        }
        let mut data = self.take_output().inspect_err(|_| self.idr_next = true)?;
        if data.is_none() && !self.drain_every_frame {
            // It wants the next frame before giving this one back, a frame
            // of delay if Booth fed it one. Drained instead, from now on.
            self.drain_every_frame = true;
            self.notes.push_str(
                ". The encoder held a frame back until drained, so every frame is drained",
            );
            self.drain().inspect_err(|_| self.idr_next = true)?;
            data = self.take_output().inspect_err(|_| self.idr_next = true)?;
        }
        let Some(data) = data else {
            self.idr_next = true;
            return Err(EncodeError::EncoderMisbehaved {
                problem: "the software encoder kept a frame even when told to drain: share again",
            });
        };
        let ready = Instant::now();

        let idr = has_idr(Codec::H264, &data);
        self.last_index = Some(frame.index);
        if idr {
            self.idr_next = false;
        }
        let said = self.output.long_term;
        if let Some(long_term) = &mut self.long_term {
            if idr {
                let marked = said.is_some_and(|info| info & 0xFFFF != NOT_MARKED);
                long_term.anchor = marked.then_some(frame.index);
                long_term.recovering = false;
                if !marked {
                    self.long_term_failed("did not keep its IDR as a long-term reference");
                }
            } else if recovering {
                long_term.recovering = false;
                long_term.spent = Some((data.len(), ready));
                if !said.is_some_and(|info| info >> 16 & 1 == 1) {
                    self.long_term_failed("did not predict from its last IDR when asked");
                    self.idr_next = true;
                }
            }
        }
        self.survives = match &mut self.long_term {
            Some(long_term) if long_term.waits(self.bitrate, ready) => {
                if long_term.holding.len() == HOLDING_KEPT {
                    long_term.holding.pop_front();
                }
                long_term.holding.push_back(frame.index);
                false
            }
            Some(_) => true,
            None => false,
        };
        Ok(AccessUnit {
            data,
            index: frame.index,
            idr,
            submitted,
            ready,
        })
    }

    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError> {
        if bits_per_second < self.fps {
            return Err(EncodeError::BadRate {
                fps: self.fps,
                bitrate: bits_per_second,
            });
        }
        platform::com()?;
        // The buffer is left as it was. This encoder takes a new size while
        // encoding and reads it back, but its output does not change by a
        // byte, even at one frame's worth. What it does do is keep sending
        // at the old rate for about 16 frames before taking the new one: at
        // 60 fps, going from 8 to 2.7 Mbit/s, the first second after the
        // change carries 3.75 Mbit.
        change_bitrate(&self.api, bits_per_second, None)
            .map_err(mf_error("change the software encoder's bitrate"))?;
        self.bitrate = bits_per_second;
        Ok(())
    }

    fn recover(&mut self, lost_frame_index: u64) -> Recovery {
        if !self.needs_idr(lost_frame_index)
            && let Some(long_term) = &mut self.long_term
        {
            long_term.recovering = true;
            return Recovery::Invalidated;
        }
        self.idr_next = true;
        Recovery::Idr
    }

    fn needs_idr(&self, lost_frame_index: u64) -> bool {
        self.idr_next
            || !self.long_term.as_ref().is_some_and(|long_term| {
                long_term
                    .anchor
                    .is_some_and(|anchor| anchor < lost_frame_index)
                    && !long_term.waits(self.bitrate, Instant::now())
                    && !long_term.holding.contains(&lost_frame_index)
            })
    }

    fn invalidates(&self) -> bool {
        self.survives
    }
}

impl Drop for Software {
    fn drop(&mut self) {
        let _ = platform::com();
        // SAFETY: plain messages to a live transform.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// Whether `buffer` is the only reference to its object. The encoder may
/// keep a frame's sample after its output is out (it does not promise
/// otherwise: MFT_INPUT_STREAM_DOES_NOT_ADDREF), and a buffer it still holds
/// must not be written. Nothing else can take a new reference while this is
/// the only one, so a count of one cannot be out of date.
fn only_holder(buffer: &IMFMediaBuffer) -> bool {
    let unknown = &buffer.vtable().base__;
    // SAFETY: one AddRef and one Release on a live interface pointer.
    // Release returns the count left.
    unsafe {
        (unknown.AddRef)(buffer.as_raw());
        (unknown.Release)(buffer.as_raw()) == 1
    }
}

fn staging(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D, EncodeError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: a valid description and out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(|source| {
        EncodeError::Direct3D {
            action: "create the texture the software encoder reads frames through",
            source,
        }
    })?;
    texture.ok_or(EncodeError::Direct3D {
        action: "create the texture the software encoder reads frames through",
        source: windows::Win32::Foundation::E_POINTER.into(),
    })
}
