// PC sound from Windows' process loopback: the sound every program on this PC
// plays except this process and the ones it started, as one stream that
// follows no device, so it has no endpoint watch and never switches. The
// activation is asynchronous: the handler only says it is done, and the
// capture thread waits for that, at most ACTIVATION_WAIT, and makes the
// client itself.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Vtbl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CoTaskMemAlloc, IAgileObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use super::{Com, Io, Signal, WasapiStream, format_refused, frames_time, hns_frames, hns_time};
use crate::audio::devices::{Choice, Direction};
use crate::audio::echo::{Echo, Effects};
use crate::audio::error::AudioError;
use crate::audio::format::{Format, RATE};
use crate::audio::stream::{Device, Refused, StreamInfo};

// How long Windows has to finish an activation. A warm audio service answers
// in milliseconds; past this something is wrong, and whatever Windows still
// holds of the handler stays alive until it lets go.
const ACTIVATION_WAIT: Duration = Duration::from_secs(5);
// How often that wait looks whether the stream was asked to stop.
const STOP_LOOK: Duration = Duration::from_millis(20);
// Front left and front right.
const STEREO: u32 = 0x3;

// PC sound's device: COM on the stream's thread, and the flag the owner sets
// when it stops the stream, so an activation in progress ends at once.
pub(crate) struct PcSoundDevice {
    stopping: Arc<AtomicBool>,
    _com: Com,
}

pub(crate) fn device(stopping: Arc<AtomicBool>) -> Result<PcSoundDevice, AudioError> {
    Ok(PcSoundDevice {
        stopping,
        _com: Com::start()?,
    })
}

impl Device for PcSoundDevice {
    type Stream = WasapiStream;

    fn open(&mut self, _: &Choice) -> Result<WasapiStream, AudioError> {
        open(&self.stopping)
    }
}

// What Initialize answered: the format refused, or anything else.
enum Attempt {
    Refused(windows::core::Error),
    Failed(AudioError),
}

// Float at 48 kHz in stereo, Windows converting whatever the programs play to
// it, at its better resampler. Nobody ships that last flag with process
// loopback, so a refused format is asked once more without it, on a fresh
// client, as Initialize's page wants after a refusal.
fn open(stopping: &AtomicBool) -> Result<WasapiStream, AudioError> {
    let format = Format::float_48k(2, STEREO);
    let signal = Signal::new().map_err(|err| classify(&err, false))?;
    let (client, refused) = match attempt(stopping, &format, true, &signal) {
        Ok(client) => (client, None),
        Err(Attempt::Refused(first)) => match attempt(stopping, &format, false, &signal) {
            Ok(client) => (
                client,
                Some(Refused {
                    engine: format,
                    code: first.code().0,
                }),
            ),
            Err(Attempt::Refused(_)) => {
                return Err(AudioError::PcSound {
                    code: first.code().0 as u32,
                    text: String::from("Windows would not give its sound as 48 kHz stereo"),
                    lost: false,
                });
            }
            Err(Attempt::Failed(err)) => return Err(err),
        },
        Err(Attempt::Failed(err)) => return Err(err),
    };
    // SAFETY: getters and GetService on an initialized client; the service
    // holds its own reference.
    let (buffer_frames, capture) = unsafe {
        (
            client
                .GetBufferSize()
                .map_err(|err| classify(&err, false))?,
            client
                .GetService::<IAudioCaptureClient>()
                .map_err(|err| classify(&err, false))?,
        )
    };
    // Neither is promised on this client: Windows names only GetMixFormat and
    // IsFormatSupported as missing. A period it does not give is 0.
    let mut period = 0i64;
    // SAFETY: one local for the call to fill.
    let period_frames = match unsafe { client.GetDevicePeriod(Some(&mut period), None) } {
        Ok(()) => hns_frames(period),
        Err(_) => 0,
    };
    // SAFETY: a getter on an initialized client.
    let latency = unsafe { client.GetStreamLatency() }.unwrap_or(0);
    Ok(WasapiStream {
        frame_bytes: format.frame_bytes(),
        info: StreamInfo {
            direction: Direction::Input,
            device: String::from("PC sound"),
            // SAFETY: a constant, NUL-terminated wide string.
            id: unsafe { VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK.to_string() }.unwrap_or_default(),
            format,
            engine: format,
            hands_free: false,
            period: frames_time(period_frames, RATE),
            period_frames,
            resampled: false,
            small_period: false,
            stream_latency: hns_time(latency),
            buffer_frames,
            pro_audio: false,
            refused,
            effects: Effects::default(),
            echo: Echo::NotAsked,
            opened_in: Duration::ZERO,
        },
        io: Io::Capture(capture),
        client,
        signal,
        pc_sound: true,
    })
}

