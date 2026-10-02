use std::fmt;

use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, E_OUTOFMEMORY};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_MODE_CHANGE_IN_PROGRESS, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
};
use windows::core::HRESULT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    // The GPU was removed, reset or its driver restarted. The device is dead,
    // and so is everything made on it, the encoder's session included.
    DeviceLost,
    // Unplugged, turned off, or no longer on the GPU it was listed on.
    MonitorGone,
    Other,
}

// Lower case, what happened and then, where there is something to do, what
// to do, like the rest of Booth's errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureError {
    kind: ErrorKind,
    message: String,
}

impl CaptureError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> CaptureError {
        CaptureError {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn other(message: impl Into<String>) -> CaptureError {
        CaptureError::new(ErrorKind::Other, message)
    }

    // `step` reads after "could not", as in "duplicate DELL U2723QE".
    pub(crate) fn windows(step: impl fmt::Display, err: &windows::core::Error) -> CaptureError {
        let kind = if is_device_lost(err.code()) {
            ErrorKind::DeviceLost
        } else {
            ErrorKind::Other
        };
        CaptureError::new(kind, format!("could not {step}: {}", meaning(err)))
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaptureError {}

pub(crate) fn is_device_lost(code: HRESULT) -> bool {
    [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_HUNG,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR,
    ]
    .contains(&code)
}

// What the code means for someone sharing a screen, with the constant's name
// for whoever reads the log.
pub(crate) fn meaning(err: &windows::core::Error) -> String {
    let code = err.code();
    let known = [
        (
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            "Windows allows only a few programs to duplicate a monitor at once and they are taken (DXGI_ERROR_NOT_CURRENTLY_AVAILABLE)",
        ),
        (
            E_ACCESSDENIED,
            "Windows is showing the secure desktop: a UAC prompt, the lock screen or Ctrl+Alt+Del (E_ACCESSDENIED)",
        ),
        (
            DXGI_ERROR_UNSUPPORTED,
            "the display driver or this kind of Windows session does not offer it (DXGI_ERROR_UNSUPPORTED)",
        ),
        (
            DXGI_ERROR_SESSION_DISCONNECTED,
            "this Windows session is disconnected, as after a Remote Desktop session ends (DXGI_ERROR_SESSION_DISCONNECTED)",
        ),
        (
            DXGI_ERROR_ACCESS_LOST,
            "the desktop changed under it: a new display mode, a rotation, or the secure desktop (DXGI_ERROR_ACCESS_LOST)",
        ),
        (
            DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
            "Windows is changing the display mode (DXGI_ERROR_MODE_CHANGE_IN_PROGRESS)",
        ),
        (
            DXGI_ERROR_DEVICE_REMOVED,
            "the graphics card was removed, or its driver was updated or restarted (DXGI_ERROR_DEVICE_REMOVED)",
        ),
        (
            DXGI_ERROR_DEVICE_HUNG,
            "the graphics card stopped responding (DXGI_ERROR_DEVICE_HUNG)",
        ),
        (
            DXGI_ERROR_DEVICE_RESET,
            "the graphics card was reset (DXGI_ERROR_DEVICE_RESET)",
        ),
        (
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
            "the display driver failed inside itself (DXGI_ERROR_DRIVER_INTERNAL_ERROR)",
        ),
        (
            DXGI_ERROR_INVALID_CALL,
            "Windows refused the call as invalid (DXGI_ERROR_INVALID_CALL)",
        ),
        (
            DXGI_ERROR_NOT_FOUND,
            "Windows did not find it (DXGI_ERROR_NOT_FOUND)",
        ),
        (
            DXGI_ERROR_WAIT_TIMEOUT,
            "the wait ran out (DXGI_ERROR_WAIT_TIMEOUT)",
        ),
        (E_INVALIDARG, "Windows refused an argument (E_INVALIDARG)"),
        (E_OUTOFMEMORY, "out of memory (E_OUTOFMEMORY)"),
    ];
    if let Some((_, text)) = known.iter().find(|(known, _)| *known == code) {
        return (*text).to_string();
    }
    let text = err.message();
    let text = text.trim_end_matches(['.', ' ', '\r', '\n']);
    if text.is_empty() {
        format!("error {:#010x}", code.0 as u32)
    } else {
        format!("{} ({:#010x})", lower_first(text), code.0 as u32)
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}
