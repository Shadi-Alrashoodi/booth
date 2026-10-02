use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_FORMAT_SUPPORT_RENDER_TARGET,
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP,
    D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11PixelShader, ID3D11Query, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::{Interface, PCSTR, s};

use crate::error::CaptureError;
use crate::monitors::Rotation;

const SHADER: &str = include_str!("convert.hlsl");

// Sizes and scale for one source size, rotation and size limit. Pure, so
// the CPU reference uses exactly the numbers the GPU gets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub source_width: u32,
    pub source_height: u32,
    pub rotation: Rotation,
    pub upright_width: u32,
    pub upright_height: u32,
    pub width: u32,
    pub height: u32,
    // Upright source pixels under one output pixel, per axis: 1 when the
    // size is kept, 1.5 for 3840x2160 to 2560x1440, 1.25 for 5120x1440 to
    // 4096x1152.
    pub footprint: (f32, f32),
}

impl Plan {
    // The limits are Options::max_width and max_height, which apply to the
    // upright picture.
    pub fn new(
        source_width: u32,
        source_height: u32,
        rotation: Rotation,
        max_width: u32,
        max_height: u32,
    ) -> Plan {
        let source_width = source_width.max(1);
        let source_height = source_height.max(1);
        let (upright_width, upright_height) = if rotation.swaps_sides() {
            (source_height, source_width)
        } else {
            (source_width, source_height)
        };
        let max_width = limit(max_width);
        let max_height = limit(max_height);
        let kept = upright_width <= max_width && upright_height <= max_height;
        let (width, height, footprint) = if kept {
            // Kept pixel for pixel. NV12 needs even sides, so an odd side
            // gets one more pixel, a copy of the edge, rather than a
            // resample that would soften every line of text.
            (even_up(upright_width), even_up(upright_height), (1.0, 1.0))
        } else {
            let (width, height) =
                scaled_to_fit(upright_width, upright_height, max_width, max_height);
            (
                width,
                height,
                (
                    upright_width as f32 / width as f32,
                    upright_height as f32 / height as f32,
                ),
            )
        };
        Plan {
            source_width,
            source_height,
            rotation,
            upright_width,
            upright_height,
            width,
            height,
            footprint,
        }
    }

    // How many output pixels one desktop pixel becomes, for the cursor.
    pub fn scale(&self) -> (f32, f32) {
        (1.0 / self.footprint.0, 1.0 / self.footprint.1)
    }
}

// 0 is no limit. A limit is rounded down to even, since NV12 needs even
// sides, and so is no limit, so that a side rounded up to even never
// passes it.
fn limit(max: u32) -> u32 {
    let max = if max == 0 { u32::MAX } else { max };
    (max & !1).max(2)
}

fn even_up(side: u32) -> u32 {
    side + (side & 1)
}

// The side further past its limit is scaled to it, and the other follows at
// the same ratio, rounded to the nearest even number. That one was under its
// own limit before rounding, and the limit is even, so it stays within it.
fn scaled_to_fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width as u64 * max_height as u64 > height as u64 * max_width as u64 {
        (max_width, scaled(height, max_width, width))
    } else {
        (scaled(width, max_height, height), max_height)
    }
}

// `side` times `to / from`, rounded to the nearest even number.
fn scaled(side: u32, to: u32, from: u32) -> u32 {
    let exact = side as u64 * to as u64;
    ((exact + from as u64) / (2 * from as u64) * 2).max(2) as u32
}

// The size a monitor of this size and rotation is captured at.
pub fn output_size(
    source_width: u32,
    source_height: u32,
    rotation: Rotation,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    let plan = Plan::new(source_width, source_height, rotation, max_width, max_height);
    (plan.width, plan.height)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PlaneConstants {
    source_size: [u32; 2],
    upright_size: [u32; 2],
    footprint: [f32; 2],
    quarter_turns: u32,
    unused: u32,
}

// A Plan with its constant buffers on the device.
pub(crate) struct Layout {
    pub plan: Plan,
    luma: ID3D11Buffer,
    chroma: ID3D11Buffer,
}

// One NV12 texture with a view on each plane.
pub(crate) struct Target {
    pub texture: ID3D11Texture2D,
    luma: ID3D11RenderTargetView,
    chroma: ID3D11RenderTargetView,
}

pub(crate) struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    lock: ID3D11Multithread,
    vertex: ID3D11VertexShader,
    luma: ID3D11PixelShader,
    chroma: ID3D11PixelShader,
    compile_time: Duration,
    timing: GpuTiming,
}

