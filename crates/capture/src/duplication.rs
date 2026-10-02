use std::ffi::c_void;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MODE_CHANGE_IN_PROGRESS, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
    IDXGIOutput, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetDpiAwarenessContextForProcess, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use crate::cap::{Decision, FpsCap};
use crate::convert::{Converter, Layout, Plan, Target};
use crate::device;
use crate::error::{CaptureError, ErrorKind, is_device_lost, meaning};
use crate::monitors::{Monitor, Rotation, find};
use crate::{CursorKind, CursorShape, CursorUpdate, Frame, Next, Options, PauseReason};

// How long next() waits for a change before it answers Idle, and how often
// it tries to get the desktop back while paused. Either is how long a share
// thread may take to notice it was told to stop.
const IDLE_WAIT: Duration = Duration::from_millis(100);
const RETRY: Duration = Duration::from_millis(100);

// DuplicateOutput1 works only for a process that is per-monitor DPI aware
// (v2). booth.exe becomes that when eframe starts its window: winit calls
// SetProcessDpiAwarenessContext before the first window exists. Tests and
// examples, which open no window, call this first.
pub fn make_process_dpi_aware() -> Result<(), CaptureError> {
    // SAFETY: a constant handle value; the call only fails.
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_ok()
        || is_per_monitor_v2()
    {
        return Ok(());
    }
    Err(CaptureError::other(
        "could not make this process per-monitor DPI aware (v2): it already chose another DPI awareness, and screen capture needs this one",
    ))
}

fn is_per_monitor_v2() -> bool {
    // SAFETY: a null process handle means this process; both handles are
    // plain values.
    unsafe {
        AreDpiAwarenessContextsEqual(
            GetDpiAwarenessContextForProcess(HANDLE::default()),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    }
    .as_bool()
}

pub struct Capture {
    monitor: Monitor,
    options: Options,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    converter: Converter,
    duplication: Option<IDXGIOutputDuplication>,
    // A new duplication's first frame is the whole desktop, even when
    // nothing was presented since and its present time is zero, so a still
    // screen still gets its first picture.
    fresh: bool,
    rotation: Rotation,
    layout: Layout,
    targets: [Target; 2],
    last_out: usize,
    // The desktop image when the duplication's own texture cannot be read
    // by a shader: a copy on the GPU, never on the CPU.
    copy: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    cap: FpsCap,
    pending: Option<Pending>,
    skipped: u32,
    paused: bool,
    number: u64,
    pointer: (i32, i32, bool),
    qpc_frequency: i64,
    // Tests make Windows refuse the desktop with this, since only the user
    // can bring up the secure desktop, and count the tries.
    #[cfg(test)]
    refuse: Option<PauseReason>,
    #[cfg(test)]
    reopens: u32,
}

struct Pending {
    present: Instant,
    acquired: Instant,
    converted: Instant,
    accumulated: u32,
    protected: bool,
}

enum Acquired {
    Timeout,
    Lost,
    Frame(DXGI_OUTDUPL_FRAME_INFO, IDXGIResource),
}

// Releases the duplication frame when dropped, so an error on the way out
// of next() never keeps it.
struct HeldFrame<'a> {
    duplication: &'a IDXGIOutputDuplication,
    released: bool,
}

impl HeldFrame<'_> {
    fn release(mut self) -> windows::core::Result<()> {
        self.released = true;
        // SAFETY: a frame was acquired on this duplication and not released.
        unsafe { self.duplication.ReleaseFrame() }
    }
}

impl Drop for HeldFrame<'_> {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: as in release.
            let _ = unsafe { self.duplication.ReleaseFrame() };
        }
    }
}

