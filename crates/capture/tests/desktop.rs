// The real desktop, numbers only: frames are counted and timed, and their
// pixels never leave the GPU.

use std::time::{Duration, Instant};

use capture::{Capture, Monitor, Next, Options};

fn primary() -> Option<Monitor> {
    capture::make_process_dpi_aware().unwrap();
    if capture::adapters().unwrap().is_empty() {
        println!("skipped: this PC has no hardware graphics adapter");
        return None;
    }
    let monitors = capture::monitors().unwrap();
    let monitor = monitors
        .iter()
        .find(|m| m.primary)
        .or(monitors.first())
        .cloned();
    if monitor.is_none() {
        println!("skipped: no monitor is attached to the desktop");
    }
    monitor
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// In ms, negative when `later` is not.
fn after(later: Instant, earlier: Instant) -> f64 {
    if later >= earlier {
        ms(later - earlier)
    } else {
        -ms(earlier - later)
    }
}

fn summary(mut times: Vec<Duration>) -> String {
    if times.is_empty() {
        return "none".to_string();
    }
    times.sort();
    let at = |share: f64| ms(times[((times.len() - 1) as f64 * share).round() as usize]);
    format!(
        "median {:.3} ms, 95th {:.3} ms, longest {:.3} ms",
        at(0.5),
        at(0.95),
        at(1.0)
    )
}

#[test]
fn two_seconds_of_the_primary_monitor() {
    let Some(monitor) = primary() else { return };
    println!("capturing {monitor}");
    let opened = Instant::now();
    let mut capture = Capture::open(&monitor, Options::default()).unwrap();
    println!(
        "opened in {:.1} ms, shader compiled in {:.1} ms, frames of {}x{}",
        ms(opened.elapsed()),
        ms(capture.compile_time()),
        capture.plan().width,
        capture.plan().height
    );
    let (mut frames, mut cursors, mut idles, mut paused, mut skipped) = (0, 0, 0, 0, 0);
    let (mut waits, mut converts, mut gpu, mut idle_waits) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut to_out, mut held_to_out) = (Vec::new(), Vec::new());
    let mut last_gpu = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        let asked = Instant::now();
        let next = capture.next().unwrap();
        let out = Instant::now();
        match next {
            Next::Frame(frame) => {
                frames += 1;
                skipped += frame.skipped;
                let plan = capture.plan();
                assert_eq!((frame.width, frame.height), (plan.width, plan.height));
                assert!(
                    frame.present <= frame.acquired
                        && frame.acquired <= frame.converted
                        && frame.converted <= out,
                    "acquired {:.3} ms, converted {:.3} ms and out {:.3} ms after the present",
                    after(frame.acquired, frame.present),
                    after(frame.converted, frame.present),
                    after(out, frame.present)
                );
                waits.push(frame.acquired - frame.present);
                converts.push(frame.converted - frame.acquired);
                // A frame the fps cap held went out on a later call, when
                // its time came, rather than as soon as it was converted.
                if out - frame.converted > Duration::from_millis(1) {
                    held_to_out.push(out - frame.present);
                } else {
                    to_out.push(out - frame.present);
                }
                if let Some(time) = frame.gpu_convert
                    && frame.gpu_convert != last_gpu
                {
                    gpu.push(time);
                    last_gpu = frame.gpu_convert;
                }
            }
            Next::Cursor(_) => cursors += 1,
            Next::Idle => {
                idles += 1;
                idle_waits.push(asked.elapsed());
            }
            Next::Paused(reason) => {
                paused += 1;
                println!("paused: {reason}");
            }
        }
    }
    println!(
        "in 2 s: {frames} frames, {skipped} skipped by the cap, {cursors} pointer updates, {idles} idle, {paused} paused"
    );
    println!("present to acquired: {}", summary(waits));
    println!("acquired to converted: {}", summary(converts));
    println!("conversion on the GPU: {}", summary(gpu));
    println!(
        "present to out, sent at once: {} ({})",
        summary(to_out.clone()),
        to_out.len()
    );
    println!(
        "present to out, held for the cap first: {} ({})",
        summary(held_to_out.clone()),
        held_to_out.len()
    );
    println!("idle waits: {}", summary(idle_waits.clone()));
    // A still desktop gives only Idle, one about every 100 ms.
    assert!(frames + cursors + idles + paused > 0);
    for wait in idle_waits {
        assert!(
            wait >= Duration::from_millis(100),
            "Idle after {:.3} ms",
            ms(wait)
        );
    }

    // Windows allows one live duplication of a monitor per process.
    let second = Capture::open(&monitor, Options::default())
        .err()
        .expect("a second duplication opened");
    println!("a second open while the first lives: {second}");
    assert!(
        second.to_string().contains("already duplicating"),
        "{second}"
    );
    drop(capture);
    Capture::open(&monitor, Options::default()).unwrap();
}