impl Converter {
    pub(crate) fn new(device: &ID3D11Device, gpu: &str) -> Result<Converter, CaptureError> {
        // SAFETY: a query on a live device.
        let support = unsafe { device.CheckFormatSupport(DXGI_FORMAT_NV12) }.unwrap_or(0);
        if support & D3D11_FORMAT_SUPPORT_RENDER_TARGET.0 as u32 == 0 {
            return Err(CaptureError::other(format!(
                "could not use {gpu} for sharing: its driver cannot draw into NV12 textures, which the colour conversion needs. Update the graphics driver"
            )));
        }
        // SAFETY: GetImmediateContext hands back an owned reference.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|err| CaptureError::windows(format!("start Direct3D 11 on {gpu}"), &err))?;
        let lock: ID3D11Multithread = context.cast().map_err(|err| {
            CaptureError::windows(format!("share the Direct3D 11 device on {gpu}"), &err)
        })?;
        let started = Instant::now();
        let vertex_code = compile(s!("vs_main"), s!("vs_5_0"))?;
        let luma_code = compile(s!("ps_luma"), s!("ps_5_0"))?;
        let chroma_code = compile(s!("ps_chroma"), s!("ps_5_0"))?;
        let compile_time = started.elapsed();
        let fail = |err: windows::core::Error| {
            CaptureError::windows(format!("load the colour conversion shader on {gpu}"), &err)
        };
        let mut vertex = None;
        let mut luma = None;
        let mut chroma = None;
        // SAFETY: each byte slice is a whole compiled shader, alive for the
        // call, and each out parameter a live local.
        unsafe {
            device
                .CreateVertexShader(&vertex_code, None, Some(&mut vertex))
                .map_err(fail)?;
            device
                .CreatePixelShader(&luma_code, None, Some(&mut luma))
                .map_err(fail)?;
            device
                .CreatePixelShader(&chroma_code, None, Some(&mut chroma))
                .map_err(fail)?;
        }
        let missing = || {
            CaptureError::other(format!(
                "could not load the colour conversion shader on {gpu}: Direct3D returned no shader"
            ))
        };
        Ok(Converter {
            timing: GpuTiming::new(device)?,
            device: device.clone(),
            context,
            lock,
            vertex: vertex.ok_or_else(missing)?,
            luma: luma.ok_or_else(missing)?,
            chroma: chroma.ok_or_else(missing)?,
            compile_time,
        })
    }

    pub(crate) fn compile_time(&self) -> Duration {
        self.compile_time
    }

    // The GPU time of the newest conversion whose timestamps are in. It
    // trails by a frame or two, since reading it never waits for the GPU.
    pub(crate) fn gpu_time(&mut self) -> Option<Duration> {
        let _locked = Locked::enter(&self.lock);
        self.timing.poll(&self.context);
        self.timing.latest
    }

    pub(crate) fn layout(&self, plan: Plan) -> Result<Layout, CaptureError> {
        let constants = |footprint: (f32, f32)| PlaneConstants {
            source_size: [plan.source_width, plan.source_height],
            upright_size: [plan.upright_width, plan.upright_height],
            footprint: [footprint.0, footprint.1],
            quarter_turns: plan.rotation.degrees() / 90,
            unused: 0,
        };
        let luma = self.constant_buffer(constants(plan.footprint))?;
        let chroma =
            self.constant_buffer(constants((plan.footprint.0 * 2.0, plan.footprint.1 * 2.0)))?;
        Ok(Layout { plan, luma, chroma })
    }

