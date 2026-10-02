// Every call into the Windows audio API (WASAPI and the device enumerator)
// is in this file, and all of Booth's unsafe audio code with it. Everything
// above it sees plain Rust types.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::slice;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName,
};
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, E_NOINTERFACE, E_POINTER, HANDLE, PROPERTYKEY, RPC_E_CHANGED_MODE,
    S_OK, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_E_ENGINE_PERIODICITY_LOCKED, AUDCLNT_E_RESOURCES_INVALIDATED,
    AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioCaptureClient, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, IMMEndpoint, IMMNotificationClient,
    IMMNotificationClient_Vtbl, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture,
    eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, PWSTR, w};

use super::devices::{Choice, DeviceList, Direction, Endpoint, Lists};
use super::error::AudioError;
use super::format::{Format, RATE, plan};
use super::microphone::hands_free;
use super::stream::{Device, DeviceStream, Packet, StreamInfo, Wake};

// HRESULT_FROM_WIN32(ERROR_NOT_FOUND): no default device, or no device with
// that id.
const E_NOTFOUND: HRESULT = HRESULT(0x8007_0490_u32 as i32);
// Windows counts stream times in 100 ns units.
const HNS_PER_SEC: u64 = 10_000_000;

// COM on the calling thread, in the multithreaded apartment, for as long as
// this lives. It stays on the thread that made it and is dropped after every
// COM pointer made under it.
struct Com {
    paired: bool,
    _thread: PhantomData<*const ()>,
}

impl Com {
    fn start() -> Result<Com, AudioError> {
        // SAFETY: no reserved pointer is passed. S_OK and S_FALSE are paired
        // with the CoUninitialize in drop. RPC_E_CHANGED_MODE means this
        // thread already has COM in a single-threaded apartment, which the
        // device API works in as well, and there is nothing to pair.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Com {
                paired: false,
                _thread: PhantomData,
            });
        }
        result
            .ok()
            .map_err(|err| windows_error("start COM for the sound devices", &err))?;
        Ok(Com {
            paired: true,
            _thread: PhantomData,
        })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.paired {
            // SAFETY: Com::start initialised COM on this thread, and the
            // owners of every COM pointer made under it drop them first.
            unsafe { CoUninitialize() };
        }
    }
}

// The device enumerator on one thread.
pub(crate) struct Session {
    enumerator: IMMDeviceEnumerator,
    // Last, so it is dropped after the enumerator.
    _com: Com,
}

