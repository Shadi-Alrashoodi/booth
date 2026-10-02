// A moving test picture with the same Frame as a capture, for every test
// that must not use the real screen. Its NV12 comes out of the same
// conversion shader, so it tests that too, and unlike a capture it may be
// read back.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

use crate::convert::{Converter, Layout, Plan, Target};
use crate::error::CaptureError;
use crate::monitors::{Rotation, wide_to_string};
use crate::reference::Nv12Image;
use crate::timer::Timer;
use crate::{Frame, Options};

pub struct Pattern {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    converter: Converter,
    layout: Layout,
    background: ID3D11Texture2D,
    source: ID3D11Texture2D,
    source_view: ID3D11ShaderResourceView,
    targets: [Target; 2],
    last_out: usize,
    readback: Option<ID3D11Texture2D>,
    period: Option<Duration>,
    timer: Timer,
    due: Option<Instant>,
    number: u64,
}

impl Pattern {
    // Frames of width x height at fps a second; fps 0 means as fast as they
    // are asked for.
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Pattern, CaptureError> {
        let kept = Options {
            max_width: 0,
            max_height: 0,
            max_fps: fps,
        };
        Pattern::with_source(device, width, height, Rotation::Identity, kept)
    }

    // A source picture of the given size, as the duplication of a monitor
    // with that rotation would hand it over, converted the way Capture::open
    // with these options converts one: turned upright and scaled down to fit
    // max_width and max_height. max_fps is the pattern's own rate.
    pub fn with_source(
        device: &ID3D11Device,
        source_width: u32,
        source_height: u32,
        rotation: Rotation,
        options: Options,
    ) -> Result<Pattern, CaptureError> {
        let gpu = adapter_name(device);
        // SAFETY: GetImmediateContext hands back an owned reference.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|err| CaptureError::windows(format!("start Direct3D 11 on {gpu}"), &err))?;
        let converter = Converter::new(device, &gpu)?;
        let plan = Plan::new(
            source_width,
            source_height,
            rotation,
            options.max_width,
            options.max_height,
        );
        let layout = converter.layout(plan)?;
        let picture = background(plan.source_width, plan.source_height);
        let background = bgra_texture(device, plan.source_width, plan.source_height, &picture)?;
        let source = bgra_texture(device, plan.source_width, plan.source_height, &picture)?;
        let mut source_view = None;
        // SAFETY: a live texture made with shader resource binding, the
        // default view, a live out parameter.
        unsafe { device.CreateShaderResourceView(&source, None, Some(&mut source_view)) }
            .map_err(|err| CaptureError::windows("make the pattern's source view", &err))?;
        let source_view = source_view.ok_or_else(|| {
            CaptureError::other("could not make the pattern's source view: Direct3D returned none")
        })?;
        let targets = [
            converter.target(plan.width, plan.height)?,
            converter.target(plan.width, plan.height)?,
        ];
        Ok(Pattern {
            device: device.clone(),
            context,
            converter,
            layout,
            background,
            source,
            source_view,
            targets,
            last_out: 1,
            readback: None,
            period: (options.max_fps > 0).then(|| Duration::from_secs(1) / options.max_fps),
            timer: Timer::new()?,
            due: None,
            number: 0,
        })
    }