    fn constant_buffer(&self, constants: PlaneConstants) -> Result<ID3D11Buffer, CaptureError> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<PlaneConstants>() as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: &constants as *const PlaneConstants as *const c_void,
            ..Default::default()
        };
        let mut buffer = None;
        // SAFETY: `data` points at `constants`, which is ByteWidth bytes and
        // outlives the call; the out parameter is a live local.
        unsafe {
            self.device
                .CreateBuffer(&desc, Some(&data), Some(&mut buffer))
        }
        .map_err(|err| CaptureError::windows("make the colour conversion's constants", &err))?;
        buffer.ok_or_else(|| {
            CaptureError::other(
                "could not make the colour conversion's constants: Direct3D returned no buffer",
            )
        })
    }

    pub(crate) fn target(&self, width: u32, height: u32) -> Result<Target, CaptureError> {
        let step = || format!("make a {width}x{height} NV12 texture");
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
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: a full description and a live out parameter.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|err| CaptureError::windows(step(), &err))?;
        let texture = texture.ok_or_else(|| {
            CaptureError::other(format!(
                "could not {}: Direct3D returned no texture",
                step()
            ))
        })?;
        let luma = self.plane_view(&texture, DXGI_FORMAT_R8_UNORM, &step)?;
        let chroma = self.plane_view(&texture, DXGI_FORMAT_R8G8_UNORM, &step)?;
        Ok(Target {
            texture,
            luma,
            chroma,
        })
    }

    // On a D3D11.0 device the view's format picks the NV12 plane: R8 is the
    // Y plane, R8G8 the interleaved UV plane at half size. The PlaneSlice
    // field that says so explicitly needs D3D11.3, and this works on every
    // driver that reports NV12 render target support at feature level 11.
    fn plane_view(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        step: &dyn Fn() -> String,
    ) -> Result<ID3D11RenderTargetView, CaptureError> {
        let desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: a live texture, a full description, a live out parameter.
        unsafe {
            self.device
                .CreateRenderTargetView(texture, Some(&desc), Some(&mut view))
        }
        .map_err(|err| CaptureError::windows(step(), &err))?;
        view.ok_or_else(|| {
            CaptureError::other(format!("could not {}: Direct3D returned no view", step()))
        })
    }

    // Queues both passes. Nothing waits for the GPU here.
    pub(crate) fn convert(
        &mut self,
        source: &ID3D11ShaderResourceView,
        layout: &Layout,
        target: &Target,
    ) {
        // The encoder, and later Media Foundation, call this context from
        // their own threads. Multithread protection keeps each call whole,
        // but one of theirs landing between binding the target and drawing
        // changes what gets drawn. On my PC a ClearState that landed there
        // once made the driver reset the GPU engine and remove the device.
        let _locked = Locked::enter(&self.lock);
        self.timing.poll(&self.context);
        let context = &self.context;
        let plan = &layout.plan;
        // SAFETY: every object passed is alive and was made on this device;
        // the state set here is set in full on every call, since the encoder
        // shares the context.
        unsafe {
            let slot = self.timing.begin(context);
            context.ClearState();
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShaderResources(0, Some(&[Some(source.clone())]));

            context.OMSetRenderTargets(Some(&[Some(target.luma.clone())]), None);
            context.RSSetViewports(Some(&[viewport(plan.width, plan.height)]));
            context.PSSetShader(&self.luma, None);
            context.PSSetConstantBuffers(0, Some(&[Some(layout.luma.clone())]));
            context.Draw(3, 0);

            context.OMSetRenderTargets(Some(&[Some(target.chroma.clone())]), None);
            context.RSSetViewports(Some(&[viewport(plan.width / 2, plan.height / 2)]));
            context.PSSetShader(&self.chroma, None);
            context.PSSetConstantBuffers(0, Some(&[Some(layout.chroma.clone())]));
            context.Draw(3, 0);

            // Unbound, so neither the source nor the target stays tied to
            // the pipeline once the frame is released or handed on.
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
            self.timing.end(context, slot);
        }
    }
}

// The device's own multithread lock, held until dropped. It is the one
// every protected call takes, and it nests, so the calls made while it is
// held take it again without waiting.
struct Locked<'a>(&'a ID3D11Multithread);