fn attempt(
    stopping: &AtomicBool,
    format: &Format,
    quality: bool,
    signal: &Signal,
) -> Result<IAudioClient, Attempt> {
    let client = activate(stopping).map_err(Attempt::Failed)?;
    let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    if quality {
        flags |= AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    }
    let bytes = format.to_bytes();
    let wave = bytes.as_ptr().cast::<WAVEFORMATEX>();
    // SAFETY: `wave` points into `bytes`, a whole format alive for the call.
    // An event-driven shared stream passes 0 for its buffer and period.
    match unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, wave, None) } {
        Ok(()) => {}
        Err(err) if format_refused(err.code()) => return Err(Attempt::Refused(err)),
        Err(err) => return Err(Attempt::Failed(classify(&err, false))),
    }
    // SAFETY: the event outlives the client, which WasapiStream drops first.
    // Set before Start, or a stream released without one makes Windows wait.
    unsafe { client.SetEventHandle(signal.0) }
        .map_err(|err| Attempt::Failed(classify(&err, false)))?;
    Ok(client)
}

// Asks Windows for a loopback client that leaves this process's tree out, and
// waits for it.
fn activate(stopping: &AtomicBool) -> Result<IAudioClient, AudioError> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: std::process::id(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Windows frees the blob once the activation is over, as PropVariantClear
    // would, so it has to come from COM's allocator: pointed at a local, as
    // Microsoft's sample does, the heap broke at the first activation on
    // this PC. Booth never frees it itself, and loses its 12 bytes in the
    // rare case where Windows refuses before taking it.
    // SAFETY: a plain allocation, checked for null below.
    let blob = unsafe { CoTaskMemAlloc(size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>()) }
        .cast::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    if blob.is_null() {
        return Err(AudioError::PcSound {
            code: 0,
            text: String::from("Windows had no memory to start it"),
            lost: false,
        });
    }
    let mut variant = PROPVARIANT::default();
    // SAFETY: `blob` holds room for one set of params, written whole; then
    // the plain fields of a zeroed PROPVARIANT, read as a blob from here on.
    unsafe {
        blob.write(params);
        let value = &mut *variant.Anonymous.Anonymous;
        value.vt = VT_BLOB;
        value.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: blob.cast(),
        };
    }
    let done = handler();
    // SAFETY: a constant device path, IAudioClient's IID, the PROPVARIANT
    // above, and a handler that holds a reference of its own for Windows.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &done,
        )
    }
    .map_err(|err| classify(&err, false))?;
    wait(&done, stopping)?;
    let mut result = HRESULT(0);
    let mut activated: Option<IUnknown> = None;
    // SAFETY: called after ActivateCompleted, with two locals to fill.
    unsafe { operation.GetActivateResult(&mut result, &mut activated) }
        .map_err(|err| classify(&err, false))?;
    result.ok().map_err(|err| classify(&err, false))?;
    let activated = activated.ok_or_else(|| AudioError::PcSound {
        code: 0,
        text: String::from("Windows said it started but gave nothing to read"),
        lost: false,
    })?;
    activated.cast().map_err(|err| classify(&err, false))
}

