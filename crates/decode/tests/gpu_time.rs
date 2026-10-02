// Decode timed on the GPU: the pattern encoded with NVENC in H.264 and in
// HEVC, decoded, and the GPU's times for the pictures taken from later calls,
// as the viewer takes them. Nothing is read back to the CPU here, not even
// the corner the decoder copies to time the picture.

mod common;

use std::time::{Duration, Instant};

use decode::{Codec, GpuTime};

use common::Stream;

const FRAMES: u64 = 240;
// For the last pictures' times after the last decode: the GPU is a couple
// of milliseconds behind at most.
const TAIL: Duration = Duration::from_millis(200);

struct Measured {
    gpu: Vec<Duration>,
    cpu: Vec<Duration>,
    // The whole decode call, the timing and its two flushes included.
    whole: Vec<Duration>,
    // Decodes between a picture's own and the call its time came back from.
    late: Vec<u64>,
    // Came back only after the last decode, when asked again.
    after_the_last: usize,
}

impl Measured {
    fn print(&self, what: &str) {
        let (gpu_median, gpu_p95, gpu_max) = common::spread(&self.gpu);
        let (cpu_median, cpu_p95, _) = common::spread(&self.cpu);
        let (whole_median, whole_p95, _) = common::spread(&self.whole);
        let mut late = self.late.clone();
        late.sort_unstable();
        println!(
            "  {what}: GPU times for {} of {FRAMES} pictures ({} of them only after the last decode), {} to {} decodes late, median {}; GPU median {gpu_median:.3} ms, 95th {gpu_p95:.3}, max {gpu_max:.3}; FFmpeg call median {cpu_median:.3} ms, 95th {cpu_p95:.3}; whole decode call median {whole_median:.3} ms, 95th {whole_p95:.3}",
            self.gpu.len(),
            self.after_the_last,
            late.first().copied().unwrap_or(0),
            late.last().copied().unwrap_or(0),
            late.get(late.len() / 2).copied().unwrap_or(0),
        );
    }

    fn median_ms(&self) -> f64 {
        common::spread(&self.gpu).0
    }
}

fn measure(gpu: &common::Gpu, codec: Codec, width: u32, height: u32) -> Option<Measured> {
    let mut decoder = common::decoder(gpu, codec)?;
    let mut stream = Stream::new(gpu, codec, width, height);
    println!("{codec} {width}x{height} with {}", stream.encoder.name());
    let mut measured = Measured {
        gpu: Vec::new(),
        cpu: Vec::new(),
        whole: Vec::new(),
        late: Vec::new(),
        after_the_last: 0,
    };
    let mut seen = vec![false; FRAMES as usize + 1];
    let mut last_began: Option<Instant> = None;
    let mut times: Vec<GpuTime> = Vec::new();
    let mut take = |times: &mut Vec<GpuTime>, now: u64, measured: &mut Measured| {
        for time in times.drain(..) {
            let unit = usize::try_from(time.unit).expect("a unit number");
            assert!(
                (1..=FRAMES as usize).contains(&unit) && !seen[unit],
                "a GPU time for unit {} came back twice or out of nowhere",
                time.unit
            );
            seen[unit] = true;
            // Oldest first, and read only once the picture was done, which
            // is at began + took or later.
            assert!(last_began.is_none_or(|last| last < time.began));
            last_began = Some(time.began);
            assert!(time.began + time.took <= Instant::now());
            measured.gpu.push(time.took);
            measured.late.push(now - time.unit);
        }
    };
    for n in 0..FRAMES {
        let unit = stream.next(false);
        let called = Instant::now();
        let decoded = decoder
            .decode(&unit.data)
            .unwrap_or_else(|e| panic!("frame {n}: {e}"))
            .unwrap_or_else(|| panic!("frame {n} gave no picture"));
        measured.whole.push(called.elapsed());
        assert_eq!(decoded.unit, n + 1);
        measured.cpu.push(decoded.submit_time);
        decoder.gpu_times(&mut times);
        take(&mut times, decoded.unit, &mut measured);
    }
    let before_tail = measured.gpu.len();
    let tail_ends = Instant::now() + TAIL;
    while measured.gpu.len() < FRAMES as usize && Instant::now() < tail_ends {
        std::thread::sleep(Duration::from_millis(2));
        decoder.gpu_times(&mut times);
        take(&mut times, FRAMES, &mut measured);
    }
    measured.after_the_last = measured.gpu.len() - before_tail;
    Some(measured)
}

// Each codec at both sizes, printed side by side.
fn both_sizes(gpu: &common::Gpu, codec: Codec) -> Option<(Measured, Measured)> {
    let small = measure(gpu, codec, 1280, 720)?;
    let large = measure(gpu, codec, 2560, 1440)?;
    small.print(&format!("{codec} 1280x720"));
    large.print(&format!("{codec} 2560x1440"));
    for (what, measured) in [("720p", &small), ("1440p", &large)] {
        // Nearly all: a measurement is dropped only when the GPU is eight
        // decodes behind or changes its clock mid-picture.
        assert!(
            measured.gpu.len() as u64 >= FRAMES * 95 / 100,
            "{codec} {what}: GPU times for only {} of {FRAMES} pictures",
            measured.gpu.len()
        );
        let (gpu_median, cpu_median) = (measured.median_ms(), common::spread(&measured.cpu).0);
        assert!(
            gpu_median > cpu_median,
            "{codec} {what}: the GPU's {gpu_median:.3} ms is not above FFmpeg's call, {cpu_median:.3} ms"
        );
    }
    // Sanity bounds, not a target: well above a bare query's few
    // microseconds, well under a frame at 100 fps. HEVC takes about half of
    // H.264's time here (0.8 to 0.95 ms at 1440p), so the floor sits well
    // under that.
    let median = large.median_ms();
    assert!(
        (0.25..10.0).contains(&median),
        "{codec} 1440p: GPU median {median:.3} ms"
    );
    assert!(
        small.median_ms() < median,
        "{codec}: 720p takes {:.3} ms on the GPU and 1440p {median:.3}",
        small.median_ms()
    );
    println!(
        "  {codec}: a quarter of the pixels in {:.0} percent of the GPU time",
        small.median_ms() * 100.0 / median
    );
    Some((small, large))
}

#[test]
fn gpu_decode_time() {
    let _turn = common::turn();
    let Some(gpu) = common::nvidia() else { return };
    let Some((h264_small, h264_large)) = both_sizes(&gpu, Codec::H264) else {
        return;
    };
    let Some((hevc_small, hevc_large)) = both_sizes(&gpu, Codec::Hevc) else {
        return;
    };
    for (what, h264, hevc) in [
        ("1280x720", &h264_small, &hevc_small),
        ("2560x1440", &h264_large, &hevc_large),
    ] {
        println!(
            "  {what}: HEVC median {:.3} ms on the GPU against H.264's {:.3}",
            hevc.median_ms(),
            h264.median_ms()
        );
    }
}
