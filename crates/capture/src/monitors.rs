use std::collections::HashMap;
use std::fmt;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, LUID};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::core::Interface;

use crate::error::{CaptureError, ErrorKind};

// The Microsoft Basic Render Driver: the CPU pretending to be a GPU, which
// has no outputs worth sharing and no encoder.
const BASIC_RENDER: (u32, u32) = (0x1414, 0x8c);

// winuser.h; the windows crate keeps it with the window functions, which
// this crate does not otherwise need.
const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adapter {
    pub description: String,
    pub vendor_id: u32,
    pub device_id: u32,
    // The adapter's LUID packed as high part and low part. It changes when
    // Windows restarts, so it is only good for this session.
    pub luid: u64,
}

// What a monitor is opened by: its GDI device name, unique among the
// monitors attached now, and the adapter that drives it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MonitorId {
    pub device_name: String,
    pub adapter_luid: u64,
}

// DXGI's rotation of a monitor. The duplication image comes as the panel
// scans it; the upright picture is that image turned clockwise by this
// much, which is how Microsoft's Desktop Duplication sample draws it. Not
// seen on a rotated monitor yet: I have none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    Identity,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Rotation {
    pub fn degrees(self) -> u32 {
        match self {
            Rotation::Identity => 0,
            Rotation::Rotate90 => 90,
            Rotation::Rotate180 => 180,
            Rotation::Rotate270 => 270,
        }
    }

    // A quarter turn swaps the duplication image's width and height.
    pub fn swaps_sides(self) -> bool {
        matches!(self, Rotation::Rotate90 | Rotation::Rotate270)
    }

    pub(crate) fn from_dxgi(rotation: DXGI_MODE_ROTATION) -> Rotation {
        match rotation {
            DXGI_MODE_ROTATION_ROTATE90 => Rotation::Rotate90,
            DXGI_MODE_ROTATION_ROTATE180 => Rotation::Rotate180,
            DXGI_MODE_ROTATION_ROTATE270 => Rotation::Rotate270,
            // UNSPECIFIED only comes from outputs that cannot rotate.
            _ => Rotation::Identity,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub id: MonitorId,
    // What Windows Settings shows, or the device name if Windows has none.
    pub name: String,
    // Desktop coordinates in physical pixels, upright as the user sees it.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub rotation: Rotation,
    pub primary: bool,
    // HDR is not captured yet; an HDR monitor is duplicated as SDR by
    // Windows.
    pub hdr: bool,
    pub adapter: Adapter,
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}), {}x{} at {:.2} Hz, at {},{}, rotated {}, {}{}, on {}",
            self.name,
            self.id.device_name,
            self.width,
            self.height,
            self.refresh_hz,
            self.left,
            self.top,
            self.rotation.degrees(),
            if self.primary {
                "primary"
            } else {
                "not primary"
            },
            if self.hdr { ", hdr on" } else { "" },
            self.adapter.description,
        )
    }
}

pub(crate) fn pack_luid(luid: LUID) -> u64 {
    ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64
}

pub(crate) fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

pub(crate) fn factory() -> Result<IDXGIFactory1, CaptureError> {
    // SAFETY: no arguments besides the interface asked for.
    unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
        .map_err(|err| CaptureError::windows("list the graphics adapters", &err))
}

// Every hardware adapter with its description, in DXGI's order (the one
// driving the primary monitor first).
pub(crate) fn hardware_adapters(
    factory: &IDXGIFactory1,
) -> Result<Vec<(IDXGIAdapter1, Adapter)>, CaptureError> {
    let mut found = Vec::new();
    for index in 0.. {
        // SAFETY: a plain index; the end of the list is DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(CaptureError::windows("list the graphics adapters", &err)),
        };
        // SAFETY: a getter on a live adapter.
        let desc: DXGI_ADAPTER_DESC1 = unsafe { adapter.GetDesc1() }
            .map_err(|err| CaptureError::windows("read a graphics adapter's name", &err))?;
        let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
        if software || (desc.VendorId, desc.DeviceId) == BASIC_RENDER {
            continue;
        }
        let info = Adapter {
            description: wide_to_string(&desc.Description),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
            luid: pack_luid(desc.AdapterLuid),
        };
        found.push((adapter, info));
    }
    Ok(found)
}

