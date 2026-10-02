// The sharer's pointer, drawn by the viewer from its shape and position so
// it moves without waiting for an encode. The shape comes as Desktop
// Duplication hands it over; it is turned into textures once per change.

use std::ffi::c_void;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_INV_SRC_COLOR,
    D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_SRC_COLOR,
    D3D11_BLEND_ZERO, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER,
    D3D11_CULL_NONE, D3D11_FILL_SOLID, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC,
    D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT, ID3D11BlendState, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::s;

use crate::control;
use crate::error::ViewerError;
use crate::picture::Placement;
use crate::shader;

const SOURCE: &str = include_str!("cursor.hlsl");

// Desktop Duplication pointers are 32 to 256 pixels a side; anything much
// bigger is not a pointer and is not turned into a texture.
const MAX_SIDE: u32 = 1024;

// Picture pixels per desktop pixel. A 4K desktop shared at 720p is a third
// and nothing is shared bigger than one to one, so every real pointer is
// inside this range with room to spare; a sharer's scale outside it is
// clamped, so it cannot stretch the pointer across the picture.
const SCALE_RANGE: (f32, f32) = (1.0 / 16.0, 16.0);

// Position and hotspot come from the sharer and can be anything an i32
// holds. An edge this far past any target is off screen either way, and
// clamping to it keeps the sums in rect() from overflowing. f32, which the
// shader gets, holds every whole number up to here exactly.
const FAR: f64 = 16_777_216.0;

// Where the pointer is and what it looks like, as the sharer's capture
// reports it (capture::CursorUpdate carries the same fields).
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    // The pointer image's top left corner in the picture's pixels. It can
    // be negative or past the edge when the pointer is partly off screen.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    // Picture pixels per desktop pixel of the sharer's monitor, for
    // drawing the shape to scale.
    pub scale: f32,
    // Only when the shape changed.
    pub shape: Option<CursorShape>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    // 1 bit per pixel, an AND mask followed by an XOR mask, so `height` is
    // twice the pointer's height.
    Monochrome,
    // 32-bit BGRA with alpha.
    Color,
    // 32-bit BGR where the top byte says whether the pixel is XORed with
    // the screen (0xFF) or replaces it (0).
    MaskedColor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub kind: CursorKind,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bytes: Vec<u8>,
}

// The controller's mouse over the picture while this PC controls in
// absolute mode: where it is in the target's pixels, and how long it has
// been there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    pub at: (i32, i32),
    pub still: std::time::Duration,
}

// A shape as BGRA pictures. Alpha is drawn over the picture with its alpha;
// Masks multiplies the picture by `and` and then XORs `xor` into it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Images {
    Alpha {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    },
    Masks {
        width: u32,
        height: u32,
        and: Vec<u8>,
        xor: Vec<u8>,
    },
}

const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];

pub(crate) fn images(shape: &CursorShape) -> Result<Images, String> {
    let (width, height) = match shape.kind {
        CursorKind::Monochrome => (shape.width, shape.height / 2),
        CursorKind::Color | CursorKind::MaskedColor => (shape.width, shape.height),
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!("a pointer of {width}x{height} pixels"));
    }
    let row_bytes = match shape.kind {
        CursorKind::Monochrome => width.div_ceil(8),
        CursorKind::Color | CursorKind::MaskedColor => width * 4,
    };
    let rows = shape.height as usize;
    let pitch = shape.pitch as usize;
    if shape.pitch < row_bytes || shape.bytes.len() < pitch * rows {
        return Err(format!(
            "{} bytes where {rows} rows of {} bytes need {}",
            shape.bytes.len(),
            shape.pitch,
            pitch * rows
        ));
    }
    let pixels = (width * height) as usize;
    let at = |x: u32, y: u32| y as usize * pitch + x as usize * 4;
    Ok(match shape.kind {
        CursorKind::Monochrome => {
            let bit = |x: u32, y: u32| {
                let byte = shape.bytes[y as usize * pitch + (x / 8) as usize];
                byte >> (7 - x % 8) & 1 == 1
            };
            let mut and = Vec::with_capacity(pixels * 4);
            let mut xor = Vec::with_capacity(pixels * 4);
            for y in 0..height {
                for x in 0..width {
                    and.extend_from_slice(if bit(x, y) { &WHITE } else { &BLACK });
                    xor.extend_from_slice(if bit(x, y + height) { &WHITE } else { &BLACK });
                }
            }
            Images::Masks {
                width,
                height,
                and,
                xor,
            }
        }
        CursorKind::MaskedColor => {
            let mut and = Vec::with_capacity(pixels * 4);
            let mut xor = Vec::with_capacity(pixels * 4);
            for y in 0..height {
                for x in 0..width {
                    let pixel = &shape.bytes[at(x, y)..at(x, y) + 4];
                    and.extend_from_slice(if pixel[3] == 0 { &BLACK } else { &WHITE });
                    xor.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
            }
            Images::Masks {
                width,
                height,
                and,
                xor,
            }
        }
        CursorKind::Color => {
            let mut bgra = Vec::with_capacity(pixels * 4);
            for y in 0..height {
                bgra.extend_from_slice(&shape.bytes[at(0, y)..at(0, y) + width as usize * 4]);
            }
            Images::Alpha {
                width,
                height,
                bgra,
            }
        }
    })
}