impl Capture {
    pub fn open(monitor: &Monitor, options: Options) -> Result<Capture, CaptureError> {
        let name = &monitor.name;
        if !is_per_monitor_v2() {
            return Err(CaptureError::other(format!(
                "could not duplicate {name}: this process is not per-monitor DPI aware (v2), which Windows requires for it. Call make_process_dpi_aware() before any window opens"
            )));
        }
        let (adapter, output) = find(&monitor.id)?;
        let gpu = &monitor.adapter.description;
        let (device, context) = device::create(&adapter, gpu)?;
        let converter = Converter::new(&device, gpu)?;
        let (duplication, plan) =
            duplicate(&device, &output, monitor, options).map_err(|refused| match refused {
                Refused::Paused(_, err) | Refused::Failed(err) => err,
            })?;
        let layout = converter.layout(plan)?;
        let targets = [
            converter.target(plan.width, plan.height)?,
            converter.target(plan.width, plan.height)?,
        ];
        let mut qpc_frequency = 0;
        // SAFETY: fills one live i64; it cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut qpc_frequency) };
        Ok(Capture {
            monitor: monitor.clone(),
            options,
            device,
            context,
            converter,
            duplication: Some(duplication),
            fresh: true,
            rotation: plan.rotation,
            layout,
            targets,
            last_out: 1,
            copy: None,
            cap: FpsCap::new(options.max_fps),
            pending: None,
            skipped: 0,
            paused: false,
            number: 0,
            pointer: (0, 0, false),
            qpc_frequency: qpc_frequency.max(1),
            #[cfg(test)]
            refuse: None,
            #[cfg(test)]
            reopens: 0,
        })
    }

