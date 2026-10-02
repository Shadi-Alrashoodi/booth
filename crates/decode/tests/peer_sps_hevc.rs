// tests/peer_sps.rs for HEVC: SPSs a Booth sharer never sends but a
// friend's broken or infected PC could, spliced into NVENC's own HEVC
// stream. HEVC's SPS carries the size, the bit depth and the reordering
// near its start, so each case rewrites those fields and copies every other
// bit as it was, which keeps the PPS and the slice headers parsing against
// it.

mod common;

use std::time::{Duration, Instant};

use decode::{Codec, DecodeError, Decoder};

use common::{Reader, Stream};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const NAL_SPS: u8 = 33;

fn kind(nal: &annexb::Nal<'_>) -> u8 {
    nal.data[0] >> 1 & 0x3f
}

// The bits of an RBSP, most significant first, emulation prevention taken
// out.
struct BitReader {
    bytes: Vec<u8>,
    at: usize,
    // Where the stop bit of rbsp_trailing_bits is.
    end: usize,
}

impl BitReader {
    fn new(payload: &[u8]) -> BitReader {
        let mut bytes = Vec::with_capacity(payload.len());
        let mut zeros = 0;
        for &byte in payload {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            bytes.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let last = *bytes.last().expect("an SPS with a payload");
        assert_ne!(last, 0, "an RBSP ends in its stop bit");
        let end = bytes.len() * 8 - 1 - last.trailing_zeros() as usize;
        BitReader { bytes, at: 0, end }
    }

    fn bits(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            assert!(self.at < self.end, "read past the end of the SPS");
            let bit = self.bytes[self.at / 8] >> (7 - self.at % 8) & 1;
            value = value << 1 | u32::from(bit);
            self.at += 1;
        }
        value
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bits(1) == 0 {
            zeros += 1;
        }
        (1 << zeros) - 1 + self.bits(zeros)
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            bytes: Vec::new(),
            used: 8,
        }
    }

    fn bits(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            if self.used == 8 {
                self.bytes.push(0);
                self.used = 0;
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= ((value >> bit & 1) as u8) << (7 - self.used);
            self.used += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = value + 1;
        let len = 32 - coded.leading_zeros();
        self.bits(0, len - 1);
        self.bits(coded, len);
    }

    // rbsp_trailing_bits, emulation prevention, start code and the two
    // header bytes.
    fn nal(mut self, header: [u8; 2]) -> Vec<u8> {
        self.bits(1, 1);
        let mut out = vec![0, 0, 0, 1, header[0], header[1]];
        let mut zeros = 0;
        for byte in self.bytes {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Changes {
    // The coded size; None keeps the SPS's own and its conformance window.
    size: Option<(u32, u32)>,
    depth: u32,
    // sps_max_num_reorder_pics, and a DPB that holds that many.
    reorder: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Found {
    width: u32,
    height: u32,
    reorder: u32,
}

// `sps` is the NAL unit as it came, header included. Returns it in Annex B
// with the changes made, and what the original said.
fn rewrite(sps: &[u8], changes: Changes) -> (Vec<u8>, Found) {
    let mut r = BitReader::new(&sps[2..]);
    let mut w = BitWriter::new();
    let copy = |r: &mut BitReader, w: &mut BitWriter, count| w.bits(r.bits(count), count);
    copy(&mut r, &mut w, 4); // sps_video_parameter_set_id
    let sub_layers = r.bits(3);
    assert_eq!(sub_layers, 0, "NVENC sends one temporal layer");
    w.bits(sub_layers, 3);
    copy(&mut r, &mut w, 1); // sps_temporal_id_nesting_flag
    // profile_tier_level(1, 0)
    copy(&mut r, &mut w, 3); // general_profile_space, general_tier_flag
    let profile = r.bits(5);
    let compatible = r.bits(32);
    if changes.depth == 8 {
        w.bits(profile, 5);
        w.bits(compatible, 32);
    } else {
        // Main 10, and compatible with it alone.
        w.bits(2, 5);
        w.bits(1 << (31 - 2), 32);
    }
    copy(&mut r, &mut w, 4); // progressive, interlaced, non-packed, frame-only
    copy(&mut r, &mut w, 32); // the 43 constraint bits
    copy(&mut r, &mut w, 11);
    copy(&mut r, &mut w, 1); // general_inbld_flag
    copy(&mut r, &mut w, 8); // general_level_idc
    let id = r.ue();
    w.ue(id);
    let chroma = r.ue();
    assert_eq!(chroma, 1, "NVENC sends 4:2:0");
    w.ue(chroma);
    let (width, height) = (r.ue(), r.ue());
    let (coded_width, coded_height) = changes.size.unwrap_or((width, height));
    w.ue(coded_width);
    w.ue(coded_height);
    let window = r.bits(1) == 1;
    let offsets: Vec<u32> = if window {
        (0..4).map(|_| r.ue()).collect()
    } else {
        Vec::new()
    };
    // The window crops the coded size; another size gets none.
    if changes.size.is_none() {
        w.bits(window as u32, 1);
        for offset in offsets {
            w.ue(offset);
        }
    } else {
        w.bits(0, 1);
    }
    let (luma, chroma) = (r.ue(), r.ue());
    assert_eq!((luma, chroma), (0, 0), "NVENC sends 8 bits");
    w.ue(changes.depth - 8);
    w.ue(changes.depth - 8);
    let poc = r.ue();
    w.ue(poc);
    copy(&mut r, &mut w, 1); // sps_sub_layer_ordering_info_present_flag
    let (dpb, reorder, latency) = (r.ue(), r.ue(), r.ue());
    match changes.reorder {
        Some(frames) => {
            w.ue(dpb.max(frames));
            w.ue(frames);
        }
        None => {
            w.ue(dpb);
            w.ue(reorder);
        }
    }
    w.ue(latency);
    while r.at < r.end {
        copy(&mut r, &mut w, 1);
    }
    (
        w.nal([sps[0], sps[1]]),
        Found {
            width,
            height,
            reorder,
        },
    )
}

// The IDR access unit with its SPS swapped for `sps`.
fn with_sps(idr: &[u8], sps: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in annexb::nal_units(idr) {
        if kind(&nal) == NAL_SPS {
            out.extend_from_slice(sps);
        } else {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal.data);
        }
    }
    out
}

struct Setup {
    gpu: common::Gpu,
    decoder: Decoder,
    reader: Reader,
    // Frame 0 an IDR, 1 and 2 P frames, 3 an IDR, 4 a P frame.
    units: Vec<Vec<u8>>,
    sps: Vec<u8>,
    _turn: std::sync::MutexGuard<'static, ()>,
}

fn setup() -> Option<Setup> {
    let turn = common::turn();
    let gpu = common::nvidia()?;
    let decoder = common::decoder(&gpu, Codec::Hevc)?;
    let mut stream = Stream::new(&gpu, Codec::Hevc, WIDTH, HEIGHT);
    let units: Vec<Vec<u8>> = (0..5).map(|n| stream.next(n == 3).data).collect();
    drop(stream);
    let sps = annexb::nal_units(&units[0])
        .find(|nal| kind(nal) == NAL_SPS)
        .expect("an SPS in the first frame")
        .data
        .to_vec();
    Some(Setup {
        reader: Reader::new(&gpu),
        gpu,
        decoder,
        units,
        sps,
        _turn: turn,
    })
}

impl Setup {
    fn whole(&mut self, n: usize) {
        let decoded = self
            .decoder
            .decode(&self.units[n])
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| panic!("frame {n} gave no picture"));
        assert_eq!(
            self.reader.frame_number(&decoded),
            Some(n as u32),
            "frame {n}"
        );
    }

    fn idr_with(&self, changes: Changes) -> Vec<u8> {
        with_sps(&self.units[0], &rewrite(&self.sps, changes).0)
    }
}

const AS_SENT: Changes = Changes {
    size: None,
    depth: 8,
    reorder: None,
};

#[test]
fn unchanged_rewrite() {
    // Checks the rewriter: with nothing changed it must give the SPS back
    // bit for bit, or the refusals below would prove nothing.
    let Some(mut s) = setup() else { return };
    let (same, found) = rewrite(&s.sps, AS_SENT);
    // NVENC codes whole blocks and crops the rest away with the conformance
    // window: 720 lines were coded as 736 on my PC.
    println!("NVENC's SPS for {WIDTH}x{HEIGHT}: {found:?}");
    assert!(found.width >= WIDTH && found.height >= HEIGHT, "{found:?}");
    assert_eq!(found.reorder, 0, "NVENC asks for no reordering");
    assert_eq!(&same[4..], &s.sps[..]);
    let decoded = s
        .decoder
        .decode(&s.idr_with(AS_SENT))
        .unwrap_or_else(|e| panic!("{e}"))
        .expect("a picture");
    assert_eq!(s.reader.frame_number(&decoded), Some(0));
    s.whole(1);
    s.whole(2);
}

#[test]
fn size_the_gpu_refuses() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    // Inside Booth's limit, and a size the probe says this GPU's HEVC
    // decoder does not take.
    let (width, height) = (8192, 64);
    let probed = decode::probe(&s.gpu.device, Codec::Hevc, width, height);
    println!("the probe: {probed:?}");
    if probed.is_ok() {
        println!(
            "skipped: the {} decodes HEVC at {width}x{height}",
            s.gpu.name
        );
        return;
    }
    let idr = s.idr_with(Changes {
        size: Some((width, height)),
        ..AS_SENT
    });
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(
        matches!(
            err,
            DecodeError::Unsupported {
                width: 8192,
                height: 64,
                ..
            }
        ),
        "{err:?}"
    );
    s.whole(3);
    s.whole(4);
}

#[test]
fn reordering_refused() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    let idr = s.idr_with(Changes {
        reorder: Some(2),
        ..AS_SENT
    });
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(matches!(err, DecodeError::HeldBack { .. }), "{err:?}");
    // The reset took every reference, so a P frame shows nothing, even with
    // the stream's own parameter sets in front of it again. FFmpeg would
    // make it a picture of whatever its pool held.
    let mut sets: Vec<u8> = annexb::nal_units(&s.units[0])
        .filter(|nal| (32..=34).contains(&kind(nal)))
        .flat_map(|nal| [&[0u8, 0, 0, 1][..], nal.data].concat())
        .collect();
    sets.extend_from_slice(&s.units[1]);
    let after = s.decoder.decode(&sets);
    assert!(matches!(after, Ok(None)), "{after:?}");
    // Nothing of the refused stream comes out later with these.
    s.whole(3);
    s.whole(4);
}