impl<'a> Locked<'a> {
    fn enter(lock: &'a ID3D11Multithread) -> Locked<'a> {
        // SAFETY: a call on a live interface, undone by the drop below.
        unsafe { lock.Enter() };
        Locked(lock)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: entered in Locked::enter on this thread.
        unsafe { self.0.Leave() };
    }
}

fn viewport(width: u32, height: u32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width as f32,
        Height: height as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, CaptureError> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source pointer and length describe SHADER, which is
    // static; the names are NUL-terminated literals; both out parameters
    // are live locals.
    let compiled = unsafe {
        D3DCompile(
            SHADER.as_ptr() as *const c_void,
            SHADER.len(),
            s!("convert.hlsl"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    // SAFETY: the entry point name is a NUL-terminated literal.
    let name = unsafe { entry.to_string() }.unwrap_or_default();
    match (compiled, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code).to_vec()),
        (result, _) => {
            let detail = errors
                .map(|blob| {
                    String::from_utf8_lossy(blob_bytes(&blob))
                        .trim()
                        .to_string()
                })
                .filter(|text| !text.is_empty())
                .or_else(|| result.err().map(|err| crate::error::meaning(&err)))
                .unwrap_or_else(|| "no code came back".to_string());
            Err(CaptureError::other(format!(
                "could not compile the colour conversion shader ({name}) with d3dcompiler_47.dll: {detail}"
            )))
        }
    }
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob owns this many bytes at this pointer for as long as
    // it lives, and the slice borrows the blob.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

// GPU timestamps around each conversion, read back later without waiting.
struct GpuTiming {
    sets: Vec<QuerySet>,
    next: usize,
    latest: Option<Duration>,
}

struct QuerySet {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    waiting: bool,
}

// Four in flight covers the GPU running a few frames behind at 240 Hz.
const TIMING_SETS: usize = 4;

impl GpuTiming {
    fn new(device: &ID3D11Device) -> Result<GpuTiming, CaptureError> {
        let query = |kind| {
            let desc = D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            };
            let mut query = None;
            // SAFETY: a full description and a live out parameter.
            unsafe { device.CreateQuery(&desc, Some(&mut query)) }
                .map_err(|err| CaptureError::windows("make a GPU timer", &err))?;
            query.ok_or_else(|| {
                CaptureError::other("could not make a GPU timer: Direct3D returned no query")
            })
        };
        let mut sets = Vec::with_capacity(TIMING_SETS);
        for _ in 0..TIMING_SETS {
            sets.push(QuerySet {
                disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                begin: query(D3D11_QUERY_TIMESTAMP)?,
                end: query(D3D11_QUERY_TIMESTAMP)?,
                waiting: false,
            });
        }
        Ok(GpuTiming {
            sets,
            next: 0,
            latest: None,
        })
    }

    // SAFETY: the caller passes the immediate context of the device the
    // queries were made on.
    unsafe fn begin(&mut self, context: &ID3D11DeviceContext) -> usize {
        let slot = self.next;
        self.next = (self.next + 1) % self.sets.len();
        let set = &mut self.sets[slot];
        // A set still unread after a full lap is dropped, never waited for.
        set.waiting = false;
        // SAFETY: as the function's contract says.
        unsafe {
            context.Begin(&set.disjoint);
            context.End(&set.begin);
        }
        slot
    }

    // SAFETY: as for begin.
    unsafe fn end(&mut self, context: &ID3D11DeviceContext, slot: usize) {
        let set = &mut self.sets[slot];
        // SAFETY: as the function's contract says.
        unsafe {
            context.End(&set.end);
            context.End(&set.disjoint);
        }
        set.waiting = true;
    }

    fn poll(&mut self, context: &ID3D11DeviceContext) {
        // Oldest first, so `latest` ends on the newest one that is ready.
        for offset in 0..self.sets.len() {
            let slot = (self.next + offset) % self.sets.len();
            let set = &mut self.sets[slot];
            if !set.waiting {
                continue;
            }
            let Some(disjoint) =
                query_data::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(context, &set.disjoint)
            else {
                continue;
            };
            let (Some(begin), Some(end)) = (
                query_data::<u64>(context, &set.begin),
                query_data::<u64>(context, &set.end),
            ) else {
                continue;
            };
            set.waiting = false;
            if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || end < begin {
                continue;
            }
            let nanos = (end - begin) as u128 * 1_000_000_000 / disjoint.Frequency as u128;
            self.latest = Some(Duration::from_nanos(nanos as u64));
        }
    }
}

// GetData answers S_FALSE while the GPU has not got there yet, and the
// windows crate folds S_FALSE into Ok, so the call goes through the vtable
// to see the difference.
fn query_data<T: Default>(context: &ID3D11DeviceContext, query: &ID3D11Query) -> Option<T> {
    let mut data = T::default();
    // SAFETY: both pointers are live interfaces (ID3D11Query derives from
    // ID3D11Asynchronous, so its pointer is one), and `data` is a live T of
    // the size passed, which is the size this query's data has.
    let result = unsafe {
        (Interface::vtable(context).GetData)(
            Interface::as_raw(context),
            Interface::as_raw(query),
            &mut data as *mut T as *mut c_void,
            size_of::<T>() as u32,
            D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
        )
    };
    (result == S_OK).then_some(data)
}