impl Session {
    pub(crate) fn new() -> Result<Session, AudioError> {
        let com = Com::start()?;
        // SAFETY: COM is running on this thread (com above).
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|err| windows_error("reach the Windows sound devices", &err))?;
        Ok(Session {
            enumerator,
            _com: com,
        })
    }

    pub(crate) fn lists(&self) -> Result<Lists, AudioError> {
        Ok(Lists {
            inputs: self.list(Direction::Input)?,
            outputs: self.list(Direction::Output)?,
        })
    }

    pub(crate) fn list(&self, direction: Direction) -> Result<DeviceList, AudioError> {
        let default = self.default_id(direction)?;
        let endpoints = self.endpoints(direction)?;
        Ok(DeviceList::from_endpoints(endpoints, default.as_deref()))
    }

    fn endpoints(&self, direction: Direction) -> Result<Vec<Endpoint>, AudioError> {
        let step = || format!("list the {direction} devices");
        // SAFETY: a call on a live interface; the collection comes back with
        // its own reference.
        let collection = unsafe {
            self.enumerator
                .EnumAudioEndpoints(flow(direction), DEVICE_STATE_ACTIVE)
        }
        .map_err(|err| windows_error(&step(), &err))?;
        // SAFETY: a getter on a live interface.
        let count = unsafe { collection.GetCount() }.map_err(|err| windows_error(&step(), &err))?;
        let mut endpoints = Vec::with_capacity(count as usize);
        for index in 0..count {
            // SAFETY: index is below the count the collection gave.
            let device =
                unsafe { collection.Item(index) }.map_err(|err| windows_error(&step(), &err))?;
            let Some(id) = device_id(&device) else {
                continue;
            };
            endpoints.push(Endpoint {
                id,
                name: friendly_name(&device),
                active: state(&device) == Some(DEVICE_STATE_ACTIVE),
            });
        }
        Ok(endpoints)
    }

    pub(crate) fn default_id(&self, direction: Direction) -> Result<Option<String>, AudioError> {
        // SAFETY: a call on a live interface with two plain values.
        match unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(flow(direction), eConsole)
        } {
            Ok(device) => Ok(device_id(&device)),
            Err(err) if err.code() == E_NOTFOUND => Ok(None),
            Err(err) => Err(windows_error(
                &format!("find the default {direction} device"),
                &err,
            )),
        }
    }

    // The device a choice names, with its id and name, if it can be opened.
    fn find(
        &self,
        direction: Direction,
        choice: &Choice,
    ) -> Result<(IMMDevice, String, String), AudioError> {
        let device = match choice {
            Choice::Default => {
                // SAFETY: as in default_id.
                match unsafe {
                    self.enumerator
                        .GetDefaultAudioEndpoint(flow(direction), eConsole)
                } {
                    Ok(device) => device,
                    Err(err) if err.code() == E_NOTFOUND => {
                        return Err(AudioError::NoDevice(direction));
                    }
                    Err(err) => {
                        return Err(windows_error(
                            &format!("find the default {direction} device"),
                            &err,
                        ));
                    }
                }
            }
            Choice::Device(id) => {
                let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
                let missing = |name| AudioError::NotConnected { direction, name };
                // SAFETY: `wide` is NUL-terminated and outlives the call.
                let Ok(device) = (unsafe { self.enumerator.GetDevice(PCWSTR(wide.as_ptr())) })
                else {
                    return Err(missing(None));
                };
                let same_way = endpoint_flow(&device) == Some(flow(direction));
                if !same_way || state(&device) != Some(DEVICE_STATE_ACTIVE) {
                    return Err(missing(friendly_name(&device)));
                }
                device
            }
        };
        let id = device_id(&device).unwrap_or_default();
        let name = friendly_name(&device).unwrap_or_else(|| String::from("the sound device"));
        Ok((device, id, name))
    }

    pub(crate) fn probe(&self, direction: Direction, choice: &Choice) -> Result<Probe, AudioError> {
        let (device, id, name) = self.find(direction, choice)?;
        let fail = |step: &'static str| {
            let name = name.clone();
            move |err: windows::core::Error| classify(&err, direction, &name, step)
        };
        let client: IAudioClient = activate(&device).map_err(fail("open"))?;
        let (engine, engine_bytes) = mix_format(&client, direction, &name)?;
        let mut device_default = 0i64;
        let mut device_min = 0i64;
        // SAFETY: two locals for the call to fill.
        unsafe { client.GetDevicePeriod(Some(&mut device_default), Some(&mut device_min)) }
            .map_err(fail("ask for the period of"))?;
        let (engine_periods, engine_running) = match activate::<IAudioClient3>(&device) {
            Ok(client3) => (
                engine_periods(&client3, &engine_bytes).ok(),
                current_period(&client3),
            ),
            Err(_) => (None, None),
        };
        let plan = plan(&engine);
        let period = match engine_periods {
            Some(periods) if !plan.resampled => {
                frames_time(small_period(periods, engine_running), engine.rate)
            }
            _ => hns_time(device_default),
        };
        Ok(Probe {
            direction,
            device: name,
            id,
            engine,
            hands_free: is_hands_free(&device),
            resampled: plan.resampled,
            period,
            engine_periods,
            engine_running,
            device_default_period: hns_time(device_default),
            device_min_period: hns_time(device_min),
        })
    }

    fn open(&self, direction: Direction, choice: &Choice) -> Result<WasapiStream, AudioError> {
        let (device, id, name) = self.find(direction, choice)?;
        let fail = |step: &'static str| {
            let name = name.clone();
            move |err: windows::core::Error| classify(&err, direction, &name, step)
        };
        let client: IAudioClient = activate(&device).map_err(fail("open"))?;
        let (engine, engine_bytes) = mix_format(&client, direction, &name)?;
        drop(client);
        let plan = plan(&engine);
        // At 48 kHz the engine's own bytes, exactly as Windows gave them.
        let wave_bytes = if plan.resampled {
            plan.format.to_bytes().to_vec()
        } else {
            engine_bytes
        };
        let wave = wave_bytes.as_ptr().cast::<WAVEFORMATEX>();

        // The small period first. It needs the engine's own format, so only
        // when that is already 48 kHz.
        let small = if plan.resampled {
            None
        } else {
            activate::<IAudioClient3>(&device)
                .ok()
                .and_then(|client3| init_small(&device, client3, &wave_bytes))
        };
        let (client, period_frames, small_period) = match small {
            Some((client3, frames)) => {
                let client: IAudioClient = client3.cast().map_err(fail("open"))?;
                (client, frames, true)
            }
            None => {
                // A fresh client: one that refused the small period is left
                // in a state Initialize may not accept.
                let client: IAudioClient = activate(&device).map_err(fail("open"))?;
                let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
                if plan.resampled {
                    flags |= AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
                }
                // SAFETY: `wave` points into `wave_bytes`, a whole format
                // that lives until the end of this function. Zero buffer and
                // period ask for the default in shared mode.
                unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, wave, None) }
                    .map_err(fail("open"))?;
                let mut default = 0i64;
                // SAFETY: one local for the call to fill.
                unsafe { client.GetDevicePeriod(Some(&mut default), None) }
                    .map_err(fail("ask for the period of"))?;
                (client, hns_frames(default), false)
            }
        };

        let signal = Signal::new().map_err(fail("make an event for"))?;
        // SAFETY: the event stays open for as long as the client lives:
        // both are in the stream, and the stream drops the client first.
        unsafe { client.SetEventHandle(signal.0) }.map_err(fail("open"))?;
        // SAFETY: getters on an initialized client.
        let buffer_frames = unsafe { client.GetBufferSize() }.map_err(fail("open"))?;
        // SAFETY: as above.
        let latency = unsafe { client.GetStreamLatency() }.map_err(fail("open"))?;
        let io = match direction {
            // SAFETY: GetService on an initialized client; the service holds
            // its own reference.
            Direction::Input => Io::Capture(unsafe { client.GetService() }.map_err(fail("open"))?),
            // SAFETY: as above.
            Direction::Output => Io::Render(unsafe { client.GetService() }.map_err(fail("open"))?),
        };
        Ok(WasapiStream {
            frame_bytes: plan.format.frame_bytes(),
            info: StreamInfo {
                direction,
                device: name,
                id,
                format: plan.format,
                engine,
                hands_free: is_hands_free(&device),
                period: frames_time(period_frames, RATE),
                period_frames,
                resampled: plan.resampled,
                small_period,
                stream_latency: hns_time(latency),
                buffer_frames,
                pro_audio: false,
            },
            io,
            client,
            signal,
        })
    }

    // Calls `notify` from a Windows thread whenever a device is added,
    // removed, turned on or off, renamed, or becomes the default.
    pub(crate) fn watch(
        &self,
        notify: impl Fn(Notice) + Send + Sync + 'static,
    ) -> Result<Registration, AudioError> {
        let client = notification_client(Box::new(notify));
        // SAFETY: a call on a live interface; the enumerator keeps its own
        // reference to the client until it is unregistered in drop.
        unsafe {
            self.enumerator
                .RegisterEndpointNotificationCallback(&client)
        }
        .map_err(|err| windows_error("watch the sound devices", &err))?;
        Ok(Registration {
            enumerator: self.enumerator.clone(),
            client,
        })
    }
}