// Where the pointer image goes in the target, in whole pixels so a pointer
// at one to one stays sharp. The hotspot, the pixel that points, lands on
// the scaled position of the sharer's hotspot.
#[cfg(test)]
pub(crate) fn rect(
    x: i32,
    y: i32,
    scale: f32,
    size: (u32, u32),
    hotspot: (i32, i32),
    placed: &Placement,
) -> RECT {
    around(
        hot(x, y, scale, hotspot, placed),
        scale,
        size,
        hotspot,
        placed,
    )
}

// Where the sharer's hotspot is on the target, in pixels.
fn hot(x: i32, y: i32, scale: f32, hotspot: (i32, i32), placed: &Placement) -> (f64, f64) {
    let scale = scale as f64;
    let sx = placed.width as f64 / placed.source.0.max(1) as f64;
    let sy = placed.height as f64 / placed.source.1.max(1) as f64;
    (
        placed.left as f64 + (x as f64 + hotspot.0 as f64 * scale) * sx,
        placed.top as f64 + (y as f64 + hotspot.1 as f64 * scale) * sy,
    )
}

// The pointer image with its hotspot at `hot`.
fn around(
    (hot_x, hot_y): (f64, f64),
    scale: f32,
    size: (u32, u32),
    hotspot: (i32, i32),
    placed: &Placement,
) -> RECT {
    let scale = scale as f64;
    let sx = placed.width as f64 / placed.source.0.max(1) as f64;
    let sy = placed.height as f64 / placed.source.1.max(1) as f64;
    let (per_x, per_y) = (scale * sx, scale * sy);
    let left = (hot_x - hotspot.0 as f64 * per_x).round();
    let top = (hot_y - hotspot.1 as f64 * per_y).round();
    let width = (size.0 as f64 * per_x).round().max(1.0);
    let height = (size.1 as f64 * per_y).round().max(1.0);
    let edge = |value: f64| value.clamp(-FAR, FAR) as i32;
    RECT {
        left: edge(left),
        top: edge(top),
        right: edge(left + width),
        bottom: edge(top + height),
    }
}

// None for a scale that says nothing (zero, negative, not a number), which
// keeps the one before.
fn usable_scale(scale: f32) -> Option<f32> {
    (scale.is_finite() && scale > 0.0).then(|| scale.clamp(SCALE_RANGE.0, SCALE_RANGE.1))
}

struct Shape {
    // The alpha image, or the AND mask.
    first: ID3D11ShaderResourceView,
    xor: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
    hotspot: (i32, i32),
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    rect: [f32; 4],
    target_size: [f32; 2],
    unused: [f32; 2],
}

pub(crate) struct Pointer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    alpha: ID3D11BlendState,
    multiply: ID3D11BlendState,
    exclusion: ID3D11BlendState,
    scissor: ID3D11RasterizerState,
    shape: Option<Shape>,
    at: (i32, i32),
    scale: f32,
    visible: bool,
}

