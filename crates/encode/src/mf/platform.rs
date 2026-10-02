// Media Foundation's start-up, and COM on the threads that use it.
//
// mfplat.dll is loaded from System32 at the first open instead of being
// linked: N editions of Windows ship without Media Foundation, and a linked
// import would stop booth.exe from starting there at all, voice and chat
// included. Once loaded it stays loaded; MFStartup and MFShutdown are what
// start and stop Media Foundation's worker threads.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{ERROR_MOD_NOT_FOUND, RPC_E_CHANGED_MODE};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFT_REGISTER_TYPE_INFO};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{BOOL, GUID, HRESULT, PCSTR, s, w};

use crate::EncodeError;

// What GetProcAddress hands back, before it is given its real signature.
type Proc = unsafe extern "system" fn() -> isize;
type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type Shutdown = unsafe extern "system" fn() -> HRESULT;
pub(crate) type CreateAttributes = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
pub(crate) type CreateObject = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
pub(crate) type CreateMemoryBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
pub(crate) type CreateSurfaceBuffer =
    unsafe extern "system" fn(*const GUID, *mut c_void, u32, BOOL, *mut *mut c_void) -> HRESULT;
pub(crate) type CreateDeviceManager =
    unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
pub(crate) type Enumerate = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut c_void,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;

/// The mfplat.dll exports the encoders call. The signatures are the ones in
/// mfapi.h.
#[derive(Clone, Copy)]
pub(crate) struct Functions {
    startup: Startup,
    shutdown: Shutdown,
    pub(crate) create_attributes: CreateAttributes,
    pub(crate) create_media_type: CreateObject,
    pub(crate) create_sample: CreateObject,
    pub(crate) create_memory_buffer: CreateMemoryBuffer,
    pub(crate) create_surface_buffer: CreateSurfaceBuffer,
    pub(crate) create_device_manager: CreateDeviceManager,
    /// MFTEnum2, which can ask for one GPU's encoders (Windows 10 1703).
    pub(crate) enumerate: Enumerate,
}

#[derive(Clone, Copy)]
enum LoadFailure {
    Missing,
    Failed(HRESULT),
    Export(&'static str),
}

fn load() -> Result<Functions, LoadFailure> {
    // SAFETY: the name is a NUL-terminated wide string literal. System32
    // only, like nvEncodeAPI64.dll: never a copy next to the exe.
    let module = unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|e| {
            if e.code() == HRESULT::from_win32(ERROR_MOD_NOT_FOUND.0) {
                LoadFailure::Missing
            } else {
                LoadFailure::Failed(e.code())
            }
        })?;
    let export = |name: PCSTR, text: &'static str| {
        // SAFETY: the module is never freed, and the name is a
        // NUL-terminated string literal.
        unsafe { GetProcAddress(module, name) }.ok_or(LoadFailure::Export(text))
    };
    // SAFETY, for every transmute below: each export has the signature of its
    // declaration in mfapi.h, which the types above copy.
    unsafe {
        Ok(Functions {
            startup: std::mem::transmute::<Proc, Startup>(export(s!("MFStartup"), "MFStartup")?),
            shutdown: std::mem::transmute::<Proc, Shutdown>(export(
                s!("MFShutdown"),
                "MFShutdown",
            )?),
            create_attributes: std::mem::transmute::<Proc, CreateAttributes>(export(
                s!("MFCreateAttributes"),
                "MFCreateAttributes",
            )?),
            create_media_type: std::mem::transmute::<Proc, CreateObject>(export(
                s!("MFCreateMediaType"),
                "MFCreateMediaType",
            )?),
            create_sample: std::mem::transmute::<Proc, CreateObject>(export(
                s!("MFCreateSample"),
                "MFCreateSample",
            )?),
            create_memory_buffer: std::mem::transmute::<Proc, CreateMemoryBuffer>(export(
                s!("MFCreateMemoryBuffer"),
                "MFCreateMemoryBuffer",
            )?),
            create_surface_buffer: std::mem::transmute::<Proc, CreateSurfaceBuffer>(export(
                s!("MFCreateDXGISurfaceBuffer"),
                "MFCreateDXGISurfaceBuffer",
            )?),
            create_device_manager: std::mem::transmute::<Proc, CreateDeviceManager>(export(
                s!("MFCreateDXGIDeviceManager"),
                "MFCreateDXGIDeviceManager",
            )?),
            enumerate: std::mem::transmute::<Proc, Enumerate>(export(s!("MFTEnum2"), "MFTEnum2")?),
        })
    }
}

fn functions() -> Result<Functions, EncodeError> {
    static LOADED: OnceLock<Result<Functions, LoadFailure>> = OnceLock::new();
    match *LOADED.get_or_init(load) {
        Ok(functions) => Ok(functions),
        Err(LoadFailure::Missing) => Err(EncodeError::MediaFoundationMissing),
        Err(LoadFailure::Failed(code)) => Err(EncodeError::MediaFoundation {
            action: "load mfplat.dll from System32",
            source: code.into(),
        }),
        Err(LoadFailure::Export(name)) => Err(EncodeError::MediaFoundationExportMissing { name }),
    }
}

// Live encoders. MFStartup runs when the first one opens and MFShutdown
// when the last one is dropped, so the calls always pair up and nothing of
// Media Foundation runs between shares.
static USERS: Mutex<usize> = Mutex::new(0);

/// Media Foundation, started for as long as this lives.
pub(crate) struct Mf {
    pub(crate) fns: Functions,
}

impl Mf {
    pub(crate) fn start() -> Result<Mf, EncodeError> {
        com()?;
        let fns = functions()?;
        let mut users = USERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *users == 0 {
            // SAFETY: plain call with the version this code was written for.
            unsafe { (fns.startup)(MF_VERSION, MFSTARTUP_LITE) }
                .ok()
                .map_err(|source| EncodeError::MediaFoundation {
                    action: "start",
                    source,
                })?;
        }
        *users += 1;
        Ok(Mf { fns })
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        let mut users = USERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *users -= 1;
        if *users == 0 {
            // SAFETY: pairs with the MFStartup made when the count left 0.
            let _ = unsafe { (self.fns.shutdown)() };
        }
    }
}

// COM in the multithreaded apartment on every thread that calls into the
// encoders. Media Foundation's objects are free-threaded and the hardware
// encoder raises its events on Media Foundation's own worker threads, so
// nothing needs a message loop; a single-threaded apartment would want one
// on a thread that never pumps messages. Initialized on first use and
// uninitialized when the thread ends.
struct Apartment {
    joined: HRESULT,
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.joined.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx on this same
            // thread, which is where thread-local destructors run.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static APARTMENT: Apartment = Apartment {
        // SAFETY: plain call; the result decides whether Drop pairs it.
        joined: unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) },
    };
    static CHECKED: Cell<bool> = const { Cell::new(false) };
}

/// Joins this thread to COM's multithreaded apartment unless it already is
/// in one. A thread the caller already made single-threaded stays that way:
/// Media Foundation works there too, since nothing it does for Booth's
/// encoders needs a call marshalled back into the thread.
pub(crate) fn com() -> Result<(), EncodeError> {
    if CHECKED.with(Cell::get) {
        return Ok(());
    }
    let joined = APARTMENT.with(|a| a.joined);
    if joined.is_err() && joined != RPC_E_CHANGED_MODE {
        return Err(EncodeError::MediaFoundation {
            action: "start COM on the encode thread",
            source: joined.into(),
        });
    }
    CHECKED.with(|c| c.set(true));
    Ok(())
}
