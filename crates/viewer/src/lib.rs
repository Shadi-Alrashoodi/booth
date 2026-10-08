//! The window a shared screen is watched in: its own D3D11 swap chain,
//! presented the moment a frame is decoded.

#![deny(unsafe_op_in_unsafe_fn)]

mod control;
mod cursor;
mod device;
mod error;
#[cfg(test)]
mod offscreen;
mod palette;
mod path;
mod picture;
mod scene;
mod shader;
mod strip;
mod text;
mod window;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_FRAME_STATISTICS_MEDIA, DXGI_MWA_NO_ALT_ENTER,
    DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice,
    IDXGIFactory2, IDXGIFactory5, IDXGISwapChain1, IDXGISwapChainMedia,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::core::{BOOL, Interface};

use crate::cursor::Local;
use crate::device::{Gpu, Locked};
use crate::path::{Reading, Stats};
use crate::scene::{Scene, Target};
use crate::strip::{Band, Look};
use crate::window::{Fullscreen, Window};

pub use crate::control::{Capturing, ControlOut, MouseButton, MouseMode, Pointing};
pub use crate::cursor::{Cursor, CursorKind, CursorShape};
pub use crate::error::{ErrorKind, ViewerError};
pub use crate::strip::{LinkState, PathWord, PresentPath, Strip};

// Two buffers: one on screen, one being drawn. Flip discard needs at least
// two, and a third would only be a frame waiting in line.
const BUFFERS: u32 = 2;

// needs_redraw asks for the still picture again, for the present path word,
// only once nothing has been presented for this long. A share that is
// sending frames brings the word back with them, and a present of the same
// picture between two of theirs would only cost GPU time and, with vsync, a
// place in the queue ahead of the next one.
const QUIET: Duration = Duration::from_millis(100);

// The setting that hides the strip in fullscreen until the mouse moves. One
// for every viewer in the process, set by the panel as a room opens, since
// the room's share thread opens viewers without it.
static HIDE_STRIP: AtomicBool = AtomicBool::new(false);

pub fn hide_strip_in_fullscreen(on: bool) {
    HIDE_STRIP.store(on, Ordering::Relaxed);
}

#[derive(Clone)]
pub struct Options {
    pub title: String,
    // The video's size, which the window opens at, scaled down to fit.
    pub video_width: u32,
    pub video_height: u32,
    // Present on the monitor's refresh instead of the moment a frame is
    // ready: no tearing, up to a refresh of delay. Off unless the person
    // turns it on in settings.
    pub vsync: bool,
    pub show: Show,
    // Called on the window's thread when something happened that the
    // present thread should act on now: a new size or DPI, fullscreen on or
    // off, another monitor or new display settings, a close, a click on the
    // strip. A still share sends no frames, so a loop that blocks on its
    // channel would otherwise not hear of them until the sharer's screen
    // changed. Dropping the Viewer waits for the window's thread, so this
    // must return at once and never wait for the present thread: a send on
    // an unbounded channel, or setting an event.
    pub wake: Option<Arc<dyn Fn() + Send + Sync>>,
    // Where this viewer's input goes while this PC controls the share it
    // shows (set_control). None, and this viewer never captures anything.
    pub control: Option<Arc<dyn ControlOut>>,
}

