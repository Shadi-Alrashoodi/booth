// What the viewer draws, checked by drawing into a texture and reading it
// back: the same Scene the window uses, fed the capture crate's test
// pattern and nothing from the screen.

use std::time::Duration;

use capture::{Nv12Image, Pattern};
use stats::{Level, TraceSample};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};

use crate::Video;
use crate::control;
use crate::cursor::{Cursor, CursorKind, CursorShape, Local};
use crate::device::{self, Gpu};
use crate::palette::{AMBER, Colour, INK, LINE, SAGE, WARN};
use crate::picture::VIEW_IDLE;
use crate::scene::{Scene, Target};
use crate::strip::{Band, LinkState, Look, PathWord, PresentPath, Strip, band_height};
use crate::window::min_client;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

fn gpu() -> Option<Gpu> {
    // SAFETY: a plain lookup; the origin is on the primary monitor.
    let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    match device::gpu_for(primary) {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            println!("skipped: {err}");
            None
        }
    }
}

struct Offscreen {
    gpu: Gpu,
    scene: Scene,
    pattern: Pattern,
    band: Band,
    // The controller's mouse, while this PC controls in absolute mode.
    local: Option<Local>,
}

// BGRA, top row first.
struct Image {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

impl Image {
    fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        assert!(
            x < self.width && y < self.height,
            "{x},{y} is outside the image"
        );
        let at = ((y * self.width + x) * 4) as usize;
        [self.bgra[at + 2], self.bgra[at + 1], self.bgra[at]]
    }
}

impl Offscreen {
    fn new() -> Option<Offscreen> {
        let gpu = gpu()?;
        let scene = Scene::new(&gpu).unwrap();
        let pattern = Pattern::new(&gpu.device, WIDTH, HEIGHT, 0).unwrap();
        Some(Offscreen {
            gpu,
            scene,
            pattern,
            band: Band::Below,
            local: None,
        })
    }

    fn texture(&self, width: u32, height: u32, staging: bool) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            BindFlags: if staging {
                0
            } else {
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
            },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: a full description and a live out parameter.
        unsafe {
            self.gpu
                .device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .unwrap()
        };
        texture.unwrap()
    }

    // One pattern frame, drawn into a target of the given size (which
    // includes the strip's band) and read back, with the NV12 it came from.
    fn draw(
        &mut self,
        size: (u32, u32),
        cursor: Option<&Cursor>,
        strip: &Strip,
        dpi: u32,
    ) -> (Image, Nv12Image) {
        let frame = self.pattern.next().unwrap();
        let nv12 = self.pattern.read_back(&frame).unwrap();
        let video = Video {
            texture: &frame.texture,
            index: 0,
            width: frame.width,
            height: frame.height,
        };
        let image = self.draw_video(size, Some(&video), cursor, strip, dpi);
        (image, nv12)
    }

    fn draw_video(
        &mut self,
        size: (u32, u32),
        video: Option<&Video>,
        cursor: Option<&Cursor>,
        strip: &Strip,
        dpi: u32,
    ) -> Image {
        let texture = self.texture(size.0, size.1, false);
        let target: Target = self.scene.target(&texture).unwrap();
        let look = Look {
            size,
            dpi,
            scrolling: true,
            present: Some(PresentPath::Flip),
            band: self.band,
        };
        self.scene
            .draw(&target, video, cursor, self.local.as_ref(), strip, &look)
            .unwrap();
        if let Some(video) = video {
            self.scene.keep(video).unwrap();
        }
        drop(target);
        self.read(&texture, size)
    }

    fn read(&self, texture: &ID3D11Texture2D, size: (u32, u32)) -> Image {
        let staging = self.texture(size.0, size.1, true);
        let context = &self.gpu.context;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures are BGRA of the same size on this device;
        // Map waits for the copy; `mapped` is a live out parameter.
        unsafe {
            context.CopyResource(&staging, texture);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
        }
        let pitch = mapped.RowPitch as usize;
        let row = size.0 as usize * 4;
        // SAFETY: a mapped texture is `height` rows of `pitch` bytes and stays
        // mapped until Unmap below.
        let bytes = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * size.1 as usize)
        };
        let mut bgra = Vec::with_capacity(row * size.1 as usize);
        for y in 0..size.1 as usize {
            bgra.extend_from_slice(&bytes[y * pitch..y * pitch + row]);
        }
        // SAFETY: mapped above; `bytes` is not used past here.
        unsafe { context.Unmap(&staging, 0) };
        Image {
            width: size.0,
            height: size.1,
            bgra,
        }
    }
}

