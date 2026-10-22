// The colour conversion shader against the CPU reference, on test pictures
// only.

use std::time::Instant;

use capture::reference::{region_to_nv12, to_nv12};
use capture::{
    Nv12Image, Options, Pattern, Plan, Region, Rotation, SMALLEST_SIDE, pattern_image,
    read_frame_number,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

fn device() -> Option<ID3D11Device> {
    let adapters = capture::adapters().unwrap();
    let Some(adapter) = adapters.first() else {
        println!("skipped: this PC has no hardware graphics adapter");
        return None;
    };
    Some(capture::device_on(adapter).unwrap())
}

fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
    bgra.iter()
        .copied()
        .cycle()
        .take((width * height * 4) as usize)
        .collect()
}

// The limits a share opens capture with, and frames as fast as the test
// asks for them.
fn unpaced() -> Options {
    Options {
        max_fps: 0,
        ..Options::default()
    }
}

// 0 is no limit on that side.
fn under(max_width: u32, max_height: u32) -> Options {
    Options {
        max_width,
        max_height,
        max_fps: 0,
    }
}

// On the GPU, the way Capture converts a monitor of this size.
fn convert(
    device: &ID3D11Device,
    bgra: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
    options: Options,
) -> Nv12Image {
    let mut pattern = Pattern::with_source(device, width, height, rotation, options).unwrap();
    let frame = pattern.convert(bgra).unwrap();
    pattern.read_back(&frame).unwrap()
}

// Largest difference in Y and in U or V, with where the Y one is.
fn largest_differences(got: &Nv12Image, want: &Nv12Image) -> (u8, (u32, u32), u8) {
    assert_eq!((got.width, got.height), (want.width, want.height));
    let mut luma = (0, (0, 0));
    for (at, (a, b)) in got.y.iter().zip(&want.y).enumerate() {
        let diff = a.abs_diff(*b);
        if diff > luma.0 {
            let at = at as u32;
            luma = (diff, (at % got.width, at / got.width));
        }
    }
    let chroma = got
        .uv
        .iter()
        .zip(&want.uv)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    (luma.0, luma.1, chroma)
}

fn assert_matches_reference(got: &Nv12Image, want: &Nv12Image, what: &str) {
    let (luma, at, chroma) = largest_differences(got, want);
    println!(
        "{what}: largest difference from the CPU reference: Y {luma} (at {at:?}), U or V {chroma}"
    );
    assert!(luma <= 1, "{what}: Y is {luma} codes off at {at:?}");
    assert!(chroma <= 2, "{what}: U or V is {chroma} codes off");
}

#[test]
fn primary_colours_come_out_as_bt709_limited_range() {
    let Some(device) = device() else { return };
    // Y, U, V from the BT.709 equations, rounded.
    let cases = [
        ("white", [255, 255, 255, 255], (235, 128, 128)),
        ("black", [0, 0, 0, 255], (16, 128, 128)),
        ("red", [0, 0, 255, 255], (63, 102, 240)),
        ("green", [0, 255, 0, 255], (173, 42, 26)),
        ("blue", [255, 0, 0, 255], (32, 240, 118)),
    ];
    for (name, bgra, (y, u, v)) in cases {
        let bgra = solid(256, 256, bgra);
        let image = convert(&device, &bgra, 256, 256, Rotation::Identity, unpaced());
        let ys: Vec<u8> = image.y.iter().copied().filter(|&got| got != y).collect();
        assert!(
            ys.is_empty(),
            "{name}: Y should be {y}, found {:?}",
            &ys[..ys.len().min(4)]
        );
        for pair in image.uv.chunks(2) {
            assert_eq!((pair[0], pair[1]), (u, v), "{name}: U and V");
        }
        println!("{name}: {y}/{u}/{v}");
    }
}

#[test]
fn a_grey_ramp_keeps_every_step() {
    let Some(device) = device() else { return };
    let (width, height) = (256, 8);
    let mut bgra = Vec::new();
    for _ in 0..height {
        for grey in 0..width {
            bgra.extend_from_slice(&[grey as u8, grey as u8, grey as u8, 255]);
        }
    }
    let image = convert(&device, &bgra, width, height, Rotation::Identity, unpaced());
    for grey in 0..width {
        let want = (16.0 + 219.0 * grey as f64 / 255.0).round() as i32;
        let got = image.luma(grey, 3) as i32;
        assert!(
            (got - want).abs() <= 1,
            "grey {grey} gave Y {got}, want {want}"
        );
    }
    assert!(image.uv.iter().all(|&c| c == 128), "grey has colour in it");
    // Never a step backwards: a ramp stays a ramp.
    for grey in 1..width {
        assert!(image.luma(grey, 3) >= image.luma(grey - 1, 3));
    }
}

