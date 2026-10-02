// Fallback 1: the hardware H.264 or HEVC encoder a GPU's driver registers
// with Media Foundation. These encoders are asynchronous: they say through
// their event generator, from their own threads, when they want a frame and
// when one is done. Booth still keeps one frame in flight: encode() feeds a
// frame and returns once that frame's bitstream is out.

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LUID, S_OK};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaEventGenerator, IMFSample, IMFShutdown, IMFTransform,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformDrainComplete, METransformHaveOutput,
    METransformNeedInput, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_REGISTER_TYPE_INFO, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{HRESULT, Interface, PWSTR};

use super::codec::CodecApi;
use super::platform::{self, Functions, Mf};
use super::{
    Buffer, Output, change_bitrate, check_idr_on_request, configure, create, has_idr, has_slice,
    keyframe_without_idr, mf_error, request_idr, sample_time, subtype,
};
use crate::gpu::{self, Adapter};
use crate::{AccessUnit, Codec, EncodeError, Encoder, Frame, Kind, Recovery, Request, Settings};

// Far longer than any encode: a frame that takes this long is not coming.
const GIVE_UP: Duration = Duration::from_secs(2);
// How long a frame may be out with the encoder asking for the next one
// before it counts as held back. Longer than any encode Booth would use.
const HELD: Duration = Duration::from_millis(100);
// Frames held back in a row before every frame is drained. One slow frame,
// a GPU stall while a game loads, is not an encoder that holds frames.
const HELD_IN_A_ROW: u32 = 3;

// Why a frame is drained out of the encoder instead of waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    // The encoder asked for the next frame before giving this one back.
    Ahead,
    // It said its output format changed and gave no frame after it.
    NewFormat,
}

// Until when wait_for_output waits for the next event of a frame that went
// in at `submitted`, and what running out of that time means: Some, the
// frame is drained out, None, it is given up on. After a new output format
// the wait counts from the change, so a slow first frame that also changes
// the format is not drained for having been slow before it.
fn wait_limit(
    submitted: Instant,
    changed: Option<Instant>,
    wanted: u32,
    drained: bool,
    output_wait: Duration,
) -> (Instant, Option<Held>) {
    match changed {
        _ if drained => (submitted + output_wait, None),
        Some(at) => (at + HELD, Some(Held::NewFormat)),
        None if wanted > 0 => (submitted + HELD, Some(Held::Ahead)),
        None => (submitted + output_wait, None),
    }
}

enum Event {
    Raised { kind: u32, status: HRESULT },
    Ended(windows::core::Error),
}

struct Generator(IMFMediaEventGenerator);

// SAFETY: Media Foundation objects are free-threaded, and an asynchronous
// encoder's event generator is meant to be waited on from a thread of the
// client's choosing.
unsafe impl Send for Generator {}

// The thread that waits on the encoder's events and passes them on, so
// that encode() can wait for them with a time limit: GetEvent itself waits
// forever, and an encoder that holds a frame back would never answer.
// Costs one hand-over between threads per event.
fn pump(generator: Generator, events: Sender<Event>) {
    let _ = platform::com();
    loop {
        // SAFETY: a blocking wait on a live generator. It returns an error
        // once the encoder is shut down, which ends this thread.
        let event = unsafe {
            generator
                .0
                .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
        };
        let message = match event {
            // SAFETY: getters on a live event.
            Ok(event) => unsafe {
                Event::Raised {
                    kind: event.GetType().unwrap_or(0),
                    status: event.GetStatus().unwrap_or(S_OK),
                }
            },
            Err(e) => {
                let _ = events.send(Event::Ended(e));
                return;
            }
        };
        if events.send(message).is_err() {
            return;
        }
    }
}

/// The activated encoder and its event thread, shut down together however
/// far opening got.
struct Mft {
    transform: IMFTransform,
    activate: IMFActivate,
    events: Option<Receiver<Event>>,
    pump: Option<JoinHandle<()>>,
    // What shut_down found the first time: whether the event thread ended.
    ended: Option<bool>,
}