// BT.709 limited range back to full range RGB, in doubles.
fn reference(y: u8, u: u8, v: u8) -> [u8; 3] {
    let luma = (y as f64 - 16.0) / 219.0;
    let blue_diff = (u as f64 - 128.0) / 224.0;
    let red_diff = (v as f64 - 128.0) / 224.0;
    let b = luma + 1.8556 * blue_diff;
    let r = luma + 1.5748 * red_diff;
    let g = (luma - 0.2126 * r - 0.0722 * b) / 0.7152;
    [r, g, b].map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn nv12_rgb(nv12: &Nv12Image, x: u32, y: u32) -> [u8; 3] {
    let (u, v) = nv12.chroma(x / 2, y / 2);
    reference(nv12.luma(x, y), u, v)
}

fn off_by(a: [u8; 3], b: [u8; 3]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(a, b)| a.abs_diff(b))
        .max()
        .unwrap_or(0)
}

fn close(a: [u8; 3], b: Colour) -> bool {
    off_by(a, b.rgb()) <= 1
}

// The eight colour bars sit between 3/8 and 1/2 of the pattern's height.
fn bar_centres() -> Vec<(u32, u32)> {
    (0..8)
        .map(|bar| ((bar * 2 + 1) * WIDTH / 16, HEIGHT * 7 / 16))
        .collect()
}

fn plain() -> Strip {
    Strip::default()
}

#[test]
fn colour_bars_survive_scaling() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    for (name, scale) in [("one to one", 1.0), ("half", 0.5), ("one and a half", 1.5)] {
        let picture = (
            (WIDTH as f64 * scale) as u32,
            (HEIGHT as f64 * scale) as u32,
        );
        let size = (picture.0, picture.1 + band_height(96));
        let (image, nv12) = screen.draw(size, None, &plain(), 96);
        let mut worst = 0;
        for (x, y) in bar_centres() {
            let want = nv12_rgb(&nv12, x, y);
            let at = ((x as f64 * scale) as u32, (y as f64 * scale) as u32);
            let got = image.rgb(at.0, at.1);
            worst = worst.max(off_by(got, want));
            assert!(
                off_by(got, want) <= 2,
                "{name}: the bar at {x},{y} came out {got:?}, the reference is {want:?}"
            );
        }
        println!("{name}: colour bars within {worst} codes of the reference");
    }
}

#[test]
fn one_to_one_is_an_exact_copy_everywhere() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    let (image, nv12) = screen.draw(size, None, &plain(), 96);
    let mut worst = 0;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            worst = worst.max(off_by(image.rgb(x, y), nv12_rgb(&nv12, x, y)));
        }
    }
    println!("one to one: every pixel within {worst} codes of the reference");
    assert!(worst <= 2, "a pixel is {worst} codes off at one to one");
}