#[test]
fn pattern_frames_match_the_cpu_reference() {
    let Some(device) = device() else { return };
    let (width, height) = (640, 360);
    let mut pattern = Pattern::new(&device, width, height, 0).unwrap();
    for _ in 0..40 {
        let frame = pattern.next().unwrap();
        assert_eq!((frame.width, frame.height), (width, height));
        if ![0, 1, 2, 39].contains(&frame.number) {
            continue;
        }
        let got = pattern.read_back(&frame).unwrap();
        let want = to_nv12(
            &pattern_image(frame.number, width, height),
            width,
            height,
            Rotation::Identity,
            under(0, 0),
        );
        assert_matches_reference(
            &got,
            &want,
            &format!("{width}x{height} frame {}", frame.number),
        );
        assert_eq!(
            read_frame_number(&got.y, width as usize, width, height),
            Some(frame.number as u32)
        );
    }
}

#[test]
fn scaling_down_by_one_and_a_half_matches_the_cpu_reference() {
    let Some(device) = device() else { return };
    let (width, height) = (960, 540);
    let bgra = pattern_image(7, width, height);
    let options = under(4096, 360);
    let got = convert(&device, &bgra, width, height, Rotation::Identity, options);
    assert_eq!((got.width, got.height), (640, 360));
    let want = to_nv12(&bgra, width, height, Rotation::Identity, options);
    assert_matches_reference(&got, &want, "960x540 to 640x360");
}

// White one-pixel lines every fourth row and column on black: 7/16 of the
// picture is white, and the mean Y of the scaled picture must stay within
// rounding of that. It catches a scale-down that loses or adds light over a
// whole picture, or tints grey. It does not tell the filter from bilinear or
// nearest point, which keep this grid's mean too; the line sweep below does.
#[test]
fn scaling_4k_to_1440p_keeps_the_light_of_a_one_pixel_grid() {
    let Some(device) = device() else { return };
    let (width, height) = (3840, 2160);
    let mut bgra = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let value = if x % 4 == 0 || y % 4 == 0 { 255 } else { 0 };
            bgra.extend_from_slice(&[value, value, value, 255]);
        }
    }
    let image = convert(&device, &bgra, width, height, Rotation::Identity, unpaced());
    assert_eq!((image.width, image.height), (2560, 1440));
    let mean = image.y.iter().map(|&y| y as f64).sum::<f64>() / image.y.len() as f64;
    let want = 16.0 + 219.0 * 7.0 / 16.0;
    println!("4K grid scaled to 1440p: mean Y {mean:.3}, source {want:.3}");
    // Rounding each pixel to a whole code moves the mean by well under half
    // a code; half a code of 219 is 0.2 percent of the range.
    assert!((mean - want).abs() < 0.5, "mean Y {mean} against {want}");
    assert!(image.uv.iter().all(|&c| c == 128), "grey lines grew colour");
}

// What the area average is for. At 1.5x, 3840 to 2560, a one-pixel line
// falls on one of three places against the output pixels, and its light
// must come out the same on each: two thirds of a pixel's worth, spread
// over the one or two output pixels it lies under. Bilinear gives 3/4, 1/2
// and 3/4 of a pixel for the three, nearest point all or nothing, and that
// is text shimmering as it scrolls.
#[test]
fn a_one_pixel_line_keeps_its_light_anywhere() {
    let Some(device) = device() else { return };
    let (width, height) = (3840u32, 2160u32);
    // Far enough apart that no output pixel sees two of them.
    let places = [1200u32, 1501, 1802];
    let white = |at: u32| places.contains(&at);
    let mut upright = Vec::with_capacity((width * height * 4) as usize);
    let mut lying = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let value = if white(x) { 255 } else { 0 };
            upright.extend_from_slice(&[value, value, value, 255]);
            let value = if white(y) { 255 } else { 0 };
            lying.extend_from_slice(&[value, value, value, 255]);
        }
    }
    let options = unpaced();
    let across = convert(
        &device,
        &upright,
        width,
        height,
        Rotation::Identity,
        options,
    );
    let down = convert(&device, &lying, width, height, Rotation::Identity, options);
    assert_eq!((across.width, across.height), (2560, 1440));
    // 219 codes from black to white, two thirds of a pixel: 146. Rounding
    // each of the two output pixels moves it by at most half a code.
    let want = 219.0 * 2.0 / 3.0;
    for place in places {
        let out = place * 2 / 3;
        let light = |luma: &dyn Fn(u32) -> u8| -> f64 {
            (out - 3..=out + 3)
                .map(|at| luma(at) as f64 - 16.0)
                .sum::<f64>()
        };
        let upright_light = light(&|x| across.luma(x, 700));
        let lying_light = light(&|y| down.luma(1200, y));
        println!(
            "line at {place} ({} of 3): {upright_light} upright, {lying_light} lying, want {want:.1}",
            place % 3
        );
        assert!(
            (upright_light - want).abs() <= 1.0,
            "an upright line at x {place} came out with {upright_light} codes of light, want {want:.1}"
        );
        assert!(
            (lying_light - want).abs() <= 1.0,
            "a lying line at y {place} came out with {lying_light} codes of light, want {want:.1}"
        );
    }
}