impl Options {
    pub fn new(title: impl Into<String>, video_width: u32, video_height: u32) -> Options {
        Options {
            title: title.into(),
            video_width,
            video_height,
            vsync: false,
            show: Show::Activate,
            wake: None,
            control: None,
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("title", &self.title)
            .field("video_width", &self.video_width)
            .field("video_height", &self.video_height)
            .field("vsync", &self.vsync)
            .field("show", &self.show)
            .field("wake", &self.wake.is_some())
            .field("control", &self.control.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    // Shown and given the focus, for a viewer someone asked to open.
    Activate,
    // Shown without taking the focus from whatever has it: a game, or
    // whoever is running the tests.
    NoActivate,
    // Never shown. Everything else works, presents included.
    Hidden,
}

// A decoded picture: one slice of an NV12 texture on the viewer's device,
// BT.709 limited range. The texture needs shader resource binding, and the
// picture's own size, which may be smaller than the texture's, is even.
#[derive(Clone, Copy, Debug)]
pub struct Video<'a> {
    pub texture: &'a ID3D11Texture2D,
    pub index: u32,
    pub width: u32,
    pub height: u32,
}

pub struct Frame<'a> {
    // None draws the last picture again: for a pointer that moved on a
    // still screen, new strip numbers, or a window that changed size.
    pub video: Option<Video<'a>>,
    pub cursor: Option<&'a Cursor>,
    pub strip: &'a Strip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presented {
    // From the call to present() to the swap chain's Present returning.
    pub took: Duration,
    // How recent frames reached the screen. None after a resize, fullscreen
    // on or off, or another monitor, until Windows has reported on a present
    // made since, which is a few presents later. Also None while its
    // statistics fail, stop following the window, or name no path
    // (path.rs).
    pub path: Option<PresentPath>,
    // False when the window is minimized and nothing was drawn.
    pub shown: bool,
}

pub struct Viewer {
    // Dropped in this order: the swap chain before the device it was made
    // on, and the window last, once nothing presents to it.
    swap: Swap,
    scene: Scene,
    gpu: Gpu,
    window: Window,
    tearing: bool,
    vsync: bool,
    path: Reading,
    // Fullscreen and Shared::output_changes as the last present found them.
    was_fullscreen: bool,
    output_changes: u32,
    // When the last call to present() returned.
    presented_at: Instant,
    // Where the last present put the strip.
    band: Band,
    // The controller's mouse on the picture as the last present found it,
    // and since when it has been there; whether a present was made after it
    // came to rest (control::DRIFT_AFTER).
    local: Option<((i32, i32), Instant)>,
    rested: bool,
}

struct Swap {
    chain: IDXGISwapChain1,
    media: Option<IDXGISwapChainMedia>,
    target: Option<Target>,
    size: (u32, u32),
    flags: DXGI_SWAP_CHAIN_FLAG,
}

impl Viewer {
    pub fn open(options: &Options) -> Result<Viewer, ViewerError> {
        let window = Window::open(
            &options.title,
            (options.video_width, options.video_height),
            options.show,
            options.wake.clone(),
            options.control.clone(),
        )?;
        // SAFETY: a plain lookup on a live window.
        let monitor = unsafe { MonitorFromWindow(window.hwnd(), MONITOR_DEFAULTTONEAREST) };
        let gpu = device::gpu_for(monitor)?;
        let scene = Scene::new(&gpu)?;
        let factory = factory_of(&gpu)?;
        let tearing = allows_tearing(&factory);
        let swap = Swap::new(&gpu, &scene, &factory, &window, tearing)?;
        Ok(Viewer {
            swap,
            scene,
            gpu,
            window,
            tearing,
            vsync: options.vsync,
            path: Reading::default(),
            was_fullscreen: false,
            output_changes: 0,
            presented_at: Instant::now(),
            band: Band::Below,
            local: None,
            rested: false,
        })
    }

    // The device the decoder must decode on: made on the GPU that drives
    // the window's monitor, with video support, and multithread protected.
    pub fn device(&self) -> &ID3D11Device {
        &self.gpu.device
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.name
    }

    pub fn window(&self) -> HWND {
        self.window.hwnd()
    }

    // The swap chain's buffers, which follow the client area at each present.
    pub fn buffer_size(&self) -> (u32, u32) {
        self.swap.size
    }

    // Whether Windows allows presents to tear here, which presenting the
    // moment a frame is ready needs to skip the wait for the refresh.
    pub fn tearing(&self) -> bool {
        self.tearing
    }

    // The person closed the window: stop watching. Nothing else ends.
    pub fn closed(&self) -> bool {
        self.window.shared().closed()
    }

    // The window changed size, DPI, fullscreen or monitor since the last
    // present, or the strip is due to hide or show again, so a still picture
    // should be presented again (Frame with no video). Options::wake says
    // when to ask; the strip's hide comes STRIP_STAYS after the mouse
    // stopped, and is seen at the next ask after that. After a change it
    // also says yes a few more times, each QUIET after the last present,
    // until Windows has told the present path again.
    pub fn needs_redraw(&self) -> bool {
        self.window.shared().changed()
            || self.band_now() != self.band
            || (self.path.wants_present() && self.presented_at.elapsed() >= QUIET)
            || self.window.shared().local_moved()
            || (!self.rested
                && self
                    .local
                    .is_some_and(|(_, since)| since.elapsed() >= control::DRIFT_AFTER))
    }

    // This PC controls the share shown, with the mouse going as `mode` says,
    // or it no longer does (control.rs). Capture runs only while the window
    // is in front as well, which its thread works out a moment after this
    // call. The strip's words come with the Strip.
    pub fn set_control(&self, mode: Option<MouseMode>) {
        if self.window.shared().set_mode(mode) {
            self.window.follow_control();
        }
    }