impl Pointer {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Pointer, ViewerError> {
        let file = s!("cursor.hlsl");
        let vertex_code = shader::compile(SOURCE, file, s!("vs_main"), s!("vs_5_0"))?;
        let pixel_code = shader::compile(SOURCE, file, s!("ps_main"), s!("ps_5_0"))?;
        // SAFETY: GetImmediateContext hands back an owned reference.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|err| ViewerError::windows("reach the Direct3D 11 context", &err))?;
        Ok(Pointer {
            device: device.clone(),
            context,
            vertex: shader::vertex(device, &vertex_code, "pointer")?,
            pixel: shader::pixel(device, &pixel_code, "pointer")?,
            constants: constant_buffer(device)?,
            sampler: sampler(device)?,
            alpha: blend(device, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA)?,
            multiply: blend(device, D3D11_BLEND_ZERO, D3D11_BLEND_SRC_COLOR)?,
            exclusion: blend(
                device,
                D3D11_BLEND_INV_DEST_COLOR,
                D3D11_BLEND_INV_SRC_COLOR,
            )?,
            scissor: scissor(device)?,
            shape: None,
            at: (0, 0),
            scale: 1.0,
            visible: false,
        })
    }

    // A shape that cannot be read leaves the pointer hidden until the next
    // one, rather than failing the frame.
    pub(crate) fn update(&mut self, cursor: &Cursor) -> Result<(), ViewerError> {
        self.at = (cursor.x, cursor.y);
        self.visible = cursor.visible;
        if let Some(scale) = usable_scale(cursor.scale) {
            self.scale = scale;
        }
        if let Some(shape) = &cursor.shape {
            self.shape = None;
            if let Ok(images) = images(shape) {
                self.shape = Some(self.upload(&images, (shape.hotspot_x, shape.hotspot_y))?);
            }
        }
        Ok(())
    }

    fn upload(&self, images: &Images, hotspot: (i32, i32)) -> Result<Shape, ViewerError> {
        Ok(match images {
            Images::Alpha {
                width,
                height,
                bgra,
            } => Shape {
                first: texture(&self.device, *width, *height, bgra)?,
                xor: None,
                width: *width,
                height: *height,
                hotspot,
            },
            Images::Masks {
                width,
                height,
                and,
                xor,
            } => Shape {
                first: texture(&self.device, *width, *height, and)?,
                xor: Some(texture(&self.device, *width, *height, xor)?),
                width: *width,
                height: *height,
                hotspot,
            },
        })
    }

    pub(crate) fn has_shape(&self) -> bool {
        self.shape.is_some()
    }