// Fullscreen with the strip hidden: a picture the monitor's size fills it
// one to one, bottom rows included. The strip the mouse brings back goes
// over those rows, and the picture above stays where it was.
#[test]
fn a_hidden_strip_gives_the_picture_the_whole_window() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT);
    let band = band_height(96);
    screen.band = Band::Hidden;
    let (hidden, nv12) = screen.draw(size, None, &plain(), 96);
    let mut worst = 0;
    for y in [0, HEIGHT / 2, HEIGHT - band, HEIGHT - 1] {
        for x in [0, WIDTH / 2, WIDTH - 1] {
            worst = worst.max(off_by(hidden.rgb(x, y), nv12_rgb(&nv12, x, y)));
        }
    }
    assert!(worst <= 2, "hidden: a pixel is {worst} codes off");

    screen.band = Band::Over;
    let (over, nv12) = screen.draw(size, None, &plain(), 96);
    for x in [0, WIDTH / 2] {
        let above = HEIGHT - band - 1;
        assert!(
            off_by(over.rgb(x, above), nv12_rgb(&nv12, x, above)) <= 2,
            "over: the picture moved at {x},{above}"
        );
    }
    // The strip's own ink, left of its first word.
    assert!(
        close(over.rgb(2, HEIGHT - 3), INK),
        "over: no strip at the bottom"
    );
}

// At 0.3 an output pixel covers three and a third source pixels. Bilinear
// or nearest sampling steps over some one pixel lines altogether; the area
// average keeps every one of them, split over two output pixels at worst.
#[test]
fn one_pixel_lines_all_survive_scaling_down() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH * 3 / 10, HEIGHT * 3 / 10);
    for vertical in [true, false] {
        let on = |x: u32, y: u32| if vertical { x % 7 == 3 } else { y % 7 == 3 };
        let mut bgra = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                bgra.extend_from_slice(if on(x, y) {
                    &[255, 255, 255, 255]
                } else {
                    &[0, 0, 0, 255]
                });
            }
        }
        let frame = screen.pattern.convert(&bgra).unwrap();
        let video = Video {
            texture: &frame.texture,
            index: 0,
            width: frame.width,
            height: frame.height,
        };
        let image = screen.draw_video(
            (size.0, size.1 + band_height(96)),
            Some(&video),
            None,
            &plain(),
            96,
        );
        let across = if vertical { WIDTH } else { HEIGHT };
        let mut dimmest = u8::MAX;
        for line in (3..across).step_by(7) {
            // Output pixel k covers source pixels 10k/3 up to 10(k+1)/3.
            let first = line * 3 / 10;
            let last = ((line + 1) * 3 - 1) / 10;
            let brightest = (first..=last)
                .map(|at| {
                    let (x, y) = if vertical {
                        (at, size.1 / 2)
                    } else {
                        (size.0 / 2, at)
                    };
                    image.rgb(x, y)[1]
                })
                .max()
                .unwrap_or(0);
            dimmest = dimmest.min(brightest);
            // Half a line under a pixel of 10/3: 38 codes over black.
            assert!(
                brightest >= 25,
                "{} line {line} faded to {brightest} at 0.3",
                if vertical { "column" } else { "row" }
            );
        }
        println!(
            "{} lines at 0.3: the dimmest at {dimmest} over black",
            if vertical {
                "one pixel wide"
            } else {
                "one pixel tall"
            }
        );
    }
}

#[test]
fn bars_around_the_picture() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let band = band_height(96);
    // 800 wide for a 640 wide picture: 80 px of ink each side.
    let (wide, _) = screen.draw((800, HEIGHT + band), None, &plain(), 96);
    for y in [0, HEIGHT / 2, HEIGHT - 1] {
        for x in [0, 79, 720, 799] {
            assert!(close(wide.rgb(x, y), INK), "wide: {x},{y} is not ink");
        }
        assert!(!close(wide.rgb(80, HEIGHT * 7 / 16), INK));
    }
    // 600 tall above the strip for a 360 tall picture: 120 px each end.
    let (tall, _) = screen.draw((WIDTH, 600 + band), None, &plain(), 96);
    for x in [0, WIDTH / 2, WIDTH - 1] {
        for y in [0, 119, 480, 599] {
            assert!(close(tall.rgb(x, y), INK), "tall: {x},{y} is not ink");
        }
    }
    // The white bar's top left corner is the picture's first row there.
    assert!(!close(tall.rgb(0, 120 + HEIGHT * 3 / 8), INK));
}

