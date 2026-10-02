use std::sync::Mutex;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    IDXGIAdapter1, IDXGIDevice1, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

use crate::error::ViewerError;

// The Microsoft Basic Render Driver: the CPU pretending to be a GPU.
const BASIC_RENDER: (u32, u32) = (0x1414, 0x8c);

#[derive(Clone)]
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub lock: ID3D11Multithread,
    pub name: String,
    luid: u64,
}

// One device per graphics card for the life of the process, shared by every
// viewer on that card. The NVIDIA driver keeps two handles for each device
// that ever made anything, after the device is gone, so a device per viewer
// would leak them each time someone starts watching. A device that was lost
// is replaced the next time.
static DEVICES: Mutex<Vec<Gpu>> = Mutex::new(Vec::new());

// A device on the GPU that drives `monitor`, so a frame never crosses to
// another adapter on its way to the screen. A monitor no hardware adapter
// lists (a display-only adapter, or one that just went away) gets the first
// hardware GPU instead.
pub(crate) fn gpu_for(monitor: HMONITOR) -> Result<Gpu, ViewerError> {
    // SAFETY: a plain factory call.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|err| ViewerError::windows("list the graphics cards", &err))?;
    let mut chosen = None;
    for index in 0.. {
        // SAFETY: a getter on a live factory; NOT_FOUND ends the list.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(ViewerError::windows("list the graphics cards", &err)),
        };
        // SAFETY: a getter on a live adapter.
        let desc = unsafe { adapter.GetDesc1() }
            .map_err(|err| ViewerError::windows("read a graphics card's name", &err))?;
        if !is_hardware(&desc) {
            continue;
        }
        if drives(&adapter, monitor) {
            chosen = Some((adapter, desc));
            break;
        }
        chosen.get_or_insert((adapter, desc));
    }
    let Some((adapter, desc)) = chosen else {
        return Err(ViewerError::other(
            "could not open the viewer: this PC has no graphics card Direct3D 11 can use. Update the graphics driver",
        ));
    };
    let luid = (desc.AdapterLuid.HighPart as u32 as u64) << 32 | desc.AdapterLuid.LowPart as u64;
    let mut devices = DEVICES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: a getter that answers for a removed device too.
    devices.retain(|gpu| unsafe { gpu.device.GetDeviceRemovedReason() }.is_ok());
    if let Some(gpu) = devices.iter().find(|gpu| gpu.luid == luid) {
        return Ok(gpu.clone());
    }
    let gpu = create(&adapter, &name(&desc), luid)?;
    devices.push(gpu.clone());
    Ok(gpu)
}

fn is_hardware(desc: &DXGI_ADAPTER_DESC1) -> bool {
    desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
        && (desc.VendorId, desc.DeviceId) != BASIC_RENDER
}

fn drives(adapter: &IDXGIAdapter1, monitor: HMONITOR) -> bool {
    for index in 0.. {
        // SAFETY: a getter on a live adapter; any error ends the list.
        let Ok(output) = (unsafe { adapter.EnumOutputs(index) }) else {
            return false;
        };
        // SAFETY: a getter on a live output.
        if let Ok(desc) = unsafe { output.GetDesc() }
            && desc.Monitor == monitor
        {
            return true;
        }
    }
    false
}

fn name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..end])
}

// BGRA for Direct2D; video support for the decoder, which decodes on this
// device; multithread protection because the decoder is handed the device
// and may call it from its own thread.
fn create(adapter: &IDXGIAdapter1, name: &str, luid: u64) -> Result<Gpu, ViewerError> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let with_video = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let (device, context) = try_create(adapter, with_video, &levels)
        .or_else(|_| try_create(adapter, D3D11_CREATE_DEVICE_BGRA_SUPPORT, &levels))
        .map_err(|err| ViewerError::windows(format!("start Direct3D 11 on {name}"), &err))?;
    let lock: ID3D11Multithread = context.cast().map_err(|err| {
        ViewerError::windows(format!("share the Direct3D 11 device on {name}"), &err)
    })?;
    // SAFETY: a setter on a live interface. The returned BOOL is the previous
    // setting, which does not matter here.
    unsafe {
        let _ = lock.SetMultithreadProtected(true);
    }
    // One frame queued ahead at most. Without a waitable object this is
    // the device's setting, and any more is a frame of delay.
    let dxgi: IDXGIDevice1 = device
        .cast()
        .map_err(|err| ViewerError::windows(format!("reach DXGI on {name}"), &err))?;
    // SAFETY: a setter on a live interface.
    unsafe { dxgi.SetMaximumFrameLatency(1) }
        .map_err(|err| ViewerError::windows(format!("limit the frame queue on {name}"), &err))?;
    Ok(Gpu {
        device,
        context,
        lock,
        name: name.to_string(),
        luid,
    })
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

// The device's own multithread lock, held until dropped. It is the one
// every protected call takes, and it nests, so the calls made while it is
// held take it again without waiting. Held around a whole frame's drawing,
// so a call from the decoder's thread cannot land between setting the
// pipeline up and drawing with it.
pub(crate) struct Locked<'a>(&'a ID3D11Multithread);

impl<'a> Locked<'a> {
    pub(crate) fn enter(lock: &'a ID3D11Multithread) -> Locked<'a> {
        // SAFETY: a call on a live interface, undone by the drop below.
        unsafe { lock.Enter() };
        Locked(lock)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: entered in Locked::enter on this thread.
        unsafe { self.0.Leave() };
    }
}