impl Drop for Mft {
    fn drop(&mut self) {
        self.shut_down();
    }
}

impl Mft {
    // Whether the event thread ended, or there was none. One that did not
    // still holds the encoder through its generator, and the encoder its
    // Direct3D device manager, until the driver lets GetEvent return.
    fn shut_down(&mut self) -> bool {
        if let Some(ended) = self.ended {
            return ended;
        }
        let _ = platform::com();
        // SAFETY: plain calls on live interfaces. An asynchronous encoder
        // must be shut down, which also ends a GetEvent waiting on it.
        let shut = unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let by_itself = self
                .transform
                .cast::<IMFShutdown>()
                .and_then(|s| s.Shutdown())
                .is_ok();
            self.activate.ShutdownObject().is_ok() || by_itself
        };
        let ended = match (self.events.take(), self.pump.take()) {
            (Some(events), Some(pump)) => shut && joined(&events, pump),
            _ => true,
        };
        self.ended = Some(ended);
        #[cfg(feature = "fault")]
        crate::fault::shut_down(ended);
        ended
    }
}

// Joined only once the thread has said it is leaving: an encoder that
// ignored the shutdown would otherwise hang the caller here.
fn joined(events: &Receiver<Event>, pump: JoinHandle<()>) -> bool {
    let deadline = Instant::now() + GIVE_UP;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Raised { .. }) => continue,
            Ok(Event::Ended(_)) | Err(RecvTimeoutError::Disconnected) => {
                let _ = pump.join();
                return true;
            }
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

pub(crate) struct Hardware {
    mft: Mft,
    api: CodecApi,
    fns: Functions,
    device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    name: String,
    notes: String,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    // METransformNeedInput events not yet answered with a frame.
    wanted: u32,
    output: Output,
    // How long encode() waits for a frame's output. GIVE_UP, except in the
    // test that makes a frame late.
    output_wait: Duration,
    held_in_a_row: u32,
    // Set once the encoder has held HELD_IN_A_ROW frames back.
    drain_every_frame: bool,
    threw_away_late_output: bool,
    said_format_change: bool,
    said_drain: bool,
    idr_next: bool,
    last_index: Option<u64>,
    // Frames given back.
    encoded: u64,
    // From this many frames given back on, the encoder's requests for
    // frames go unheard: crate::fault's stall, None outside a test.
    stall_after: Option<u64>,
    // Last, so Media Foundation stops only after everything above is gone.
    _mf: Mf,
}

// SAFETY: Media Foundation objects are free-threaded and every entry point
// joins the calling thread to COM first. Nothing here is shared: Hardware
// is not Sync and every call takes &mut self. The Direct3D device's own
// thread rule is the caller's, as open_codec() in lib.rs says.
unsafe impl Send for Hardware {}