    fn band_now(&self) -> Band {
        self.window
            .shared()
            .band(HIDE_STRIP.load(Ordering::Relaxed))
    }

    // A click on the strip since the last call, which opens the stats panel
    // in the panel window.
    pub fn strip_clicked(&self) -> bool {
        self.window.shared().take_strip_click()
    }

    pub fn fullscreen(&self) -> bool {
        self.window.shared().fullscreen()
    }

    // What F11 does. It happens on the window's thread a moment later; the
    // next present picks up the new size.
    pub fn set_fullscreen(&self, on: bool) {
        self.window.fullscreen(if on {
            Fullscreen::Enter
        } else {
            Fullscreen::Leave
        });
    }

    pub fn toggle_fullscreen(&self) {
        self.window.fullscreen(Fullscreen::Toggle);
    }

    pub fn present(&mut self, frame: &Frame) -> Result<Presented, ViewerError> {
        let started = Instant::now();
        if self.window.shared().closed() {
            return Err(ViewerError::closed());
        }
        self.path.presenting();
        self.take_window_changes();
        let local = self.local_pointer();
        let shared = self.window.shared();
        let size = shared.size();
        if shared.minimized() || size.0 == 0 || size.1 == 0 {
            self.scene.hold(frame.video.as_ref(), frame.cursor)?;
            self.presented_at = Instant::now();
            return Ok(Presented {
                took: started.elapsed(),
                path: self.path.word(),
                shown: false,
            });
        }
        // No target means the last resize failed after ResizeBuffers, maybe
        // for a moment's lack of memory; trying again is the way back.
        if size != self.swap.size || self.swap.target.is_none() {
            self.path.changed(self.swap.last_present());
            self.swap.resize(&self.gpu, &self.scene, size)?;
        }
        let look = Look {
            size,
            dpi: shared.dpi(),
            scrolling: shared.scrolling(),
            present: self.path.word(),
            band: self.band,
        };
        let Some(target) = &self.swap.target else {
            return Err(ViewerError::other(
                "could not draw the frame: the viewer has no buffer to draw into after a failed resize",
            ));
        };
        let placed = self.scene.draw(
            target,
            frame.video.as_ref(),
            frame.cursor,
            local.as_ref(),
            frame.strip,
            &look,
        )?;
        shared.set_placed(placed);
        shared.set_has_shape(self.scene.has_shape());
        let (interval, flags) = if self.vsync {
            (1, DXGI_PRESENT(0))
        } else if self.tearing {
            (0, DXGI_PRESENT_ALLOW_TEARING)
        } else {
            (0, DXGI_PRESENT(0))
        };
        // SAFETY: a live swap chain; ALLOW_TEARING is passed only when the
        // swap chain was made with the matching flag.
        let presented = unsafe { self.swap.chain.Present(interval, flags) };
        let took = started.elapsed();
        presented
            .ok()
            .map_err(|err| ViewerError::windows("show the frame", &err))?;
        if let Some(video) = &frame.video {
            self.scene.keep(video)?;
        }
        self.read_path();
        self.presented_at = Instant::now();
        Ok(Presented {
            took,
            path: self.path.word(),
            shown: true,
        })
    }

    // What the window thread recorded, taken before the size is read: a
    // change that lands after this stays flagged for the next present, and
    // the present made now counts as one from before it.
    fn take_window_changes(&mut self) {
        let shared = self.window.shared();
        shared.take_changed();
        // Also for a minimized window, which present() returns early for,
        // so needs_redraw does not ask again at every wake while nothing can
        // be drawn.
        self.band = self.band_now();
        shared.set_strip_hidden(self.band == Band::Hidden);
        let (fullscreen, output_changes) = (shared.fullscreen(), shared.output_changes());
        if fullscreen != self.was_fullscreen || output_changes != self.output_changes {
            self.was_fullscreen = fullscreen;
            self.output_changes = output_changes;
            self.path.changed(self.swap.last_present());
        }
    }

    // Where the controller's mouse is on the picture, if this PC controls in
    // absolute mode, and how long it has rested there.
    fn local_pointer(&mut self) -> Option<Local> {
        let shared = self.window.shared();
        shared.take_local_moved();
        let now = Instant::now();
        self.local = match (shared.local(), self.local) {
            (Some(at), Some((was, since))) if at == was => Some((at, since)),
            (Some(at), _) => Some((at, now)),
            (None, _) => None,
        };
        let local = self.local.map(|(at, since)| Local {
            at,
            still: now.saturating_duration_since(since),
        });
        self.rested = local.is_some_and(|local| local.still >= control::DRIFT_AFTER);
        local
    }

