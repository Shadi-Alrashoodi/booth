// A stand-in for a driver's asynchronous encoder that keeps Media Foundation's
// rules for them, where the encoders on this PC need not, and gives the orders
// of events a real one cannot be made to. No GPU: the frames are blank textures
// on Windows' software rasterizer, which it never reads.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, E_UNEXPECTED, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, ICodecAPI_Impl, IMFAttributes, IMFMediaEvent,
    IMFMediaType, IMFTransform_Impl, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_TYPE,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Ref, implement, s, w};

use super::super::output_sample;
use super::*;

const WIDTH: u32 = 16;
const HEIGHT: u32 = 16;
// One frame a second, so a sample's time gives its frame number back whole.
const FPS: u32 = 1;
const SECOND: i64 = 10_000_000;
// Past HELD, as a frame behind a GPU stall is.
const SLOW: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    // A frame comes back at once, and the next request with it.
    Prompt,
    // The same, SLOW after the frame went in.
    Slow,
    // It asks for the next frame on taking one, and holds that one until it
    // is drained.
    AsksAhead,
    // It says nothing on taking a frame: the test says what comes next.
    Silent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fed {
    frame: u64,
    discontinuity: bool,
}

struct State {
    mode: Mode,
    // Requests raised and not answered with a frame.
    pending: u32,
    // Frames taken whose output has not been announced.
    held: Vec<Vec<u8>>,
    // Announced, for ProcessOutput.
    ready: VecDeque<Vec<u8>>,
    // From a drain to the next start of stream: no requests and no input.
    stopped: bool,
    idr_due: bool,
    idr_asks: u32,
    ignores_idr_asks: bool,
    // A drain only stops the stream. finish_drain() does the rest.
    drain_waits: bool,
    // A drain takes the requests with it, which the rules do not allow for,
    // and the next request comes only once a frame has been refused.
    drain_drops_requests: bool,
    ask_when_refused: bool,
    // When the request made after a refusal comes. Until then it takes no input.
    asked_after_refusal: Option<Instant>,
    // It asks on every start of stream, though it has a request out.
    asks_on_every_start: bool,
    refuses_drain: bool,
    fed: Vec<Fed>,
    refused: u32,
    starts: u32,
}

struct Script {
    state: RefCell<State>,
    events: Sender<Event>,
    fns: Functions,
}

impl Script {
    fn raise(&self, kind: i32) {
        self.events
            .send(Event::Raised {
                kind: kind as u32,
                status: S_OK,
            })
            .expect("the encoder is open");
    }

    fn ask(&self) {
        self.state.borrow_mut().pending += 1;
        self.raise(METransformNeedInput.0);
    }

    fn announce(&self, bytes: Vec<u8>) {
        self.state.borrow_mut().ready.push_back(bytes);
        self.raise(METransformHaveOutput.0);
    }

    // Everything taken comes out now.
    fn release(&self) {
        let held = std::mem::take(&mut self.state.borrow_mut().held);
        for bytes in held {
            self.announce(bytes);
        }
    }

    // The same SLOW from now, from a thread of its own as a driver's would be.
    fn release_later(&self) {
        let events = self.events.clone();
        let mut state = self.state.borrow_mut();
        let held = std::mem::take(&mut state.held);
        let count = held.len();
        state.ready.extend(held);
        thread::spawn(move || {
            thread::sleep(SLOW);
            for _ in 0..count {
                let _ = events.send(Event::Raised {
                    kind: METransformHaveOutput.0 as u32,
                    status: S_OK,
                });
            }
        });
    }

    fn finish_drain(&self) {
        self.release();
        self.raise(METransformDrainComplete.0);
    }

    fn set_mode(&self, mode: Mode) {
        self.state.borrow_mut().mode = mode;
    }

    fn fed(&self) -> Vec<Fed> {
        self.state.borrow().fed.clone()
    }

    fn starts(&self) -> u32 {
        self.state.borrow().starts
    }

    fn refused(&self) -> u32 {
        self.state.borrow().refused
    }
}

#[implement(IMFTransform, ICodecAPI)]
struct Transform(Rc<Script>);

fn not_scripted<T>() -> windows::core::Result<T> {
    Err(E_NOTIMPL.into())
}