// What a device would give a stream, asked without opening one.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    pub direction: Direction,
    pub device: String,
    pub id: String,
    pub engine: Format,
    // A Bluetooth headset in hands-free mode, from the endpoint's properties.
    pub hands_free: bool,
    pub resampled: bool,
    // The period a stream would run at.
    pub period: Duration,
    // From IAudioClient3, in frames at the engine rate.
    pub engine_periods: Option<EnginePeriods>,
    // What the engine runs at now, in frames at the engine rate.
    pub engine_running: Option<u32>,
    pub device_default_period: Duration,
    pub device_min_period: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnginePeriods {
    pub default: u32,
    pub fundamental: u32,
    pub min: u32,
    pub max: u32,
}

// `wave` is a whole format as Windows lays it out, parsed by Format::parse
// or made by Format::to_bytes.
fn engine_periods(client: &IAudioClient3, wave: &[u8]) -> windows::core::Result<EnginePeriods> {
    let mut periods = EnginePeriods {
        default: 0,
        fundamental: 0,
        min: 0,
        max: 0,
    };
    // SAFETY: `wave` is a whole format that outlives the call, and the four
    // outputs are fields of a local.
    unsafe {
        client.GetSharedModeEnginePeriod(
            wave.as_ptr().cast::<WAVEFORMATEX>(),
            &mut periods.default,
            &mut periods.fundamental,
            &mut periods.min,
            &mut periods.max,
        )
    }?;
    Ok(periods)
}