    // The device the frames are on, as Capture::device: the encoder opens on
    // this one.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn width(&self) -> u32 {
        self.layout.plan.width
    }

    pub fn height(&self) -> u32 {
        self.layout.plan.height
    }

    pub fn plan(&self) -> Plan {
        self.layout.plan
    }

    pub fn compile_time(&self) -> Duration {
        self.converter.compile_time()
    }

    // The next frame, pattern_image(number) converted, at its time. A caller
    // that falls behind gets the next one at once, and the pace starts again
    // from there rather than catching up in a burst. Named like
    // Capture::next, and no more an Iterator than it is.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Frame, CaptureError> {
        if let Some(period) = self.period {
            let due = self.due.unwrap_or_else(Instant::now);
            self.timer.wait_until(due);
            self.due = Some((due + period).max(Instant::now()));
        }
        let present = Instant::now();
        let plan = self.layout.plan;
        // SAFETY: both textures were made on this device with the same size
        // and format, and each patch lies inside the source texture with as
        // many bytes as its box needs.
        unsafe {
            self.context.CopyResource(&self.source, &self.background);
            for patch in patches(self.number, plan.source_width, plan.source_height) {
                let area = D3D11_BOX {
                    left: patch.left,
                    top: patch.top,
                    front: 0,
                    right: patch.left + patch.width,
                    bottom: patch.top + patch.height,
                    back: 1,
                };
                self.context.UpdateSubresource(
                    &self.source,
                    0,
                    Some(&area),
                    patch.pixels.as_ptr() as *const c_void,
                    patch.width * 4,
                    0,
                );
            }
        }
        Ok(self.convert_source(present))
    }

    // Converts a picture of the source size, BGRA, the caller made. It does
    // not change what next() draws.
    pub fn convert(&mut self, bgra: &[u8]) -> Result<Frame, CaptureError> {
        let plan = self.layout.plan;
        let size = (plan.source_width * plan.source_height * 4) as usize;
        if bgra.len() != size {
            return Err(CaptureError::other(format!(
                "could not convert the picture: it has {} bytes where {}x{} BGRA has {size}",
                bgra.len(),
                plan.source_width,
                plan.source_height
            )));
        }
        let present = Instant::now();
        // SAFETY: `bgra` holds the whole texture at the pitch passed.
        unsafe {
            self.context.UpdateSubresource(
                &self.source,
                0,
                None,
                bgra.as_ptr() as *const c_void,
                plan.source_width * 4,
                0,
            );
        }
        Ok(self.convert_source(present))
    }

    fn convert_source(&mut self, present: Instant) -> Frame {
        let slot = 1 - self.last_out;
        self.converter
            .convert(&self.source_view, &self.layout, &self.targets[slot]);
        // SAFETY: a call on the live immediate context.
        unsafe { self.context.Flush() };
        let converted = Instant::now();
        self.last_out = slot;
        let number = self.number;
        self.number += 1;
        Frame {
            texture: self.targets[slot].texture.clone(),
            width: self.layout.plan.width,
            height: self.layout.plan.height,
            number,
            present,
            acquired: present,
            converted,
            skipped: 0,
            accumulated: 1,
            protected: false,
            cursor: None,
            gpu_convert: self.converter.gpu_time(),
        }
    }

    // Copies a frame this pattern made back to the CPU. Only this pattern's
    // own textures are read: a Frame from a real capture is refused, so
    // nothing from the screen can come back this way.
    pub fn read_back(&mut self, frame: &Frame) -> Result<Nv12Image, CaptureError> {
        let ours = self
            .targets
            .iter()
            .any(|target| target.texture.as_raw() == frame.texture.as_raw());
        if !ours {
            return Err(CaptureError::other(
                "could not read the frame back: it is not one this pattern made, and Booth never reads a captured screen",
            ));
        }
        let plan = self.layout.plan;
        if self.readback.is_none() {
            self.readback = Some(staging_nv12(&self.device, plan.width, plan.height)?);
        }
        let Some(staging) = &self.readback else {
            unreachable!("made just above");
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures are NV12 of the same size on this device;
        // Map waits for the copy and `mapped` is a live out parameter.
        unsafe {
            self.context.CopyResource(staging, &frame.texture);
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|err| CaptureError::windows("read the pattern frame back", &err))?;
        }
        let pitch = mapped.RowPitch as usize;
        let width = plan.width as usize;
        let height = plan.height as usize;
        // SAFETY: a mapped NV12 texture is the Y plane, height rows of
        // `pitch` bytes, followed by the UV plane, height / 2 rows, and stays
        // mapped until Unmap below.
        let bytes = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (height + height / 2))
        };
        let mut image = Nv12Image {
            width: plan.width,
            height: plan.height,
            y: Vec::with_capacity(width * height),
            uv: Vec::with_capacity(width * height / 2),
        };
        for row in 0..height {
            image
                .y
                .extend_from_slice(&bytes[row * pitch..row * pitch + width]);
        }
        for row in height..height + height / 2 {
            image
                .uv
                .extend_from_slice(&bytes[row * pitch..row * pitch + width]);
        }
        // SAFETY: mapped above, and `bytes` is not used past here.
        unsafe { self.context.Unmap(staging, 0) };
        Ok(image)
    }
}

