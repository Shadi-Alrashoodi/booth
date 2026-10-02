use std::time::{Duration, Instant};

use capture::{Options, Pattern, Rotation};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

fn device() -> Option<ID3D11Device> {
    let adapters = capture::adapters().unwrap();
    let Some(adapter) = adapters.first() else {
        println!("skipped: this PC has no hardware graphics adapter");
        return None;
    };
    Some(capture::device_on(adapter).unwrap())
}

fn percentile(sorted: &[Duration], share: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * share).round() as usize]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[test]
fn the_pattern_paces_itself_to_its_fps() {
    let Some(device) = device() else { return };
    let mut pattern = Pattern::new(&device, 1280, 720, 120).unwrap();
    let mut presents = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        presents.push(pattern.next().unwrap().present);
    }
    let mut gaps: Vec<Duration> = presents.windows(2).map(|pair| pair[1] - pair[0]).collect();
    gaps.sort();
    let median = percentile(&gaps, 0.5);
    println!(
        "120 fps pattern: {} frames in 1 s, interval median {:.3} ms, 95th {:.3} ms, longest {:.3} ms",
        presents.len(),
        ms(median),
        ms(percentile(&gaps, 0.95)),
        ms(*gaps.last().unwrap())
    );
    assert!(
        (115..=122).contains(&presents.len()),
        "{} frames",
        presents.len()
    );
    assert!(
        (ms(median) - 1000.0 / 120.0).abs() < 0.5,
        "median interval {:.3} ms",
        ms(median)
    );
}

#[test]
fn the_pattern_hands_out_the_device_its_frames_are_on() {
    let Some(device) = device() else { return };
    let pattern = Pattern::new(&device, 640, 360, 0).unwrap();
    assert_eq!(
        pattern.device(),
        &device,
        "the encoder opens on this device"
    );
}

#[test]
fn an_unpaced_pattern_does_not_wait() {
    let Some(device) = device() else { return };
    let mut pattern = Pattern::new(&device, 640, 360, 0).unwrap();
    let start = Instant::now();
    for number in 0..200 {
        assert_eq!(pattern.next().unwrap().number, number);
    }
    let took = start.elapsed();
    println!("200 unpaced 640x360 frames in {:.1} ms", ms(took));
    assert!(took < Duration::from_secs(2), "{took:?}");
}

// Prints the colour conversion's time on the GPU, which may take about
// 0.3 ms of the 20 ms from capture to display.
#[test]
fn conversion_gpu_time() {
    let Some(device) = device() else { return };
    let unpaced = Options {
        max_fps: 0,
        ..Options::default()
    };
    for (width, height) in [(2560, 1440), (3840, 2160)] {
        let mut pattern =
            Pattern::with_source(&device, width, height, Rotation::Identity, unpaced).unwrap();
        let mut times = Vec::new();
        let mut frames = 0;
        // Timestamps come back a frame or two late; each frame reports the
        // newest one in, so repeats are counted once.
        let mut last = None;
        while times.len() < 200 && frames < 2000 {
            let frame = pattern.next().unwrap();
            frames += 1;
            // Give the GPU room, as a real 120 fps source would.
            std::thread::sleep(Duration::from_millis(2));
            if let Some(time) = frame.gpu_convert
                && frame.gpu_convert != last
            {
                times.push(time);
                last = frame.gpu_convert;
            }
        }
        assert!(!times.is_empty(), "no GPU timestamps came back");
        times.sort();
        println!(
            "conversion of {width}x{height} to {}x{}: GPU median {:.3} ms, 95th {:.3} ms over {} frames; shader compiled in {:.1} ms",
            pattern.width(),
            pattern.height(),
            ms(percentile(&times, 0.5)),
            ms(percentile(&times, 0.95)),
            times.len(),
            ms(pattern.compile_time())
        );
    }
}

// The share thread opens the capture and the encoder takes frames on it, but
// the viewer and tests move them between threads.
#[test]
fn capture_pattern_and_frames_can_move_to_another_thread() {
    fn movable<T: Send>() {}
    movable::<capture::Capture>();
    movable::<Pattern>();
    movable::<capture::Frame>();
    movable::<capture::Next>();
}