#[test]
fn frame_number_reads_after_4k_to_1440p() {
    let Some(device) = device() else { return };
    let mut pattern =
        Pattern::with_source(&device, 3840, 2160, Rotation::Identity, unpaced()).unwrap();
    assert_eq!((pattern.width(), pattern.height()), (2560, 1440));
    for _ in 0..3 {
        let frame = pattern.next().unwrap();
        let image = pattern.read_back(&frame).unwrap();
        assert_eq!(
            read_frame_number(&image.y, 2560, 2560, 1440),
            Some(frame.number as u32)
        );
    }
}

#[test]
fn odd_sizes_come_out_even() {
    let cases = [
        ((3840, 2160, Rotation::Identity, 4096, 1440), (2560, 1440)),
        ((2560, 1440, Rotation::Identity, 4096, 1440), (2560, 1440)),
        ((1365, 767, Rotation::Identity, 4096, 1440), (1366, 768)),
        ((3841, 2161, Rotation::Identity, 4096, 1440), (2560, 1440)),
        ((3840, 2160, Rotation::Rotate90, 4096, 1440), (810, 1440)),
        ((1080, 1920, Rotation::Identity, 4096, 1440), (810, 1440)),
        ((1001, 1501, Rotation::Identity, 4096, 1000), (666, 1000)),
        ((1501, 1001, Rotation::Identity, 1000, 0), (1000, 666)),
        ((1920, 1080, Rotation::Identity, 4096, 1081), (1920, 1080)),
        ((5120, 1440, Rotation::Identity, 4095, 1440), (4094, 1152)),
        // No limit on one side or either.
        ((5120, 1440, Rotation::Identity, 0, 1440), (5120, 1440)),
        ((3840, 2160, Rotation::Identity, 0, 0), (3840, 2160)),
        ((3841, 2161, Rotation::Rotate90, 0, 0), (2162, 3842)),
        // A limit under 256 counts as 256, and no frame is smaller.
        ((3840, 2160, Rotation::Identity, 4096, 1), (456, 256)),
        ((3840, 2160, Rotation::Identity, 1, 1440), (256, 256)),
    ];
    for ((width, height, rotation, max_width, max_height), want) in cases {
        let got = capture::output_size(width, height, rotation, max_width, max_height);
        assert_eq!(
            got,
            want,
            "{width}x{height} rotated {} under {max_width}x{max_height}",
            rotation.degrees()
        );
        assert!(got.0.is_multiple_of(2) && got.1.is_multiple_of(2));
    }
}

// The picture within both limits, both sides even, and the shape kept: the
// side that was not scaled to its limit is within a pixel of where the other
// one puts it. The frame is the picture, padded to 256 a side.
fn assert_fits(plan: &Plan, max_width: u32, max_height: u32, what: &str) {
    let (width, height) = plan.picture;
    assert!(
        width <= max_width && height <= max_height,
        "{what}: {plan:?}"
    );
    assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "{what}: {plan:?}"
    );
    assert_eq!(
        (plan.width, plan.height),
        (width.max(SMALLEST_SIDE), height.max(SMALLEST_SIDE)),
        "{what}: {plan:?}"
    );
    if plan.footprint == (1.0, 1.0) {
        return;
    }
    let upright = (plan.upright_width as f64, plan.upright_height as f64);
    let off = if width == max_width {
        height as f64 - upright.1 * width as f64 / upright.0
    } else {
        assert_eq!(height, max_height, "{what}: scaled to neither limit");
        width as f64 - upright.0 * height as f64 / upright.1
    };
    assert!(
        off.abs() <= 1.0,
        "{what}: the shape is {off:.2} pixels off, {plan:?}"
    );
}