impl Hardware {
    pub(crate) fn open(
        device: &ID3D11Device,
        adapter: &Adapter,
        request: &Request,
        settings: &Settings,
        passed_over: String,
    ) -> Result<Hardware, EncodeError> {
        let Request {
            codec,
            width,
            height,
            fps,
        } = *request;
        let _opening = gpu::opening();
        let mf = Mf::start()?;
        let fns = mf.fns;
        let Some(activate) = enumerate(&fns, adapter.luid, codec)?.into_iter().next() else {
            return Err(EncodeError::NoHardwareEncoder {
                gpu: adapter.name.clone(),
                codec,
            });
        };
        let friendly = friendly_name(&activate);
        // SAFETY: activating a live activation object.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(mf_error("start the hardware encoder"))?;
        let mut mft = Mft {
            transform,
            activate,
            events: None,
            pump: None,
            ended: None,
        };
        #[cfg(feature = "fault")]
        crate::fault::started();
        #[cfg(feature = "fault")]
        let stall_after = crate::fault::take_stall(codec);
        #[cfg(not(feature = "fault"))]
        let stall_after = None;
        let transform = mft.transform.clone();

        // SAFETY: attribute calls on the encoder's own live store.
        unsafe {
            let attributes: IMFAttributes = transform
                .GetAttributes()
                .map_err(mf_error("read the hardware encoder's attributes"))?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(EncodeError::EncoderMisbehaved {
                    problem: "the hardware encoder is not asynchronous, which Windows requires of hardware encoders",
                });
            }
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(mf_error("unlock the asynchronous hardware encoder"))?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(EncodeError::EncoderMisbehaved {
                    problem: "the hardware encoder does not take Direct3D 11 textures, so it cannot read frames where capture leaves them",
                });
            }
        }

        let mut api = CodecApi::of(&transform);
        check_idr_on_request(
            &mut api,
            "the hardware encoder cannot make an IDR on request, which Booth needs to recover from a lost frame: update the graphics driver",
        )?;
        protect(device, &mut api)?;
        let mut token = 0;
        // SAFETY: MFCreateDXGIDeviceManager's signature, valid out pointers.
        let manager: IMFDXGIDeviceManager =
            create("create a Direct3D device manager", |out| unsafe {
                (fns.create_device_manager)(&mut token, out)
            })?;
        // SAFETY: a live device, the token that came with the manager, and
        // the manager's pointer handed to an encoder that AddRefs it.
        unsafe {
            manager
                .ResetDevice(device, token)
                .map_err(mf_error("hand the Direct3D device to Media Foundation"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(mf_error("hand the Direct3D device to the hardware encoder"))?;
        }

        configure(
            &mut api,
            fps,
            settings.bitrate,
            settings.preset,
            Buffer::OneFrame,
        );
        let output_type = super::output_type(&fns, codec, width, height, fps, settings.bitrate)?;
        let nv12 = super::video_type(&fns, &MFVideoFormat_NV12, width, height, fps)?;
        // SAFETY: live media types; encoders take the output type first.
        unsafe {
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(mf_error(match codec {
                    Codec::H264 => "set the hardware encoder's H.264 output",
                    Codec::Hevc => "set the hardware encoder's HEVC output",
                }))?;
            transform
                .SetInputType(0, &nv12, 0)
                .map_err(mf_error("set the hardware encoder's NV12 input"))?;
        }
        api.reapply();
        let output = Output::of(&transform, true)?;

        // An asynchronous encoder is its own event generator.
        let generator: IMFMediaEventGenerator = transform
            .cast()
            .map_err(mf_error("reach the hardware encoder's events"))?;
        let (sender, receiver) = mpsc::channel();
        let generator = Generator(generator);
        let pump = thread::Builder::new()
            .name("encoder events".to_string())
            .spawn(move || pump(generator, sender))
            .map_err(|e| EncodeError::MediaFoundation {
                action: "start the thread that waits for the hardware encoder",
                source: windows::core::Error::from(e),
            })?;
        mft.events = Some(receiver);
        mft.pump = Some(pump);

        // SAFETY: the types are set, so the encoder may start.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(mf_error("start the hardware encoder's stream"))?;
        }

        let notes = [passed_over, api.report()]
            .into_iter()
            .filter(|n| !n.is_empty())
            .collect::<Vec<_>>()
            .join(". ");
        Ok(Hardware {
            mft,
            api,
            fns,
            device: device.clone(),
            _manager: manager,
            name: format!("Media Foundation hardware {codec} ({friendly})"),
            notes,
            codec,
            width,
            height,
            fps,
            wanted: 0,
            output,
            output_wait: GIVE_UP,
            held_in_a_row: 0,
            drain_every_frame: false,
            threw_away_late_output: false,
            said_format_change: false,
            said_drain: false,
            idr_next: true,
            last_index: None,
            encoded: 0,
            stall_after,
            _mf: mf,
        })
    }

    fn next_event(&mut self, until: Instant) -> Result<Option<(u32, HRESULT)>, EncodeError> {
        let Some(events) = &self.mft.events else {
            return Ok(None);
        };
        match events.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(Event::Raised { kind, status }) => Ok(Some((kind, status))),
            Ok(Event::Ended(source)) => Err(EncodeError::MediaFoundation {
                action: "hear from the hardware encoder, which stopped sending events",
                source,
            }),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(EncodeError::EncoderMisbehaved {
                problem: "the thread that waits for the hardware encoder's events ended: share again",
            }),
        }
    }

    fn sample(&self, texture: &ID3D11Texture2D, index: u64) -> Result<IMFSample, EncodeError> {
        let fns = &self.fns;
        // SAFETY: MFCreateDXGISurfaceBuffer's signature: the texture's IID
        // and a live texture, which the buffer AddRefs.
        let buffer: IMFMediaBuffer = create("wrap the frame texture", |out| unsafe {
            (fns.create_surface_buffer)(
                &ID3D11Texture2D::IID,
                texture.as_raw(),
                0,
                false.into(),
                out,
            )
        })?;
        // SAFETY: MFCreateSample's signature.
        let sample: IMFSample =
            create("create a sample", |out| unsafe { (fns.create_sample)(out) })?;
        let error = mf_error("describe the frame to the hardware encoder");
        // SAFETY: plain calls on live objects made above. Some encoders read
        // the buffer's length before its surface, so it is set to the whole
        // picture.
        unsafe {
            if let Ok(picture) = buffer.cast::<IMF2DBuffer>() {
                let length = picture.GetContiguousLength().map_err(&error)?;
                buffer.SetCurrentLength(length).map_err(&error)?;
            }
            sample.AddBuffer(&buffer).map_err(&error)?;
            sample
                .SetSampleTime(sample_time(index, self.fps))
                .map_err(&error)?;
            sample
                .SetSampleDuration(sample_time(1, self.fps))
                .map_err(&error)?;
        }
        Ok(sample)
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        self.wanted = 0;
        // SAFETY: a plain message to a streaming encoder.
        unsafe {
            self.mft
                .transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(mf_error("drain the hardware encoder"))
        }
    }

    /// Waits for the frame just fed in and returns its bitstream.
    fn wait_for_output(
        &mut self,
        index: u64,
        submitted: Instant,
    ) -> Result<(Vec<u8>, Instant), EncodeError> {
        let mut data = Vec::new();
        let mut drained = self.drain_every_frame;
        if drained {
            self.drain()?;
        }
        // When the output format changed, while no frame has come after it.
        let mut changed = None;
        loop {
            let (limit, held) =
                wait_limit(submitted, changed, self.wanted, drained, self.output_wait);
            let Some((kind, status)) = self.next_event(limit)? else {
                let Some(held) = held else {
                    return Err(EncodeError::NoOutput {
                        index,
                        waited: submitted.elapsed(),
                    });
                };
                if held == Held::Ahead {
                    // It wants the next frame before giving this one back:
                    // a frame of delay if Booth fed it one. This frame is
                    // drained out instead. A slow first frame or a GPU stall
                    // lands here once; an encoder that holds frames does it
                    // every time, and then every frame is drained.
                    self.held_in_a_row += 1;
                    if self.held_in_a_row == HELD_IN_A_ROW {
                        self.drain_every_frame = true;
                        self.notes.push_str(&format!(
                            ". The encoder held {HELD_IN_A_ROW} frames in a row back until drained, so every frame from {index} on is drained"
                        ));
                    }
                }
                // An encoder that sends no METransformHaveOutput after a new
                // output format gives the frame up to a drain as well.
                self.note_drain(index, held, submitted.elapsed());
                drained = true;
                self.drain()?;
                continue;
            };
            if status.is_err() {
                return Err(EncodeError::MediaFoundation {
                    action: "encode a frame on the hardware encoder",
                    source: status.into(),
                });
            }
            if kind == METransformNeedInput.0 as u32 {
                self.hear_request();
            } else if kind == METransformHaveOutput.0 as u32 {
                let taken = self.output.take(&self.mft.transform, &self.fns)?;
                if self.output.format_changed() {
                    changed = Some(Instant::now());
                    self.note_format_change(index);
                }
                if let Some(bytes) = taken {
                    changed = None;
                    if has_slice(self.codec, &data) && has_slice(self.codec, &bytes) {
                        return Err(EncodeError::EncoderMisbehaved {
                            problem: "the hardware encoder gave two frames back for one: share again, and update the graphics driver if it keeps happening",
                        });
                    }
                    data.extend_from_slice(&bytes);
                    if has_slice(self.codec, &data) && !drained {
                        break;
                    }
                }
            } else if kind == METransformDrainComplete.0 as u32 {
                break;
            }
        }
        if !has_slice(self.codec, &data) {
            return Err(EncodeError::NoOutput {
                index,
                waited: submitted.elapsed(),
            });
        }
        if !drained {
            self.held_in_a_row = 0;
        }
        Ok((data, Instant::now()))
    }

    /// Waits until the encoder asks for a frame. The output of a frame
    /// encode() gave up on can still come out here: it is taken and thrown
    /// away, since that frame's caller already had the error and the next
    /// frame is an IDR. Left in the encoder, it would stop it from ever
    /// asking for another frame. `index` is the frame about to go in.
    fn wait_for_request(&mut self, index: u64) -> Result<(), EncodeError> {
        while self.wanted == 0 {
            let Some((kind, status)) = self.next_event(Instant::now() + GIVE_UP)? else {
                return Err(EncodeError::EncoderMisbehaved {
                    problem: "the hardware encoder has not asked for a frame in 2 s: its driver may have stopped responding",
                });
            };
            if status.is_err() {
                return Err(EncodeError::MediaFoundation {
                    action: "wait for the hardware encoder to ask for a frame",
                    source: status.into(),
                });
            }
            if kind == METransformNeedInput.0 as u32 {
                self.hear_request();
                continue;
            }
            if kind != METransformHaveOutput.0 as u32 {
                continue;
            }
            let late = self.output.take(&self.mft.transform, &self.fns)?;
            if self.output.format_changed() {
                self.note_format_change(index);
            }
            if late.is_some() {
                self.idr_next = true;
                if !self.threw_away_late_output {
                    self.threw_away_late_output = true;
                    self.notes.push_str(
                        ". A frame came out after Booth had given up on it and was thrown away",
                    );
                }
            }
        }
        Ok(())
    }

    fn stalled(&self) -> bool {
        self.stall_after.is_some_and(|after| self.encoded >= after)
    }

    fn hear_request(&mut self) {
        if !self.stalled() {
            self.wanted += 1;
        }
    }

    // Once per encoder, for the log: which encoders change their output
    // format mid-stream only a share on one can tell, and Booth has run on
    // NVIDIA's only.
    fn note_format_change(&mut self, index: u64) {
        if !self.said_format_change {
            self.said_format_change = true;
            self.notes.push_str(&format!(
                ". The encoder changed its output format at frame {index}"
            ));
        }
    }

    // Once per encoder, for the log, as the frames held back in a row are:
    // a drain costs the frame its wait, and whether an encoder asks for
    // frames again after one only a share on it can tell.
    fn note_drain(&mut self, index: u64, held: Held, waited: Duration) {
        if self.said_drain {
            return;
        }
        self.said_drain = true;
        let why = match held {
            Held::Ahead => "the encoder asked for the next frame before giving it back",
            Held::NewFormat => "no frame followed the encoder's new output format",
        };
        self.notes.push_str(&format!(
            ". Frame {index} was drained out {:.1} ms after it went in: {why}",
            waited.as_secs_f64() * 1000.0
        ));
    }
}

