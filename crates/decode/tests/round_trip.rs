// The pattern encoded with NVENC, in H.264 and in HEVC, and decoded here
// frame by frame: every access unit must give its own picture back from
// the same call.

mod common;

use std::time::{Duration, Instant};

use decode::Codec;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use common::{Reader, Stream};

const FRAMES: u64 = 240;
// Forced after the stream, to time the largest frames there are.
const IDRS: u64 = 16;

#[derive(Default)]
struct Times {
    submit: Vec<Duration>,
    gpu: Vec<Duration>,
    // HEVC only: reading every SPS in the frame, which the decoder does in
    // Rust before FFmpeg sees the frame.
    sps: Vec<Duration>,
    bytes: usize,
}

impl Times {
    fn print(&self, what: &str) {
        let (median, p95, max) = common::spread(&self.submit);
        let (gpu_median, gpu_p95, gpu_max) = common::spread(&self.gpu);
        println!(
            "  {what}: {} frames, {:.1} KB each; send to frame back median {median:.3} ms, 95th {p95:.3}, max {max:.3}; send to picture done on the GPU median {gpu_median:.3} ms, 95th {gpu_p95:.3}, max {gpu_max:.3}",
            self.submit.len(),
            self.bytes as f64 / self.submit.len() as f64 / 1000.0
        );
        if !self.sps.is_empty() {
            let (median, p95, max) = common::spread(&self.sps);
            println!(
                "    reading every SPS before FFmpeg: median {:.1} us, 95th {:.1}, max {:.1}",
                median * 1000.0,
                p95 * 1000.0,
                max * 1000.0
            );
        }
    }
}

fn round_trip(codec: Codec, width: u32, height: u32) {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some(mut decoder) = common::decoder(&gpu, codec) else {
        return;
    };
    let mut stream = Stream::new(&gpu, codec, width, height);
    let mut reader = Reader::new(&gpu);
    let mut first = Times::default();
    let mut predicted = Times::default();
    let mut idrs = Times::default();

    for n in 0..FRAMES + IDRS {
        let unit = stream.next(n >= FRAMES);
        assert_eq!(unit.index, n);
        assert_eq!(
            unit.idr,
            n == 0 || n >= FRAMES,
            "frame {n}: an IDR where none was asked for, or none where one was"
        );
        let sps = (codec == Codec::Hevc).then(|| {
            let started = Instant::now();
            let sizes: Vec<_> = annexb::hevc::coded_sizes(&unit.data).collect();
            let took = started.elapsed();
            // NVENC puts one in front of every IDR and none anywhere else.
            assert_eq!(
                sizes.len(),
                usize::from(unit.idr),
                "frame {n}: SPSs read {sizes:?}"
            );
            assert!(sizes.iter().all(Option::is_some), "frame {n}: {sizes:?}");
            took
        });
        let started = Instant::now();
        let decoded = decoder
            .decode(&unit.data)
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| {
                panic!(
                    "frame {n} ({} bytes) gave no picture: the decoder held it back",
                    unit.len()
                )
            });
        let gpu_done = reader.wait_for_picture(&decoded) - started;
        let times = match n {
            0 => &mut first,
            n if n < FRAMES => &mut predicted,
            _ => &mut idrs,
        };
        times.submit.push(decoded.submit_time);
        times.gpu.push(gpu_done);
        times.sps.extend(sps);
        times.bytes += unit.len();

        assert_eq!(
            (decoded.width, decoded.height),
            (width, height),
            "frame {n}"
        );
        if n == 0 {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: a getter on a live texture.
            unsafe { decoded.texture.GetDesc(&mut desc) };
            println!(
                "{codec} {width}x{height} from {} on {}: FFmpeg's pool is one {}x{} texture of {} slices, bind flags {:#x}",
                stream.encoder.name(),
                gpu.name,
                desc.Width,
                desc.Height,
                desc.ArraySize,
                desc.BindFlags
            );
            assert_eq!(desc.Format, DXGI_FORMAT_NV12);
            assert!(desc.Width >= width && desc.Height >= height);
            assert!(decoded.index < desc.ArraySize);
            assert_ne!(desc.BindFlags & D3D11_BIND_DECODER.0 as u32, 0);
            assert_ne!(
                desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32,
                0,
                "the viewer draws from the decoded texture, so it needs shader resource binding"
            );
        }
        assert_eq!(
            reader.frame_number(&decoded),
            Some(n as u32),
            "frame {n} decoded to the wrong picture"
        );
    }

    first.print("first frame, an IDR, with the GPU decoder's setup");
    predicted.print("P frames");
    idrs.print("forced IDRs");
}

#[test]
fn h264_720p() {
    round_trip(Codec::H264, 1280, 720);
}

#[test]
fn h264_1440p() {
    round_trip(Codec::H264, 2560, 1440);
}

#[test]
fn hevc_720p() {
    round_trip(Codec::Hevc, 1280, 720);
}

#[test]
fn hevc_1440p() {
    round_trip(Codec::Hevc, 2560, 1440);
}
