// Opening and dropping the Media Foundation encoders 30 times each, the
// hardware one in both codecs, leaves nothing behind: handles, threads,
// private memory and GPU memory of this test process are counted before and
// after. A process of its own, so no other test moves the numbers.

mod common;

use std::mem::size_of;

use encode::{Codec, EncodeError, Encoder, Frame, Kind, Preset, Settings};
use windows::Win32::Foundation::{CloseHandle, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter, IDXGIAdapter3, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFTransform, MF_VERSION, MFMediaType_Video, MFSTARTUP_LITE, MFShutdown,
    MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount,
};
use windows::core::Interface;

use common::Gpu;

// Windows' thread pool opens and closes a few handles of its own from one
// moment to the next: after 10 opens, what was left beyond the driver's own
// ran from -2 to 9, as much as a leak of one handle per open would leave.
// After 30, five runs left 0 to 2 for the hardware encoder and 2 to 8 for
// the software one, and 60 left no more than 30 did, so it is noise and
// not a slope. A leak of one handle per open would leave 30; the limit is
// half that.
const OPENS: i64 = 30;

#[derive(Debug, Clone, Copy)]
struct Usage {
    handles: i64,
    threads: i64,
    private_mb: f64,
    gpu_mb: f64,
}

/// A device on the first GPU that is not Windows' software rasterizer, of
/// any vendor, so this runs on an AMD or Intel PC too. WARP when there is
/// none, which has the software encoder only.
fn gpu() -> Gpu {
    // SAFETY: plain DXGI and Direct3D calls; EnumAdapters1 fails past the
    // last adapter and every out pointer is valid for its call.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        for adapter in (0..).map_while(|i| factory.EnumAdapters1(i).ok()) {
            let desc = adapter.GetDesc1().expect("adapter description");
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let adapter: IDXGIAdapter = adapter.cast().expect("IDXGIAdapter");
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("D3D11 device");
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(0);
            return Gpu {
                device: device.expect("device"),
                context: context.expect("context"),
                name: String::from_utf16_lossy(&desc.Description[..len]),
            };
        }
    }
    common::warp()
}

fn usage(gpu: &Gpu) -> Usage {
    let mut handles = 0;
    // SAFETY: the pseudo handle of this process and a valid out pointer.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) }.expect("handle count");

    // SAFETY: a thread snapshot walked with a correctly sized entry, then
    // closed.
    let threads = unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).expect("thread snapshot");
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let me = GetCurrentProcessId();
        let mut count = 0;
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == me {
                count += 1;
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        count
    };

    let mut memory = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: the EX struct starts with the plain one, and cb says which.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut memory as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    assert!(ok.as_bool(), "process memory counters");

    // WARP may not report video memory; 0 then, before and after alike.
    let mut video = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    let dxgi: IDXGIDevice = gpu.device.cast().expect("IDXGIDevice");
    // SAFETY: plain DXGI calls with a valid out pointer.
    let _ = unsafe {
        dxgi.GetAdapter()
            .and_then(|a| a.cast::<IDXGIAdapter3>())
            .and_then(|a| a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut video))
    };

    Usage {
        handles: i64::from(handles),
        threads,
        private_mb: memory.PrivateUsage as f64 / 1e6,
        gpu_mb: video.CurrentUsage as f64 / 1e6,
    }
}

/// Handles that `count` of the driver's own hardware encoder objects leave
/// behind, created and released with nothing of Booth's around them.
/// NVIDIA's leaves 2 each and never gives them back (seen through 200), so
/// that much of what the Booth encoder leaves is not Booth's.
fn bare_encoder_handles(encoder_name: &str, codec: Codec, count: i64) -> i64 {
    let friendly = encoder_name
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .expect("the driver's encoder name in brackets");
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: match codec {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
        },
    };
    // SAFETY: COM and Media Foundation started and stopped in pairs around
    // plain calls; the activation array is taken over element by element
    // and freed once.
    unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE).expect("MFStartup");
        let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut listed = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut array,
            &mut listed,
        )
        .expect("MFTEnumEx");
        let list: Vec<IMFActivate> = (0..listed as usize)
            .filter_map(|i| array.add(i).read())
            .collect();
        CoTaskMemFree(Some(array as *const _));
        let class = list
            .iter()
            .find(|a| {
                let mut text = windows::core::PWSTR::null();
                let mut length = 0;
                a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut length)
                    .is_ok()
                    && {
                        let name = text.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(text.0 as *const _));
                        name == friendly
                    }
            })
            .and_then(|a| a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute).ok())
            .expect("the driver's encoder in Windows' list");
        drop(list);
        let make = || {
            let _: IMFTransform =
                CoCreateInstance(&class, None, CLSCTX_INPROC_SERVER).expect("the bare encoder");
        };
        make();
        let mut before = 0;
        GetProcessHandleCount(GetCurrentProcess(), &mut before).expect("handle count");
        for _ in 0..count {
            make();
        }
        let mut after = 0;
        GetProcessHandleCount(GetCurrentProcess(), &mut after).expect("handle count");
        let _ = MFShutdown();
        if com.is_ok() {
            CoUninitialize();
        }
        i64::from(after) - i64::from(before)
    }
}