impl Encoder for Hardware {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> Kind {
        Kind::MfHardware
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn notes(&self) -> &str {
        &self.notes
    }

    fn shut_down(&mut self) -> Option<String> {
        (!self.mft.shut_down()).then(|| {
            format!(
                "its event thread had not ended {} s after the encoder was shut down, so its driver may still hold the encoder and its Direct3D device manager",
                GIVE_UP.as_secs()
            )
        })
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
        let sample = self.sample(frame.texture, frame.index)?;

        if self.stalled() {
            self.wanted = 0;
        }
        self.wait_for_request(frame.index)?;
        let asked_for_idr = self.idr_next || frame.force_idr;
        if asked_for_idr {
            request_idr(&self.api, self.last_index.is_none())?;
        }

        let submitted = Instant::now();
        // SAFETY: the encoder asked for input, and the sample wraps a live
        // NV12 texture on the encoder's device.
        unsafe { self.mft.transform.ProcessInput(0, &sample, 0) }.map_err(|source| {
            // The encoder may keep this frame as a reference though the
            // viewer never gets it.
            self.idr_next = true;
            EncodeError::MediaFoundation {
                action: "feed a frame to the hardware encoder",
                source,
            }
        })?;
        self.wanted -= 1;
        let (data, ready) = self
            .wait_for_output(frame.index, submitted)
            .inspect_err(|_| self.idr_next = true)?;

        let idr = has_idr(self.codec, &data);
        self.last_index = Some(frame.index);
        if asked_for_idr && keyframe_without_idr(self.codec, &data) {
            // Asking again would make every frame a keyframe the viewer
            // never starts from, at several frames' worth each.
            self.idr_next = true;
            return Err(EncodeError::EncoderMisbehaved {
                problem: "the hardware HEVC encoder answered a request for an IDR with a CRA picture, and a viewer starts and recovers only at an IDR: update the graphics driver, or share in H.264",
            });
        }
        if idr {
            self.idr_next = false;
        }
        self.encoded += 1;
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
        change_bitrate(&self.api, bits_per_second, Some(bits_per_second / self.fps))
            .map_err(mf_error("change the hardware encoder's bitrate"))?;
        Ok(())
    }

