// Windows.Graphics.Capture of one window, for a window share: what Windows
// composes of it, at its own size, converted to NV12 by the same shader as a
// monitor's picture. The pointer is left out of the pictures where Windows
// allows that (build 19041 and later). Frames come through a free-threaded
// frame pool, whose thread only sets an event; everything else happens on
// the caller's thread.

use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindow};
use windows::core::{Interface, factory};

use crate::cap::{Decision, FpsCap};
use crate::convert::{Converter, Layout, Plan, Target};
use crate::device::device_on;
use crate::duplication::{IDLE_WAIT, present_instant, qpc};
use crate::error::CaptureError;
use crate::monitors::{Adapter, Rotation};
use crate::{Frame, Next, Options, PauseReason};

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
// SystemRelativeTime counts in these a second.
const TICKS: i64 = 10_000_000;
// A window coming back from minimized is sent once more at the size of
// the caption it was minimized to, sometimes after IsIconic, and even
// DWM's bounds of the window, already say it is back. For this long after
// it is back, frames of the caption's size or of a size other than the
// window's own are dropped.
const RESTORING: Duration = Duration::from_millis(500);

pub struct WindowCapture {
    hwnd: isize,
    options: Options,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    graphics: IDirect3DDevice,
    converter: Converter,
    layout: Layout,
    targets: [Target; 2],
    last_out: usize,
    // Kept for its Closed event; the session and the pool close when this
    // is dropped (Drop below).
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    arrived: Arc<Arrived>,
    closed: Arc<AtomicBool>,
    cap: FpsCap,
    // A picture converted and held for the fps cap.
    pending: Option<(Instant, Instant, Instant)>,
    skipped: u32,
    minimized: bool,
    // The size DWM has of the window while it is minimized: its caption.
    caption: Option<(i32, i32)>,
    restoring_until: Option<Instant>,
    number: u64,
    qpc_frequency: i64,
}

// An auto-reset event the frame pool's thread sets, kept as a number so it
// can be shared with that thread.
struct Arrived(isize);

impl Arrived {
    fn new() -> Result<Arrived, CaptureError> {
        // SAFETY: no name and no security attributes; the handle is closed in
        // Drop.
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|err| CaptureError::windows("make the window capture's event", &err))?;
        Ok(Arrived(event.0 as isize))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }

    fn set(&self) {
        // SAFETY: a live event; a failure only means a wait runs to its end.
        let _ = unsafe { SetEvent(self.handle()) };
    }

    fn wait(&self, at_most: Duration) {
        let ms = u32::try_from(at_most.as_millis().max(1)).unwrap_or(INFINITE - 1);
        // SAFETY: a live event.
        unsafe { WaitForSingleObject(self.handle(), ms) };
    }
}

impl Drop for Arrived {
    fn drop(&mut self) {
        // SAFETY: the event made in new, closed once.
        let _ = unsafe { CloseHandle(self.handle()) };
    }
}

// The Windows Runtime in the multithreaded apartment on every thread that
// opens a window capture, from the first open until the thread ends. A
// thread already in the other mode stays that way: the capture objects
// used here are agile.
struct WinRt {
    joined: bool,
}

impl Drop for WinRt {
    fn drop(&mut self) {
        if self.joined {
            // SAFETY: pairs with the successful RoInitialize on this same
            // thread, which is where thread-local destructors run.
            unsafe { RoUninitialize() };
        }
    }
}

thread_local! {
    static WINRT: WinRt = WinRt {
        // SAFETY: a plain call with a constant. S_OK and S_FALSE are paired
        // in Drop; RPC_E_CHANGED_MODE has nothing to pair.
        joined: unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok(),
    };
}

