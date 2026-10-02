// A viewer makes a new decoder whenever a share starts or changes size or
// codec, so one that leaks shows up over an evening. Twenty in a row, each
// decoding a few frames so FFmpeg makes its GPU surfaces, must leave the
// process as they found it, and must leave the viewer's device and its
// context with the references they had: one kept would keep the device, and
// everything made on it, alive after the viewer lets go of it.

mod common;

use decode::{Codec, Decoder};
use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIDevice,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
};
use windows::core::{IUnknown_Vtbl, Interface};

use common::{Gpu, Stream};

const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1440;
const DECODERS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct References {
    device: u32,
    context: u32,
}

#[derive(Debug, Clone, Copy)]
struct Counts {
    handles: u32,
    gdi: u32,
    user: u32,
    video_memory: u64,
    references: References,
}

// Handles and video memory do not move when all that leaks is a count.
fn references(object: &impl Interface) -> u32 {
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose vtable starts with IUnknown's. An
    // AddRef and a Release leave it as it was, and Release returns the count
    // that is left.
    unsafe {
        let vtable = *(raw as *const *const IUnknown_Vtbl);
        ((*vtable).AddRef)(raw);
        ((*vtable).Release)(raw)
    }
}

fn counts(gpu: &Gpu) -> Counts {
    // The runtime frees released textures only once the context is flushed.
    // SAFETY: a call on the live immediate context, on this thread only.
    unsafe { gpu.context.Flush() };
    let references = References {
        device: references(&gpu.device),
        context: references(&gpu.context),
    };
    let mut handles = 0;
    // SAFETY: the pseudo handle of this process and a live out parameter.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) }.expect("handle count");
    // SAFETY: as above.
    let gdi = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    // SAFETY: as above.
    let user = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };
    let adapter: IDXGIAdapter3 = gpu
        .device
        .cast::<IDXGIDevice>()
        // SAFETY: a getter on a live interface.
        .and_then(|dxgi| unsafe { dxgi.GetAdapter() })
        .and_then(|adapter| adapter.cast())
        .expect("the device's adapter");
    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    // SAFETY: node 0 of a live adapter and a live out parameter.
    unsafe { adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }
        .expect("video memory use");
    Counts {
        handles,
        gdi,
        user,
        video_memory: info.CurrentUsage,
        references,
    }
}

fn run(gpu: &Gpu, codec: Codec, units: &[Vec<u8>]) {
    let mut decoder = Decoder::new(&gpu.device, codec).unwrap_or_else(|e| panic!("{e}"));
    for (n, unit) in units.iter().enumerate() {
        let decoded = decoder
            .decode(unit)
            .unwrap_or_else(|e| panic!("frame {n}: {e}"));
        assert!(decoded.is_some(), "frame {n} gave no picture");
    }
}

#[test]
fn twenty_h264_decoders() {
    twenty_decoders(Codec::H264);
}

#[test]
fn twenty_hevc_decoders() {
    twenty_decoders(Codec::Hevc);
}

fn twenty_decoders(codec: Codec) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    // Loads FFmpeg and lets the driver set up its decode path once.
    let Some(first) = common::decoder(&gpu, codec) else {
        return;
    };
    drop(first);
    let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..4).map(|_| stream.next(false).data).collect();
    drop(stream);
    run(&gpu, codec, &units);

    let before = counts(&gpu);
    let mut peak = before.video_memory;
    for _ in 0..DECODERS {
        run(&gpu, codec, &units);
        peak = peak.max(counts(&gpu).video_memory);
    }
    let after = counts(&gpu);
    let mb = |bytes: u64| bytes as f64 / (1 << 20) as f64;
    println!(
        "{DECODERS} {codec} decoders at {WIDTH}x{HEIGHT}: handles {} then {}, GDI objects {} then {}, USER objects {} then {}, video memory {:.1} MB then {:.1} MB (highest after a decoder went: {:.1} MB), references to the device {} then {}, to its context {} then {}",
        before.handles,
        after.handles,
        before.gdi,
        after.gdi,
        before.user,
        after.user,
        mb(before.video_memory),
        mb(after.video_memory),
        mb(peak),
        before.references.device,
        after.references.device,
        before.references.context,
        after.references.context
    );
    assert_eq!(
        after.references, before.references,
        "references to the viewer's device and context"
    );
    assert!(
        after.handles <= before.handles + 2,
        "handles went from {} to {} over {DECODERS} decoders",
        before.handles,
        after.handles
    );
    assert_eq!(after.gdi, before.gdi, "GDI objects");
    assert_eq!(after.user, before.user, "USER objects");
    // One decoder's surfaces at this size are about 110 MB in H.264; HEVC's
    // are a little taller, aligned to 128 lines.
    assert!(
        after.video_memory <= before.video_memory + (16 << 20),
        "video memory went from {:.1} MB to {:.1} MB",
        mb(before.video_memory),
        mb(after.video_memory)
    );
}