impl IMFTransform_Impl for Transform_Impl {
    fn GetStreamLimits(
        &self,
        _: *mut u32,
        _: *mut u32,
        _: *mut u32,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetStreamCount(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetStreamIDs(&self, _: u32, _: *mut u32, _: u32, _: *mut u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetInputStreamInfo(
        &self,
        _: u32,
        _: *mut MFT_INPUT_STREAM_INFO,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetOutputStreamInfo(&self, _: u32) -> windows::core::Result<MFT_OUTPUT_STREAM_INFO> {
        Ok(MFT_OUTPUT_STREAM_INFO {
            dwFlags: MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32,
            cbSize: 0,
            cbAlignment: 0,
        })
    }

    fn GetAttributes(&self) -> windows::core::Result<IMFAttributes> {
        not_scripted()
    }

    fn GetInputStreamAttributes(&self, _: u32) -> windows::core::Result<IMFAttributes> {
        not_scripted()
    }

    fn GetOutputStreamAttributes(&self, _: u32) -> windows::core::Result<IMFAttributes> {
        not_scripted()
    }

    fn DeleteInputStream(&self, _: u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn AddInputStreams(&self, _: u32, _: *const u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetInputAvailableType(&self, _: u32, _: u32) -> windows::core::Result<IMFMediaType> {
        not_scripted()
    }

    fn GetOutputAvailableType(&self, _: u32, _: u32) -> windows::core::Result<IMFMediaType> {
        not_scripted()
    }

    fn SetInputType(&self, _: u32, _: Ref<IMFMediaType>, _: u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetOutputType(&self, _: u32, _: Ref<IMFMediaType>, _: u32) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetInputCurrentType(&self, _: u32) -> windows::core::Result<IMFMediaType> {
        not_scripted()
    }

    fn GetOutputCurrentType(&self, _: u32) -> windows::core::Result<IMFMediaType> {
        not_scripted()
    }

    fn GetInputStatus(&self, _: u32) -> windows::core::Result<u32> {
        not_scripted()
    }

    fn GetOutputStatus(&self) -> windows::core::Result<u32> {
        not_scripted()
    }

    fn SetOutputBounds(&self, _: i64, _: i64) -> windows::core::Result<()> {
        not_scripted()
    }

    fn ProcessEvent(&self, _: u32, _: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        not_scripted()
    }

    fn ProcessMessage(&self, message: MFT_MESSAGE_TYPE, _: usize) -> windows::core::Result<()> {
        let script = &self.0;
        if message == MFT_MESSAGE_COMMAND_DRAIN {
            if script.state.borrow().refuses_drain {
                return Err(E_FAIL.into());
            }
            let waits = {
                let mut state = script.state.borrow_mut();
                state.stopped = true;
                if state.drain_drops_requests {
                    state.pending = 0;
                }
                state.drain_waits
            };
            if !waits {
                script.finish_drain();
            }
        } else if message == MFT_MESSAGE_NOTIFY_START_OF_STREAM {
            let ask = {
                let mut state = script.state.borrow_mut();
                state.starts += 1;
                state.stopped = false;
                state.ask_when_refused = state.drain_drops_requests;
                (state.pending == 0 || state.asks_on_every_start) && !state.drain_drops_requests
            };
            if ask {
                script.ask();
            }
        }
        Ok(())
    }

    fn ProcessInput(&self, _: u32, sample: Ref<IMFSample>, _: u32) -> windows::core::Result<()> {
        let script = &self.0;
        let sample = sample.ok()?;
        let mode = {
            let mut state = script.state.borrow_mut();
            let asking = state
                .asked_after_refusal
                .is_some_and(|at| Instant::now() < at);
            if state.stopped || state.pending == 0 || asking {
                state.refused += 1;
                if std::mem::take(&mut state.ask_when_refused) {
                    // From a thread of its own, as a driver's would be, so a
                    // frame sent again without waiting for it is refused too.
                    let later = Duration::from_millis(5);
                    state.asked_after_refusal = Some(Instant::now() + later);
                    state.pending += 1;
                    let events = script.events.clone();
                    thread::spawn(move || {
                        thread::sleep(later);
                        let _ = events.send(Event::Raised {
                            kind: METransformNeedInput.0 as u32,
                            status: S_OK,
                        });
                    });
                }
                return Err(MF_E_NOTACCEPTING.into());
            }
            state.pending -= 1;
            // SAFETY: getters on the live sample the caller passed.
            let (time, discontinuity) = unsafe {
                (
                    sample.GetSampleTime()?,
                    sample
                        .GetUINT32(&MFSampleExtension_Discontinuity)
                        .unwrap_or(0)
                        != 0,
                )
            };
            let frame = (time / SECOND) as u64;
            state.fed.push(Fed {
                frame,
                discontinuity,
            });
            let idr = std::mem::take(&mut state.idr_due);
            state.held.push(coded(idr, frame));
            state.mode
        };
        match mode {
            Mode::Prompt => script.release(),
            Mode::Slow => script.release_later(),
            Mode::AsksAhead => script.ask(),
            Mode::Silent => {}
        }
        Ok(())
    }

    fn ProcessOutput(
        &self,
        _: u32,
        _: u32,
        buffers: *mut MFT_OUTPUT_DATA_BUFFER,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        let script = &self.0;
        let (bytes, ask) = {
            let mut state = script.state.borrow_mut();
            let Some(bytes) = state.ready.pop_front() else {
                return Err(E_UNEXPECTED.into());
            };
            let answers = state.mode == Mode::Prompt || state.mode == Mode::Slow;
            (bytes, answers && !state.stopped)
        };
        let sample = sample_of(&script.fns, &bytes)?;
        // SAFETY: Output::take passes one buffer, with no sample in it since
        // this encoder brings its own.
        unsafe { (*buffers).pSample = ManuallyDrop::new(Some(sample)) };
        if ask {
            script.ask();
        }
        Ok(())
    }
}

impl ICodecAPI_Impl for Transform_Impl {
    fn IsSupported(&self, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }

    fn IsModifiable(&self, _: *const GUID) -> HRESULT {
        S_OK
    }

    fn GetParameterRange(
        &self,
        _: *const GUID,
        _: *mut VARIANT,
        _: *mut VARIANT,
        _: *mut VARIANT,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetParameterValues(
        &self,
        _: *const GUID,
        _: *mut *mut VARIANT,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetDefaultValue(&self, _: *const GUID) -> windows::core::Result<VARIANT> {
        not_scripted()
    }

    fn GetValue(&self, _: *const GUID) -> windows::core::Result<VARIANT> {
        not_scripted()
    }

    fn SetValue(&self, api: *const GUID, _: *const VARIANT) -> windows::core::Result<()> {
        // SAFETY: CodecApi::set passes a valid GUID.
        if unsafe { *api } == CODECAPI_AVEncVideoForceKeyFrame {
            let mut state = self.0.state.borrow_mut();
            state.idr_asks += 1;
            state.idr_due = !state.ignores_idr_asks;
        }
        Ok(())
    }

    fn RegisterForEvent(&self, _: *const GUID, _: isize) -> windows::core::Result<()> {
        not_scripted()
    }

    fn UnregisterForEvent(&self, _: *const GUID) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetAllDefaults(&self) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetValueWithNotify(
        &self,
        _: *const GUID,
        _: *const VARIANT,
        _: *mut *mut GUID,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetAllDefaultsWithNotify(
        &self,
        _: *mut *mut GUID,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        not_scripted()
    }

    fn GetAllSettings(&self, _: Ref<IStream>) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetAllSettings(&self, _: Ref<IStream>) -> windows::core::Result<()> {
        not_scripted()
    }

    fn SetAllSettingsWithNotify(
        &self,
        _: Ref<IStream>,
        _: *mut *mut GUID,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        not_scripted()
    }
}

// One H.264 slice with the frame's number in it, so a test can tell whose
// bytes came back.
fn coded(idr: bool, frame: u64) -> Vec<u8> {
    vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }, frame as u8 + 1]
}

fn sample_of(fns: &Functions, bytes: &[u8]) -> Result<IMFSample, windows::core::Error> {
    let sample = output_sample(fns, bytes.len() as u32).map_err(|_| E_UNEXPECTED)?;
    // SAFETY: plain calls on the sample made above; while locked, `data`
    // points at a buffer of at least the size it was made with.
    unsafe {
        let buffer = sample.GetBufferByIndex(0)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
    }
    Ok(sample)
}

struct Rig {
    encoder: Hardware,
    script: Rc<Script>,
    texture: ID3D11Texture2D,
}

impl Rig {
    fn encode(&mut self, index: u64) -> Result<AccessUnit, EncodeError> {
        self.encode_with(index, false)
    }

    fn encode_with(&mut self, index: u64, force_idr: bool) -> Result<AccessUnit, EncodeError> {
        self.encoder.encode(&Frame {
            texture: &self.texture,
            index,
            force_idr,
        })
    }

    // Frame 0, an IDR, out and gone, with the next request on its way.
    fn first_frame(&mut self) {
        let first = self.encode(0).unwrap_or_else(|e| panic!("frame 0: {e}"));
        assert_eq!(first.data, coded(true, 0));
    }

    // The frame times out at once, as one stuck behind a GPU stall does after
    // 2 s. The encoder keeps it.
    fn give_up_on(&mut self, index: u64) {
        self.encoder.output_wait = Duration::ZERO;
        match self.encode(index) {
            Err(EncodeError::NoOutput { .. }) => {}
            Ok(_) => panic!("frame {index} came back"),
            Err(e) => panic!("frame {index}: {e}"),
        }
        self.encoder.output_wait = GIVE_UP;
    }
}

fn warp_device() -> ID3D11Device {
    let mut device = None;
    // SAFETY: the out pointer is valid for the call.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .expect("WARP device");
    device.expect("WARP device")
}

fn blank_nv12(device: &ID3D11Device) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: a valid description and out pointer.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("NV12 texture");
    texture.expect("NV12 texture")
}

// Found by name for the reason in platform.rs: linked, it would stop this whole
// test program from starting on an N edition, where its other tests say skipped.
fn transform_activate() -> IMFActivate {
    // SAFETY: Mf::start loaded mfplat.dll, which stays loaded, and the names are
    // NUL-terminated literals. The export has the signature of its declaration
    // in mfidl.h, which CreateObject copies.
    unsafe {
        let module = GetModuleHandleW(w!("mfplat.dll")).expect("mfplat.dll");
        let export = GetProcAddress(module, s!("MFCreateTransformActivate"))
            .expect("MFCreateTransformActivate");
        let export = std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            platform::CreateObject,
        >(export);
        create("create an activation object", |out| export(out)).unwrap_or_else(|e| panic!("{e}"))
    }
}

// An encoder as Hardware::open leaves one, past what only a real one has, with
// its first request made. The test's events go straight to the channel the
// event thread would fill. None on an edition of Windows with no Media
// Foundation.
fn rig(mode: Mode) -> Option<Rig> {
    let mf = match Mf::start() {
        Ok(mf) => mf,
        Err(e @ EncodeError::MediaFoundationMissing) => {
            println!("skipped: {e}");
            return None;
        }
        Err(e) => panic!("{e}"),
    };
    let fns = mf.fns;
    let (events, receiver) = mpsc::channel();
    let script = Rc::new(Script {
        state: RefCell::new(State {
            mode,
            pending: 0,
            held: Vec::new(),
            ready: VecDeque::new(),
            stopped: false,
            idr_due: false,
            idr_asks: 0,
            ignores_idr_asks: false,
            drain_waits: false,
            drain_drops_requests: false,
            ask_when_refused: false,
            asked_after_refusal: None,
            asks_on_every_start: false,
            refuses_drain: false,
            fed: Vec::new(),
            refused: 0,
            starts: 0,
        }),
        events,
        fns,
    });
    let transform: IMFTransform = Transform(script.clone()).into();
    let activate = transform_activate();
    #[cfg(feature = "fault")]
    crate::fault::started();
    let mft = Mft {
        transform: transform.clone(),
        activate,
        events: Some(receiver),
        pump: None,
        ended: None,
    };
    let device = warp_device();
    let mut token = 0;
    // SAFETY: MFCreateDXGIDeviceManager's signature, valid out pointers.
    let manager: IMFDXGIDeviceManager = create("create a Direct3D device manager", |out| unsafe {
        (fns.create_device_manager)(&mut token, out)
    })
    .unwrap_or_else(|e| panic!("{e}"));
    let output = Output::of(&transform, true).unwrap_or_else(|e| panic!("{e}"));
    let texture = blank_nv12(&device);
    script.ask();
    let encoder = Hardware {
        mft,
        api: CodecApi::of(&transform),
        fns,
        device,
        _manager: manager,
        name: String::from("scripted"),
        notes: String::new(),
        codec: Codec::H264,
        width: WIDTH,
        height: HEIGHT,
        fps: FPS,
        wanted: 0,
        output,
        output_wait: GIVE_UP,
        held_in_a_row: 0,
        drain_every_frame: false,
        draining: false,
        discontinuity: false,
        restarted: false,
        late_frame: false,
        idr_asks_ignored: 0,
        discarded_late_output: false,
        format_change_logged: false,
        drain_logged: false,
        idr_next: true,
        last_index: None,
        encoded: 0,
        stall_after: None,
        _mf: mf,
    };
    Some(Rig {
        encoder,
        script,
        texture,
    })
}

fn fed(frame: u64, discontinuity: bool) -> Fed {
    Fed {
        frame,
        discontinuity,
    }
}

// After a drain the encoder makes no request until it hears the stream has
// started again, and the first frame in says it follows a break.
#[test]
fn a_drained_encoder_is_started_again() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.encoder.drain_every_frame = true;
    let drained = rig.encode(1).unwrap_or_else(|e| panic!("frame 1: {e}"));
    assert_eq!(drained.data, coded(false, 1));
    assert_eq!(rig.script.starts(), 1);
    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(false, 2));
    assert_eq!(rig.script.starts(), 2, "once a drain");

    rig.encoder.drain_every_frame = false;
    rig.encode(3).unwrap_or_else(|e| panic!("frame 3: {e}"));
    rig.encode(4).unwrap_or_else(|e| panic!("frame 4: {e}"));
    assert_eq!(rig.script.starts(), 2, "no drain, no new start");
    assert_eq!(
        rig.script.fed(),
        [
            fed(0, false),
            fed(1, false),
            fed(2, true),
            fed(3, true),
            fed(4, false)
        ]
    );
}

// A request made before a drain still stands, and an encoder with one out
// need not make another.
#[test]
fn a_drain_keeps_the_request_out() {
    let Some(mut rig) = rig(Mode::AsksAhead) else {
        return;
    };
    let first = rig.encode(0).unwrap_or_else(|e| panic!("frame 0: {e}"));
    assert_eq!(first.data, coded(true, 0), "drained out after HELD");
    assert_eq!(rig.script.starts(), 1);
    assert_eq!(rig.script.state.borrow().pending, 1);

    rig.script.set_mode(Mode::Prompt);
    let next = rig.encode(1).unwrap_or_else(|e| panic!("frame 1: {e}"));
    assert_eq!(next.data, coded(false, 1));
    assert_eq!(rig.script.fed(), [fed(0, false), fed(1, true)]);
    assert_eq!(rig.script.refused(), 0);
}

// One that drops its requests with the drain refuses the frame the count here
// says it wants. The frame waits for a real request, and still says what it is.
#[test]
fn a_request_dropped_with_a_drain_is_waited_for() {
    let Some(mut rig) = rig(Mode::AsksAhead) else {
        return;
    };
    rig.script.state.borrow_mut().drain_drops_requests = true;
    let first = rig.encode(0).unwrap_or_else(|e| panic!("frame 0: {e}"));
    assert_eq!(first.data, coded(true, 0));

    rig.script.set_mode(Mode::Prompt);
    let next = rig.encode(1).unwrap_or_else(|e| panic!("frame 1: {e}"));
    assert_eq!(next.data, coded(false, 1));
    assert_eq!(rig.script.refused(), 1);
    assert_eq!(rig.script.fed(), [fed(0, false), fed(1, true)]);
    let asked = rig.script.state.borrow().asked_after_refusal;
    assert!(
        asked.is_some_and(|asked| next.submitted >= asked),
        "its time starts when it goes in"
    );
}

// An encoder that asks again on hearing the stream start, with a request out,
// has not asked ahead. Slow frames after that have the whole wait, so they are
// not drained and do not add up to an encoder that holds frames.
#[test]
fn a_request_made_again_after_a_drain_is_not_asking_ahead() {
    let Some(mut rig) = rig(Mode::AsksAhead) else {
        return;
    };
    rig.script.state.borrow_mut().asks_on_every_start = true;
    rig.first_frame();
    assert_eq!(rig.script.starts(), 1);

    rig.script.set_mode(Mode::Slow);
    for index in 1..=u64::from(HELD_IN_A_ROW) {
        let unit = rig
            .encode(index)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert_eq!(unit.data, coded(false, index));
        assert_eq!(rig.script.starts(), 1, "frame {index} was not drained");
    }
    assert!(!rig.encoder.drain_every_frame);
}

// Drained every frame, the requests it makes again do not add up.
#[test]
fn requests_made_again_do_not_add_up() {
    let Some(mut rig) = rig(Mode::AsksAhead) else {
        return;
    };
    rig.script.state.borrow_mut().asks_on_every_start = true;
    rig.encoder.drain_every_frame = true;
    for index in 0..8 {
        rig.encode(index)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert!(
            rig.encoder.wanted <= 1,
            "{} at frame {index}",
            rig.encoder.wanted
        );
    }
}

// A second refusal, with a real request in hand, is the encoder's failure.
#[test]
fn a_frame_refused_twice_fails() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    {
        let mut state = rig.script.state.borrow_mut();
        state.stopped = true;
        state.ask_when_refused = true;
    }
    match rig.encode(1) {
        Err(EncodeError::MediaFoundation { action, source }) => {
            assert_eq!(action, "feed a frame to the hardware encoder");
            assert_eq!(source.code(), MF_E_NOTACCEPTING);
        }
        Ok(_) => panic!("frame 1 came back"),
        Err(e) => panic!("frame 1: {e}"),
    }
    assert_eq!(rig.script.refused(), 2);
    assert!(rig.encoder.idr_next);
}

// A drain that finishes after encode() gave up on its frame is finished on the
// next call: the frame's output thrown away, the stream started again, and no
// frame fed before that, though a request is out from before the drain.
#[test]
fn a_late_drain_is_finished_before_the_next_frame() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::AsksAhead);
    rig.script.state.borrow_mut().drain_waits = true;
    rig.encoder.drain_every_frame = true;
    rig.give_up_on(1);
    assert_eq!(rig.script.starts(), 0);
    rig.encoder.drain_every_frame = false;
    rig.script.set_mode(Mode::Prompt);
    rig.script.finish_drain();

    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(true, 2), "not frame 1's, and an IDR");
    assert_eq!(rig.script.starts(), 1);
    assert_eq!(rig.script.refused(), 0);
    let after = rig.encode(3).unwrap_or_else(|e| panic!("frame 3: {e}"));
    assert_eq!(after.data, coded(false, 3));
    assert_eq!(
        rig.script.fed(),
        [fed(0, false), fed(1, false), fed(2, true), fed(3, false)]
    );
}

// A drain that empties the encoder with no frame out leaves nothing to wait
// for on the next call.
#[test]
fn a_drain_with_no_frame_out_owes_nothing() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.encoder.drain_every_frame = true;
    rig.script.state.borrow_mut().drain_waits = true;
    rig.give_up_on(1);
    assert!(rig.encoder.late_frame);
    rig.script.state.borrow_mut().held.clear();
    rig.script.raise(METransformDrainComplete.0);
    rig.encoder.drain_every_frame = false;
    rig.script.set_mode(Mode::Prompt);
    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(true, 2));
    assert!(!rig.encoder.late_frame);
}