fn adapter_name(device: &ID3D11Device) -> String {
    let name = device.cast::<IDXGIDevice>().and_then(|device| {
        // SAFETY: getters on live interfaces.
        let adapter = unsafe { device.GetAdapter() }?;
        // SAFETY: as above.
        unsafe { adapter.GetDesc() }
    });
    name.map(|desc| wide_to_string(&desc.Description))
        .unwrap_or_else(|_| "the graphics card".to_string())
}

fn bgra_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<ID3D11Texture2D, CaptureError> {
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
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const c_void,
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `pixels` holds width x height BGRA at that pitch and outlives
    // the call; the out parameter is a live local.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }.map_err(|err| {
        CaptureError::windows(format!("make a {width}x{height} pattern texture"), &err)
    })?;
    texture.ok_or_else(|| {
        CaptureError::other("could not make a pattern texture: Direct3D returned none")
    })
}

fn staging_nv12(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|err| CaptureError::windows("make a texture to read the pattern back", &err))?;
    texture.ok_or_else(|| {
        CaptureError::other(
            "could not make a texture to read the pattern back: Direct3D returned none",
        )
    })
}

// The frame number is written as 36 blocks along the top: white, black,
// the low 32 bits of the number from the highest bit down (white is 1),
// the parity of those bits and its opposite. Blocks are a 36th of the width
// square, 71 pixels at 2560 wide, which survives an encode at 2 Mbit/s,
// and they scale with the picture, so a scaled-down pattern reads too.
const BLOCKS: u32 = 36;

fn block_size(width: u32) -> u32 {
    (width / BLOCKS).max(1)
}

fn number_bits(number: u64) -> [bool; BLOCKS as usize] {
    let low = number as u32;
    let mut bits = [false; BLOCKS as usize];
    bits[0] = true;
    for bit in 0..32 {
        bits[2 + bit] = low >> (31 - bit) & 1 == 1;
    }
    let parity = low.count_ones() % 2 == 1;
    bits[34] = parity;
    bits[35] = !parity;
    bits
}

// The low 32 bits of the frame number from the Y plane of a pattern frame,
// or None if the blocks do not read as one. `nv12` starts with the Y plane,
// `pitch` bytes a row.
pub fn read_frame_number(nv12: &[u8], pitch: usize, width: u32, height: u32) -> Option<u32> {
    let size = block_size(width);
    // Narrower than 36 pixels, the blocks do not fit, and the pattern never
    // wrote them whole.
    if width < BLOCKS
        || height < size
        || pitch < width as usize
        || nv12.len() < pitch * size as usize
    {
        return None;
    }
    let inner = (size / 4, (size * 3 / 4).max(size / 4 + 1));
    let mut bits = [false; BLOCKS as usize];
    for (block, bit) in bits.iter_mut().enumerate() {
        let left = block as u32 * size;
        let mut sum = 0u64;
        let mut count = 0u64;
        for y in inner.0..inner.1 {
            for x in left + inner.0..left + inner.1 {
                sum += nv12[y as usize * pitch + x as usize] as u64;
                count += 1;
            }
        }
        // Halfway between limited range black (16) and white (235).
        *bit = sum * 2 > count * 251;
    }
    let mut low = 0u32;
    for bit in &bits[2..34] {
        low = low << 1 | *bit as u32;
    }
    (bits == number_bits(low as u64)).then_some(low)
}