    fn recover(&mut self, _lost_frame_index: u64) -> Recovery {
        self.idr_next = true;
        Recovery::Idr
    }
}

/// The name of the hardware encoder for `codec` Windows lists first for this
/// GPU, the one open() would start; the reason as a sentence when it lists
/// none. Media Foundation says nothing of an encoder's limits until it is
/// started, so a size past them shows only at open.
pub(crate) fn offers(adapter: &Adapter, codec: Codec) -> Result<String, String> {
    let mf = Mf::start().map_err(|e| e.to_string())?;
    let listed = enumerate(&mf.fns, adapter.luid, codec).map_err(|e| e.to_string())?;
    let Some(first) = listed.first() else {
        let none = EncodeError::NoHardwareEncoder {
            gpu: adapter.name.clone(),
            codec,
        };
        return Err(none.to_string());
    };
    Ok(friendly_name(first))
}

/// The hardware encoders for `codec` Windows lists for one GPU, best first.
fn enumerate(fns: &Functions, luid: LUID, codec: Codec) -> Result<Vec<IMFActivate>, EncodeError> {
    // SAFETY: MFCreateAttributes's signature, with a valid out pointer.
    let attributes: IMFAttributes = create("create an attribute store", |out| unsafe {
        (fns.create_attributes)(out, 1)
    })?;
    let luid_bytes = [luid.LowPart.to_le_bytes(), luid.HighPart.to_le_bytes()].concat();
    // SAFETY: a setter on a live store; the blob is copied.
    unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid_bytes) }
        .map_err(mf_error("name the GPU to list encoders for"))?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let mut array: *mut *mut c_void = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnum2's signature; the type infos and the attribute store
    // outlive the call, and the out pointers are valid.
    unsafe {
        (fns.enumerate)(
            MFT_CATEGORY_VIDEO_ENCODER,
            (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32,
            &input,
            &output,
            attributes.as_raw(),
            &mut array,
            &mut count,
        )
    }
    .ok()
    .map_err(mf_error(match codec {
        Codec::H264 => "list the hardware H.264 encoders",
        Codec::Hevc => "list the hardware HEVC encoders",
    }))?;
    let mut list = Vec::new();
    if !array.is_null() {
        for i in 0..count as usize {
            // SAFETY: the array holds `count` pointers, each one reference
            // that is now ours; the array itself is freed below.
            let raw = unsafe { *array.add(i) };
            if !raw.is_null() {
                list.push(unsafe { IMFActivate::from_raw(raw) });
            }
        }
        // SAFETY: allocated by MFTEnum2 with CoTaskMemAlloc, freed once.
        unsafe { CoTaskMemFree(Some(array as *const c_void)) };
    }
    Ok(list)
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut text = PWSTR::null();
    let mut length = 0;
    // SAFETY: the attribute store allocates the string, which is copied and
    // freed here.
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)
            .is_err()
            || text.is_null()
        {
            return "unnamed".to_string();
        }
        let name = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as *const c_void));
        name
    }
}