// The request and the output of a frame encode() gave up on come in either
// order. With the request first, the output must not pass for the next frame's.
#[test]
fn a_late_output_after_the_request_is_not_the_next_frame() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.give_up_on(1);

    rig.script.set_mode(Mode::Prompt);
    rig.script.ask();
    rig.script.release();
    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(true, 2), "not frame 1's, and an IDR");
    assert_eq!(next.index, 2);
    assert!(next.idr);
    assert!(
        rig.encoder.notes().contains("Discarded the output"),
        "{}",
        rig.encoder.notes()
    );
    let after = rig.encode(3).unwrap_or_else(|e| panic!("frame 3: {e}"));
    assert_eq!(after.data, coded(false, 3));
}

// Parameter sets of the late frame in a sample of their own are not its output.
#[test]
fn a_late_frame_is_waited_for_past_its_parameter_sets() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.give_up_on(1);

    rig.script.set_mode(Mode::Prompt);
    rig.script.ask();
    rig.script.announce(vec![0, 0, 0, 1, 0x67, 1]);
    rig.script.release();
    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(true, 2));
}

// A drain the encoder refuses is not waited for, and the frame it was sent for
// is still to come out.
#[test]
fn a_refused_drain_leaves_its_frame_to_wait_for() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.script.state.borrow_mut().refuses_drain = true;
    rig.encoder.drain_every_frame = true;
    match rig.encode(1) {
        Err(EncodeError::MediaFoundation { action, .. }) => {
            assert_eq!(action, "drain the hardware encoder");
        }
        Ok(_) => panic!("frame 1 came back"),
        Err(e) => panic!("frame 1: {e}"),
    }
    rig.script.state.borrow_mut().refuses_drain = false;
    rig.encoder.drain_every_frame = false;

    rig.script.set_mode(Mode::Prompt);
    rig.script.ask();
    rig.script.release();
    let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
    assert_eq!(next.data, coded(true, 2), "not frame 1's, and an IDR");
    assert_eq!(rig.script.starts(), 0);
}