#[test]
fn ten_bit_refused_as_format() {
    // Most GPUs decode HEVC Main 10, but into P010, which the viewer does
    // not draw, so it must be refused before FFmpeg sets the GPU up for it.
    let Some(mut s) = setup() else { return };
    s.whole(0);
    let idr = s.idr_with(Changes {
        depth: 10,
        ..AS_SENT
    });
    let err = s.decoder.decode(&idr).unwrap_err();
    println!("{err}");
    assert!(
        matches!(&err, DecodeError::WrongFormat { profile, .. } if profile == "Main 10"),
        "{err:?}"
    );
    s.whole(3);
    s.whole(4);
}

// This process's committed memory now, and the most it has had committed at
// any moment so far.
fn commit() -> (u64, u64) {
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
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
    (
        counters.PrivateUsage as u64,
        counters.PeakPagefileUsage as u64,
    )
}

fn video_memory(gpu: &common::Gpu) -> u64 {
    use windows::Win32::Graphics::Dxgi::{
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIDevice,
    };
    use windows::core::Interface;
    // The runtime frees released textures only once the context is flushed.
    // SAFETY: a call on the live immediate context, on this thread only.
    unsafe { gpu.context.Flush() };
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
    info.CurrentUsage
}

// While FFmpeg read the SPS first, refusing 16384x16000 committed about
// 165 MB for 25 to 28 ms (FFmpeg 8.1.3). Read in Rust first, it is refused
// in about 0.01 ms before anything is sized. The commit is what tells the
// two apart, and its bound leaves room for what the rest of the process
// does meanwhile. The time bound only catches a refusal gone badly slow: a
// single preemption of this thread on a busy gaming PC can pass 20 ms.
const MOST_COMMITTED: u64 = 1 << 20;
const LONGEST: Duration = Duration::from_millis(100);