// H.264 on NVENC takes at most 4096 wide. A super-wide monitor is scaled
// down to that with its shape kept; a 4K one is still decided by its height.
#[test]
fn a_share_is_never_wider_than_4096_or_taller_than_1440() {
    let Options {
        max_width,
        max_height,
        ..
    } = Options::default();
    assert_eq!((max_width, max_height), (4096, 1440));
    let cases = [
        ((5120, 1440, Rotation::Identity), (4096, 1152)),
        ((7680, 2160, Rotation::Identity), (4096, 1152)),
        ((5760, 1080, Rotation::Identity), (4096, 768)),
        ((3440, 1440, Rotation::Identity), (3440, 1440)),
        ((3840, 1080, Rotation::Identity), (3840, 1080)),
        ((3840, 2160, Rotation::Identity), (2560, 1440)),
        ((5120, 2160, Rotation::Identity), (3414, 1440)),
        // Turned upright first: a super-wide panel hung on its side is tall,
        // and one that comes from Windows on its side is wide once turned.
        ((5120, 1440, Rotation::Rotate90), (406, 1440)),
        ((7680, 2160, Rotation::Rotate270), (406, 1440)),
        ((1440, 5120, Rotation::Rotate90), (4096, 1152)),
        ((2160, 7680, Rotation::Rotate270), (4096, 1152)),
        ((5120, 1440, Rotation::Rotate180), (4096, 1152)),
    ];
    for ((width, height, rotation), want) in cases {
        let plan = Plan::new(width, height, rotation, max_width, max_height);
        let what = format!("{width}x{height} rotated {}", rotation.degrees());
        assert_eq!((plan.width, plan.height), want, "{what}");
        assert_fits(&plan, max_width, max_height, &what);
    }
    // What the shader is told: source pixels under one output pixel.
    let footprint = |width, height| {
        Plan::new(width, height, Rotation::Identity, max_width, max_height).footprint
    };
    assert_eq!(footprint(5120, 1440), (1.25, 1.25));
    assert_eq!(footprint(7680, 2160), (1.875, 1.875));

    // Every size in between.
    for rotation in [Rotation::Identity, Rotation::Rotate90] {
        for width in (640..=8192).step_by(97) {
            for height in (360..=4320).step_by(89) {
                let plan = Plan::new(width, height, rotation, max_width, max_height);
                let what = format!("{width}x{height} rotated {}", rotation.degrees());
                assert_fits(&plan, max_width, max_height, &what);
            }
        }
    }
}

#[test]
fn odd_sizes_convert_like_the_cpu_reference() {
    let Some(device) = device() else { return };
    // Kept as it is with the edge copied one pixel further each way (and
    // black under it to 256), then scaled down to a height and to a width.
    for (width, height, max_width, max_height) in [
        (333, 187, 4096, 1440),
        (1001, 1501, 4096, 1000),
        (1501, 1001, 1000, 1440),
    ] {
        let bgra = pattern_image(3, width, height);
        let options = under(max_width, max_height);
        let got = convert(&device, &bgra, width, height, Rotation::Identity, options);
        let want = to_nv12(&bgra, width, height, Rotation::Identity, options);
        assert_matches_reference(
            &got,
            &want,
            &format!("{width}x{height} under {max_width}x{max_height}"),
        );
    }
}

// A super-wide monitor's whole picture brought to 4096 wide, the GPU against
// the CPU reference, turned upright too, and the pattern's frame number
// still read at that size. 3440x1440 is kept as it is.
#[test]
fn super_wide_monitors_convert_like_the_cpu_reference() {
    let Some(device) = device() else { return };
    let cases = [
        (5120, 1440, Rotation::Identity, (4096, 1152)),
        (7680, 2160, Rotation::Identity, (4096, 1152)),
        (3440, 1440, Rotation::Identity, (3440, 1440)),
        (1440, 5120, Rotation::Rotate90, (4096, 1152)),
        (5120, 1440, Rotation::Rotate180, (4096, 1152)),
    ];
    for (width, height, rotation, size) in cases {
        let what = format!("{width}x{height} rotated {}", rotation.degrees());
        let bgra = pattern_image(11, width, height);
        let started = Instant::now();
        let got = convert(&device, &bgra, width, height, rotation, unpaced());
        assert_eq!((got.width, got.height), size, "{what}");
        let want = to_nv12(&bgra, width, height, rotation, unpaced());
        assert_matches_reference(&got, &want, &what);
        println!(
            "{what}: converted and checked in {:.1} s",
            started.elapsed().as_secs_f64()
        );
    }
    let mut pattern =
        Pattern::with_source(&device, 5120, 1440, Rotation::Identity, unpaced()).unwrap();
    assert_eq!((pattern.width(), pattern.height()), (4096, 1152));
    for _ in 0..3 {
        let frame = pattern.next().unwrap();
        let image = pattern.read_back(&frame).unwrap();
        assert_eq!(
            read_frame_number(&image.y, 4096, 4096, 1152),
            Some(frame.number as u32)
        );
    }
}