// An error from the encoder, in its event or in taking the output, says nothing
// of the frame still in it: a failed event, then an announced output not there.
#[test]
fn a_frame_out_when_an_error_comes_is_waited_for() {
    for status in [E_FAIL, S_OK] {
        let Some(mut rig) = rig(Mode::Prompt) else {
            return;
        };
        rig.first_frame();
        rig.script.set_mode(Mode::Silent);
        rig.script
            .events
            .send(Event::Raised {
                kind: METransformHaveOutput.0 as u32,
                status,
            })
            .expect("the encoder is open");
        match rig.encode(1) {
            Err(EncodeError::MediaFoundation { .. }) => {}
            Ok(_) => panic!("frame 1 came back"),
            Err(e) => panic!("frame 1: {e}"),
        }

        rig.script.set_mode(Mode::Prompt);
        rig.script.ask();
        rig.script.release();
        let next = rig.encode(2).unwrap_or_else(|e| panic!("frame 2: {e}"));
        assert_eq!(next.data, coded(true, 2), "not frame 1's, and an IDR");
    }
}

// With the late frame out and DrainComplete not, the error says what is missing.
#[test]
fn a_drain_that_never_finishes_says_so() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.script.state.borrow_mut().drain_waits = true;
    rig.encoder.drain_every_frame = true;
    rig.give_up_on(1);
    rig.encoder.drain_every_frame = false;

    rig.script.ask();
    rig.script.release();
    match rig.encode(2) {
        Err(EncodeError::EncoderMisbehaved { problem }) => {
            assert!(problem.contains("finished draining"), "{problem}");
        }
        Ok(_) => panic!("frame 2 came back"),
        Err(e) => panic!("frame 2: {e}"),
    }
    assert!(!rig.encoder.late_frame);
    assert_eq!(rig.script.fed().len(), 2);
}