/// The encoder works on the device from its own threads.
fn protect(device: &ID3D11Device, api: &mut CodecApi) -> Result<(), EncodeError> {
    let error = |source| EncodeError::Direct3D {
        action: "turn on multithread protection for the hardware encoder",
        source,
    };
    // SAFETY: plain calls on a live device and its context.
    unsafe {
        let context = device.GetImmediateContext().map_err(error)?;
        let multithread: ID3D11Multithread = context.cast().map_err(error)?;
        if !multithread.GetMultithreadProtected().as_bool() {
            let _ = multithread.SetMultithreadProtected(true);
            api.note("turned on the device's multithread protection".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{E_UNEXPECTED, HMODULE};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIFactory1,
    };

    use super::*;

    /// A device on the first GPU that is not Windows' software rasterizer,
    /// of any vendor.
    fn hardware_device() -> Option<ID3D11Device> {
        // SAFETY: plain DXGI and Direct3D calls; EnumAdapters1 fails past the
        // last adapter and every out pointer is valid for its call.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let adapter = (0..)
                .map_while(|i| factory.EnumAdapters1(i).ok())
                .find(|a| {
                    a.GetDesc1()
                        .is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0)
                })?;
            let adapter: IDXGIAdapter = adapter.cast().ok()?;
            let mut device = None;
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            device
        }
    }

    // Blank, which is all this test needs: it looks at frame types only.
    fn nv12(device: &ID3D11Device, width: u32, height: u32) -> ID3D11Texture2D {
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
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a valid description and out pointer.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("NV12 texture");
        texture.expect("NV12 texture")
    }

    #[test]
    fn frame_given_up_on() {
        let _turn = gpu::test_turn();
        let Some(device) = hardware_device() else {
            println!("skipped: no GPU on this PC, so there is no hardware encoder to test");
            return;
        };
        let adapter = gpu::adapter_of(&device).unwrap_or_else(|e| panic!("{e}"));
        let request = Request {
            codec: Codec::H264,
            width: 1280,
            height: 720,
            fps: 60,
        };
        let opened = Hardware::open(
            &device,
            &adapter,
            &request,
            &Settings::default(),
            String::new(),
        );
        let mut encoder = match opened {
            Ok(encoder) => encoder,
            Err(
                e @ (EncodeError::NoHardwareEncoder { .. } | EncodeError::MediaFoundationMissing),
            ) => {
                println!("skipped: {e}");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let texture = nv12(&device, 1280, 720);
        let frame = |index| Frame {
            texture: &texture,
            index,
            force_idr: false,
        };
        let first = encoder
            .encode(&frame(0))
            .unwrap_or_else(|e| panic!("frame 0: {e}"));
        assert!(first.idr);

        // No encoder answers in 0 ms, so frame 1 is given up on the way a
        // frame stuck behind a GPU stall is after 2 s, and its output comes
        // out later.
        encoder.output_wait = Duration::ZERO;
        match encoder.encode(&frame(1)) {
            Err(EncodeError::NoOutput { .. }) => {}
            Ok(_) => {
                println!(
                    "skipped: this encoder asks for the next frame before the last is out, so frame 1 was drained out instead of given up on"
                );
                return;
            }
            Err(e) => panic!("frame 1: {e}"),
        }
        encoder.output_wait = GIVE_UP;

        for i in 2..6 {
            let unit = encoder
                .encode(&frame(i))
                .unwrap_or_else(|e| panic!("frame {i}: {e}"));
            assert_eq!(unit.idr, i == 2, "frame {i}");
        }
        assert!(
            encoder.notes().contains("thrown away"),
            "{}",
            encoder.notes()
        );
    }

    // A frame whose encoder changes its output format 150 ms after it went
    // in, as a slow first frame may, is waited on HELD from the change, not
    // drained at once because HELD from going in had passed.
    #[test]
    fn wait_from_format_change() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let changed = Some(at(150));
        assert_eq!(wait_limit(start, None, 0, false, GIVE_UP), (at(2000), None));
        assert_eq!(
            wait_limit(start, None, 1, false, GIVE_UP),
            (at(100), Some(Held::Ahead))
        );
        for wanted in [0, 1] {
            assert_eq!(
                wait_limit(start, changed, wanted, false, GIVE_UP),
                (at(250), Some(Held::NewFormat))
            );
        }
        // Drained, it has the whole wait and is given up on after it.
        assert_eq!(
            wait_limit(start, changed, 1, true, GIVE_UP),
            (at(2000), None)
        );
        assert_eq!(wait_limit(start, None, 1, true, GIVE_UP), (at(2000), None));
    }

    // One ProcessOutput with no METransformHaveOutput before it.
    fn unannounced(encoder: &Hardware) -> HRESULT {
        let own = (!encoder.output.provides_samples).then(|| {
            super::super::output_sample(&encoder.fns, encoder.output.size)
                .unwrap_or_else(|e| panic!("{e}"))
        });
        let mut buffers = [
            windows::Win32::Media::MediaFoundation::MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            },
        ];
        let result =
            super::super::Outputs::process_output(&encoder.mft.transform, &mut buffers, &mut 0);
        for buffer in &mut buffers {
            // SAFETY: each is taken once, here, and not used after.
            unsafe {
                std::mem::ManuallyDrop::drop(&mut buffer.pSample);
                std::mem::ManuallyDrop::drop(&mut buffer.pEvents);
            }
        }
        result.map_or_else(|e| e.code(), |()| S_OK)
    }

    // The code in the log of the Iris Xe laptop whose H.264 encoder failed
    // on its first frame, 0x8000ffff, is what an asynchronous encoder says
    // to a ProcessOutput no event announced: NVIDIA's do, before the first
    // frame and between frames, and go on encoding. So after a new output
    // format Output::take waits for the next event instead of asking again.
    #[test]
    fn unannounced_output_refused() {
        let _turn = gpu::test_turn();
        let Some(device) = hardware_device() else {
            println!("skipped: no GPU on this PC, so there is no hardware encoder to test");
            return;
        };
        let adapter = gpu::adapter_of(&device).unwrap_or_else(|e| panic!("{e}"));
        let texture = nv12(&device, 1920, 1200);
        for codec in Codec::ALL {
            let request = Request {
                codec,
                width: 1920,
                height: 1200,
                fps: 60,
            };
            let opened = Hardware::open(
                &device,
                &adapter,
                &request,
                &Settings::default(),
                String::new(),
            );
            let mut encoder = match opened {
                Ok(encoder) => encoder,
                Err(
                    e @ (EncodeError::NoHardwareEncoder { .. }
                    | EncodeError::MediaFoundationMissing),
                ) => {
                    println!("skipped {codec}: {e}");
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let mut answers = vec![unannounced(&encoder)];
            for index in 0..3 {
                let unit = encoder
                    .encode(&Frame {
                        texture: &texture,
                        index,
                        force_idr: false,
                    })
                    .unwrap_or_else(|e| panic!("{codec} frame {index}: {e}"));
                assert_eq!(unit.idr, index == 0, "{codec} frame {index}");
                answers.push(unannounced(&encoder));
            }
            println!(
                "{}: {:x?}",
                encoder.name(),
                answers.iter().map(|a| a.0 as u32).collect::<Vec<_>>()
            );
            if adapter.vendor == gpu::NVIDIA {
                assert!(
                    answers.iter().all(|&a| a == E_UNEXPECTED),
                    "{codec}: {answers:?}"
                );
            }
        }
    }
}
