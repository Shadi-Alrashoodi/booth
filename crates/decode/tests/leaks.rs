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

fn private_bytes() -> u64 {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the pseudo handle of this process and a live structure of the
    // size given, which the EX form extends.
    unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        )
    }
    .ok()
    .expect("this process's memory counters");
    counters.PrivateUsage as u64
}

// One message of `blocks` times 255 bytes of type 5, the type FFmpeg keeps.
fn sei(header: &[u8], blocks: usize) -> Vec<u8> {
    let mut sei = vec![0, 0, 1];
    sei.extend_from_slice(header);
    sei.push(5);
    sei.resize(sei.len() + blocks, 0xff);
    sei.push(0);
    sei.resize(sei.len() + blocks * 255, 0x11);
    sei.push(0x80);
    sei
}

// Start code and header, then ones and zeros with the stop bit and emulation
// prevention added.
fn h264_nal(header: u8, bits: &str) -> Vec<u8> {
    let bits: Vec<u8> = bits
        .bytes()
        .chain(*b"1")
        .map(|bit| bit - b'0')
        .chain([0; 7])
        .collect();
    let mut out = vec![0, 0, 1, header];
    let mut zeros = 0;
    for byte in bits.as_chunks::<8>().0 {
        let byte = byte.iter().fold(0, |acc, bit| acc << 1 | bit);
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn ue(value: u32) -> String {
    let coded = value + 1;
    let len = 32 - coded.leading_zeros() as usize;
    format!("{:0>width$b}", coded, width = 2 * len - 1)
}

// An SPS and PPS of their own for 640x352 in fields, and the top field of an
// IDR with no bottom field to come, which leaves FFmpeg waiting for one.
fn lone_field() -> Vec<u8> {
    let sps = [
        format!("{:08b}", 100),
        format!("{:08b}", 0),
        format!("{:08b}", 51),
        ue(1),            // sps_id
        ue(1),            // chroma_format_idc: 4:2:0
        ue(0),            // bit_depth_luma_minus8
        ue(0),            // bit_depth_chroma_minus8
        "00".to_string(), // qpprime_y_zero_transform_bypass, seq_scaling_matrix_present
        ue(0),            // log2_max_frame_num_minus4
        ue(2),            // pic_order_cnt_type
        ue(1),            // max_num_ref_frames
        "0".to_string(),  // gaps_in_frame_num_allowed
        ue(39),           // width in macroblocks less 1
        ue(10),           // height in pairs of them less 1
        "0".to_string(),  // frame_mbs_only
        "0".to_string(),  // mb_adaptive_frame_field
        "1".to_string(),  // direct_8x8_inference
        "0".to_string(),  // frame_cropping
        "0".to_string(),  // vui_parameters_present
    ]
    .concat();
    let pps = [
        ue(1),             // pps_id
        ue(1),             // sps_id
        "00".to_string(),  // entropy_coding_mode, bottom_field_pic_order_in_frame_present
        ue(0),             // num_slice_groups_minus1
        ue(0),             // num_ref_idx_l0_default_active_minus1
        ue(0),             // num_ref_idx_l1_default_active_minus1
        "000".to_string(), // weighted_pred, weighted_bipred_idc
        ue(0),             // pic_init_qp_minus26
        ue(0),             // pic_init_qs_minus26
        ue(0),             // chroma_qp_index_offset
        "000".to_string(), // deblocking_filter_control, constrained_intra_pred, redundant_pic_cnt
    ]
    .concat();
    let slice = [
        ue(0),              // first_mb_in_slice
        ue(7),              // slice_type: all I
        ue(1),              // pps_id
        "0000".to_string(), // frame_num
        "10".to_string(),   // field_pic, bottom_field
        ue(0),              // idr_pic_id
        "00".to_string(),   // no_output_of_prior_pics, long_term_reference
        ue(0),              // slice_qp_delta
    ]
    .concat();
    [
        h264_nal(0x67, &sps),
        h264_nal(0x68, &pps),
        h264_nal(0x65, &slice),
    ]
    .concat()
}

// After a real IDR, frames of nothing but SEI: FFmpeg keeps every message for
// the picture it expects, and only a flush lets go of them. A flush takes the
// references too, so the stream waits for its next IDR. FFmpeg's H.264 decoder
// keeps them only after a lone field, so it gets one first.
//
// Kept, the messages raise private bytes by 2 MB or more in every window of
// 2000 frames. With the flush they still rise by what the process allocates
// for the first time: up to about 3 MB in the first window, and once about
// 0.9 MB in a later one when the twenty-decoder tests had run first. H.264's
// SEI-only units still reach FFmpeg after the flush and add under 0.5 MB a
// window until about 26,000 frames have gone; HEVC sends nothing after it. So
// the first window is not counted, and of the three after it the one that rose
// least must stay under 1 MB.
#[test]
fn sei_with_no_picture_goes() {
    const FRAMES: usize = 2000;
    const WINDOWS: usize = 3;
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    for (codec, header) in [(Codec::H264, vec![6u8]), (Codec::Hevc, vec![39 << 1, 1])] {
        let Some(mut decoder) = common::decoder(&gpu, codec) else {
            return;
        };
        let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
        let units: Vec<Vec<u8>> = (0..5).map(|n| stream.next(n == 3).data).collect();
        drop(stream);
        let picture = |decoder: &mut Decoder, n: usize| {
            let decoded = decoder
                .decode(&units[n])
                .unwrap_or_else(|e| panic!("{codec} frame {n}: {e}"));
            assert!(decoded.is_some(), "{codec} frame {n} gave no picture");
        };
        picture(&mut decoder, 0);
        if codec == Codec::H264 {
            let field = decoder.decode(&lone_field());
            assert!(matches!(field, Ok(None)), "a lone field: {field:?}");
        }
        let sei = sei(&header, 3);
        let mut top = private_bytes();
        let mut rises = Vec::new();
        for _ in 0..=WINDOWS {
            let before = top;
            for _ in 0..FRAMES {
                let _ = decoder.decode(&sei);
                top = top.max(private_bytes());
            }
            rises.push((top - before) >> 10);
        }
        println!(
            "{codec}: {} windows of {FRAMES} frames of {} bytes of SEI, private bytes up {rises:?} KB",
            rises.len(),
            sei.len()
        );
        // The flush past 256 KB comes early in the first window, so the windows
        // counted show only that nothing more is kept after it, not how much
        // was kept before; src/decoder.rs holds that to 256 KB.
        let least = rises[1..].iter().min().copied().unwrap_or(0);
        assert!(least < 1 << 10, "{codec}: private bytes up {rises:?} KB");
        let after = decoder.decode(&units[1]);
        assert!(
            !matches!(after, Ok(Some(_))),
            "{codec}: a P frame after the flush: {after:?}"
        );
        picture(&mut decoder, 3);
        picture(&mut decoder, 4);
    }
}

// A stream that shows pictures keeps nothing, however much SEI it has sent in
// all: if the count did not start again at a picture, a real stream's few
// hundred bytes with each IDR would add up to a flush and take its references.
#[test]
fn sei_between_pictures_stays() {
    const ROUNDS: usize = 8;
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    for (codec, header) in [(Codec::H264, vec![6u8]), (Codec::Hevc, vec![39 << 1, 1])] {
        let Some(mut decoder) = common::decoder(&gpu, codec) else {
            return;
        };
        let mut stream = Stream::new(&gpu, codec, WIDTH, HEIGHT);
        let units: Vec<Vec<u8>> = (0..ROUNDS).map(|_| stream.next(false).data).collect();
        drop(stream);
        // About 51 KB each, so eight pass the 256 KB the decoder keeps at most.
        let sei = sei(&header, 200);
        for (n, unit) in units.iter().enumerate() {
            let decoded = decoder
                .decode(unit)
                .unwrap_or_else(|e| panic!("{codec} frame {n}: {e}"));
            assert!(decoded.is_some(), "{codec} frame {n} gave no picture");
            let _ = decoder.decode(&sei);
        }
    }
}