    // The device the frames are on; the encoder opens on this one.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn monitor(&self) -> &Monitor {
        &self.monitor
    }

    // Sizes and scale in use; they change after a mode change.
    pub fn plan(&self) -> Plan {
        self.layout.plan
    }

    pub fn compile_time(&self) -> Duration {
        self.converter.compile_time()
    }

    // Blocks until there is something to say. Not an Iterator: it never
    // ends, and each item can be an error.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Next, CaptureError> {
        // AcquireNextFrame can time out early, on a refresh that brought
        // nothing new, so Idle waits for the whole of IDLE_WAIT.
        let mut idle_at = Instant::now() + IDLE_WAIT;
        loop {
            if self.duplication.is_none() {
                // The first try is at once, since a mode change is often
                // over by then; while paused, one try per call, RETRY apart.
                if self.paused {
                    thread::sleep(RETRY);
                }
                if let Some(reason) = self.reopen()? {
                    if self.paused {
                        return Ok(Next::Idle);
                    }
                    self.paused = true;
                    return Ok(Next::Paused(reason));
                }
                self.paused = false;
                idle_at = Instant::now() + IDLE_WAIT;
            }
            let now = Instant::now();
            let until = match self.cap.held_until() {
                Some(until) if until <= now => {
                    let Some(pending) = self.pending.take() else {
                        self.cap.drop_held();
                        continue;
                    };
                    self.cap.release();
                    return Ok(Next::Frame(self.frame_out(
                        1 - self.last_out,
                        pending,
                        None,
                    )));
                }
                Some(until) => until,
                None => idle_at,
            };
            // AcquireNextFrame counts whole milliseconds; rounding up keeps a
            // held frame from going out before its time.
            let timeout = until
                .saturating_duration_since(now)
                .as_micros()
                .div_ceil(1000)
                .max(1) as u32;
            match self.acquire(timeout)? {
                Acquired::Timeout => {
                    if self.pending.is_none() && Instant::now() >= idle_at {
                        return Ok(Next::Idle);
                    }
                }
                // A mode change, a rotation or the secure desktop.
                Acquired::Lost => self.lose(),
                Acquired::Frame(info, resource) => {
                    if let Some(next) = self.take(info, resource)? {
                        return Ok(next);
                    }
                }
            }
        }
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<Acquired, CaptureError> {
        let Some(duplication) = &self.duplication else {
            return Ok(Acquired::Lost);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: both out parameters are live locals. A frame acquired here
        // is released in take() on every path.
        let result = unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) };
        match result {
            Ok(()) => match resource {
                Some(resource) => Ok(Acquired::Frame(info, resource)),
                None => {
                    // SAFETY: the frame was acquired just above.
                    let _ = unsafe { duplication.ReleaseFrame() };
                    Ok(Acquired::Timeout)
                }
            },
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Acquired::Timeout),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => Ok(Acquired::Lost),
            Err(err) => Err(self.failed("read the next picture of", &err)),
        }
    }

    // One acquired frame: the pointer, and the picture converted and then
    // either sent, or held for the fps cap.
    fn take(
        &mut self,
        info: DXGI_OUTDUPL_FRAME_INFO,
        resource: IDXGIResource,
    ) -> Result<Option<Next>, CaptureError> {
        let acquired = Instant::now();
        let acquired_qpc = qpc();
        let Some(duplication) = self.duplication.clone() else {
            return Ok(None);
        };
        let held = HeldFrame {
            duplication: &duplication,
            released: false,
        };
        let cursor = self.cursor_update(&info, &duplication)?;
        let fresh = std::mem::take(&mut self.fresh);
        if info.LastPresentTime == 0 && !fresh {
            drop(resource);
            if let Err(err) = held.release() {
                self.release_failed(err)?;
            }
            return Ok(cursor.map(Next::Cursor));
        }
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|err| self.failed("read the picture of", &err))?;
        drop(resource);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture filling a live local.
        unsafe { texture.GetDesc(&mut desc) };
        // The texture's own size is the one that counts; the mode Windows
        // reported can be the rotated one.
        self.rebuild(Plan::new(
            desc.Width,
            desc.Height,
            self.rotation,
            self.options.max_width,
            self.options.max_height,
        ))?;
        let view = self.source_view(&texture, &desc)?;
        let slot = 1 - self.last_out;
        self.converter
            .convert(&view, &self.layout, &self.targets[slot]);
        drop(view);
        drop(texture);
        let released = held.release();
        // SAFETY: a call on the live immediate context.
        unsafe { self.context.Flush() };
        let converted = Instant::now();
        if let Err(err) = released {
            self.release_failed(err)?;
        }
        let present = if info.LastPresentTime == 0 {
            acquired
        } else {
            present_instant(
                acquired,
                acquired_qpc,
                info.LastPresentTime,
                self.qpc_frequency,
            )
        };
        let times = Pending {
            present,
            acquired,
            converted,
            accumulated: info.AccumulatedFrames,
            protected: info.ProtectedContentMaskedOut.as_bool(),
        };
        Ok(self.send_or_hold(slot, times, cursor))
    }

    // A converted picture in targets[slot] goes out now or waits for the
    // fps cap, in place of any picture already waiting.
    fn send_or_hold(
        &mut self,
        slot: usize,
        times: Pending,
        cursor: Option<CursorUpdate>,
    ) -> Option<Next> {
        if self.pending.take().is_some() {
            self.skipped += 1;
        }
        match self.cap.arrive(times.present) {
            Decision::Send => Some(Next::Frame(self.frame_out(slot, times, cursor))),
            // The desktop was lost as this picture was handed back. It is
            // from before the loss, and the targets it is in may be made
            // again before its time comes.
            Decision::Hold if self.duplication.is_none() => {
                self.cap.drop_held();
                self.skipped += 1;
                cursor.map(Next::Cursor)
            }
            Decision::Hold => {
                self.pending = Some(times);
                // The pointer never waits for the picture.
                cursor.map(Next::Cursor)
            }
        }
    }

    fn release_failed(&mut self, err: windows::core::Error) -> Result<(), CaptureError> {
        if err.code() == DXGI_ERROR_ACCESS_LOST {
            self.lose();
            return Ok(());
        }
        Err(self.failed("hand back the picture of", &err))
    }

    // The duplication is gone until reopen() makes a new one. A picture held
    // for the fps cap is from before and is dropped with it.
    fn lose(&mut self) {
        self.duplication = None;
        self.drop_pending();
    }

    fn drop_pending(&mut self) {
        if self.pending.take().is_some() {
            self.cap.drop_held();
            self.skipped += 1;
        }
    }

    fn frame_out(&mut self, slot: usize, times: Pending, cursor: Option<CursorUpdate>) -> Frame {
        self.last_out = slot;
        let number = self.number;
        self.number += 1;
        Frame {
            texture: self.targets[slot].texture.clone(),
            width: self.layout.plan.width,
            height: self.layout.plan.height,
            number,
            present: times.present,
            acquired: times.acquired,
            converted: times.converted,
            skipped: std::mem::take(&mut self.skipped),
            accumulated: times.accumulated,
            protected: times.protected,
            cursor,
            gpu_convert: self.converter.gpu_time(),
        }
    }

    // A view the shader can read the picture through. The duplication's
    // texture normally allows one; if a driver hands one that does not, the
    // picture is copied into a texture of our own, on the GPU.
    fn source_view(
        &mut self,
        texture: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11ShaderResourceView, CaptureError> {
        if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
            let mut view = None;
            // SAFETY: a live texture that allows shader reads, the default
            // view, a live out parameter.
            unsafe {
                self.device
                    .CreateShaderResourceView(texture, None, Some(&mut view))
            }
            .map_err(|err| self.failed("read the picture of", &err))?;
            return view.ok_or_else(|| {
                CaptureError::other(format!(
                    "could not read the picture of {}: Direct3D returned no view",
                    self.monitor.name
                ))
            });
        }
        let fits = self.copy.as_ref().is_some_and(|(copy, _)| {
            let mut have = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: a getter on a live texture filling a live local.
            unsafe { copy.GetDesc(&mut have) };
            (have.Width, have.Height, have.Format) == (desc.Width, desc.Height, desc.Format)
        });
        if !fits {
            self.copy = Some(self.copy_texture(desc)?);
        }
        let Some((copy, view)) = &self.copy else {
            unreachable!("made just above");
        };
        // SAFETY: two live textures of the same size and format on this
        // device; the copy stays on the GPU.
        unsafe { self.context.CopyResource(copy, texture) };
        Ok(view.clone())
    }

    fn copy_texture(
        &self,
        like: &D3D11_TEXTURE2D_DESC,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView), CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: like.Width,
            Height: like.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: like.Format,
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
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|err| self.failed("make a texture for the picture of", &err))?;
        let texture = texture.ok_or_else(|| {
            CaptureError::other("could not make a texture for the picture: Direct3D returned none")
        })?;
        let mut view = None;
        // SAFETY: a live texture made for shader reads, the default view, a
        // live out parameter.
        unsafe {
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
        }
        .map_err(|err| self.failed("read the picture of", &err))?;
        let view = view.ok_or_else(|| {
            CaptureError::other("could not read the picture: Direct3D returned no view")
        })?;
        Ok((texture, view))
    }

    fn cursor_update(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        duplication: &IDXGIOutputDuplication,
    ) -> Result<Option<CursorUpdate>, CaptureError> {
        let moved = info.LastMouseUpdateTime != 0;
        if moved {
            let position = info.PointerPosition;
            self.pointer = (
                position.Position.x,
                position.Position.y,
                position.Visible.as_bool(),
            );
        }
        let shape = if info.PointerShapeBufferSize > 0 {
            self.pointer_shape(duplication, info.PointerShapeBufferSize)?
        } else {
            None
        };
        if !moved && shape.is_none() {
            return Ok(None);
        }
        let (scale_x, scale_y) = self.layout.plan.scale();
        let (x, y, visible) = self.pointer;
        Ok(Some(CursorUpdate {
            x: (x as f32 * scale_x).round() as i32,
            y: (y as f32 * scale_y).round() as i32,
            visible,
            scale: scale_x,
            shape,
        }))
    }

    // The pointer's shape is not desktop content; the viewer draws it.
    fn pointer_shape(
        &self,
        duplication: &IDXGIOutputDuplication,
        size: u32,
    ) -> Result<Option<CursorShape>, CaptureError> {
        let mut bytes = vec![0u8; size as usize];
        let mut needed = 0;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the frame is held, `bytes` has the size passed, and the
        // other two out parameters are live locals.
        let result = unsafe {
            duplication.GetFramePointerShape(
                size,
                bytes.as_mut_ptr() as *mut c_void,
                &mut needed,
                &mut shape,
            )
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => return Ok(None),
            Err(err) => return Err(self.failed("read the pointer shape on", &err)),
        }
        bytes.truncate(needed as usize);
        let kind = match shape.Type as i32 {
            kind if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 => CursorKind::Monochrome,
            kind if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 => CursorKind::Color,
            kind if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => {
                CursorKind::MaskedColor
            }
            _ => return Ok(None),
        };
        Ok(Some(CursorShape {
            kind,
            width: shape.Width,
            height: shape.Height,
            pitch: shape.Pitch,
            hotspot_x: shape.HotSpot.x,
            hotspot_y: shape.HotSpot.y,
            bytes,
        }))
    }

    // Some(reason) when Windows will not give the desktop back yet.
    fn reopen(&mut self) -> Result<Option<PauseReason>, CaptureError> {
        #[cfg(test)]
        {
            self.reopens += 1;
            if let Some(reason) = &self.refuse {
                return Ok(Some(reason.clone()));
            }
        }
        let (_, output) = find(&self.monitor.id)?;
        match duplicate(&self.device, &output, &self.monitor, self.options) {
            Ok((duplication, plan)) => {
                self.rebuild(plan)?;
                self.duplication = Some(duplication);
                self.fresh = true;
                Ok(None)
            }
            Err(Refused::Paused(reason, _)) => Ok(Some(reason)),
            Err(Refused::Failed(err)) => Err(err),
        }
    }

    // A new mode or rotation: new conversion constants, and new targets if
    // the output size changed. Frames carry their size, so the encoder sees
    // the change on the next one.
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
        self.rotation = plan.rotation;
        Ok(())
    }

    fn failed(&self, step: &str, err: &windows::core::Error) -> CaptureError {
        let step = format!("{step} {}", self.monitor.name);
        if is_device_lost(err.code()) {
            device::lost(&self.device, &step, err)
        } else {
            CaptureError::windows(step, err)
        }
    }
}