    // The statistics describe the newest frame Windows has put on screen,
    // which is a few presents behind the one just made.
    fn read_path(&mut self) {
        let Some(media) = &self.swap.media else {
            return;
        };
        let mut stats = DXGI_FRAME_STATISTICS_MEDIA::default();
        // SAFETY: a getter on a live swap chain with a live out parameter.
        let read = unsafe { media.GetFrameStatisticsMedia(&mut stats) }
            .ok()
            .map(|()| Stats {
                present: stats.PresentCount,
                mode: stats.CompositionMode,
            });
        self.path.read(read, self.swap.last_present());
    }
}

// The factory that made the device's adapter; a swap chain has to come
// from that one.
fn factory_of(gpu: &Gpu) -> Result<IDXGIFactory2, ViewerError> {
    let step = "reach DXGI for the viewer's swap chain";
    let fail = |err: windows::core::Error| ViewerError::windows(step, &err);
    let dxgi: IDXGIDevice = gpu.device.cast().map_err(fail)?;
    // SAFETY: getters on live interfaces.
    unsafe {
        let adapter: IDXGIAdapter = dxgi.GetAdapter().map_err(fail)?;
        adapter.GetParent().map_err(fail)
    }
}

fn allows_tearing(factory: &IDXGIFactory2) -> bool {
    let Ok(factory) = factory.cast::<IDXGIFactory5>() else {
        return false;
    };
    let mut allowed = BOOL(0);
    // SAFETY: the feature's data is one BOOL, passed with its size.
    let asked = unsafe {
        factory.CheckFeatureSupport(
            DXGI_FEATURE_PRESENT_ALLOW_TEARING,
            &mut allowed as *mut BOOL as *mut _,
            size_of::<BOOL>() as u32,
        )
    };
    asked.is_ok() && allowed.as_bool()
}

impl Swap {
    fn new(
        gpu: &Gpu,
        scene: &Scene,
        factory: &IDXGIFactory2,
        window: &Window,
        tearing: bool,
    ) -> Result<Swap, ViewerError> {
        let size = window.shared().size();
        let size = (size.0.max(1), size.1.max(1));
        let flags = if tearing {
            DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
        } else {
            DXGI_SWAP_CHAIN_FLAG(0)
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0,
            Height: size.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags.0 as u32,
        };
        let step = format!("make the viewer's swap chain on {}", gpu.name);
        // SAFETY: a live device and window and a full description.
        let chain = unsafe {
            factory.CreateSwapChainForHwnd(&gpu.device, window.hwnd(), &desc, None, None)
        }
        .map_err(|err| ViewerError::windows(&step, &err))?;
        // DXGI would otherwise watch the window for Alt+Enter and switch to
        // exclusive fullscreen on its own; F11 is Booth's.
        // SAFETY: a live window and a live factory.
        unsafe {
            factory
                .MakeWindowAssociation(
                    window.hwnd(),
                    DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES,
                )
                .map_err(|err| ViewerError::windows(&step, &err))?;
        }
        let media = chain.cast::<IDXGISwapChainMedia>().ok();
        let mut swap = Swap {
            chain,
            media,
            target: None,
            size,
            flags,
        };
        swap.target = Some(swap.buffer_target(scene)?);
        Ok(swap)
    }

    fn buffer_target(&self, scene: &Scene) -> Result<Target, ViewerError> {
        // With flip model in Direct3D 11, buffer 0 is always the one to draw
        // into next; the runtime rotates the buffers behind it.
        // SAFETY: a getter on a live swap chain.
        let buffer: ID3D11Texture2D = unsafe { self.chain.GetBuffer(0) }
            .map_err(|err| ViewerError::windows("reach the viewer's back buffer", &err))?;
        scene.target(&buffer)
    }

    // On the present thread, between presents: every reference to the old
    // buffers is gone before ResizeBuffers, as it requires.
    fn resize(&mut self, gpu: &Gpu, scene: &Scene, size: (u32, u32)) -> Result<(), ViewerError> {
        self.target = None;
        {
            let _locked = Locked::enter(&gpu.lock);
            // SAFETY: plain calls on the live immediate context; nothing of
            // the swap chain's stays bound or queued after them.
            unsafe {
                gpu.context.ClearState();
                gpu.context.Flush();
            }
        }
        // SAFETY: a live swap chain with no outstanding buffer references;
        // the flags are the ones it was made with.
        unsafe {
            self.chain
                .ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, self.flags)
        }
        .map_err(|err| {
            ViewerError::windows(
                format!("resize the viewer's buffers to {}x{}", size.0, size.1),
                &err,
            )
        })?;
        self.size = size;
        self.target = Some(self.buffer_target(scene)?);
        Ok(())
    }