fn colour_pointer() -> CursorShape {
    // 16x16 opaque red with one green pixel at the hotspot (5, 3) and a
    // transparent last column.
    let mut bytes = Vec::new();
    for y in 0..16 {
        for x in 0..16 {
            let pixel = if (x, y) == (5, 3) {
                [0, 255, 0, 255]
            } else if x == 15 {
                [0, 0, 0, 0]
            } else {
                [0, 0, 255, 255]
            };
            bytes.extend_from_slice(&pixel);
        }
    }
    CursorShape {
        kind: CursorKind::Color,
        width: 16,
        height: 16,
        pitch: 64,
        hotspot_x: 5,
        hotspot_y: 3,
        bytes,
    }
}

#[test]
fn a_colour_pointer_lands_with_its_hotspot_on_the_position() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    let cursor = Cursor {
        x: 100,
        y: 50,
        visible: true,
        scale: 1.0,
        shape: Some(colour_pointer()),
    };
    let (image, nv12) = screen.draw(size, Some(&cursor), &plain(), 96);
    assert_eq!(image.rgb(105, 53), [0, 255, 0], "the hotspot pixel");
    assert_eq!(image.rgb(104, 53), [255, 0, 0]);
    assert_eq!(image.rgb(100, 50), [255, 0, 0], "the top left corner");
    assert_eq!(image.rgb(114, 65), [255, 0, 0], "the bottom right corner");
    // Transparent column and outside: the picture shows through.
    for (x, y) in [(115, 55), (99, 55), (105, 66)] {
        assert!(
            off_by(image.rgb(x, y), nv12_rgb(&nv12, x, y)) <= 2,
            "{x},{y}"
        );
    }

    // Moved, and the same shape kept without being sent again.
    let moved = Cursor {
        x: 300,
        y: 200,
        shape: None,
        ..cursor.clone()
    };
    let (image, _) = screen.draw(size, Some(&moved), &plain(), 96);
    assert_eq!(image.rgb(305, 203), [0, 255, 0]);

    // At half size the hotspot (305, 203) lands on (152.5, 101.5) and the
    // pointer is 8 pixels a side, so its top left is (150, 100).
    let half = (WIDTH / 2, HEIGHT / 2 + band_height(96));
    let (image, _) = screen.draw(half, None, &plain(), 96);
    assert_eq!(image.rgb(150, 100), [255, 0, 0], "half size: top left");
    assert_eq!(image.rgb(151, 106), [255, 0, 0], "half size: inside");
    for (x, y) in [(149, 104), (152, 99), (152, 108)] {
        let got = image.rgb(x, y);
        assert!(
            off_by(got, [255, 0, 0]) > 40,
            "half size: {x},{y} is pointer red"
        );
    }

    // Hidden by the sharer: gone.
    let hidden = Cursor {
        visible: false,
        shape: None,
        ..moved
    };
    let (image, nv12) = screen.draw(size, Some(&hidden), &plain(), 96);
    assert!(off_by(image.rgb(305, 203), nv12_rgb(&nv12, 305, 203)) <= 2);
}