    // Over the picture only: a pointer half off the shared screen is cut at
    // its edge, as it is on the sharer's monitor. With `local`, where the
    // controller's own mouse is, unless it rested away from the sharer's
    // position (control::drawn_at). The caller holds the device lock.
    pub(crate) fn draw(
        &self,
        target: &ID3D11RenderTargetView,
        target_size: (u32, u32),
        placed: &Placement,
        local: Option<&Local>,
    ) {
        let Some(shape) = &self.shape else {
            return;
        };
        if placed.width <= 0 || placed.height <= 0 {
            return;
        }
        let echo = self
            .visible
            .then(|| hot(self.at.0, self.at.1, self.scale, shape.hotspot, placed));
        let hot = match local {
            Some(local) => control::drawn_at(local.at, local.still, echo).or(echo),
            None => echo,
        };
        let Some(hot) = hot else {
            return;
        };
        let at = around(
            hot,
            self.scale,
            (shape.width, shape.height),
            shape.hotspot,
            placed,
        );
        let constants = Constants {
            rect: [
                at.left as f32,
                at.top as f32,
                at.right as f32,
                at.bottom as f32,
            ],
            target_size: [target_size.0 as f32, target_size.1 as f32],
            unused: [0.0; 2],
        };
        let clip = RECT {
            left: placed.left,
            top: placed.top,
            right: placed.left + placed.width,
            bottom: placed.top + placed.height,
        };
        let passes: &[(&ID3D11BlendState, &ID3D11ShaderResourceView)] = match &shape.xor {
            None => &[(&self.alpha, &shape.first)],
            Some(xor) => &[(&self.multiply, &shape.first), (&self.exclusion, xor)],
        };
        // SAFETY: every object is alive and made on this device; the
        // constants are the buffer's size and outlive the call.
        unsafe {
            let context = &self.context;
            context.UpdateSubresource(
                &self.constants,
                0,
                None,
                &constants as *const Constants as *const c_void,
                0,
                0,
            );
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vertex, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShader(&self.pixel, None);
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: target_size.0 as f32,
                Height: target_size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.RSSetState(&self.scissor);
            context.RSSetScissorRects(Some(&[clip]));
            for (blend, view) in passes {
                context.OMSetBlendState(*blend, None, 0xffff_ffff);
                context.PSSetShaderResources(0, Some(&[Some((*view).clone())]));
                context.Draw(4, 0);
            }
            context.OMSetBlendState(None, None, 0xffff_ffff);
            context.RSSetState(None);
            context.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

fn constant_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer, ViewerError> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: size_of::<Constants>() as u32,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    // SAFETY: a full description, no initial data, a live out parameter.
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }
        .map_err(|err| ViewerError::windows("make the pointer's constants", &err))?;
    buffer.ok_or_else(|| ViewerError::missing("make the pointer's constants", "buffer"))
}

fn sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState, ViewerError> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let mut state = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateSamplerState(&desc, Some(&mut state)) }
        .map_err(|err| ViewerError::windows("make the pointer's sampler", &err))?;
    state.ok_or_else(|| ViewerError::missing("make the pointer's sampler", "sampler"))
}

// Colour channels only; the back buffer's alpha is ignored by the display.
fn blend(
    device: &ID3D11Device,
    source: D3D11_BLEND,
    dest: D3D11_BLEND,
) -> Result<ID3D11BlendState, ViewerError> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: source,
        DestBlend: dest,
        BlendOp: D3D11_BLEND_OP_ADD,
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut state = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateBlendState(&desc, Some(&mut state)) }
        .map_err(|err| ViewerError::windows("make the pointer's blending", &err))?;
    state.ok_or_else(|| ViewerError::missing("make the pointer's blending", "blend state"))
}