impl WindowCapture {
    // `hwnd` is the window's handle as a number; the frames are made on
    // `adapter`, which the encoder then opens on.
    pub fn open(
        hwnd: isize,
        adapter: &Adapter,
        options: Options,
    ) -> Result<WindowCapture, CaptureError> {
        WINRT.with(|_| {});
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(fail("start capturing"))?;
        // SAFETY: a window handle the caller found; a gone one is refused.
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(HWND(hwnd as *mut _)) }.map_err(fail("capture"))?;
        let device = device_on(adapter)?;
        // SAFETY: GetImmediateContext hands back an owned reference.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(fail("start Direct3D 11 for"))?;
        let dxgi: IDXGIDevice = device.cast().map_err(fail("share the GPU with"))?;
        // SAFETY: a live DXGI device of a hardware Direct3D 11 device.
        let graphics: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|inspectable| inspectable.cast())
            .map_err(fail("share the GPU with"))?;
        let size = item.Size().map_err(fail("read the size of"))?;
        // Everything that can refuse comes before the pool, so a failed open
        // leaves no capture running.
        let converter = Converter::new(&device, &adapter.description)?;
        let plan = Plan::new(
            size.Width.max(2) as u32,
            size.Height.max(2) as u32,
            Rotation::Identity,
            options.max_width,
            options.max_height,
        );
        let layout = converter.layout(plan)?;
        let targets = [
            converter.target(plan.width, plan.height)?,
            converter.target(plan.width, plan.height)?,
        ];
        // Frames as big as the largest monitor, so the window maximized or
        // made bigger on any of them still fits: after a new pool Windows
        // sends nothing until the window changes again, and a window that
        // grew and then stood still would stay cut to its old size.
        let pool_size = crate::monitors()
            .unwrap_or_default()
            .iter()
            .fold(size, |most, monitor| SizeInt32 {
                Width: most.Width.max(monitor.width as i32),
                Height: most.Height.max(monitor.height as i32),
            });
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&graphics, FORMAT, 2, pool_size)
            .map_err(fail("make the frames of"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(fail("start a capture of"))?;
        // Windows 10 before 2004 has no such switch and draws the pointer in.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let arrived = Arc::new(Arrived::new()?);
        let closed = Arc::new(AtomicBool::new(false));
        let wake = Arc::clone(&arrived);
        pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            wake.set();
            Ok(())
        }))
        .map_err(fail("hear the frames of"))?;
        let (gone, wake) = (Arc::clone(&closed), Arc::clone(&arrived));
        item.Closed(&TypedEventHandler::new(move |_, _| {
            gone.store(true, Ordering::Release);
            wake.set();
            Ok(())
        }))
        .map_err(fail("hear about"))?;
        session.StartCapture().map_err(fail("start capturing"))?;
        let mut qpc_frequency = 0;
        // SAFETY: fills one live i64; it cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut qpc_frequency) };
        Ok(WindowCapture {
            hwnd,
            options,
            device,
            context,
            graphics,
            converter,
            layout,
            targets,
            last_out: 1,
            _item: item,
            pool,
            session,
            pool_size,
            arrived,
            closed,
            cap: FpsCap::new(options.max_fps),
            pending: None,
            skipped: 0,
            minimized: false,
            caption: None,
            restoring_until: None,
            number: 0,
            qpc_frequency: qpc_frequency.max(1),
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn plan(&self) -> Plan {
        self.layout.plan
    }

    // From the next picture on.
    pub fn set_limits(&mut self, max_width: u32, max_height: u32) {
        self.options.max_width = max_width;
        self.options.max_height = max_height;
    }

    // As Capture::next. A minimized window is a pause, told once, until a
    // picture comes again; a closed one is an error that ends the share.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Next, CaptureError> {
        let idle_at = Instant::now() + IDLE_WAIT;
        let hwnd = HWND(self.hwnd as *mut _);
        loop {
            // The item's Closed event does not always come for a destroyed
            // window, so its handle is asked too.
            // SAFETY: plain queries; a window that is gone answers false.
            if self.closed.load(Ordering::Acquire) || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return Err(CaptureError::other("the shared window was closed"));
            }
            // SAFETY: as above.
            if unsafe { IsIconic(hwnd) }.as_bool() {
                // What Windows composes of a minimized window is only its
                // caption: no picture to send, and no size to follow.
                while let Ok(frame) = self.pool.TryGetNextFrame() {
                    let _ = frame.Close();
                }
                if let Some(size) = self.size() {
                    self.caption = Some(size);
                }
                if !mem::replace(&mut self.minimized, true) {
                    self.drop_pending();
                    return Ok(Next::Paused(PauseReason::Minimized));
                }
                let rest = idle_at.saturating_duration_since(Instant::now());
                if rest.is_zero() {
                    return self.idle();
                }
                self.arrived.wait(rest);
                continue;
            }
            if mem::replace(&mut self.minimized, false) {
                self.restoring_until = Some(Instant::now() + RESTORING);
            }
            let now = Instant::now();
            let until = match self.cap.held_until() {
                Some(until) if until <= now => {
                    let Some(times) = self.pending.take() else {
                        self.cap.drop_held();
                        continue;
                    };
                    self.cap.release();
                    return Ok(Next::Frame(self.frame_out(1 - self.last_out, times)));
                }
                Some(until) => until,
                None => idle_at,
            };
            if let Some(frame) = self.newest() {
                if let Some(next) = self.take(&frame)? {
                    return Ok(next);
                }
                continue;
            }
            if self.pending.is_none() && Instant::now() >= idle_at {
                return self.idle();
            }
            self.arrived
                .wait(until.saturating_duration_since(Instant::now()));
        }
    }

    // Windows.Graphics.Capture has no event for a removed device, and while
    // no frame comes nothing here touches it, so a driver restart or a GPU
    // reset would pass for a still window. A monitor's capture learns it
    // from AcquireNextFrame.
    fn idle(&self) -> Result<Next, CaptureError> {
        // SAFETY: a getter on a live interface; it answers for a removed
        // device too, which is what it is for.
        match unsafe { self.device.GetDeviceRemovedReason() } {
            Ok(()) => Ok(Next::Idle),
            Err(err) => Err(crate::device::lost(
                &self.device,
                "capture the shared window",
                &err,
            )),
        }
    }

    // The newest frame waiting, the others handed back unseen.
    fn newest(&mut self) -> Option<Direct3D11CaptureFrame> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
                self.skipped += 1;
            }
        }
        newest
    }

    fn take(&mut self, frame: &Direct3D11CaptureFrame) -> Result<Option<Next>, CaptureError> {
        let acquired = Instant::now();
        let acquired_qpc = qpc();
        let content = frame.ContentSize().map_err(fail("read the size of"))?;
        if let Some(until) = self.restoring_until {
            let size = Some((content.Width, content.Height));
            if acquired < until && (size == self.caption || size != self.size()) {
                let _ = frame.Close();
                self.skipped += 1;
                return Ok(None);
            }
            self.restoring_until = None;
        }
        let time = frame
            .SystemRelativeTime()
            .map_err(fail("read the time of"))?;
        let access: IDirect3DDxgiInterfaceAccess = frame
            .Surface()
            .and_then(|surface| surface.cast())
            .map_err(fail("read the picture of"))?;
        // SAFETY: the frame's own surface, a texture on this device.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(fail("read the picture of"))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture filling a live local.
        unsafe { texture.GetDesc(&mut desc) };
        // Only a window bigger than every monitor gets here. This frame is
        // cut to the old frames' size, and the next one at the new size
        // comes when the window next changes.
        if content.Width > self.pool_size.Width || content.Height > self.pool_size.Height {
            let size = SizeInt32 {
                Width: content.Width.max(self.pool_size.Width),
                Height: content.Height.max(self.pool_size.Height),
            };
            self.pool
                .Recreate(&self.graphics, FORMAT, 2, size)
                .map_err(fail("follow the new size of"))?;
            self.pool_size = size;
        }
        // The picture is the content part of a surface that can still be
        // the old size, and never more than the surface.
        let width = (content.Width.max(2) as u32).min(desc.Width);
        let height = (content.Height.max(2) as u32).min(desc.Height);
        self.rebuild(Plan::new(
            width,
            height,
            Rotation::Identity,
            self.options.max_width,
            self.options.max_height,
        ))?;
        if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0 {
            return Err(CaptureError::other(
                "could not read the picture of the shared window: Windows handed a texture the GPU cannot read",
            ));
        }
        let mut view = None;
        // SAFETY: a live texture that allows shader reads, the default view,
        // a live out parameter.
        unsafe {
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
        }
        .map_err(fail("read the picture of"))?;
        let view = view.ok_or_else(|| {
            CaptureError::other(
                "could not read the picture of the shared window: Direct3D returned no view",
            )
        })?;
        let slot = 1 - self.last_out;
        self.converter
            .convert(&view, &self.layout, &self.targets[slot]);
        drop(view);
        drop(texture);
        let _ = frame.Close();
        // SAFETY: a call on the live immediate context.
        unsafe { self.context.Flush() };
        let converted = Instant::now();
        let present_qpc =
            (i128::from(time.Duration) * i128::from(self.qpc_frequency) / i128::from(TICKS)) as i64;
        let present = present_instant(acquired, acquired_qpc, present_qpc, self.qpc_frequency);
        let times = (present, acquired, converted);
        if self.pending.take().is_some() {
            self.skipped += 1;
        }
        Ok(match self.cap.arrive(present) {
            Decision::Send => Some(Next::Frame(self.frame_out(slot, times))),
            Decision::Hold => {
                self.pending = Some(times);
                None
            }
        })
    }

    // A new size: new constants, and new targets if the output size changed.
    fn rebuild(&mut self, plan: Plan) -> Result<(), CaptureError> {
        if plan == self.layout.plan {
            return Ok(());
        }
        if (plan.width, plan.height) != (self.layout.plan.width, self.layout.plan.height) {
            // A held picture is in the targets that go.
            self.drop_pending();
            self.targets = [
                self.converter.target(plan.width, plan.height)?,
                self.converter.target(plan.width, plan.height)?,
            ];
            self.last_out = 1;
        }
        self.layout = self.converter.layout(plan)?;
        Ok(())
    }

    // What Windows composes of the window: its frame without the shadow
    // and the invisible borders, as the captured pictures are.
    fn size(&self) -> Option<(i32, i32)> {
        let mut bounds = RECT::default();
        // SAFETY: a live RECT of the size passed; a gone window fails.
        unsafe {
            DwmGetWindowAttribute(
                HWND(self.hwnd as *mut _),
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut bounds as *mut RECT as *mut _,
                mem::size_of::<RECT>() as u32,
            )
        }
        .ok()?;
        Some((bounds.right - bounds.left, bounds.bottom - bounds.top))
    }

    fn drop_pending(&mut self) {
        if self.pending.take().is_some() {
            self.cap.drop_held();
            self.skipped += 1;
        }
    }

    fn frame_out(
        &mut self,
        slot: usize,
        (present, acquired, converted): (Instant, Instant, Instant),
    ) -> Frame {
        self.last_out = slot;
        let number = self.number;
        self.number += 1;
        Frame {
            texture: self.targets[slot].texture.clone(),
            width: self.layout.plan.width,
            height: self.layout.plan.height,
            number,
            present,
            acquired,
            converted,
            skipped: mem::take(&mut self.skipped),
            accumulated: 1,
            protected: false,
            cursor: None,
            gpu_convert: self.converter.gpu_time(),
        }
    }
}

// `step` reads before "the shared window", as in "read the size of".
fn fail(step: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |err| CaptureError::windows(format_args!("{step} the shared window"), &err)
}

// The capture stops when this is dropped, as a paused share needs: closing
// the session and the pool ends Windows' part at once.
impl Drop for WindowCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
