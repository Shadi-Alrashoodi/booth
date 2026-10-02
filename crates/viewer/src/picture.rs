// The decoded picture on the back buffer: NV12 to RGB, scaled to fit with
// the aspect ratio kept. The picture's texture belongs to the decoder, so
// after each present the viewer copies it into a texture of its own; a
// present with no new picture (the pointer moved on a still screen, the
// window changed size) draws that copy instead of a surface the decoder may
// be writing the next frame into.

use std::ffi::c_void;

use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_BUFFER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::{Interface, s};

use crate::Video;
use crate::error::ViewerError;
use crate::palette::INK;
use crate::shader;

const SOURCE: &str = include_str!("picture.hlsl");

// A view pair not drawn for this many presents is let go. A pool that a new
// one replaces goes at once (see tick); this is for a pool the decoder let go
// of with nothing in its place, so the viewer's views alone do not keep it
// in video memory for long.
pub(crate) const VIEW_IDLE: u64 = 240;

// Where the picture lands in the target, in whole pixels, and the size of
// the picture it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub source: (u32, u32),
}

// The picture scaled to fit `region` with its aspect ratio kept, centred.
// The leftover goes to the bars, split evenly, the odd pixel to the right
// or bottom one.
pub(crate) fn fit(source: (u32, u32), region: (u32, u32)) -> Placement {
    let (sw, sh) = (source.0.max(1) as f64, source.1.max(1) as f64);
    let (rw, rh) = (region.0 as f64, region.1 as f64);
    let scale = (rw / sw).min(rh / sh);
    let width = ((sw * scale).round() as i32).clamp(0, region.0 as i32);
    let height = ((sh * scale).round() as i32).clamp(0, region.1 as i32);
    Placement {
        left: (region.0 as i32 - width) / 2,
        top: (region.1 as i32 - height) / 2,
        width,
        height,
        source,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Constants {
    area: [f32; 4],
    source_size: [f32; 2],
    footprint: [f32; 2],
    ink: [f32; 4],
}

struct Planes {
    texture: ID3D11Texture2D,
    index: u32,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    used: u64,
}

struct Kept {
    planes: Planes,
    width: u32,
    height: u32,
    filled: bool,
}

pub(crate) struct Picture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    constants: ID3D11Buffer,
    views: Vec<Planes>,
    kept: Option<Kept>,
    presents: u64,
}

impl Picture {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Picture, ViewerError> {
        let file = s!("picture.hlsl");
        let vertex_code = shader::compile(SOURCE, file, s!("vs_main"), s!("vs_5_0"))?;
        let pixel_code = shader::compile(SOURCE, file, s!("ps_main"), s!("ps_5_0"))?;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Constants>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut constants = None;
        // SAFETY: a full description, no initial data, a live out parameter.
        unsafe { device.CreateBuffer(&desc, None, Some(&mut constants)) }
            .map_err(|err| ViewerError::windows("make the picture's constants", &err))?;
        // SAFETY: GetImmediateContext hands back an owned reference.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|err| ViewerError::windows("reach the Direct3D 11 context", &err))?;
        Ok(Picture {
            device: device.clone(),
            context,
            vertex: shader::vertex(device, &vertex_code, "picture")?,
            pixel: shader::pixel(device, &pixel_code, "picture")?,
            constants: constants
                .ok_or_else(|| ViewerError::missing("make the picture's constants", "buffer"))?,
            views: Vec::new(),
            kept: None,
            presents: 0,
        })
    }

    // Once a present, drawn or not, so a window that stays minimized lets go
    // of the decoder's old surfaces too. The decoder's pool is one array
    // texture, and a new decoder (a codec switch) or a new size brings a new
    // one, so a picture in a texture the viewer holds no views of lets the
    // views of every other go at once, whatever its size, instead of keeping
    // the old pool in video memory until they go idle. Only the views held
    // count as seen: a texture let go may be freed, and a new pool can come
    // back at its address. The test pattern's two textures in turn get
    // their views made again every present, about 2 us a pair.
    pub(crate) fn tick(&mut self, video: Option<&Video>) {
        self.presents += 1;
        let presents = self.presents;
        self.views
            .retain(|planes| presents - planes.used < VIEW_IDLE);
        if let Some(video) = video {
            let raw = video.texture.as_raw();
            if !self
                .views
                .iter()
                .any(|planes| planes.texture.as_raw() == raw)
            {
                self.views.clear();
            }
        }
    }

    // Draws `video`, or the copy of the last one when there is none, over
    // the `region` at the target's top left, and ink where the picture is
    // not. The caller holds the device lock and has cleared the state.
    pub(crate) fn draw(
        &mut self,
        target: &ID3D11RenderTargetView,
        region: (u32, u32),
        video: Option<&Video>,
    ) -> Result<Option<Placement>, ViewerError> {
        self.tick(video);
        if region.0 == 0 || region.1 == 0 {
            return Ok(None);
        }
        let (luma, chroma, source) = match video {
            Some(video) => {
                check(video)?;
                let planes = self.planes(video)?;
                (
                    planes.luma.clone(),
                    planes.chroma.clone(),
                    (video.width, video.height),
                )
            }
            None => match &self.kept {
                Some(kept) if kept.filled => (
                    kept.planes.luma.clone(),
                    kept.planes.chroma.clone(),
                    (kept.width, kept.height),
                ),
                _ => {
                    // SAFETY: a live view on this device.
                    unsafe {
                        self.context.ClearRenderTargetView(target, &INK.floats());
                    }
                    return Ok(None);
                }
            },
        };
        let placement = fit(source, region);
        let constants = Constants {
            area: [
                placement.left as f32,
                placement.top as f32,
                (placement.left + placement.width) as f32,
                (placement.top + placement.height) as f32,
            ],
            source_size: [source.0 as f32, source.1 as f32],
            footprint: [
                source.0 as f32 / placement.width.max(1) as f32,
                source.1 as f32 / placement.height.max(1) as f32,
            ],
            ink: INK.floats(),
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
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(luma), Some(chroma)]));
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: region.0 as f32,
                Height: region.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.Draw(3, 0);
            // Unbound, so the decoder's surface is not tied to the
            // pipeline while it decodes the next frame into another.
            context.PSSetShaderResources(0, Some(&[None, None]));
        }
        Ok(Some(placement))
    }

    // Copies the picture just drawn into the viewer's own texture. Called
    // after the present, so it never delays the frame on screen.
    pub(crate) fn keep(&mut self, video: &Video) -> Result<(), ViewerError> {
        check(video)?;
        let size_changed = self
            .kept
            .as_ref()
            .is_none_or(|kept| (kept.width, kept.height) != (video.width, video.height));
        if size_changed {
            self.kept = None;
            let texture = nv12_texture(&self.device, video.width, video.height)?;
            let planes = planes(&self.device, &texture, 0)?;
            self.kept = Some(Kept {
                planes,
                width: video.width,
                height: video.height,
                filled: false,
            });
        }
        let Some(kept) = &mut self.kept else {
            return Ok(());
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture.
        unsafe { video.texture.GetDesc(&mut desc) };
        let area = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: video.width,
            bottom: video.height,
            back: 1,
        };
        // SAFETY: both textures are NV12 on this device; the box lies inside
        // the source (checked before the draw) and fits the copy exactly;
        // an NV12 array slice is one subresource per mip level.
        unsafe {
            self.context.CopySubresourceRegion(
                &kept.planes.texture,
                0,
                0,
                0,
                0,
                video.texture,
                video.index * desc.MipLevels.max(1),
                Some(&area),
            );
        }
        kept.filled = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn cached_views(&self) -> usize {
        self.views.len()
    }

    // The views `video` is drawn with this present, made the first time its
    // slice is drawn. Called after tick.
    fn planes(&mut self, video: &Video) -> Result<&mut Planes, ViewerError> {
        let raw = video.texture.as_raw();
        let found = self
            .views
            .iter()
            .position(|planes| planes.texture.as_raw() == raw && planes.index == video.index);
        let at = match found {
            Some(at) => at,
            None => {
                self.views
                    .push(planes(&self.device, video.texture, video.index)?);
                self.views.len() - 1
            }
        };
        let planes = &mut self.views[at];
        planes.used = self.presents;
        Ok(planes)
    }
}

