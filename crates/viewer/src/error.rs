use std::fmt;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, E_INVALIDARG, E_OUTOFMEMORY};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_UNSUPPORTED,
};
use windows::core::HRESULT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    // The GPU was removed, reset or its driver restarted. The device is dead,
    // and so is everything made on it, the decoder's surfaces included.
    DeviceLost,
    // The person closed the window. Nothing is wrong; watching has stopped.
    Closed,
    Other,
}

// Lower case, what happened and then, where there is something to do, what
// to do, like the rest of Booth's errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerError {
    kind: ErrorKind,
    message: String,
}

impl ViewerError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> ViewerError {
        ViewerError {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn other(message: impl Into<String>) -> ViewerError {
        ViewerError::new(ErrorKind::Other, message)
    }

    pub(crate) fn closed() -> ViewerError {
        ViewerError::new(ErrorKind::Closed, "the viewer window was closed")
    }

    // `step` reads after "could not", as in "resize the viewer's buffers".
    pub(crate) fn windows(step: impl fmt::Display, err: &windows::core::Error) -> ViewerError {
        let kind = if is_device_lost(err.code()) {
            ErrorKind::DeviceLost
        } else {
            ErrorKind::Other
        };
        ViewerError::new(kind, format!("could not {step}: {}", meaning(err)))
    }

    // For a call that answered with a Windows object missing instead of an
    // error, which the windows crate leaves to the caller.
    pub(crate) fn missing(step: impl fmt::Display, what: &str) -> ViewerError {
        ViewerError::other(format!("could not {step}: Windows returned no {what}"))
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for ViewerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ViewerError {}

pub(crate) fn is_device_lost(code: HRESULT) -> bool {
    [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_HUNG,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        // Direct2D's way of saying the device under it is gone.
        D2DERR_RECREATE_TARGET,
    ]
    .contains(&code)
}

// What the code means for someone watching a share, with the constant's
// name for whoever reads the log.
pub(crate) fn meaning(err: &windows::core::Error) -> String {
    let code = err.code();
    let known = [
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
            D2DERR_RECREATE_TARGET,
            "the graphics card was reset or its driver restarted under Direct2D (D2DERR_RECREATE_TARGET)",
        ),
        (
            DXGI_ERROR_INVALID_CALL,
            "Windows refused the call as invalid (DXGI_ERROR_INVALID_CALL)",
        ),
        (
            DXGI_ERROR_UNSUPPORTED,
            "the display driver does not offer it (DXGI_ERROR_UNSUPPORTED)",
        ),
        (
            DXGI_ERROR_NOT_FOUND,
            "Windows did not find it (DXGI_ERROR_NOT_FOUND)",
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