// A red block marks the duplication image's top left corner. Windows hands
// a rotated monitor's picture unrotated; turned upright, that corner lands
// top right for a 90 degree rotation, bottom right for 180 and bottom left
// for 270.
#[test]
fn each_rotation_is_turned_upright() {
    let Some(device) = device() else { return };
    // No smaller than 256 either way, so no rotation pads it.
    let (width, height) = (320, 256);
    let mut bgra = pattern_image(5, width, height);
    for y in 0..8 {
        for x in 0..8 {
            let at = ((y * width + x) * 4) as usize;
            bgra[at..at + 4].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    for rotation in [
        Rotation::Identity,
        Rotation::Rotate90,
        Rotation::Rotate180,
        Rotation::Rotate270,
    ] {
        let got = convert(&device, &bgra, width, height, rotation, unpaced());
        let want = to_nv12(&bgra, width, height, rotation, unpaced());
        assert_matches_reference(&got, &want, &format!("rotated {}", rotation.degrees()));
        let (w, h) = (got.width, got.height);
        let expected_size = if rotation.swaps_sides() {
            (height, width)
        } else {
            (width, height)
        };
        assert_eq!((w, h), expected_size);
        let corner = match rotation {
            Rotation::Identity => (3, 3),
            Rotation::Rotate90 => (w - 4, 3),
            Rotation::Rotate180 => (w - 4, h - 4),
            Rotation::Rotate270 => (3, h - 4),
        };
        assert_eq!(
            got.luma(corner.0, corner.1),
            63,
            "rotated {}: no red at {corner:?}",
            rotation.degrees()
        );
    }
}

#[test]
fn the_frame_number_reader_reads_what_the_pattern_writes() {
    for number in [0u64, 1, 2, 255, 0xdead_beef, u32::MAX as u64, 1 << 40 | 77] {
        for (width, height) in [(640, 360), (2560, 1440)] {
            let image = to_nv12(
                &pattern_image(number, width, height),
                width,
                height,
                Rotation::Identity,
                under(0, 0),
            );
            assert_eq!(
                read_frame_number(&image.y, width as usize, width, height),
                Some(number as u32),
                "frame {number} at {width}x{height}"
            );
        }
    }
    // A picture without the blocks does not read as a number.
    let grey = vec![128u8; 640 * 360];
    assert_eq!(read_frame_number(&grey, 640, 640, 360), None);
    // The narrowest picture that carries the number, a pixel a block.
    let (width, height) = (36, 64);
    let image = to_nv12(
        &pattern_image(0xdead_beef, width, height),
        width,
        height,
        Rotation::Identity,
        under(0, 0),
    );
    assert_eq!(
        read_frame_number(&image.y, 36, width, height),
        Some(0xdead_beef)
    );
}

// Decode tests feed the reader whatever came out of a decoder, so no size,
// pitch or length may make it or the pattern panic.
#[test]
fn the_pattern_and_the_reader_take_any_size() {
    assert_eq!(read_frame_number(&[0; 20], 20, 20, 1), None);
    for width in 0..48u32 {
        for height in 0..6u32 {
            let image = pattern_image(9, width, height);
            assert_eq!(image.len(), (width * height * 4) as usize);
            for pitch in [width as usize, width as usize + 3] {
                let full = pitch * height as usize;
                for len in [0, full / 2, full, full * 3 / 2] {
                    let _ = read_frame_number(&vec![235; len], pitch, width, height);
                }
            }
        }
    }
}

// The encoder, and later Media Foundation, use the capture device's
// immediate context from their own threads. Multithread protection makes
// each call safe, but the conversion is a sequence of calls that set up the
// pipeline and draw; another thread's call landing in the middle of it must
// not change what gets drawn. ClearState is the worst such call: between
// binding the target and drawing, it would leave the draw with no target.
#[test]
fn another_thread_on_the_context() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let Some(device) = device() else { return };
    // SAFETY: GetImmediateContext hands back an owned reference.
    let context = unsafe { device.GetImmediateContext() }.unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicU64::new(0));
    let other = {
        let stop = stop.clone();
        let calls = calls.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: the device is multithread protected, so this call
                // may come from any thread.
                unsafe { context.ClearState() };
                calls.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    let (width, height) = (640, 360);
    let mut pattern = Pattern::new(&device, width, height, 0).unwrap();
    let mut wrong = Vec::new();
    for _ in 0..300 {
        let frame = pattern.next().unwrap();
        let image = pattern.read_back(&frame).unwrap();
        let read = read_frame_number(&image.y, width as usize, width, height);
        if read != Some(frame.number as u32) {
            wrong.push((frame.number, read));
        }
    }
    stop.store(true, Ordering::Relaxed);
    other.join().unwrap();
    println!(
        "300 conversions while another thread made {} calls on the same context",
        calls.load(Ordering::Relaxed)
    );
    assert!(
        wrong.is_empty(),
        "{} of 300 frames came out wrong (frame, number read): {:?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
}

#[test]
fn a_pattern_refuses_to_read_back_a_frame_it_did_not_make() {
    let Some(device) = device() else { return };
    let mut mine = Pattern::new(&device, 64, 64, 0).unwrap();
    let mut other = Pattern::new(&device, 64, 64, 0).unwrap();
    let frame = other.next().unwrap();
    let err = mine.read_back(&frame).unwrap_err();
    assert!(
        err.to_string().contains("not one this pattern made"),
        "{err}"
    );
    mine.next().unwrap();
}

// A region is cut to the picture where it reaches past it and its sides come
// down to even, never up, so nothing outside it goes out; one with less than
// two pixels each way on the picture is none. Turned, the region is in the
// upright picture's pixels.
#[test]
fn a_region_keeps_inside_what_was_asked() {
    let region = |left, top, width, height| Region {
        left,
        top,
        width,
        height,
    };
    let plan = |rotation, asked| Plan::region(1920, 1080, rotation, asked, 4096, 1440);
    let fitted = |rotation, asked| {
        plan(rotation, asked).map(|plan| (plan.crop, plan.upright_width, plan.upright_height))
    };
    let identity = Rotation::Identity;
    assert_eq!(
        fitted(identity, region(101, 37, 641, 361)),
        Some(((101, 37), 640, 360))
    );
    assert_eq!(
        fitted(identity, region(1800, 1000, 500, 500)),
        Some(((1800, 1000), 120, 80))
    );
    assert_eq!(fitted(identity, region(1919, 0, 10, 10)), None);
    assert_eq!(fitted(identity, region(2000, 0, 10, 10)), None);
    assert_eq!(
        fitted(Rotation::Rotate90, region(0, 1000, 1080, 920)),
        Some(((0, 1000), 1080, 920))
    );
    assert_eq!(fitted(Rotation::Rotate90, region(0, 1921, 100, 100)), None);
    let whole = plan(identity, region(0, 0, 1920, 1080)).unwrap();
    assert_eq!(whole, Plan::new(1920, 1080, identity, 4096, 1440));
    let scaled = Plan::region(3840, 2160, identity, region(0, 0, 3840, 2160), 4096, 1440).unwrap();
    assert_eq!((scaled.width, scaled.height), (2560, 1440));
}

// Regions upright and turned, kept and scaled: the GPU converts exactly
// what the CPU reference does. And a kept region at an even corner is the
// whole picture's own pixels there, chroma included, which holds the crop
// to the picture itself and not only to the reference.
#[test]
fn a_region_matches_the_cpu_reference() {
    let Some(device) = device() else { return };
    let (width, height) = (960, 540);
    let bgra = pattern_image(7, width, height);
    let region = |left, top, width, height| Region {
        left,
        top,
        width,
        height,
    };
    let cases = [
        (
            Rotation::Identity,
            region(101, 37, 641, 361),
            unpaced(),
            (640, 360),
        ),
        (
            Rotation::Identity,
            region(0, 0, 960, 540),
            under(640, 360),
            (640, 360),
        ),
        // 300x200 once scaled, padded to 256 high.
        (
            Rotation::Identity,
            region(300, 100, 600, 400),
            under(300, 200),
            (300, 256),
        ),
        (
            Rotation::Rotate90,
            region(20, 100, 300, 500),
            unpaced(),
            (300, 500),
        ),
        (
            Rotation::Rotate270,
            region(41, 300, 400, 600),
            under(200, 300),
            (256, 300),
        ),
    ];
    for (rotation, asked, options, size) in cases {
        let mut pattern =
            Pattern::with_region(&device, width, height, rotation, asked, options).unwrap();
        let frame = pattern.convert(&bgra).unwrap();
        let got = pattern.read_back(&frame).unwrap();
        assert_eq!((got.width, got.height), size, "{asked:?} {rotation:?}");
        let want = region_to_nv12(&bgra, width, height, rotation, asked, options);
        assert_matches_reference(&got, &want, &format!("{asked:?} {rotation:?}"));
    }
    let whole = convert(&device, &bgra, width, height, Rotation::Identity, unpaced());
    let (left, top) = (100, 36);
    let mut pattern = Pattern::with_region(
        &device,
        width,
        height,
        Rotation::Identity,
        region(left, top, 640, 360),
        unpaced(),
    )
    .unwrap();
    let frame = pattern.convert(&bgra).unwrap();
    let part = pattern.read_back(&frame).unwrap();
    for y in 0..part.height {
        for x in 0..part.width {
            assert_eq!(
                part.luma(x, y),
                whole.luma(left + x, top + y),
                "Y at {x},{y}"
            );
        }
    }
    for y in 0..part.height / 2 {
        for x in 0..part.width / 2 {
            let there = whole.chroma(left / 2 + x, top / 2 + y);
            assert_eq!(part.chroma(x, y), there, "UV at {x},{y}");
        }
    }
}

// Every Y past the picture is black's 16 and every U and V pair past its
// half-size box is 128, 128.
fn assert_black_outside(image: &Nv12Image, (width, height): (u32, u32), what: &str) {
    for y in 0..image.height {
        for x in 0..image.width {
            if x >= width || y >= height {
                assert_eq!(image.luma(x, y), 16, "{what}: Y at {x},{y}");
            }
        }
    }
    for y in 0..image.height / 2 {
        for x in 0..image.width / 2 {
            if x >= width / 2 || y >= height / 2 {
                assert_eq!(image.chroma(x, y), (128, 128), "{what}: UV at {x},{y}");
            }
        }
    }
}

// A picture under 256 a side goes out at the top left of a 256 frame with
// black around it, as the CPU reference has it, and its pixels are the
// picture's own: a 200x120 region is the whole picture's pixels there.
#[test]
fn a_small_picture_is_padded_with_black() {
    let Some(device) = device() else { return };
    let (width, height) = (200, 120);
    let bgra = pattern_image(3, width, height);
    let got = convert(&device, &bgra, width, height, Rotation::Identity, unpaced());
    assert_eq!((got.width, got.height), (256, 256));
    let want = to_nv12(&bgra, width, height, Rotation::Identity, unpaced());
    assert_matches_reference(&got, &want, "200x120");
    assert_black_outside(&got, (width, height), "200x120");

    let (whole_width, whole_height) = (640, 360);
    let bgra = pattern_image(4, whole_width, whole_height);
    let whole = convert(
        &device,
        &bgra,
        whole_width,
        whole_height,
        Rotation::Identity,
        unpaced(),
    );
    let (left, top) = (100, 36);
    let asked = Region {
        left,
        top,
        width,
        height,
    };
    let mut pattern = Pattern::with_region(
        &device,
        whole_width,
        whole_height,
        Rotation::Identity,
        asked,
        unpaced(),
    )
    .unwrap();
    let frame = pattern.convert(&bgra).unwrap();
    assert_eq!(pattern.plan().picture, (width, height));
    let part = pattern.read_back(&frame).unwrap();
    assert_black_outside(&part, (width, height), "a 200x120 region");
    for y in 0..height {
        for x in 0..width {
            assert_eq!(
                part.luma(x, y),
                whole.luma(left + x, top + y),
                "Y at {x},{y}"
            );
        }
    }
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            let there = whole.chroma(left / 2 + x, top / 2 + y);
            assert_eq!(part.chroma(x, y), there, "UV at {x},{y}");
        }
    }
}