pub fn adapters() -> Result<Vec<Adapter>, CaptureError> {
    let factory = factory()?;
    Ok(hardware_adapters(&factory)?
        .into_iter()
        .map(|(_, info)| info)
        .collect())
}

pub(crate) fn outputs(adapter: &IDXGIAdapter1) -> Result<Vec<IDXGIOutput>, CaptureError> {
    let mut found = Vec::new();
    for index in 0.. {
        // SAFETY: a plain index; the end of the list is DXGI_ERROR_NOT_FOUND.
        match unsafe { adapter.EnumOutputs(index) } {
            Ok(output) => found.push(output),
            Err(err) if err.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(err) => return Err(CaptureError::windows("list the monitors", &err)),
        }
    }
    Ok(found)
}

pub(crate) struct OutputDesc {
    pub device_name: String,
    pub attached: bool,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
    pub primary: bool,
    pub hdr: bool,
}

pub(crate) fn describe_output(output: &IDXGIOutput) -> Result<OutputDesc, CaptureError> {
    let fail = |err: windows::core::Error| CaptureError::windows("read a monitor's settings", &err);
    // IDXGIOutput6 is there from Windows 10 1703 on; without it there is no
    // colour space to read, and so no HDR either.
    let (name, rect, attached, rotation, monitor, hdr) = match output.cast::<IDXGIOutput6>() {
        // SAFETY: a getter on a live output.
        Ok(output6) => match unsafe { output6.GetDesc1() } {
            Ok(desc) => (
                desc.DeviceName,
                desc.DesktopCoordinates,
                desc.AttachedToDesktop.as_bool(),
                desc.Rotation,
                desc.Monitor,
                desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            ),
            Err(err) => return Err(fail(err)),
        },
        Err(_) => {
            // SAFETY: a getter on a live output.
            let desc = unsafe { output.GetDesc() }.map_err(fail)?;
            (
                desc.DeviceName,
                desc.DesktopCoordinates,
                desc.AttachedToDesktop.as_bool(),
                desc.Rotation,
                desc.Monitor,
                false,
            )
        }
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live MONITORINFO with its size filled in, and a
    // stale monitor handle only makes the call fail.
    let primary = unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool()
        && info.dwFlags & MONITORINFOF_PRIMARY != 0;
    Ok(OutputDesc {
        device_name: wide_to_string(&name),
        attached,
        left: rect.left,
        top: rect.top,
        width: (rect.right - rect.left).max(0) as u32,
        height: (rect.bottom - rect.top).max(0) as u32,
        rotation: Rotation::from_dxgi(rotation),
        primary,
        hdr,
    })
}

// What DisplayConfig knows about one GDI source: the name Settings shows,
// the refresh rate as a fraction, and the source mode in physical pixels.
struct Source {
    name: String,
    refresh_hz: f64,
    rect: Option<(i32, i32, u32, u32)>,
}

pub fn monitors() -> Result<Vec<Monitor>, CaptureError> {
    let sources = display_config();
    let factory = factory()?;
    let mut monitors = Vec::new();
    for (adapter, info) in hardware_adapters(&factory)? {
        for output in outputs(&adapter)? {
            let desc = describe_output(&output)?;
            if !desc.attached {
                continue;
            }
            let source = sources.get(&desc.device_name);
            // DisplayConfig's source mode is in physical pixels whatever the
            // calling process's DPI awareness; DXGI's rectangle is not.
            let (left, top, width, height) = source.and_then(|s| s.rect).unwrap_or((
                desc.left,
                desc.top,
                desc.width,
                desc.height,
            ));
            monitors.push(Monitor {
                id: MonitorId {
                    device_name: desc.device_name.clone(),
                    adapter_luid: info.luid,
                },
                name: source
                    .map(|s| s.name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| desc.device_name.clone()),
                left,
                top,
                width,
                height,
                refresh_hz: source.map_or(0.0, |s| s.refresh_hz),
                rotation: desc.rotation,
                primary: desc.primary,
                hdr: desc.hdr,
                adapter: info.clone(),
            });
        }
    }
    Ok(monitors)
}

// Keyed by GDI device name. A failure here only costs the friendly names and
// refresh rates, so it gives an empty map rather than an error.
fn display_config() -> HashMap<String, Source> {
    let mut sources = HashMap::new();
    let Some((paths, modes)) = active_paths() else {
        return sources;
    };
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the header starts a whole DISPLAYCONFIG_SOURCE_DEVICE_NAME
        // and says so in its size.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != ERROR_SUCCESS.0 as i32 {
            continue;
        }
        let device_name = wide_to_string(&source.viewGdiDeviceName);
        if sources.contains_key(&device_name) {
            // A cloned desktop: the first monitor showing it names it.
            continue;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: as above, for a DISPLAYCONFIG_TARGET_DEVICE_NAME.
        let name = if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) }
            == ERROR_SUCCESS.0 as i32
        {
            wide_to_string(&target.monitorFriendlyDeviceName)
        } else {
            String::new()
        };
        let rate = path.targetInfo.refreshRate;
        let refresh_hz = if rate.Denominator == 0 {
            0.0
        } else {
            rate.Numerator as f64 / rate.Denominator as f64
        };
        // SAFETY: without QDC_VIRTUAL_MODE_AWARE the union holds the plain
        // index.
        let mode_index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let rect = modes
            .get(mode_index)
            .filter(|mode| mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            .map(|mode| {
                // SAFETY: infoType says the union holds a source mode.
                let source_mode = unsafe { mode.Anonymous.sourceMode };
                (
                    source_mode.position.x,
                    source_mode.position.y,
                    source_mode.width,
                    source_mode.height,
                )
            });
        sources.insert(
            device_name,
            Source {
                name,
                refresh_hz,
                rect,
            },
        );
    }
    sources
}