// While this PC controls in absolute mode, the sharer's pointer is drawn
// where the controller's mouse is, not where the sharer last said, until
// the mouse rests a second away from the sharer's position.
#[test]
fn the_pointer_follows_the_controllers_mouse() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    let echo = Cursor {
        x: 100,
        y: 50,
        visible: true,
        scale: 1.0,
        shape: Some(colour_pointer()),
    };
    screen.local = Some(Local {
        at: (400, 200),
        still: Duration::from_millis(30),
    });
    let (image, nv12) = screen.draw(size, Some(&echo), &plain(), 96);
    assert_eq!(image.rgb(400, 200), [0, 255, 0], "the hotspot on the mouse");
    assert_eq!(image.rgb(395, 197), [255, 0, 0]);
    assert!(
        off_by(image.rgb(105, 53), nv12_rgb(&nv12, 105, 53)) <= 2,
        "the sharer's own position is not drawn as well"
    );

    // Rested a second where the sharer's pointer is not: a program there
    // moved it, and the viewer shows where it is.
    screen.local = Some(Local {
        at: (400, 200),
        still: control::DRIFT_AFTER,
    });
    let (image, nv12) = screen.draw(size, None, &plain(), 96);
    assert_eq!(
        image.rgb(105, 53),
        [0, 255, 0],
        "back where the sharer has it"
    );
    assert!(off_by(image.rgb(400, 200), nv12_rgb(&nv12, 400, 200)) <= 2);

    // Resting on it, the mouse wins.
    screen.local = Some(Local {
        at: (106, 52),
        still: control::DRIFT_AFTER,
    });
    let (image, _) = screen.draw(size, None, &plain(), 96);
    assert_eq!(image.rgb(106, 52), [0, 255, 0]);
}

#[test]
fn a_monochrome_pointer_inverts_like_windows() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    // 16x16, two bytes a row, the AND rows and then the XOR rows. Top left:
    // AND 0, XOR 1, white. Top right: AND 0, XOR 0, black. Bottom left:
    // AND 1, XOR 1, the screen inverted. Bottom right: AND 1, XOR 0, the
    // screen.
    let mut and = Vec::new();
    let mut xor = Vec::new();
    for y in 0..16 {
        and.extend_from_slice(if y < 8 { &[0x00, 0x00] } else { &[0xff, 0xff] });
        xor.extend_from_slice(&[0xff, 0x00]);
    }
    let shape = CursorShape {
        kind: CursorKind::Monochrome,
        width: 16,
        height: 32,
        pitch: 2,
        hotspot_x: 0,
        hotspot_y: 0,
        bytes: [and, xor].concat(),
    };
    // Over the red bar, which is flat.
    let (left, top) = (5 * WIDTH / 8 + 20, HEIGHT * 3 / 8 + 5);
    let cursor = Cursor {
        x: left as i32,
        y: top as i32,
        visible: true,
        scale: 1.0,
        shape: Some(shape),
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    let (image, nv12) = screen.draw(size, Some(&cursor), &plain(), 96);
    let under = nv12_rgb(&nv12, left + 4, top + 12);
    assert_eq!(image.rgb(left + 2, top + 2), [255, 255, 255], "white");
    assert_eq!(image.rgb(left + 10, top + 2), [0, 0, 0], "black");
    let inverted = under.map(|c| 255 - c);
    assert!(
        off_by(image.rgb(left + 4, top + 12), inverted) <= 2,
        "inverted: {:?} over {under:?}",
        image.rgb(left + 4, top + 12)
    );
    assert!(off_by(image.rgb(left + 12, top + 12), under) <= 2, "screen");
}

fn live_strip() -> Strip {
    Strip {
        state: LinkState::Live,
        rtt_ms: Some(4.0),
        jitter_ms: Some(0.4),
        loss_pct: Some(0.0),
        path: Some(PathWord::Lan),
        trace: (0..120).map(|_| TraceSample::Rtt(4.0)).collect(),
        encode_ms: Some(2.1),
        decode_ms: Some(1.4),
        end_to_end_ms: Some(11.0),
        end_to_end_level: Level::Good,
        ..Strip::default()
    }
}

fn is_text(rgb: [u8; 3]) -> bool {
    off_by(rgb, INK.rgb()) > 40
}

