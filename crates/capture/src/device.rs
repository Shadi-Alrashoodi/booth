use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::core::Interface;

use crate::error::{CaptureError, ErrorKind, meaning};
use crate::monitors::{Adapter, factory, hardware_adapters};

// A D3D11 device on the given adapter, made the way Capture makes its own,
// for the pattern source and for tests.
pub fn device_on(adapter: &Adapter) -> Result<ID3D11Device, CaptureError> {
    let factory = factory()?;
    let Some((found, _)) = hardware_adapters(&factory)?
        .into_iter()
        .find(|(_, info)| info.luid == adapter.luid)
    else {
        return Err(CaptureError::new(
            ErrorKind::DeviceLost,
            format!(
                "could not find {}: it was removed or its driver restarted",
                adapter.description
            ),
        ));
    };
    Ok(create(&found, &adapter.description)?.0)
}

// BGRA support for the duplication image; video support when the driver has
// it, for Media Foundation later; and multithread protection, because the
// encoder, and later Media Foundation, use this same device from their own
// calls.
pub(crate) fn create(
    adapter: &IDXGIAdapter1,
    name: &str,
) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let with_video = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let made = try_create(adapter, with_video, &levels)
        .or_else(|_| try_create(adapter, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &levels));
    let (device, context) =
        made.map_err(|err| CaptureError::windows(format!("start Direct3D 11 on {name}"), &err))?;
    let multithread: ID3D11Multithread = context.cast().map_err(|err| {
        CaptureError::windows(format!("share the Direct3D 11 device on {name}"), &err)
    })?;
    // SAFETY: a setter on a live interface. The returned BOOL is the previous
    // setting, which does not matter here.
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok((device, context))
}

fn try_create(
    adapter: &IDXGIAdapter1,
    flags: D3D11_CREATE_DEVICE_FLAG,
    levels: &[D3D_FEATURE_LEVEL],
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: an explicit adapter goes with D3D_DRIVER_TYPE_UNKNOWN and no
    // software module; the two out parameters are live locals.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(windows::core::Error::from(
            windows::Win32::Foundation::E_POINTER,
        )),
    }
}

// The error for a call that failed because the device is gone, with the
// reason Windows keeps for it. `step` reads after "could not".
pub(crate) fn lost(device: &ID3D11Device, step: &str, err: &windows::core::Error) -> CaptureError {
    // SAFETY: a getter on a live interface; it answers even for a removed
    // device, which is what it is for.
    let reason = match unsafe { device.GetDeviceRemovedReason() } {
        Ok(()) => return CaptureError::windows(step, err),
        Err(reason) => reason,
    };
    CaptureError::new(
        ErrorKind::DeviceLost,
        format!(
            "could not {step}: {}. Start sharing again, and update the graphics driver if it keeps happening",
            meaning(&reason)
        ),
    )
}
