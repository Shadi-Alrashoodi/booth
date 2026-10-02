use std::sync::{Mutex, MutexGuard};

use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

use crate::EncodeError;

pub(crate) const NVIDIA: u32 = 0x10de;

/// Held while an NVENC session or a Media Foundation hardware encoder opens,
/// so only one opens at a time in this process. NVIDIA's Media Foundation
/// encoder, which opens an NVENC session of its own, failed to start with
/// E_UNEXPECTED in 6 runs of 20 when an NVENC session opened on another
/// thread at the same moment, and in none of 20 when they took turns. Opens
/// happen once per share, so waiting here costs no frame anything.
pub(crate) fn opening() -> MutexGuard<'static, ()> {
    static OPENING: Mutex<()> = Mutex::new(());
    OPENING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The unit tests that use the GPU take turns, as the tests in tests/ do,
/// so encode times mean something and a consumer card's cap on sessions is
/// never the reason a test fails.
#[cfg(test)]
pub(crate) fn test_turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct Adapter {
    pub(crate) vendor: u32,
    pub(crate) name: String,
    /// Which of several GPUs this is, for matching a Media Foundation
    /// encoder to it: a laptop can register one per GPU.
    pub(crate) luid: LUID,
}

/// The GPU a device was created on, which decides the encoder.
pub(crate) fn adapter_of(device: &ID3D11Device) -> Result<Adapter, EncodeError> {
    let error = |source| EncodeError::Direct3D {
        action: "find out which GPU the Direct3D device is on",
        source,
    };
    let dxgi: IDXGIDevice = device.cast().map_err(error)?;
    // SAFETY: plain COM calls on live interfaces; GetDesc fills a struct it
    // returns by value.
    let desc = unsafe { dxgi.GetAdapter().and_then(|adapter| adapter.GetDesc()) }.map_err(error)?;
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    Ok(Adapter {
        vendor: desc.VendorId,
        name: String::from_utf16_lossy(&desc.Description[..len])
            .trim()
            .to_string(),
        luid: desc.AdapterLuid,
    })
}

/// Refuses a frame texture an encoder opened on `device` for `width` x
/// `height` cannot take, with the reason.
pub(crate) fn check_texture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<(), EncodeError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `desc` is a valid out pointer for the call.
    unsafe { texture.GetDesc(&mut desc) };
    // SAFETY: plain COM call on a live texture.
    let owner = unsafe { texture.GetDevice() }.map_err(|source| EncodeError::Direct3D {
        action: "ask the frame texture for its device",
        source,
    })?;

    let problem = if owner.as_raw() != device.as_raw() {
        Some("the texture is on another Direct3D device than the encoder".to_string())
    } else if desc.Format != DXGI_FORMAT_NV12 {
        Some(format!(
            "the texture is DXGI format {} and the encoder takes NV12",
            desc.Format.0
        ))
    } else if desc.Width != width || desc.Height != height {
        Some(format!(
            "the texture is {}x{} and the encoder was opened for {width}x{height}: a new size needs a new encoder",
            desc.Width, desc.Height
        ))
    } else if desc.ArraySize != 1 || desc.MipLevels != 1 || desc.SampleDesc.Count != 1 {
        Some(format!(
            "the texture has {} array slices, {} mip levels and {} samples, and the encoder takes one of each",
            desc.ArraySize, desc.MipLevels, desc.SampleDesc.Count
        ))
    } else {
        None
    };
    match problem {
        Some(problem) => Err(EncodeError::WrongFrame { problem }),
        None => Ok(()),
    }
}