#[test]
fn the_strip_has_its_words_its_hairline_and_its_trace() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    for dpi in [96, 144] {
        let band = band_height(dpi);
        let scale = dpi as f32 / 96.0;
        let size = (1200, HEIGHT + band);
        let strip = live_strip();
        let (image, _) = screen.draw(size, None, &strip, dpi);
        let top = size.1 - band;
        let look = Look {
            size,
            dpi,
            scrolling: true,
            present: Some(PresentPath::Flip),
            band: Band::Below,
        };
        let spans = screen.scene.painter().word_spans(&strip, &look).unwrap();
        assert_eq!(spans.len(), 8, "{dpi}: {spans:?}");
        let rows = top + scale.round() as u32 + 1..size.1 - 1;
        let text_in = |from: f32, to: f32| {
            let mut count = 0;
            for y in rows.clone() {
                for x in from.floor() as u32..to.ceil() as u32 {
                    if is_text(image.rgb(x, y)) {
                        count += 1;
                    }
                }
            }
            count
        };
        for (word, from, to) in &spans {
            let count = text_in(*from, *to);
            assert!(
                count > 10,
                "{dpi}: no text where {word:?} goes ({count} pixels)"
            );
        }
        // Between the words and before the first, nothing but ink.
        let gaps: Vec<(f32, f32)> = spans
            .windows(2)
            .map(|pair| (pair[0].2 + 1.0, pair[1].1 - 1.0))
            .chain([(0.0, spans[0].1 - 1.0)])
            .collect();
        for (from, to) in gaps {
            assert_eq!(text_in(from, to), 0, "{dpi}: text in the gap {from}..{to}");
        }
        // The hairline along the band's top edge.
        let line_row = top + scale.round() as u32 - 1;
        assert!(close(image.rgb(600, line_row), LINE), "{dpi}: no hairline");
        // A flat 4 ms trace: one sage row near the bottom of the slot, the
        // full 120 samples wide.
        let slot_left = size.0 as f32 - 132.0 * scale;
        let row = top + ((2.0 + 14.0) * scale).round() as u32;
        let sage = (0..120)
            .filter(|&i| {
                let x = (slot_left + (i as f32 + 0.5) * scale) as u32;
                close(image.rgb(x, row), SAGE)
            })
            .count();
        assert_eq!(sage, 120, "{dpi}: the trace is not a full sage line");
        println!("{dpi} dpi: {spans:?}");
    }
}

// "controlling, {panic key} releases" in amber after the path word, and the
// paused sentence in warn, drawn with the real face.
#[test]
fn control_words_in_their_colours() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let dpi = 96;
    let band = band_height(dpi);
    let size = (1600, HEIGHT + band);
    let strip = Strip {
        controlling: Some(String::from("Ctrl+Shift+End")),
        control_paused: true,
        ..live_strip()
    };
    let (image, _) = screen.draw(size, None, &strip, dpi);
    let look = Look {
        size,
        dpi,
        scrolling: true,
        present: Some(PresentPath::Flip),
        band: Band::Below,
    };
    let spans = screen.scene.painter().word_spans(&strip, &look).unwrap();
    let words: Vec<&str> = spans.iter().map(|span| span.0.as_str()).collect();
    let path = words.iter().position(|word| *word == "LAN").unwrap();
    assert_eq!(words[path + 1], "controlling");
    assert_eq!(
        words[path + 2],
        "The shared PC has an administrator window in front. Control is paused."
    );
    let top = size.1 - band;
    let coloured = |span: &(String, f32, f32), colour: Colour| {
        let mut count = 0;
        for y in top + 2..size.1 - 1 {
            for x in span.1.floor() as u32..span.2.ceil() as u32 {
                if close(image.rgb(x, y), colour) {
                    count += 1;
                }
            }
        }
        count
    };
    assert!(
        coloured(&spans[path + 1], AMBER) > 20,
        "controlling is not amber"
    );
    assert!(
        coloured(&spans[path + 2], WARN) > 60,
        "the sentence is not in warn"
    );
    assert_eq!(coloured(&spans[path + 2], AMBER), 0);
}