// The smallest period the driver allows. When another program already runs
// the engine at another period, Windows will not change it under that
// program, so the stream takes the period that is running.
fn init_small(
    device: &IMMDevice,
    client: IAudioClient3,
    wave_bytes: &[u8],
) -> Option<(IAudioClient3, u32)> {
    let periods = engine_periods(&client, wave_bytes).ok()?;
    let wave = wave_bytes.as_ptr().cast::<WAVEFORMATEX>();
    // SAFETY: `wave` points into the caller's whole format, alive for this
    // call.
    let first = unsafe {
        client.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            periods.min,
            wave,
            None,
        )
    };
    let client = match first {
        Ok(()) => client,
        Err(err) if err.code() == AUDCLNT_E_ENGINE_PERIODICITY_LOCKED => {
            let running = current_period(&client)?;
            let client: IAudioClient3 = activate(device).ok()?;
            // SAFETY: as above.
            unsafe {
                client.InitializeSharedAudioStream(
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    running,
                    wave,
                    None,
                )
            }
            .ok()?;
            client
        }
        Err(_) => return None,
    };
    let period = current_period(&client)?;
    Some((client, period))
}

// The period init_small would get, asked before opening anything. The engine
// runs at its default until a program asks for a smaller period; a period
// other than the default means one has, and a new stream gets that one.
fn small_period(periods: EnginePeriods, running: Option<u32>) -> u32 {
    match running {
        Some(running) if running != periods.default => running,
        _ => periods.min,
    }
}

fn current_period(client: &IAudioClient3) -> Option<u32> {
    let mut format: *mut WAVEFORMATEX = ptr::null_mut();
    let mut frames = 0u32;
    // SAFETY: two locals for the call to fill. The format it hands back is
    // ours to free, and is freed at once.
    let result = unsafe { client.GetCurrentSharedModeEnginePeriod(&mut format, &mut frames) };
    // SAFETY: allocated by COM for us, or null, which CoTaskMemFree takes.
    unsafe { CoTaskMemFree(Some(format as *const c_void)) };
    result.ok()?;
    (frames > 0).then_some(frames)
}

// The engine's own format, as a Format and as the bytes Windows gave.
fn mix_format(
    client: &IAudioClient,
    direction: Direction,
    name: &str,
) -> Result<(Format, Vec<u8>), AudioError> {
    // SAFETY: a getter on a live, not yet initialized client. The format is
    // allocated by COM for us and freed below, once copied.
    let wave = unsafe { client.GetMixFormat() }
        .map_err(|err| classify(&err, direction, name, "ask for the format of"))?;
    // SAFETY: GetMixFormat returns a whole WAVEFORMATEX, and cbSize says how
    // many bytes follow its 18, except for plain PCM, where Windows ignores
    // cbSize and so may leave anything in it.
    let mut bytes = unsafe {
        let tag = ptr::addr_of!((*wave).wFormatTag).read_unaligned();
        let extra = if u32::from(tag) == WAVE_FORMAT_PCM {
            0
        } else {
            usize::from(ptr::addr_of!((*wave).cbSize).read_unaligned())
        };
        slice::from_raw_parts(wave.cast::<u8>(), 18 + extra).to_vec()
    };
    if u32::from(u16::from_le_bytes([bytes[0], bytes[1]])) == WAVE_FORMAT_PCM {
        bytes[16..18].fill(0);
    }
    // SAFETY: as above; nothing reads it after this.
    unsafe { CoTaskMemFree(Some(wave as *const c_void)) };
    let format = Format::parse(&bytes).map_err(|error| AudioError::Format {
        direction,
        name: name.to_owned(),
        error,
    })?;
    Ok((format, bytes))
}

fn activate<T: Interface>(device: &IMMDevice) -> windows::core::Result<T> {
    // SAFETY: a call on a live interface; the new one comes back with its
    // own reference.
    unsafe { device.Activate(CLSCTX_ALL, None) }
}

fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: a getter on a live interface; the string is ours to free.
    let id = unsafe { device.GetId() }.ok()?;
    take_string(id)
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    string_property(device, &PKEY_Device_FriendlyName)
}