fn open(
    gpu: &Gpu,
    kind: Kind,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
) -> Option<Box<dyn Encoder>> {
    let settings = Settings {
        bitrate: 8_000_000,
        preset: Preset::P1,
    };
    match encode::open_kind_codec(kind, codec, &gpu.device, width, height, fps, &settings) {
        Ok(encoder) => Some(encoder),
        Err(e @ (EncodeError::NoHardwareEncoder { .. } | EncodeError::MediaFoundationMissing)) => {
            println!("skipped {kind} {codec}: {e}");
            None
        }
        Err(e) => panic!("{kind} {codec}: {e}"),
    }
}

// Blank frames: what is counted here does not depend on the picture, and
// WARP maps an NV12 texture differently from a GPU.
fn run(encoder: &mut dyn Encoder, texture: &ID3D11Texture2D) {
    for i in 0..3 {
        encoder
            .encode(&Frame {
                texture,
                index: i,
                force_idr: false,
            })
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
    }
}

#[test]
fn open_and_drop_leaks_nothing() {
    let _turn = common::turn();
    let gpu = gpu();
    println!("on {}", gpu.name);
    for (kind, codec, width, height, fps) in [
        (Kind::MfHardware, Codec::H264, 2560, 1440, 120),
        (Kind::MfHardware, Codec::Hevc, 2560, 1440, 120),
        (Kind::MfSoftware, Codec::H264, 1920, 1080, 60),
    ] {
        let texture = common::texture(&gpu, width, height, DXGI_FORMAT_NV12);
        // The first ones load DLLs and fill Windows' own caches, which stay.
        let Some(mut encoder) = open(&gpu, kind, codec, width, height, fps) else {
            continue;
        };
        run(&mut *encoder, &texture);
        drop(encoder);
        let mut encoder = open(&gpu, kind, codec, width, height, fps).expect("opened before");
        run(&mut *encoder, &texture);
        drop(encoder);

        // What one encoder holds while it is open, which is what a leaked
        // one would keep.
        let closed = usage(&gpu);
        let mut encoder = open(&gpu, kind, codec, width, height, fps).expect("opened before");
        run(&mut *encoder, &texture);
        let name = encoder.name().to_string();
        let held = usage(&gpu);
        drop(encoder);
        println!(
            "{name}: one open holds {} threads, {} handles, {:.1} MB of GPU memory",
            held.threads - closed.threads,
            held.handles - closed.handles,
            held.gpu_mb - closed.gpu_mb
        );

        let before = usage(&gpu);
        for _ in 0..OPENS {
            let mut encoder = open(&gpu, kind, codec, width, height, fps).expect("opened before");
            run(&mut *encoder, &texture);
        }
        let after = usage(&gpu);
        let not_ours = if kind == Kind::MfHardware {
            bare_encoder_handles(&name, codec, OPENS)
        } else {
            0
        };
        let handles = after.handles - before.handles - not_ours;
        println!(
            "{name}, {OPENS} opened, run and dropped: threads {} to {}, GPU {:.1} to {:.1} MB, handles {} to {} ({not_ours} of them left by {OPENS} of the driver's bare encoder objects, so {handles} left, {:.2} an open), private {:.1} to {:.1} MB",
            before.threads,
            after.threads,
            before.gpu_mb,
            after.gpu_mb,
            before.handles,
            after.handles,
            handles as f64 / OPENS as f64,
            before.private_mb,
            after.private_mb
        );
        // One thread of slack for Windows' thread pool; a leaked encoder
        // keeps all of its own, printed above.
        assert!(
            after.threads - before.threads <= 1,
            "{kind} {codec}: {} threads more",
            after.threads - before.threads
        );
        assert!(
            after.gpu_mb - before.gpu_mb < 1.0,
            "{kind} {codec}: {:.1} MB more GPU memory",
            after.gpu_mb - before.gpu_mb
        );
        assert!(
            handles <= OPENS / 2,
            "{kind} {codec}: {handles} handles more over {OPENS} opens, beyond what the driver's own objects leave"
        );
        // Private memory is printed and not held to a number: it swings by
        // up to 50 MB from one encoder to the next as the heap grows and
        // shrinks, and over 400 software encoders it stayed under the same
        // ceiling. A leaked encoder shows in its threads anyway.
    }
}