fn wait(
    done: &IActivateAudioInterfaceCompletionHandler,
    stopping: &AtomicBool,
) -> Result<(), AudioError> {
    // SAFETY: `done` was made by handler() and holds a reference.
    let state = unsafe { &*done.as_raw().cast::<Activation>() };
    let until = Instant::now() + ACTIVATION_WAIT;
    let mut finished = lock(&state.done);
    while !*finished {
        if stopping.load(Ordering::Relaxed) {
            return Err(AudioError::PcSound {
                code: 0,
                text: String::from("it was turned off while Windows started it"),
                lost: false,
            });
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(AudioError::PcSound {
                code: 0,
                text: format!(
                    "Windows did not finish starting it within {} s",
                    ACTIVATION_WAIT.as_secs()
                ),
                lost: false,
            });
        }
        finished = state
            .finished
            .wait_timeout(finished, left.min(STOP_LOOK))
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    Ok(())
}

// PC sound's errors in its own words, never the microphone's. One that comes
// while the stream runs is a loss, since the stream opens again a second
// later; so is the audio service not running, which it waits out.
pub(super) fn classify(err: &windows::core::Error, running: bool) -> AudioError {
    let code = err.code();
    let ended = code == AUDCLNT_E_DEVICE_INVALIDATED || code == AUDCLNT_E_RESOURCES_INVALIDATED;
    let text = if code == AUDCLNT_E_SERVICE_NOT_RUNNING {
        String::from("the Windows Audio service is not running")
    } else if ended {
        String::from("Windows ended it")
    } else {
        let message = err.message();
        let message = message.trim().trim_end_matches('.');
        if message.is_empty() {
            String::from("Windows gave no reason")
        } else {
            message.to_owned()
        }
    };
    AudioError::PcSound {
        code: code.0 as u32,
        text,
        lost: running || ended || code == AUDCLNT_E_SERVICE_NOT_RUNNING,
    }
}

// The completion handler, written out by hand as the notification client in
// wasapi.rs is, for the same reason. It only marks the activation done and
// wakes the capture thread: nothing of Booth's runs on Windows' thread.
#[repr(C)]
struct Activation {
    // First: COM finds the table at the address of the object.
    vtable: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
    refs: AtomicU32,
    done: Mutex<bool>,
    finished: Condvar,
}

static ACTIVATION: IActivateAudioInterfaceCompletionHandler_Vtbl =
    IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: IUnknown_Vtbl {
            QueryInterface: query_interface,
            AddRef: add_ref,
            Release: release,
        },
        ActivateCompleted: activate_completed,
    };

fn handler() -> IActivateAudioInterfaceCompletionHandler {
    let object = Box::new(Activation {
        vtable: &ACTIVATION,
        refs: AtomicU32::new(1),
        done: Mutex::new(false),
        finished: Condvar::new(),
    });
    // SAFETY: the object starts with its table and holds one reference, which
    // the interface takes over. release frees it when the last reference
    // goes, Windows' own included, however late that is.
    unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(Box::into_raw(object).cast()) }
}

// SAFETY (caller): `this` is a pointer handler() made, with a reference held
// for as long as the result is used. Windows holds one for every call.
unsafe fn object<'a>(this: *mut c_void) -> &'a Activation {
    // SAFETY: as the caller promises.
    unsafe { &*this.cast::<Activation>() }
}

// Agile as well: the activation's page asks for a handler that may be called
// from any apartment, and this one keeps nothing tied to one.
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
        if iid == IUnknown::IID
            || iid == IActivateAudioInterfaceCompletionHandler::IID
            || iid == IAgileObject::IID
        {
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
        // SAFETY: the last reference is gone, so nothing can reach the object
        // any more; it came from Box::into_raw above.
        drop(unsafe { Box::from_raw(this.cast::<Activation>()) });
    }
    left
}

unsafe extern "system" fn activate_completed(this: *mut c_void, _: *mut c_void) -> HRESULT {
    // SAFETY: see the note above object.
    let state = unsafe { object(this) };
    *lock(&state.done) = true;
    state.finished.notify_all();
    S_OK
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL};

    fn said(code: HRESULT, running: bool) -> (bool, String) {
        let err = classify(&windows::core::Error::from_hresult(code), running);
        let AudioError::PcSound { lost, .. } = &err else {
            panic!("{err:?}");
        };
        (*lost, err.to_string())
    }

    #[test]
    fn pc_sound_says_what_went_wrong_in_its_own_words() {
        assert_eq!(
            said(AUDCLNT_E_SERVICE_NOT_RUNNING, false),
            (
                true,
                String::from(
                    "PC sound stopped: the Windows Audio service is not running (0x88890010). Booth tries again every second"
                )
            )
        );
        assert_eq!(
            said(AUDCLNT_E_DEVICE_INVALIDATED, true),
            (
                true,
                String::from(
                    "PC sound stopped: Windows ended it (0x88890004). Booth tries again every second"
                )
            )
        );
        let (lost, text) = said(E_ACCESSDENIED, false);
        assert!(!lost);
        assert!(
            text.starts_with("could not start PC sound: Windows says access is denied")
                && text.contains("Privacy and security, Microphone"),
            "{text}"
        );
        let (lost, text) = said(E_FAIL, false);
        assert!(!lost);
        assert!(text.starts_with("could not start PC sound: "), "{text}");
        assert!(
            text.ends_with("(0x80004005). Try again, and restart Windows if it keeps happening"),
            "{text}"
        );
        // Whatever fails while it runs, it opens again a second later.
        assert!(said(E_FAIL, true).0);
        for code in [
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_RESOURCES_INVALIDATED,
            E_FAIL,
        ] {
            for running in [false, true] {
                let (_, text) = said(code, running);
                assert!(!text.contains("microphone"), "{text}");
            }
        }
    }
}