fn scissor(device: &ID3D11Device) -> Result<ID3D11RasterizerState, ViewerError> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        DepthClipEnable: true.into(),
        ScissorEnable: true.into(),
        ..Default::default()
    };
    let mut state = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateRasterizerState(&desc, Some(&mut state)) }
        .map_err(|err| ViewerError::windows("make the pointer's clipping", &err))?;
    state.ok_or_else(|| ViewerError::missing("make the pointer's clipping", "rasterizer state"))
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<ID3D11ShaderResourceView, ViewerError> {
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
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bgra.as_ptr() as *const c_void,
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `bgra` holds width x height BGRA at that pitch and outlives
    // the call; the out parameter is a live local.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }
        .map_err(|err| ViewerError::windows("make the pointer's texture", &err))?;
    let texture =
        texture.ok_or_else(|| ViewerError::missing("make the pointer's texture", "texture"))?;
    let mut view = None;
    // SAFETY: a live texture made for shader reads, the default view, a
    // live out parameter.
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view)) }
        .map_err(|err| ViewerError::windows("make the pointer's texture", &err))?;
    view.ok_or_else(|| ViewerError::missing("make the pointer's texture", "view"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(left: i32, top: i32, width: i32, height: i32, source: (u32, u32)) -> Placement {
        Placement {
            left,
            top,
            width,
            height,
            source,
        }
    }

    #[test]
    fn monochrome_splits_into_and_and_xor() {
        // 2x2 pointer, one byte a row: AND rows then XOR rows.
        // Pixel (0,0): AND 1, XOR 0, the screen shows through.
        // Pixel (1,0): AND 0, XOR 0, black.
        // Pixel (0,1): AND 0, XOR 1, white.
        // Pixel (1,1): AND 1, XOR 1, the screen inverted.
        let shape = CursorShape {
            kind: CursorKind::Monochrome,
            width: 2,
            height: 4,
            pitch: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![0b1000_0000, 0b0100_0000, 0b0000_0000, 0b1100_0000],
        };
        let Ok(Images::Masks { and, xor, .. }) = images(&shape) else {
            panic!("a monochrome pointer did not become masks");
        };
        let and: Vec<u8> = and.chunks(4).map(|p| p[0]).collect();
        let xor: Vec<u8> = xor.chunks(4).map(|p| p[0]).collect();
        assert_eq!(and, [255, 0, 0, 255]);
        assert_eq!(xor, [0, 0, 255, 255]);
    }

    #[test]
    fn masked_colour_replaces_or_xors_by_its_top_byte() {
        let shape = CursorShape {
            kind: CursorKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![10, 20, 30, 0, 40, 50, 60, 0xff],
        };
        let Ok(Images::Masks { and, xor, .. }) = images(&shape) else {
            panic!("a masked colour pointer did not become masks");
        };
        assert_eq!(and, [0, 0, 0, 255, 255, 255, 255, 255]);
        assert_eq!(xor, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn a_short_shape_is_refused_not_read_past() {
        let shape = CursorShape {
            kind: CursorKind::Color,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![0; 100],
        };
        assert!(images(&shape).is_err());
        let huge = CursorShape {
            width: 100_000,
            height: 2,
            pitch: 400_000,
            bytes: vec![0; 800_000],
            ..shape
        };
        assert!(images(&huge).is_err());
    }

    #[test]
    fn the_hotspot_lands_on_the_scaled_position() {
        // A 32x32 pointer with its hotspot at (10, 4), top left at (100, 50)
        // in a 2560x1440 picture shown at half size with a 20 px bar.
        let at = rect(
            100,
            50,
            1.0,
            (32, 32),
            (10, 4),
            &placed(20, 0, 1280, 720, (2560, 1440)),
        );
        // The hotspot in the picture is (110, 54), on screen (75, 27).
        assert_eq!((at.left, at.top, at.right, at.bottom), (70, 25, 86, 41));
        let one_to_one = rect(
            100,
            50,
            1.0,
            (32, 32),
            (10, 4),
            &placed(0, 0, 2560, 1440, (2560, 1440)),
        );
        assert_eq!(
            (
                one_to_one.left,
                one_to_one.top,
                one_to_one.right,
                one_to_one.bottom
            ),
            (100, 50, 132, 82)
        );
    }

    // Tests build with overflow checks, so a sum that overflows panics here
    // as it would have on the decode-and-present thread.
    #[test]
    fn hostile_cursor_values_stay_sane() {
        let half = placed(20, 0, 1280, 720, (2560, 1440));
        let cases = [
            (i32::MAX, i32::MAX, 1.0, (0, 0)),
            (i32::MIN, i32::MIN, 1.0, (0, 0)),
            (0, 0, 1.0, (i32::MAX, i32::MIN)),
            (i32::MAX, i32::MIN, 1e30, (i32::MIN, i32::MAX)),
            (100, 50, f32::MAX, (10, 4)),
        ];
        for (x, y, scale, hotspot) in cases {
            let at = rect(x, y, scale, (MAX_SIDE, MAX_SIDE), hotspot, &half);
            let edges = [at.left, at.top, at.right, at.bottom];
            assert!(
                edges.iter().all(|edge| edge.unsigned_abs() <= FAR as u32),
                "{x},{y} at {scale} with hotspot {hotspot:?}: {edges:?}"
            );
            assert!(at.right >= at.left && at.bottom >= at.top, "{edges:?}");
        }
    }

    #[test]
    fn the_sharers_scale_is_clamped_and_nonsense_ignored() {
        assert_eq!(usable_scale(0.5), Some(0.5));
        assert_eq!(usable_scale(1e30), Some(16.0));
        assert_eq!(usable_scale(1e-9), Some(1.0 / 16.0));
        for nonsense in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(usable_scale(nonsense), None, "{nonsense}");
        }
    }
}