// The words that never drop out (state, path, control and present path)
// have to fit before the trace in the smallest window at every scaling,
// measured with the real face, or the trace paints over the last of them.
#[test]
fn kept_words_fit_the_smallest_window() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let longest = Strip {
        state: LinkState::Reconnecting,
        path: Some(PathWord::Direct),
        ..live_strip()
    };
    // While controlling, with a long release key, and paused as well.
    let controlling = Strip {
        controlling: Some(String::from("Ctrl+Shift+Alt+Page Down")),
        ..longest.clone()
    };
    let paused = Strip {
        control_paused: true,
        ..controlling.clone()
    };
    let cases: [(&Strip, &[&str]); 3] = [
        (&longest, &["reconnecting", "direct", "composed"]),
        (
            &controlling,
            &["reconnecting", "direct", "controlling", "composed"],
        ),
        (
            &paused,
            &[
                "reconnecting",
                "direct",
                "controlling",
                "Control is paused.",
                "composed",
            ],
        ),
    ];
    for (strip, stay) in cases {
        for dpi in [96, 120, 144, 168, 192, 240, 288] {
            let scale = dpi as f32 / 96.0;
            let width = min_client(dpi).0 as u32;
            let look = Look {
                size: (width, 400),
                dpi,
                scrolling: true,
                present: Some(PresentPath::Composed),
                band: Band::Below,
            };
            let spans = screen.scene.painter().word_spans(strip, &look).unwrap();
            let words: Vec<&str> = spans.iter().map(|span| span.0.as_str()).collect();
            for kept in stay {
                assert!(words.contains(kept), "{dpi}: {kept} was dropped: {words:?}");
            }
            let end = spans.iter().map(|span| span.2).fold(0.0, f32::max);
            // The trace starts 132 points from the right, a gap after the words.
            let trace = width as f32 - 132.0 * scale;
            println!(
                "{dpi} dpi, {width} px, {} words: they end at {end:.1}, the trace starts at {trace:.1}",
                stay.len()
            );
            assert!(
                end + 10.0 * scale <= trace + 0.5,
                "{dpi} dpi, {width} px wide: the words end at {end} and run into the trace at {trace}: {spans:?}"
            );
        }
    }
}

#[test]
fn old_decoder_textures_are_let_go_while_minimized_too() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    screen.draw(size, None, &plain(), 96);
    assert_eq!(screen.scene.cached_views(), 1);
    // Minimized, presents keep coming and nothing is drawn.
    for _ in 0..VIEW_IDLE {
        screen.scene.hold(None, None).unwrap();
    }
    assert_eq!(
        screen.scene.cached_views(),
        0,
        "an idle view outlived {VIEW_IDLE} presents while minimized"
    );

    // The sharer changes resolution while the window is minimized: the
    // decoder's new pool is another size, and the old one goes at once.
    screen.draw(size, None, &plain(), 96);
    assert_eq!(screen.scene.cached_views(), 1);
    let mut smaller = Pattern::new(&screen.gpu.device, WIDTH / 2, HEIGHT / 2, 0).unwrap();
    let frame = smaller.next().unwrap();
    let video = Video {
        texture: &frame.texture,
        index: 0,
        width: frame.width,
        height: frame.height,
    };
    screen.scene.hold(Some(&video), None).unwrap();
    assert_eq!(
        screen.scene.cached_views(),
        0,
        "the old size's views outlived a picture of a new size"
    );
}

#[test]
fn a_still_frame_draws_the_kept_picture() {
    let Some(mut screen) = Offscreen::new() else {
        return;
    };
    let size = (WIDTH, HEIGHT + band_height(96));
    let (first, _) = screen.draw(size, None, &plain(), 96);
    // No new picture: the copy kept after the last one is drawn instead.
    let again = screen.draw_video(size, None, None, &plain(), 96);
    let differ = (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .filter(|&(x, y)| off_by(first.rgb(x, y), again.rgb(x, y)) > 0)
        .count();
    assert_eq!(differ, 0, "{differ} pixels differ from the frame before");
}