// What the decoder hands over has to be what this code reads: NV12 that a
// shader may sample, with the slice and the picture inside it.
fn check(video: &Video) -> Result<(), ViewerError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: a getter on a live texture.
    unsafe { video.texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_NV12 {
        return Err(ViewerError::other(format!(
            "could not show the picture: its texture is format {} where the viewer takes NV12 ({})",
            desc.Format.0, DXGI_FORMAT_NV12.0
        )));
    }
    if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0 {
        return Err(ViewerError::other(
            "could not show the picture: its texture was made without shader resource binding, so the viewer cannot read it",
        ));
    }
    if video.index >= desc.ArraySize {
        return Err(ViewerError::other(format!(
            "could not show the picture: it is slice {} of a texture that has {}",
            video.index, desc.ArraySize
        )));
    }
    let fits = video.width > 0
        && video.height > 0
        && video.width <= desc.Width
        && video.height <= desc.Height
        && video.width.is_multiple_of(2)
        && video.height.is_multiple_of(2);
    if !fits {
        return Err(ViewerError::other(format!(
            "could not show the picture: {}x{} is not an even size inside its {}x{} texture",
            video.width, video.height, desc.Width, desc.Height
        )));
    }
    Ok(())
}

fn planes(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    index: u32,
) -> Result<Planes, ViewerError> {
    Ok(Planes {
        texture: texture.clone(),
        index,
        luma: plane_view(device, texture, index, DXGI_FORMAT_R8_UNORM)?,
        chroma: plane_view(device, texture, index, DXGI_FORMAT_R8G8_UNORM)?,
        used: 0,
    })
}