struct Patch {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Patch {
    fn draw(&self, image: &mut [u8], image_width: u32) {
        for row in 0..self.height {
            let from = (row * self.width * 4) as usize;
            let to = (((self.top + row) * image_width + self.left) * 4) as usize;
            let len = (self.width * 4) as usize;
            image[to..to + len].copy_from_slice(&self.pixels[from..from + len]);
        }
    }
}

// What changes from frame to frame: the number row and the moving box.
fn patches(number: u64, width: u32, height: u32) -> Vec<Patch> {
    let mut patches = Vec::with_capacity(2);
    if width == 0 || height == 0 {
        return patches;
    }
    let size = block_size(width);
    if size <= height {
        let bits = number_bits(number);
        let mut pixels = Vec::with_capacity((width * size * 4) as usize);
        for _ in 0..size {
            for x in 0..width {
                let block = x / size;
                let value = match bits.get(block as usize) {
                    Some(true) => 255,
                    Some(false) => 0,
                    None => 128,
                };
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        patches.push(Patch {
            left: 0,
            top: 0,
            width,
            height: size,
            pixels,
        });
    }
    let side = (height / 12).max(4).min(width).min(height);
    // Four pixels a frame, back and forth, so there is no jump at the edge.
    let travel = (width - side) as u64;
    let left = if travel == 0 {
        0
    } else {
        let step = number.wrapping_mul(4) % (2 * travel);
        (if step <= travel {
            step
        } else {
            2 * travel - step
        }) as u32
    };
    let top = (size + height / 16).min(height - side);
    let mut pixels = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let border = x < 2 || y < 2 || x + 2 >= side || y + 2 >= side;
            let bgra = if border {
                [255, 255, 255, 255]
            } else {
                [0, 96, 255, 255]
            };
            pixels.extend_from_slice(&bgra);
        }
    }
    patches.push(Patch {
        left,
        top,
        width: side,
        height: side,
        pixels,
    });
    patches
}

// Frame `number` of the pattern at this size, BGRA, top row first. A pure
// function of its arguments: gradients, colour bars, one-pixel lines,
// text-like detail, a moving box and the number row. A size of 0 gives no
// bytes.
pub fn pattern_image(number: u64, width: u32, height: u32) -> Vec<u8> {
    let mut image = background(width, height);
    for patch in patches(number, width, height) {
        patch.draw(&mut image, width);
    }
    image
}

const BARS: [[u8; 4]; 8] = [
    [255, 255, 255, 255],
    [0, 255, 255, 255],
    [255, 255, 0, 255],
    [0, 255, 0, 255],
    [255, 0, 255, 255],
    [0, 0, 255, 255],
    [255, 0, 0, 255],
    [0, 0, 0, 255],
];

fn background(width: u32, height: u32) -> Vec<u8> {
    let mut image = Vec::with_capacity((width * height * 4) as usize);
    let across = width.saturating_sub(1).max(1);
    let down = height.saturating_sub(1).max(1);
    for y in 0..height {
        for x in 0..width {
            let pixel = if y < height * 3 / 8 {
                // Smooth gradients, where banding and blocking show.
                let r = x * 255 / across;
                let g = y * 255 / down;
                let b = (x + y) * 255 / (across + down);
                [b as u8, g as u8, r as u8, 255]
            } else if y < height / 2 {
                BARS[(x * 8 / width) as usize]
            } else if y < height * 5 / 8 {
                // One-pixel lines, upright on the left, lying on the right.
                let on = if x < width / 2 {
                    x % 2 == 0
                } else {
                    y % 2 == 0
                };
                let value = if on { 255 } else { 0 };
                [value, value, value, 255]
            } else {
                let value = if text_ink(x, y) { 0 } else { 255 };
                [value, value, value, 255]
            };
            image.extend_from_slice(&pixel);
        }
    }
    image
}

// Black glyphs on white: 5x7 dots in 6x10 cells, some cells left blank as
// gaps between words. The dots are a hash, which is enough to look like
// text to an encoder.
fn text_ink(x: u32, y: u32) -> bool {
    let (cell_x, cell_y) = (x / 6, y / 10);
    let (dot_x, dot_y) = (x % 6, y % 10);
    if dot_x == 5 || !(1..8).contains(&dot_y) || mix(cell_x, cell_y).is_multiple_of(7) {
        return false;
    }
    mix(cell_x * 8 + dot_x, cell_y * 16 + dot_y) % 5 < 2
}

fn mix(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9e37_79b1) ^ b.wrapping_mul(0x85eb_ca77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^ (h >> 12)
}
