// The colour conversion done on the CPU, in doubles, the slow and obvious
// way. Tests hold the shader to it; decode tests can compare against it.
// Only ever fed test pictures.

use crate::Options;
use crate::convert::Plan;
use crate::monitors::Rotation;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nv12Image {
    pub width: u32,
    pub height: u32,
    // width bytes per row, height rows.
    pub y: Vec<u8>,
    // U and V interleaved, width bytes per row, height / 2 rows.
    pub uv: Vec<u8>,
}

impl Nv12Image {
    pub fn luma(&self, x: u32, y: u32) -> u8 {
        self.y[(y * self.width + x) as usize]
    }

    pub fn chroma(&self, x: u32, y: u32) -> (u8, u8) {
        let at = (y * self.width + x * 2) as usize;
        (self.uv[at], self.uv[at + 1])
    }
}

const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

// As Capture::open with these options converts a monitor's picture of this
// size and rotation; the frame rate plays no part.
pub fn to_nv12(
    bgra: &[u8],
    source_width: u32,
    source_height: u32,
    rotation: Rotation,
    options: Options,
) -> Nv12Image {
    let plan = Plan::new(
        source_width,
        source_height,
        rotation,
        options.max_width,
        options.max_height,
    );
    assert_eq!(
        bgra.len(),
        (plan.source_width * plan.source_height * 4) as usize,
        "the picture is not {}x{} BGRA",
        plan.source_width,
        plan.source_height
    );
    let mut y = vec![0; (plan.width * plan.height) as usize];
    by_rows(&mut y, plan.width, |row, out| {
        for (column, luma) in (0..).zip(out.iter_mut()) {
            let rgb = area_average(bgra, &plan, column, row, plan.footprint);
            *luma = code(16.0 + 219.0 * dot(rgb));
        }
    });
    let double = (plan.footprint.0 * 2.0, plan.footprint.1 * 2.0);
    let mut uv = vec![0; (plan.width * plan.height / 2) as usize];
    by_rows(&mut uv, plan.width, |row, out| {
        for (column, [u, v]) in (0..).zip(out.as_chunks_mut::<2>().0) {
            let rgb = area_average(bgra, &plan, column, row, double);
            let luma = dot(rgb);
            *u = code(128.0 + 224.0 * (rgb[2] - luma) / 1.8556);
            *v = code(128.0 + 224.0 * (rgb[0] - luma) / 1.5748);
        }
    });
    Nv12Image {
        width: plan.width,
        height: plan.height,
        y,
        uv,
    }
}

// Rows of `width` bytes, filled by `fill(row, bytes)`, spread over the
// CPU's cores: tests run this unoptimised on pictures up to 7680x2160,
// which takes seconds on one.
fn by_rows(plane: &mut [u8], width: u32, fill: impl Fn(u32, &mut [u8]) + Sync) {
    let width = width as usize;
    let rows = plane.len() / width;
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get());
    let per_thread = rows.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (first, chunk) in (0usize..)
            .step_by(per_thread)
            .zip(plane.chunks_mut(per_thread * width))
        {
            let fill = &fill;
            scope.spawn(move || {
                for (row, bytes) in (first..).zip(chunk.chunks_mut(width)) {
                    fill(row as u32, bytes);
                }
            });
        }
    });
}

fn dot(rgb: [f64; 3]) -> f64 {
    rgb[0] * LUMA[0] + rgb[1] * LUMA[1] + rgb[2] * LUMA[2]
}

fn code(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

// The same footprint and tap range as the shader, whose positions are f32.
fn area_average(
    bgra: &[u8],
    plan: &Plan,
    column: u32,
    row: u32,
    footprint: (f32, f32),
) -> [f64; 3] {
    let start = (column as f32 * footprint.0, row as f32 * footprint.1);
    let end = (start.0 + footprint.0, start.1 + footprint.1);
    let mut sum = [0.0; 3];
    let mut total = 0.0;
    for y in start.1.floor() as i64..end.1.ceil() as i64 {
        let wy = end.1.min(y as f32 + 1.0) as f64 - start.1.max(y as f32) as f64;
        for x in start.0.floor() as i64..end.0.ceil() as i64 {
            let w = wy * (end.0.min(x as f32 + 1.0) as f64 - start.0.max(x as f32) as f64);
            let rgb = load_upright(bgra, plan, x, y);
            for (sum, value) in sum.iter_mut().zip(rgb) {
                *sum += w * value;
            }
            total += w;
        }
    }
    sum.map(|value| value / total)
}

fn load_upright(bgra: &[u8], plan: &Plan, x: i64, y: i64) -> [f64; 3] {
    let x = x.clamp(0, plan.upright_width as i64 - 1);
    let y = y.clamp(0, plan.upright_height as i64 - 1);
    let last_x = plan.source_width as i64 - 1;
    let last_y = plan.source_height as i64 - 1;
    let (sx, sy) = match plan.rotation {
        Rotation::Identity => (x, y),
        Rotation::Rotate90 => (y, last_y - x),
        Rotation::Rotate180 => (last_x - x, last_y - y),
        Rotation::Rotate270 => (last_x - y, x),
    };
    let at = ((sy * plan.source_width as i64 + sx) * 4) as usize;
    [
        bgra[at + 2] as f64 / 255.0,
        bgra[at + 1] as f64 / 255.0,
        bgra[at] as f64 / 255.0,
    ]
}