    // The id of the last present, which the statistics' PresentCount is
    // compared with. 0 before the first.
    fn last_present(&self) -> u32 {
        // SAFETY: a getter on a live swap chain.
        unsafe { self.chain.GetLastPresentCount() }.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::Graphics::Dxgi::DXGI_FRAME_PRESENTATION_MODE_OVERLAY;
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_DISPLAYCHANGE};

    use super::*;

    // One viewer at a time, as tests/window.rs has it and for its reason.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    pub(crate) fn one_at_a_time() -> MutexGuard<'static, ()> {
        ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // A never shown window, so nothing reaches the screen.
    fn hidden(title: &str) -> Option<Viewer> {
        let mut options = Options::new(title, 320, 180);
        options.show = Show::Hidden;
        match Viewer::open(&options) {
            Ok(viewer) => Some(viewer),
            Err(err) if err.to_string().contains("no graphics card") => {
                println!("skipped: {err}");
                None
            }
            Err(err) => panic!("{err}"),
        }
    }

    // What the window's thread records reaches the present path word: new
    // display settings and fullscreen on and off each clear it, and the
    // viewer then asks for the still picture again once it has been quiet.
    // Nothing is presented, so the word comes only from the made-up
    // statistics here.
    #[test]
    fn new_display_settings_and_fullscreen_clear_the_word() {
        let _screen = one_at_a_time();
        let Some(mut viewer) = hidden("Booth viewer path test") else {
            return;
        };
        let mut told = 0;
        let mut flip = |viewer: &mut Viewer| {
            told += 1;
            let stats = Stats {
                present: told,
                mode: DXGI_FRAME_PRESENTATION_MODE_OVERLAY,
            };
            viewer.path.read(Some(stats), told);
            assert_eq!(viewer.path.word(), Some(PresentPath::Flip));
        };
        let wait_for_fullscreen = |viewer: &Viewer, on: bool| {
            let started = Instant::now();
            while viewer.fullscreen() != on {
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "gave up waiting for fullscreen {on}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };

        flip(&mut viewer);
        viewer.take_window_changes();
        assert_eq!(
            viewer.path.word(),
            Some(PresentPath::Flip),
            "cleared with nothing changed"
        );

        // SAFETY: the viewer's live window. SendMessageW returns once the
        // window's thread has handled it.
        unsafe {
            SendMessageW(
                viewer.window(),
                WM_DISPLAYCHANGE,
                Some(WPARAM(32)),
                Some(LPARAM(0)),
            )
        };
        viewer.take_window_changes();
        assert_eq!(
            viewer.path.word(),
            None,
            "kept through new display settings"
        );
        viewer.presented_at = Instant::now();
        assert!(
            !viewer.needs_redraw(),
            "asked for a present right after one"
        );
        viewer.presented_at -= QUIET;
        assert!(
            viewer.needs_redraw(),
            "never asked for a present for the word"
        );

        flip(&mut viewer);
        viewer.set_fullscreen(true);
        wait_for_fullscreen(&viewer, true);
        viewer.take_window_changes();
        assert_eq!(viewer.path.word(), None, "kept through fullscreen");

        flip(&mut viewer);
        viewer.set_fullscreen(false);
        wait_for_fullscreen(&viewer, false);
        viewer.take_window_changes();
        assert_eq!(viewer.path.word(), None, "kept through leaving fullscreen");
    }

    #[test]
    fn a_lost_target_comes_back_at_the_next_present() {
        let _screen = one_at_a_time();
        let Some(mut viewer) = hidden("Booth viewer target test") else {
            return;
        };
        let strip = Strip::default();
        let still = Frame {
            video: None,
            cursor: None,
            strip: &strip,
        };
        viewer.present(&still).unwrap();
        // What ResizeBuffers going through and buffer_target() failing
        // after it leave behind: the new size, and nothing to draw into.
        viewer.swap.target = None;
        let presented = viewer.present(&still).unwrap();
        assert!(presented.shown);
        assert!(viewer.swap.target.is_some());
        assert!(viewer.present(&still).is_ok());
    }
}