// The endpoint store carries the enumerator of the device behind it, so no
// device node has to be looked up.
fn is_hands_free(device: &IMMDevice) -> bool {
    let enumerator = string_property(device, &PKEY_Device_EnumeratorName);
    hands_free(enumerator.as_deref(), friendly_name(device).as_deref())
}

fn string_property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: a getter on a live interface.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    // SAFETY: the key outlives the call; the value is ours to clear.
    let mut value = unsafe { store.GetValue(key) }.ok()?;
    // SAFETY: `value` is the PROPVARIANT just filled in; the string that
    // comes back is ours to free.
    let text = unsafe { PropVariantToStringAlloc(&value) };
    // SAFETY: cleared once, after the last read.
    let _ = unsafe { PropVariantClear(&mut value) };
    take_string(text.ok()?)
}

// Copies a string COM allocated for us, and frees it.
fn take_string(text: PWSTR) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string from COM, read before it is freed.
    let copy = unsafe { text.to_string() }.ok();
    // SAFETY: allocated by COM for the caller; freed once.
    unsafe { CoTaskMemFree(Some(text.0 as *const c_void)) };
    copy
}

fn state(device: &IMMDevice) -> Option<DEVICE_STATE> {
    // SAFETY: a getter on a live interface.
    unsafe { device.GetState() }.ok()
}

fn endpoint_flow(device: &IMMDevice) -> Option<EDataFlow> {
    let endpoint: IMMEndpoint = device.cast().ok()?;
    // SAFETY: a getter on a live interface.
    unsafe { endpoint.GetDataFlow() }.ok()
}

fn flow(direction: Direction) -> EDataFlow {
    match direction {
        Direction::Input => eCapture,
        Direction::Output => eRender,
    }
}

fn hns_time(hns: i64) -> Duration {
    Duration::from_nanos(u64::try_from(hns).unwrap_or(0) * 100)
}

// A device period, from 100 ns units to frames of the 48 kHz stream.
fn hns_frames(hns: i64) -> u32 {
    let hns = u64::try_from(hns).unwrap_or(0);
    ((hns * u64::from(RATE) + HNS_PER_SEC / 2) / HNS_PER_SEC) as u32
}

fn frames_time(frames: u32, rate: u32) -> Duration {
    Duration::from_secs_f64(f64::from(frames) / f64::from(rate))
}

fn classify(
    err: &windows::core::Error,
    direction: Direction,
    name: &str,
    step: &str,
) -> AudioError {
    let code = err.code();
    if code == E_ACCESSDENIED {
        AudioError::AccessDenied(direction)
    } else if code == AUDCLNT_E_DEVICE_INVALIDATED || code == AUDCLNT_E_RESOURCES_INVALIDATED {
        AudioError::Lost {
            direction,
            name: name.to_owned(),
        }
    } else if code == AUDCLNT_E_DEVICE_IN_USE {
        AudioError::InUse {
            direction,
            name: name.to_owned(),
        }
    } else if code == AUDCLNT_E_SERVICE_NOT_RUNNING {
        AudioError::ServiceStopped(direction)
    } else {
        windows_error(&format!("{step} {} ({name})", direction.noun()), err)
    }
}

fn windows_error(step: &str, err: &windows::core::Error) -> AudioError {
    let text = err.message();
    let text = text.trim().trim_end_matches('.');
    AudioError::Windows {
        step: step.to_owned(),
        code: err.code().0 as u32,
        text: if text.is_empty() {
            String::from("Windows gave no reason")
        } else {
            text.to_owned()
        },
    }
}

// An auto-reset event the audio engine sets once per period.
struct Signal(HANDLE);

