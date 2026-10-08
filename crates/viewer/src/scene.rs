// One frame's drawing into a target: the picture, the pointer over it, the
// strip under it. The window draws into its back buffer with this, and the
// tests draw into a texture they read back, so what they check is what the
// window shows.

use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11DeviceContext, ID3D11Multithread, ID3D11RenderTargetView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows::core::Interface;

use crate::Video;
use crate::cursor::{Cursor, Local, Pointer};
use crate::device::{Gpu, Locked};
use crate::error::ViewerError;
use crate::palette::WINDOW;
use crate::picture::{Picture, Placement};
use crate::strip::{self, Band, Look, Painter, Strip, Sweep};

// A BGRA texture the scene can draw into, seen by Direct3D and Direct2D.
pub(crate) struct Target {
    pub view: ID3D11RenderTargetView,
    pub bitmap: ID2D1Bitmap1,
    pub size: (u32, u32),
}

pub(crate) struct Scene {
    context: ID3D11DeviceContext,
    lock: ID3D11Multithread,
    // Declared before the Direct2D device and factory, so it goes first.
    painter: Painter,
    d2d: ID2D1DeviceContext,
    _d2d_device: ID2D1Device,
    _factory: ID2D1Factory1,
    picture: Picture,
    pointer: Pointer,
    sweep: Sweep,
}

impl Scene {
    pub(crate) fn new(gpu: &Gpu) -> Result<Scene, ViewerError> {
        let step = "start Direct2D for the strip";
        let fail = |err: windows::core::Error| ViewerError::windows(step, &err);
        let dxgi: IDXGIDevice = gpu.device.cast().map_err(fail)?;
        // Single-threaded: everything this Direct2D device does happens on
        // one thread at a time, inside the device lock, which other viewers
        // and the decoder on the same device respect too. The lock is not
        // held for the rest, the shader compiles above all, which take long
        // enough to stall another viewer's presents.
        let (factory, d2d_device, d2d, brush) = {
            let _locked = Locked::enter(&gpu.lock);
            // SAFETY: plain calls; the options and the colour outlive them.
            unsafe {
                let factory: ID2D1Factory1 = D2D1CreateFactory(
                    D2D1_FACTORY_TYPE_SINGLE_THREADED,
                    Some(&D2D1_FACTORY_OPTIONS::default()),
                )
                .map_err(fail)?;
                let d2d_device = factory.CreateDevice(&dxgi).map_err(fail)?;
                let d2d = d2d_device
                    .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                    .map_err(fail)?;
                // Pixels, not DIPs: the strip does its own DPI scaling so it
                // can round the way the panel does.
                d2d.SetDpi(96.0, 96.0);
                let brush = d2d
                    .CreateSolidColorBrush(&WINDOW.d2d(), None)
                    .map_err(fail)?;
                (factory, d2d_device, d2d, brush)
            }
        };
        Ok(Scene {
            context: gpu.context.clone(),
            lock: gpu.lock.clone(),
            painter: Painter::new(&d2d, brush)?,
            d2d,
            _d2d_device: d2d_device,
            _factory: factory,
            picture: Picture::new(&gpu.device)?,
            pointer: Pointer::new(&gpu.device)?,
            sweep: Sweep::default(),
        })
    }

    // `texture` is B8G8R8A8_UNORM with render target binding: a swap chain
    // buffer, or a test's texture.
    pub(crate) fn target(&self, texture: &ID3D11Texture2D) -> Result<Target, ViewerError> {
        let step = "draw into the viewer's buffer";
        let fail = |err: windows::core::Error| ViewerError::windows(step, &err);
        let _locked = Locked::enter(&self.lock);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture.
        unsafe { texture.GetDesc(&mut desc) };
        // SAFETY: a getter on a live texture.
        let device = unsafe { texture.GetDevice() }.map_err(fail)?;
        let mut view = None;
        // SAFETY: a live texture made for rendering, the default view, a
        // live out parameter.
        unsafe { device.CreateRenderTargetView(texture, None, Some(&mut view)) }.map_err(fail)?;
        let view = view.ok_or_else(|| ViewerError::missing(step, "view"))?;
        let surface: IDXGISurface = texture.cast().map_err(fail)?;
        let properties = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            ..Default::default()
        };
        // SAFETY: a live surface on the device this context was made from,
        // and properties that outlive the call.
        let bitmap = unsafe {
            self.d2d
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))
        }
        .map_err(fail)?;
        Ok(Target {
            view,
            bitmap,
            size: (desc.Width, desc.Height),
        })
    }

    // Where the picture went, for the window to map the mouse through.
    pub(crate) fn draw(
        &mut self,
        target: &Target,
        video: Option<&Video>,
        cursor: Option<&Cursor>,
        local: Option<&Local>,
        strip: &Strip,
        look: &Look,
    ) -> Result<Option<Placement>, ViewerError> {
        if let Some(cursor) = cursor {
            self.pointer.update(cursor)?;
        }
        self.sweep.update(&strip.trace);
        let region = match look.band {
            Band::Below => (
                target.size.0,
                target.size.1.saturating_sub(strip::band_height(look.dpi)),
            ),
            Band::Over | Band::Hidden => target.size,
        };
        let _locked = Locked::enter(&self.lock);
        // SAFETY: a call on the live immediate context. The decoder and the
        // capture pattern share it, so the state is set in full each frame.
        unsafe { self.context.ClearState() };
        let placed = self.picture.draw(&target.view, region, video)?;
        if let Some(placed) = &placed {
            self.pointer.draw(&target.view, target.size, placed, local);
        }
        if look.band == Band::Hidden {
            // SAFETY: a call on the live immediate context.
            unsafe { self.context.OMSetRenderTargets(None, None) };
            return Ok(placed);
        }
        // SAFETY: plain calls on the live contexts; the bitmap wraps the
        // same texture as the view, which is unbound before Direct2D draws.
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.d2d.SetTarget(&target.bitmap);
            self.d2d.BeginDraw();
        }
        let drawn = self.painter.draw(strip, &self.sweep, look);
        // SAFETY: ends the BeginDraw above on the same context.
        let ended = unsafe { self.d2d.EndDraw(None, None) };
        // SAFETY: releases the target so the buffer can be resized.
        unsafe { self.d2d.SetTarget(None) };
        drawn?;
        ended.map_err(|err| ViewerError::windows("draw the strip", &err))?;
        Ok(placed)
    }

    pub(crate) fn has_shape(&self) -> bool {
        self.pointer.has_shape()
    }

    pub(crate) fn keep(&mut self, video: &Video) -> Result<(), ViewerError> {
        let _locked = Locked::enter(&self.lock);
        self.picture.keep(video)
    }

    // While minimized nothing is drawn, but the pointer's shape and the last
    // picture are kept for when the window is back.
    pub(crate) fn hold(
        &mut self,
        video: Option<&Video>,
        cursor: Option<&Cursor>,
    ) -> Result<(), ViewerError> {
        if let Some(cursor) = cursor {
            self.pointer.update(cursor)?;
        }
        let _locked = Locked::enter(&self.lock);
        self.picture.tick(video);
        match video {
            Some(video) => self.picture.keep(video),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn painter(&mut self) -> &mut Painter {
        &mut self.painter
    }

    #[cfg(test)]
    pub(crate) fn cached_views(&self) -> usize {
        self.picture.cached_views()
    }
}