// With the frame out, whether the drain then stalls or an error comes, what is
// missing is the drain. Queued ahead of the frame, these are what its wait reads.
#[test]
fn a_frame_out_of_an_unfinished_drain_is_not_late() {
    for then in [None, Some(E_FAIL), Some(S_OK)] {
        let Some(mut rig) = rig(Mode::Prompt) else {
            return;
        };
        rig.first_frame();
        rig.script.set_mode(Mode::Silent);
        rig.script.state.borrow_mut().drain_waits = true;
        rig.encoder.drain_every_frame = true;
        rig.script.announce(coded(false, 1));
        if let Some(status) = then {
            rig.script
                .events
                .send(Event::Raised {
                    kind: METransformHaveOutput.0 as u32,
                    status,
                })
                .expect("the encoder is open");
        }
        rig.encoder.output_wait = Duration::ZERO;
        assert!(rig.encode(1).is_err(), "frame 1 came back");
        assert!(!rig.encoder.late_frame);
        rig.encoder.drain_every_frame = false;

        match rig.encode(2) {
            Err(EncodeError::EncoderMisbehaved { problem }) => {
                assert!(problem.contains("finished draining"), "{problem}");
            }
            Ok(_) => panic!("frame 2 came back"),
            Err(e) => panic!("frame 2: {e}"),
        }
    }
}