// On a D3D11.0 device the view's format picks the NV12 plane: R8 is the Y
// plane, R8G8 the UV plane at half size. An array view of one slice works
// for a decoder's texture array and for a plain texture alike.
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    index: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11ShaderResourceView, ViewerError> {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: index,
                ArraySize: 1,
            },
        },
    };
    let mut view = None;
    // SAFETY: a live texture, a full description, a live out parameter.
    unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }
        .map_err(|err| ViewerError::windows("read the picture's texture", &err))?;
    view.ok_or_else(|| ViewerError::missing("read the picture's texture", "view"))
}

fn nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, ViewerError> {
    let step = || format!("make a {width}x{height} texture for the last picture");
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
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|err| ViewerError::windows(step(), &err))?;
    texture.ok_or_else(|| ViewerError::missing(step(), "texture"))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_DECODER;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};

    use super::*;
    use crate::device::{self, Gpu, Locked};

    const SLICES: u32 = 4;

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

    // A made-up decoder pool, bound as FFmpeg binds its own: an NV12 array
    // the video engine decodes into and the viewer samples. Never written,
    // and the same small size every time, as two decoders for the same
    // share would make it.
    fn pool(device: &ID3D11Device) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 256,
            Height: 144,
            MipLevels: 1,
            ArraySize: SLICES,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: a full description and a live out parameter.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        texture.unwrap()
    }

    fn slice(texture: &ID3D11Texture2D, index: u32) -> Video<'_> {
        Video {
            texture,
            index,
            width: 256,
            height: 144,
        }
    }

    // What draw does with a picture, short of drawing it.
    fn show(picture: &mut Picture, video: &Video) {
        picture.tick(Some(video));
        check(video).unwrap();
        picture.planes(video).unwrap();
    }

    fn held(picture: &Picture) -> Vec<(usize, u32)> {
        picture
            .views
            .iter()
            .map(|planes| (planes.texture.as_raw() as usize, planes.index))
            .collect()
    }

    // A new decoder for the same codec, or a codec switch at a size where
    // FFmpeg's rounding (16 for H.264, 128 for HEVC) gives both pools the
    // same size, 2048x1152 for one.
    #[test]
    fn a_new_pool_of_the_same_size_lets_the_old_one_go_at_once() {
        let Some(gpu) = gpu() else {
            return;
        };
        let mut picture = Picture::new(&gpu.device).unwrap();
        let _locked = Locked::enter(&gpu.lock);
        let old = pool(&gpu.device);
        for index in 0..SLICES {
            show(&mut picture, &slice(&old, index));
        }
        show(&mut picture, &slice(&old, 0));
        assert_eq!(picture.cached_views(), SLICES as usize);

        // Minimized, so only the tick: the old pool goes before anything
        // is drawn from the new one.
        let new = pool(&gpu.device);
        picture.tick(Some(&slice(&new, 2)));
        assert_eq!(
            picture.cached_views(),
            0,
            "views of the old pool outlived the first picture from a new one of the same size"
        );
        show(&mut picture, &slice(&new, 2));
        assert_eq!(held(&picture), vec![(new.as_raw() as usize, 2)]);
    }

    // Once the viewer lets go of the old pool it is freed at the next flush,
    // as every present makes, and the next pool can come back at its
    // address. It is still new, so nothing may remember a texture by its
    // address past its views.
    #[test]
    fn a_pool_made_where_a_freed_one_was_is_still_new() {
        let Some(gpu) = gpu() else {
            return;
        };
        let mut picture = Picture::new(&gpu.device).unwrap();
        let _locked = Locked::enter(&gpu.lock);
        let mut freed = Vec::new();
        let mut current = pool(&gpu.device);
        show(&mut picture, &slice(&current, 0));
        let mut same_address = 0;
        for switch in 0..8 {
            let next = pool(&gpu.device);
            let raw = next.as_raw() as usize;
            if freed.contains(&raw) {
                same_address += 1;
            }
            show(&mut picture, &slice(&next, 1));
            assert_eq!(held(&picture), vec![(raw, 1)], "switch {switch}");
            freed.push(current.as_raw() as usize);
            current = next;
            // SAFETY: a call on the live immediate context, inside the lock.
            unsafe { gpu.context.Flush() };
        }
        println!("{same_address} of 8 new pools came at the address of one freed before");
    }

    #[test]
    fn a_wide_window_puts_the_bars_left_and_right() {
        let placed = fit((2560, 1440), (2000, 900));
        assert_eq!((placed.width, placed.height), (1600, 900));
        assert_eq!((placed.left, placed.top), (200, 0));
    }

    #[test]
    fn a_tall_window_puts_the_bars_above_and_below() {
        let placed = fit((2560, 1440), (1280, 1000));
        assert_eq!((placed.width, placed.height), (1280, 720));
        assert_eq!((placed.left, placed.top), (0, 140));
    }

    #[test]
    fn an_exact_fit_has_no_bars() {
        let placed = fit((2560, 1440), (2560, 1440));
        assert_eq!(
            (placed.left, placed.top, placed.width, placed.height),
            (0, 0, 2560, 1440)
        );
    }
}