// Scaled and then padded, padded one way only, odd and padded, and a turned
// region padded: the GPU as the CPU reference, and black past the picture.
#[test]
fn small_pictures_scale_then_pad_like_the_cpu_reference() {
    let Some(device) = device() else { return };
    let cases = [
        ((3000, 100), under(1920, 1080), (1920, 256), (1920, 64)),
        ((120, 600), unpaced(), (256, 600), (120, 600)),
        ((201, 121), unpaced(), (256, 256), (202, 122)),
    ];
    for ((width, height), options, frame, picture) in cases {
        let what = format!("{width}x{height}");
        let bgra = pattern_image(9, width, height);
        let got = convert(&device, &bgra, width, height, Rotation::Identity, options);
        assert_eq!((got.width, got.height), frame, "{what}");
        let want = to_nv12(&bgra, width, height, Rotation::Identity, options);
        assert_matches_reference(&got, &want, &what);
        assert_black_outside(&got, picture, &what);
    }
    let (width, height) = (640, 360);
    let bgra = pattern_image(10, width, height);
    let asked = Region {
        left: 20,
        top: 100,
        width: 200,
        height: 120,
    };
    let mut pattern =
        Pattern::with_region(&device, width, height, Rotation::Rotate90, asked, unpaced()).unwrap();
    let frame = pattern.convert(&bgra).unwrap();
    let got = pattern.read_back(&frame).unwrap();
    assert_eq!((got.width, got.height), (256, 256));
    let want = region_to_nv12(&bgra, width, height, Rotation::Rotate90, asked, unpaced());
    assert_matches_reference(&got, &want, "a turned 200x120 region");
    assert_black_outside(&got, (200, 120), "a turned 200x120 region");
}