fn active_paths() -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    // The topology can change between asking for the sizes and asking for
    // the paths; a few tries cover a monitor being plugged in meanwhile.
    for _ in 0..4 {
        let mut path_count = 0;
        let mut mode_count = 0;
        // SAFETY: two locals for the call to fill.
        let sized = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if sized != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the arrays hold exactly the counts passed with them, and
        // Windows writes back how many it filled.
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if queried == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if queried != ERROR_SUCCESS {
            return None;
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Some((paths, modes));
    }
    None
}

// The adapter by LUID and the output on it by device name, looked up fresh,
// since outputs are recreated when monitors come and go.
pub(crate) fn find(id: &MonitorId) -> Result<(IDXGIAdapter1, IDXGIOutput), CaptureError> {
    let factory = factory()?;
    let adapters = hardware_adapters(&factory)?;
    let Some((adapter, info)) = adapters
        .into_iter()
        .find(|(_, info)| info.luid == id.adapter_luid)
    else {
        return Err(CaptureError::new(
            ErrorKind::MonitorGone,
            format!(
                "could not find the graphics adapter driving {}: it was removed or its driver restarted. Choose the monitor again",
                id.device_name
            ),
        ));
    };
    for output in outputs(&adapter)? {
        let desc = describe_output(&output)?;
        if desc.device_name == id.device_name && desc.attached {
            return Ok((adapter, output));
        }
    }
    Err(CaptureError::new(
        ErrorKind::MonitorGone,
        format!(
            "could not find {} on {}: it was unplugged, turned off, or moved to another graphics card. Choose the monitor again",
            id.device_name, info.description
        ),
    ))
}