fn qpc() -> i64 {
    let mut counter = 0;
    // SAFETY: fills one live i64; it cannot fail on Windows XP and later.
    let _ = unsafe { QueryPerformanceCounter(&mut counter) };
    counter
}

// LastPresentTime is a performance counter value. An Instant is one too
// underneath but has no public constructor, so the present time is carried
// over as its age when the frame was acquired. The counter is read just
// after `acquired`, so a thread switch between the two reads can only make
// the age look longer, never put the present time after `acquired`.
fn present_instant(
    acquired: Instant,
    acquired_qpc: i64,
    present_qpc: i64,
    frequency: i64,
) -> Instant {
    let ticks = acquired_qpc.saturating_sub(present_qpc).max(0) as u128;
    let nanos = ticks * 1_000_000_000 / frequency.max(1) as u128;
    let age = Duration::from_nanos(nanos.min(u64::MAX as u128) as u64);
    acquired.checked_sub(age).unwrap_or(acquired)
}

// Paused carries the error too: while sharing it means wait and try again,
// but at open it is the answer.
enum Refused {
    Paused(PauseReason, CaptureError),
    Failed(CaptureError),
}

fn duplicate(
    device: &ID3D11Device,
    output: &IDXGIOutput,
    monitor: &Monitor,
    options: Options,
) -> Result<(IDXGIOutputDuplication, Plan), Refused> {
    let name = &monitor.name;
    let output: IDXGIOutput5 = output.cast().map_err(|_| {
        Refused::Failed(CaptureError::other(format!(
            "could not duplicate {name}: this Windows is older than Windows 10 1703, which Booth needs for screen sharing"
        )))
    })?;
    // SAFETY: a live device made on the adapter this output belongs to, and
    // a format list that outlives the call.
    let duplication = unsafe { output.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) }
        .map_err(|err| {
            let code = err.code();
            let step = format!("duplicate {name}");
            if is_device_lost(code) {
                return Refused::Failed(device::lost(device, &step, &err));
            }
            let reason = match code {
                E_ACCESSDENIED => PauseReason::SecureDesktop,
                DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => PauseReason::Taken,
                DXGI_ERROR_SESSION_DISCONNECTED => PauseReason::Disconnected,
                DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_MODE_CHANGE_IN_PROGRESS => {
                    PauseReason::Changing(meaning(&err))
                }
                // Measured: a second live duplication of one monitor in the
                // same process gets this, from the same device or another.
                E_INVALIDARG => {
                    return Refused::Failed(CaptureError::other(format!(
                        "could not {step}: this program is already duplicating it, and Windows allows one duplication of a monitor per program (E_INVALIDARG). Stop the other share first"
                    )));
                }
                _ => return Refused::Failed(CaptureError::windows(step, &err)),
            };
            Refused::Paused(reason, CaptureError::windows(step, &err))
        })?;
    // SAFETY: a getter on a live duplication.
    let desc = unsafe { duplication.GetDesc() };
    if desc.DesktopImageInSystemMemory.as_bool() {
        return Err(Refused::Failed(CaptureError::new(
            ErrorKind::Other,
            format!(
                "could not duplicate {name}: Windows keeps its picture in system memory instead of on {}, which Booth does not capture from",
                monitor.adapter.description
            ),
        )));
    }
    let rotation = Rotation::from_dxgi(desc.Rotation);
    let plan = Plan::new(
        desc.ModeDesc.Width,
        desc.ModeDesc.Height,
        rotation,
        options.max_width,
        options.max_height,
    );
    Ok((duplication, plan))
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // Windows allows one duplication of a monitor per process, and these
    // tests run side by side in one, so they take turns.
    static DESKTOP: Mutex<()> = Mutex::new(());

    fn desktop_turn() -> MutexGuard<'static, ()> {
        DESKTOP
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // Frames are only counted and timed here; no pixel leaves the GPU.
    fn primary_capture() -> Option<Capture> {
        make_process_dpi_aware().unwrap();
        let monitors = crate::monitors().unwrap();
        let Some(monitor) = monitors.iter().find(|m| m.primary).or(monitors.first()) else {
            println!("skipped: no monitor is attached to the desktop");
            return None;
        };
        Some(Capture::open(monitor, Options::default()).unwrap())
    }

    fn times(present: Instant) -> Pending {
        Pending {
            present,
            acquired: present,
            converted: present,
            accumulated: 1,
            protected: false,
        }
    }

    // Two pictures a millisecond apart: the first goes out, the second is
    // held for the fps cap.
    fn hold_one(capture: &mut Capture) -> Instant {
        let start = Instant::now();
        assert!(matches!(
            capture.send_or_hold(0, times(start), None),
            Some(Next::Frame(_))
        ));
        let slot = 1 - capture.last_out;
        assert!(
            capture
                .send_or_hold(slot, times(start + Duration::from_millis(1)), None)
                .is_none()
        );
        assert!(capture.pending.is_some() && capture.cap.held_until().is_some());
        start
    }

    #[test]
    fn a_present_time_is_never_after_the_frame_was_acquired() {
        let acquired = Instant::now();
        // A 10 MHz counter, as on most PCs: 1000 ticks are 100 microseconds.
        let frequency = 10_000_000;
        assert_eq!(
            present_instant(acquired, 5_000_000, 4_999_000, frequency),
            acquired - Duration::from_micros(100)
        );
        // Presented after the counter was read, which a thread switch
        // between the two reads can make it look like.
        assert_eq!(
            present_instant(acquired, 5_000_000, 5_000_300, frequency),
            acquired
        );
        // Older than an Instant can reach back.
        assert_eq!(
            present_instant(acquired, i64::MAX, i64::MIN, frequency),
            acquired
        );
    }

    // Windows cannot be made to take the desktop away inside a test: a UAC
    // prompt, the lock screen or a mode change all need the user. So the
    // loss is played the way next() meets DXGI_ERROR_ACCESS_LOST, by
    // dropping the duplication, and the next calls must make a new one and
    // bring the whole desktop again.
    #[test]
    fn a_lost_duplication_is_made_again_on_the_next_call() {
        let _turn = desktop_turn();
        let Some(mut capture) = primary_capture() else {
            return;
        };
        let plan = capture.plan();
        for round in 0..3 {
            capture.duplication = None;
            let lost = Instant::now();
            let mut calls = 0;
            loop {
                calls += 1;
                match capture.next().unwrap() {
                    Next::Frame(frame) => {
                        assert_eq!((frame.width, frame.height), (plan.width, plan.height));
                        break;
                    }
                    Next::Cursor(_) if calls < 10 => {}
                    Next::Cursor(_) | Next::Idle => {
                        panic!("round {round}: no picture after the duplication was made again")
                    }
                    Next::Paused(reason) => panic!("round {round}: paused: {reason}"),
                }
            }
            assert!(capture.duplication.is_some());
            println!(
                "round {round}: a picture {:.1} ms after the loss",
                lost.elapsed().as_secs_f64() * 1000.0
            );
        }
        assert_eq!(capture.plan(), plan);
    }

    // ReleaseFrame answering DXGI_ERROR_ACCESS_LOST. A held picture would
    // otherwise go out after the duplication is made again, from before the
    // loss, and after a mode change from a new target never drawn into.
    #[test]
    fn a_held_frame_dies_with_the_desktop() {
        let _turn = desktop_turn();
        let Some(mut capture) = primary_capture() else {
            return;
        };
        let start = hold_one(&mut capture);
        capture
            .release_failed(windows::core::Error::from(DXGI_ERROR_ACCESS_LOST))
            .unwrap();
        assert!(capture.duplication.is_none());
        assert!(capture.pending.is_none());
        assert_eq!(capture.cap.held_until(), None);
        assert_eq!(capture.skipped, 1);
        // Nor is the picture that was being handed back held: it too is from
        // before the loss.
        let slot = 1 - capture.last_out;
        assert!(
            capture
                .send_or_hold(slot, times(start + Duration::from_millis(2)), None)
                .is_none()
        );
        assert!(capture.pending.is_none());
        assert_eq!(capture.cap.held_until(), None);
        assert_eq!(capture.skipped, 2);
        let plan = capture.plan();
        for _ in 0..10 {
            match capture.next().unwrap() {
                Next::Frame(frame) => {
                    assert_eq!((frame.width, frame.height), (plan.width, plan.height));
                    assert!(frame.skipped >= 2, "{} skipped", frame.skipped);
                    return;
                }
                Next::Cursor(_) => {}
                Next::Idle => panic!("no picture after the duplication was made again"),
                Next::Paused(reason) => panic!("paused: {reason}"),
            }
        }
        panic!("only pointer updates after the duplication was made again");
    }

    // The picture Windows hands over changed size, as in a mode change that
    // came without DXGI_ERROR_ACCESS_LOST. The held picture is in the old
    // targets and must not go out from the new ones.
    #[test]
    fn a_held_frame_is_dropped_when_the_targets_are_made_again() {
        let _turn = desktop_turn();
        let Some(mut capture) = primary_capture() else {
            return;
        };
        let plan = capture.plan();
        hold_one(&mut capture);
        let smaller = Plan::new(
            plan.source_width / 2,
            plan.source_height / 2,
            plan.rotation,
            capture.options.max_width,
            capture.options.max_height,
        );
        capture.rebuild(smaller).unwrap();
        assert!(capture.pending.is_none());
        assert_eq!(capture.cap.held_until(), None);
        assert_eq!(capture.skipped, 1);
        // The real picture turns the targets back to the monitor's size.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            if let Next::Frame(frame) = capture.next().unwrap() {
                assert_eq!((frame.width, frame.height), (plan.width, plan.height));
                return;
            }
        }
        println!("no picture came: the desktop is still");
    }

    #[test]
    fn while_paused_each_call_waits_then_tries_once() {
        let _turn = desktop_turn();
        let Some(mut capture) = primary_capture() else {
            return;
        };
        capture.duplication = None;
        capture.refuse = Some(PauseReason::SecureDesktop);
        let lost = Instant::now();
        assert!(matches!(
            capture.next().unwrap(),
            Next::Paused(PauseReason::SecureDesktop)
        ));
        assert_eq!(capture.reopens, 1);
        assert!(lost.elapsed() < RETRY, "the first try waited");
        for call in 2..=4 {
            let asked = Instant::now();
            assert!(matches!(capture.next().unwrap(), Next::Idle));
            assert_eq!(capture.reopens, call);
            assert!(asked.elapsed() >= RETRY, "call {call} did not wait");
        }
        capture.refuse = None;
        for _ in 0..10 {
            match capture.next().unwrap() {
                Next::Frame(_) => {
                    assert_eq!(capture.reopens, 5);
                    assert!(!capture.paused);
                    return;
                }
                Next::Cursor(_) => {}
                other => panic!("{other:?} once the desktop was back"),
            }
        }
        panic!("only pointer updates once the desktop was back");
    }
}