// Whatever the source and the limits, the frame is the picture padded to
// 256 a side, both even, and the picture keeps within the limits, a limit
// under 256 counting as 256.
#[test]
fn no_frame_is_smaller_than_256_a_side() {
    let picture_and_frame = |plan: Plan| (plan.picture, (plan.width, plan.height));
    assert_eq!(
        picture_and_frame(Plan::new(200, 120, Rotation::Identity, 4096, 1440)),
        ((200, 120), (256, 256))
    );
    assert_eq!(
        picture_and_frame(Plan::new(3000, 100, Rotation::Identity, 1920, 1080)),
        ((1920, 64), (1920, 256))
    );
    assert_eq!(
        picture_and_frame(Plan::new(7680, 200, Rotation::Identity, 4096, 1440)),
        ((4096, 106), (4096, 256))
    );
    let narrow = Region {
        left: 0,
        top: 0,
        width: 256,
        height: 1440,
    };
    let fitted = Plan::region(2560, 1440, Rotation::Identity, narrow, 4096, 1080).unwrap();
    assert_eq!(picture_and_frame(fitted), ((192, 1080), (256, 1080)));
    let boxes = [
        (0, 0),
        (1, 1),
        (255, 255),
        (4096, 1080),
        (4096, 1440),
        (1920, 1080),
    ];
    for width in (2..4400).step_by(37) {
        for height in (2..2400).step_by(29) {
            for (max_width, max_height) in boxes {
                let plan = Plan::new(width, height, Rotation::Identity, max_width, max_height);
                let what = format!("{width}x{height} under {max_width}x{max_height}: {plan:?}");
                let (picture_width, picture_height) = plan.picture;
                assert_eq!(
                    (plan.width, plan.height),
                    (
                        picture_width.max(SMALLEST_SIDE),
                        picture_height.max(SMALLEST_SIDE)
                    ),
                    "{what}"
                );
                assert!(
                    plan.width.is_multiple_of(2) && plan.height.is_multiple_of(2),
                    "{what}"
                );
                let most = |max: u32| {
                    if max == 0 {
                        u32::MAX
                    } else {
                        max.max(SMALLEST_SIDE)
                    }
                };
                assert!(
                    plan.width <= most(max_width) && plan.height <= most(max_height),
                    "{what}"
                );
            }
        }
    }
}