#[test]
fn oversized_refused_before_ffmpeg() {
    // 8192x4608 is four times the area Booth takes; 16384x16000 about the
    // largest FFmpeg's own check lets through (16248x16248 it turns down
    // itself). Each is refused from a stream in its own size, and the next
    // IDR of that stream decodes whole.
    //
    // Unlike its H.264 decoder, FFmpeg's HEVC decoder sizes tables in CPU
    // memory by the SPS before it asks for a format, so the decoder reads
    // every SPS itself first (src/guard.rs) and FFmpeg never sees these.
    // The peak commit after the call, less the commit before it, is at least
    // the most the call had committed at once.
    let Some(mut s) = setup() else { return };
    let mb = |bytes: u64| bytes as f64 / (1 << 20) as f64;
    for (width, height) in [(8192, 4608), (16384, 16000)] {
        s.whole(3);
        s.whole(4);
        let idr = s.idr_with(Changes {
            size: Some((width, height)),
            ..AS_SENT
        });
        let ((cpu_before, peak_before), gpu_before) = (commit(), video_memory(&s.gpu));
        // Earlier tests leave the peak above the commit now, which would
        // hide a call that stays under it and add to one that does not.
        // Committing the gap, untouched, starts the call at the peak.
        let gap = Vec::<u8>::with_capacity(peak_before.saturating_sub(cpu_before) as usize);
        std::hint::black_box(&gap);
        let (start, _) = commit();
        let started = Instant::now();
        let err = s.decoder.decode(&idr).unwrap_err();
        let took = started.elapsed();
        let (_, peak) = commit();
        drop(gap);
        let ((cpu, _), gpu) = (commit(), video_memory(&s.gpu));
        let most = peak.saturating_sub(start);
        println!(
            "{err}; in {:.3} ms, at most {:.3} MB committed during the call (the peak before was {:.1} MB above the commit), afterwards private bytes {:+.1} MB, video memory {:+.1} MB",
            took.as_secs_f64() * 1000.0,
            mb(most),
            mb(peak_before.saturating_sub(cpu_before)),
            mb(cpu) - mb(cpu_before),
            mb(gpu) - mb(gpu_before)
        );
        assert!(
            matches!(err, DecodeError::Oversized { width: w, height: h } if (w, h) == (width, height)),
            "{err:?}"
        );
        assert!(
            most < MOST_COMMITTED && took < LONGEST,
            "{width}x{height}: up to {:.3} MB committed and {took:?} to refuse it",
            mb(most)
        );
        assert!(
            cpu <= cpu_before + (16 << 20) && gpu <= gpu_before + (16 << 20),
            "{width}x{height}: private bytes {:.1} MB to {:.1} MB, video memory {:.1} MB to {:.1} MB",
            mb(cpu_before),
            mb(cpu),
            mb(gpu_before),
            mb(gpu)
        );
    }
    s.whole(3);
    s.whole(4);
}

#[test]
fn sps_in_p_frame_and_cut_sps() {
    let Some(mut s) = setup() else { return };
    s.whole(0);
    // FFmpeg takes an SPS from any frame, and the slice after it in the
    // same frame brings it in.
    let (huge, _) = rewrite(
        &s.sps,
        Changes {
            size: Some((16384, 16000)),
            ..AS_SENT
        },
    );
    let err = s
        .decoder
        .decode(&[&huge[..], &s.units[1]].concat())
        .unwrap_err();
    println!("{err}");
    assert!(
        matches!(
            err,
            DecodeError::Oversized {
                width: 16384,
                height: 16000
            }
        ),
        "{err:?}"
    );
    // The stream's own SPS cut in half, in its IDR.
    let (whole, _) = rewrite(&s.sps, AS_SENT);
    let err = s
        .decoder
        .decode(&with_sps(&s.units[0], &whole[..whole.len() / 2]))
        .unwrap_err();
    println!("{err}");
    assert!(matches!(err, DecodeError::UnreadableSps), "{err:?}");
    s.whole(3);
    s.whole(4);
}