// An encoder that keeps asking with the late frame out does not put off the
// limit on waiting for it.
#[test]
fn requests_do_not_put_off_the_wait_for_a_late_frame() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.give_up_on(1);

    let events = rig.script.events.clone();
    let started = Instant::now();
    thread::spawn(move || {
        while started.elapsed() < GIVE_UP * 4
            && events
                .send(Event::Raised {
                    kind: METransformNeedInput.0 as u32,
                    status: S_OK,
                })
                .is_ok()
        {
            thread::sleep(Duration::from_millis(50));
        }
    });
    match rig.encode(2) {
        Err(EncodeError::EncoderMisbehaved { problem }) => {
            assert!(problem.contains("still holds a frame"), "{problem}");
        }
        Ok(_) => panic!("frame 2 came back"),
        Err(e) => panic!("frame 2: {e}"),
    }
    assert!(started.elapsed() < GIVE_UP * 2, "{:?}", started.elapsed());
    assert_eq!(rig.script.fed().len(), 2);
}

// With a request in hand and the late frame not out, no frame goes in.
#[test]
fn no_frame_goes_in_while_a_late_one_is_out() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.set_mode(Mode::Silent);
    rig.give_up_on(1);

    rig.script.ask();
    let started = Instant::now();
    match rig.encode(2) {
        Err(EncodeError::EncoderMisbehaved { problem }) => {
            assert!(problem.contains("still holds a frame"), "{problem}");
        }
        Ok(_) => panic!("frame 2 came back"),
        Err(e) => panic!("frame 2: {e}"),
    }
    assert!(started.elapsed() >= GIVE_UP);
    assert_eq!(rig.script.fed().len(), 2);
}