// Under new limits a plan comes out as the plan made under them from the
// start: a monitor's as Plan::new, a region's as Plan::region.
#[test]
fn refit_is_the_plan_under_new_limits() {
    let boxes = [(4096, 1440), (4096, 1080), (4096, 2160), (1920, 1080)];
    let region = Region {
        left: 300,
        top: 100,
        width: 2000,
        height: 1200,
    };
    for (max_width, max_height) in boxes {
        for (width, height, rotation) in [
            (3840, 2160, Rotation::Identity),
            (2560, 1440, Rotation::Rotate90),
            (300, 200, Rotation::Identity),
        ] {
            let plan = Plan::new(width, height, rotation, 4096, 1440);
            assert_eq!(
                plan.refit(max_width, max_height),
                Plan::new(width, height, rotation, max_width, max_height),
                "{width}x{height} {rotation:?} under {max_width}x{max_height}"
            );
        }
        let plan = Plan::region(2560, 1440, Rotation::Identity, region, 4096, 1440).unwrap();
        assert_eq!(
            plan.refit(max_width, max_height),
            Plan::region(
                2560,
                1440,
                Rotation::Identity,
                region,
                max_width,
                max_height
            )
            .unwrap(),
            "{region:?} under {max_width}x{max_height}"
        );
    }
}