impl Signal {
    fn new() -> windows::core::Result<Signal> {
        // SAFETY: no attributes and no name; the handle is closed in drop.
        unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map(Signal)
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: created in Signal::new and closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

enum Io {
    Capture(IAudioCaptureClient),
    Render(IAudioRenderClient),
}

pub(crate) struct WasapiStream {
    info: StreamInfo,
    frame_bytes: usize,
    // Dropped in this order: the service, the client, then the event the
    // client was told to set.
    io: Io,
    client: IAudioClient,
    signal: Signal,
}

impl WasapiStream {
    fn fail(&self, step: &'static str) -> impl Fn(windows::core::Error) -> AudioError + '_ {
        move |err| classify(&err, self.info.direction, &self.info.device, step)
    }
}

impl DeviceStream for WasapiStream {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn start(&mut self) -> Result<(), AudioError> {
        // SAFETY: a call on an initialized client.
        unsafe { self.client.Start() }.map_err(self.fail("start"))
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: the event is open for as long as the stream lives.
        let result = unsafe { WaitForSingleObject(self.signal.0, ms) };
        if result == WAIT_OBJECT_0 {
            Ok(true)
        } else if result == WAIT_TIMEOUT {
            Ok(false)
        } else {
            Err(self.fail("wait for")(windows::core::Error::from_thread()))
        }
    }

    fn read(&mut self, packet: &mut dyn FnMut(Packet<'_>)) -> Result<(), AudioError> {
        let Io::Capture(capture) = &self.io else {
            return Ok(());
        };
        loop {
            // SAFETY: a getter on a started capture service.
            let next = unsafe { capture.GetNextPacketSize() }.map_err(self.fail("read"))?;
            if next == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            // SAFETY: four locals for the call to fill; the device position
            // is not asked for.
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .map_err(self.fail("read"))?;
            // AUDCLNT_S_BUFFER_EMPTY: nothing was handed out, so nothing is
            // released.
            if frames == 0 {
                return Ok(());
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            let bytes: &[u8] = if data.is_null() {
                &[]
            } else {
                // SAFETY: GetBuffer handed out `frames` whole frames at
                // `data`, valid until the ReleaseBuffer below.
                unsafe { slice::from_raw_parts(data, frames as usize * self.frame_bytes) }
            };
            let time_ok = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0;
            packet(Packet {
                data: bytes,
                frames,
                time: time_ok.then(|| qpc_instant(qpc)),
                silent,
                glitch: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
            });
            // SAFETY: gives back exactly what GetBuffer handed out.
            unsafe { capture.ReleaseBuffer(frames) }.map_err(self.fail("read"))?;
        }
    }

    fn queued(&mut self) -> Result<u32, AudioError> {
        // SAFETY: a getter on an initialized client.
        unsafe { self.client.GetCurrentPadding() }.map_err(self.fail("write to"))
    }

    fn check(&mut self) -> Result<(), AudioError> {
        let step = match self.info.direction {
            Direction::Input => "read",
            Direction::Output => "write to",
        };
        // SAFETY: a getter on an initialized client. It works on capture
        // and render clients alike, and fails with AUDCLNT_E_DEVICE_INVALIDATED
        // once the device is gone.
        unsafe { self.client.GetCurrentPadding() }
            .map(drop)
            .map_err(self.fail(step))
    }

    fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), AudioError> {
        let Io::Render(render) = &self.io else {
            return Ok(());
        };
        if frames == 0 {
            return Ok(());
        }
        // SAFETY: the caller asks for no more than the free space, which
        // GetBuffer checks as well.
        let data = unsafe { render.GetBuffer(frames) }.map_err(self.fail("write to"))?;
        // SAFETY: GetBuffer handed out room for `frames` whole frames at
        // `data`, ours until the ReleaseBuffer below.
        let bytes = unsafe { slice::from_raw_parts_mut(data, frames as usize * self.frame_bytes) };
        fill(bytes);
        // SAFETY: gives back exactly what GetBuffer handed out, all written.
        unsafe { render.ReleaseBuffer(frames, 0) }.map_err(self.fail("write to"))
    }

    fn stop(&mut self) {
        // SAFETY: a call on an initialized client. A device that has gone
        // away refuses, and there is nothing left to stop then.
        let _ = unsafe { self.client.Stop() };
    }
}

// The device the stream threads use in the app. Made on the stream's own
// thread, which is where its COM pointers have to stay.
pub(crate) struct Windows {
    // Dropped first: unregistered while the enumerator and COM are there.
    _watch: Option<Registration>,
    session: Session,
    direction: Direction,
}

pub(crate) fn device(direction: Direction, wake: Wake) -> Result<Windows, AudioError> {
    let session = Session::new()?;
    // Without the watch the stream still runs; it only stays on the old
    // default until it is opened again.
    let watch = session
        .watch(move |notice| {
            if let Notice::DefaultChanged {
                direction: which,
                id,
            } = notice
                && which == direction
            {
                wake.default_changed(id);
            }
        })
        .ok();
    Ok(Windows {
        _watch: watch,
        session,
        direction,
    })
}

impl Device for Windows {
    type Stream = WasapiStream;

    fn open(&mut self, choice: &Choice) -> Result<WasapiStream, AudioError> {
        self.session.open(self.direction, choice)
    }
}

pub(crate) enum Notice {
    DefaultChanged {
        direction: Direction,
        id: Option<String>,
    },
    DevicesChanged,
}

// The notification client, written out by hand. A COM object is a pointer to
// a table of functions, followed by whatever the object keeps. The windows
// crate's implement macro writes the same thing, but only for crates that
// name windows-core directly, and this one keeps to the windows crate.
#[repr(C)]
struct Notifications {
    // First: COM finds the table at the address of the object.
    vtable: *const IMMNotificationClient_Vtbl,
    refs: AtomicU32,
    notify: Box<dyn Fn(Notice) + Send + Sync>,
}

static NOTIFICATIONS: IMMNotificationClient_Vtbl = IMMNotificationClient_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: query_interface,
        AddRef: add_ref,
        Release: release,
    },
    OnDeviceStateChanged: device_state_changed,
    OnDeviceAdded: device_added,
    OnDeviceRemoved: device_removed,
    OnDefaultDeviceChanged: default_device_changed,
    OnPropertyValueChanged: property_value_changed,
};

fn notification_client(notify: Box<dyn Fn(Notice) + Send + Sync>) -> IMMNotificationClient {
    let object = Box::new(Notifications {
        vtable: &NOTIFICATIONS,
        refs: AtomicU32::new(1),
        notify,
    });
    // SAFETY: the object starts with its table and holds one reference,
    // which the interface takes over. release frees it when the last
    // reference goes, however many Windows took in between.
    unsafe { IMMNotificationClient::from_raw(Box::into_raw(object).cast()) }
}

// The functions below are called by Windows, on its own threads, only
// through a pointer notification_client made, and each call holds a
// reference while it runs, so the object is alive for its length. None of
// them blocks: each one only passes a message on.

// SAFETY (caller): `this` is a pointer notification_client made, with a
// reference held for as long as the result is used.
unsafe fn object<'a>(this: *mut c_void) -> &'a Notifications {
    // SAFETY: as the caller promises.
    unsafe { &*this.cast::<Notifications>() }
}

unsafe extern "system" fn query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // SAFETY: both pointers were checked above, and COM passes them valid.
    unsafe {
        let iid = *iid;
        if iid == IUnknown::IID || iid == IMMNotificationClient::IID {
            add_ref(this);
            *out = this;
            S_OK
        } else {
            *out = ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: see the note above object.
    unsafe { object(this) }.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: see the note above object.
    let left = unsafe { object(this) }.refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        // Every earlier use of the object happens before it is freed.
        fence(Ordering::Acquire);
        // SAFETY: the last reference is gone, so nothing can reach the
        // object any more; it came from Box::into_raw above.
        drop(unsafe { Box::from_raw(this.cast::<Notifications>()) });
    }
    left
}

unsafe extern "system" fn device_state_changed(
    this: *mut c_void,
    _: PCWSTR,
    _: DEVICE_STATE,
) -> HRESULT {
    // SAFETY: see the note above object.
    (unsafe { object(this) }.notify)(Notice::DevicesChanged);
    S_OK
}

unsafe extern "system" fn device_added(this: *mut c_void, _: PCWSTR) -> HRESULT {
    // SAFETY: see the note above object.
    (unsafe { object(this) }.notify)(Notice::DevicesChanged);
    S_OK
}

unsafe extern "system" fn device_removed(this: *mut c_void, _: PCWSTR) -> HRESULT {
    // SAFETY: see the note above object.
    (unsafe { object(this) }.notify)(Notice::DevicesChanged);
    S_OK
}

unsafe extern "system" fn default_device_changed(
    this: *mut c_void,
    flow: EDataFlow,
    role: ERole,
    id: PCWSTR,
) -> HRESULT {
    // The same change comes once for each role; the console role is the
    // one the sound settings page shows as the default.
    if role != eConsole {
        return S_OK;
    }
    let direction = if flow == eCapture {
        Direction::Input
    } else if flow == eRender {
        Direction::Output
    } else {
        return S_OK;
    };
    let id = if id.is_null() {
        None
    } else {
        // SAFETY: Windows passes a NUL-terminated id that lives for the
        // length of this call.
        unsafe { id.to_string() }.ok()
    };
    // SAFETY: see the note above object.
    (unsafe { object(this) }.notify)(Notice::DefaultChanged { direction, id });
    S_OK
}

unsafe extern "system" fn property_value_changed(
    this: *mut c_void,
    _: PCWSTR,
    key: PROPERTYKEY,
) -> HRESULT {
    // Drivers change other properties often; only a new name matters here.
    if key == PKEY_Device_FriendlyName {
        // SAFETY: see the note above object.
        (unsafe { object(this) }.notify)(Notice::DevicesChanged);
    }
    S_OK
}

pub(crate) struct Registration {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: registered in Session::watch on this enumerator. Windows
        // waits for any call in progress before this returns.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}

// The "Pro Audio" scheduling class for the calling thread, until dropped.
// Windows then runs the thread ahead of normal work, so a busy game does
// not make it late.
pub(crate) struct Raised(HANDLE);

pub(crate) fn raise_thread() -> Option<Raised> {
    let mut index = 0u32;
    // SAFETY: a static NUL-terminated task name and a local for the index.
    // The handle is reverted in drop on the same thread, since Raised holds
    // a raw handle and cannot be sent elsewhere.
    unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }
        .ok()
        .map(Raised)
}

impl Drop for Raised {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this
        // thread and is reverted once.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

// GetBuffer's timestamp is the performance counter in 100 ns units. Rust's
// Instant on Windows reads the same counter, so the capture time is now
// less how long ago the counter stood there, with no second clock involved.
fn qpc_instant(position: u64) -> Instant {
    let now = Instant::now();
    let mut counter = 0i64;
    // SAFETY: writes one i64 into a local.
    if unsafe { QueryPerformanceCounter(&mut counter) }.is_err() {
        return now;
    }
    let Some(frequency) = qpc_frequency() else {
        return now;
    };
    let now_hns =
        (u128::from(counter as u64) * u128::from(HNS_PER_SEC) / u128::from(frequency)) as u64;
    let age = Duration::from_nanos(now_hns.saturating_sub(position).saturating_mul(100));
    now.checked_sub(age).unwrap_or(now)
}

// Fixed at boot, so asked once.
fn qpc_frequency() -> Option<u64> {
    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut frequency = 0i64;
        // SAFETY: writes one i64 into a local.
        match unsafe { QueryPerformanceFrequency(&mut frequency) } {
            Ok(()) => u64::try_from(frequency).unwrap_or(0),
            Err(_) => 0,
        }
    });
    (frequency > 0).then_some(frequency)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The performance counter now, in the 100 ns units GetBuffer uses.
    fn qpc_now() -> u64 {
        let mut counter = 0i64;
        // SAFETY: writes one i64 into a local.
        unsafe { QueryPerformanceCounter(&mut counter) }.expect("read the performance counter");
        let frequency = qpc_frequency().expect("the counter's frequency");
        (u128::from(counter as u64) * u128::from(HNS_PER_SEC) / u128::from(frequency)) as u64
    }

    #[test]
    fn a_capture_timestamp_becomes_the_instant_it_names() {
        let position = qpc_now();
        let now = Instant::now();
        let at = qpc_instant(position);
        let off = now.max(at) - now.min(at);
        assert!(off < Duration::from_millis(1), "{off:?}");
        // 10 ms earlier on the counter is 10 ms earlier as an Instant.
        let earlier = qpc_instant(position - 100_000);
        let step = at - earlier;
        assert!(
            step.abs_diff(Duration::from_millis(10)) < Duration::from_millis(1),
            "{step:?}"
        );
        // A timestamp from the future, which a driver should never give, is
        // taken as now rather than wrapped around.
        let ahead = qpc_instant(qpc_now() + HNS_PER_SEC);
        assert!(ahead - now < Duration::from_millis(100), "{ahead:?}");
    }

    #[test]
    fn smallest_period_unless_another_is_held() {
        let periods = EnginePeriods {
            default: 480,
            fundamental: 32,
            min: 128,
            max: 480,
        };
        assert_eq!(small_period(periods, None), 128);
        // Nobody asked for a small period: the engine idles at its default.
        assert_eq!(small_period(periods, Some(480)), 128);
        assert_eq!(small_period(periods, Some(128)), 128);
        // Another program runs the engine at 256 frames, which Windows keeps
        // for as long as that program's stream runs.
        assert_eq!(small_period(periods, Some(256)), 256);
    }
}