// An encoder that takes the request and answers with something else leaves
// every viewer waiting with no error anywhere, so it goes the way of one that
// refuses. A request answered clears the count, and frames not asked for do not.
#[test]
fn ignored_idr_asks_fail_the_encoder() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.state.borrow_mut().ignores_idr_asks = true;
    for (index, force_idr) in [(1, true), (2, false), (3, true)] {
        let unit = rig
            .encode_with(index, force_idr)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert!(!unit.idr);
    }
    rig.script.state.borrow_mut().ignores_idr_asks = false;
    let answered = rig
        .encode_with(4, true)
        .unwrap_or_else(|e| panic!("frame 4: {e}"));
    assert!(answered.idr);

    rig.script.state.borrow_mut().ignores_idr_asks = true;
    let asks = rig.script.state.borrow().idr_asks;
    for index in [5, 6] {
        rig.encode_with(index, true)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
    }
    match rig.encode_with(7, true) {
        Err(EncodeError::EncoderMisbehaved { problem }) => {
            assert!(problem.contains("requests for an IDR"), "{problem}");
        }
        Ok(_) => panic!("frame 7 came back"),
        Err(e) => panic!("frame 7: {e}"),
    }
    assert!(rig.encoder.idr_next);
    assert_eq!(rig.script.state.borrow().idr_asks, asks + 3);
}

// The same with a request that stays set, as after a lost frame: asked on
// every frame, it fails on the third.
#[test]
fn a_recovery_the_encoder_ignores_fails_it() {
    let Some(mut rig) = rig(Mode::Prompt) else {
        return;
    };
    rig.first_frame();
    rig.script.state.borrow_mut().ignores_idr_asks = true;
    assert_eq!(rig.encoder.recover(0), Recovery::Idr);
    for index in [1, 2] {
        let unit = rig
            .encode(index)
            .unwrap_or_else(|e| panic!("frame {index}: {e}"));
        assert!(!unit.idr);
    }
    assert!(matches!(
        rig.encode(3),
        Err(EncodeError::EncoderMisbehaved { .. })
    ));
}
