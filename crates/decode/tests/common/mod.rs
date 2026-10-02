// What the decoder tests share: a device on the NVIDIA GPU, a stream of the
// capture crate's test pattern encoded with NVENC on that device, in
// H.264 or HEVC, and a way to read the frame number back out of a decoded
// picture. Nothing here touches the screen: every picture is the pattern,
// since a test must never read back, save or show a real capture.
//
// Each test runs on one thread and does everything there, capture pattern,
// encode, decode and read back, so the device's immediate context is never
// used from two threads at once.

#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use capture::Pattern;
use decode::{Codec, DecodeError, Decoded, Decoder};
use encode::{AccessUnit, Encoder, Frame, Kind, Settings};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

pub const NVIDIA: u32 = 0x10de;
pub const FPS: u32 = 120;
pub const CODECS: [Codec; 2] = [Codec::H264, Codec::Hevc];

// The GPU tests take turns, so decode times are not another test's encode
// and a consumer card's few encode sessions are never all in use.
static TURN: Mutex<()> = Mutex::new(());

pub fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub name: String,
}

/// A device on the first NVIDIA adapter, made the way capture makes its
/// own (video support, multithread protection), or None with the reason
/// printed.
pub fn nvidia() -> Option<Gpu> {
    let adapters = capture::adapters().unwrap_or_else(|e| panic!("{e}"));
    let Some(adapter) = adapters.iter().find(|a| a.vendor_id == NVIDIA) else {
        println!("skipped: no NVIDIA GPU on this PC, so there is no NVENC to make a stream with");
        return None;
    };
    let device = capture::device_on(adapter).unwrap_or_else(|e| panic!("{e}"));
    // SAFETY: a getter on a live device.
    let context = unsafe { device.GetImmediateContext() }.expect("immediate context");
    Some(Gpu {
        device,
        context,
        name: adapter.description.clone(),
    })
}

/// The decoder, or None with the reason printed when this PC cannot run
/// it: no FFmpeg DLLs, or no hardware decoder for the codec.
pub fn decoder(gpu: &Gpu, codec: Codec) -> Option<Decoder> {
    match Decoder::new(&gpu.device, codec) {
        Ok(decoder) => Some(decoder),
        Err(err @ (DecodeError::Missing { .. } | DecodeError::Unsupported { .. })) => {
            println!("skipped: {err}");
            None
        }
        Err(err) => panic!("{err}"),
    }
}

/// The pattern at a size, encoded frame by frame with NVENC.
pub struct Stream {
    pub pattern: Pattern,
    pub encoder: Box<dyn Encoder>,
}

impl Stream {
    pub fn new(gpu: &Gpu, codec: Codec, width: u32, height: u32) -> Stream {
        let pattern = Pattern::new(&gpu.device, width, height, 0).unwrap_or_else(|e| panic!("{e}"));
        let codec = match codec {
            Codec::H264 => encode::Codec::H264,
            Codec::Hevc => encode::Codec::Hevc,
        };
        let encoder = encode::open_kind_codec(
            Kind::Nvenc,
            codec,
            &gpu.device,
            width,
            height,
            FPS,
            &Settings::default(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        Stream { pattern, encoder }
    }

    pub fn next(&mut self, force_idr: bool) -> AccessUnit {
        let frame = self.pattern.next().unwrap_or_else(|e| panic!("{e}"));
        self.encoder
            .encode(&Frame {
                texture: &frame.texture,
                index: frame.number,
                force_idr,
            })
            .unwrap_or_else(|e| panic!("frame {}: {e}", frame.number))
    }
}

/// Reads decoded pattern pictures back to the CPU and times the GPU.
pub struct Reader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    corner: ID3D11Texture2D,
}

// The decode runs on the GPU's video engine, which an event query on the
// immediate context does not wait for (measured: it signals about 1.6 ms
// before a 1440p picture can be read). A copy out of the picture does wait,
// so copying a 16x16 corner and mapping it shows when the picture is done,
// at a cost of a few microseconds.
const CORNER: u32 = 16;

impl Reader {
    pub fn new(gpu: &Gpu) -> Reader {
        Reader {
            device: gpu.device.clone(),
            context: gpu.context.clone(),
            staging: None,
            corner: staging(&gpu.device, CORNER, CORNER),
        }
    }

    /// Waits until the GPU has finished decoding the picture and returns
    /// when it saw that.
    pub fn wait_for_picture(&self, decoded: &Decoded) -> Instant {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture.
        unsafe { decoded.texture.GetDesc(&mut desc) };
        let area = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: CORNER,
            bottom: CORNER,
            back: 1,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both NV12 on this device; the box is inside the slice and
        // even, as NV12 needs. Map waits for the copy.
        unsafe {
            self.context.CopySubresourceRegion(
                &self.corner,
                0,
                0,
                0,
                0,
                &decoded.texture,
                decoded.index * desc.MipLevels,
                Some(&area),
            );
            self.context
                .Map(&self.corner, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .expect("map the corner of the decoded picture");
        }
        let done = Instant::now();
        // SAFETY: mapped above.
        unsafe { self.context.Unmap(&self.corner, 0) };
        done
    }

    /// The frame number the pattern wrote into the picture, if it reads as
    /// one.
    pub fn frame_number(&mut self, decoded: &Decoded) -> Option<u32> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture.
        unsafe { decoded.texture.GetDesc(&mut desc) };
        let fits =
            matches!(&self.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
        if !fits {
            self.staging = Some((
                staging(&self.device, desc.Width, desc.Height),
                desc.Width,
                desc.Height,
            ));
        }
        let Some((staging, _, _)) = &self.staging else {
            unreachable!("made just above");
        };
        let subresource = decoded.index * desc.MipLevels;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both NV12 of the same size on this device; the slice is in
        // the array. Map waits for the copy, and the copy for the decode.
        unsafe {
            self.context.CopySubresourceRegion(
                staging,
                0,
                0,
                0,
                0,
                &decoded.texture,
                subresource,
                None,
            );
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .expect("map the decoded picture");
        }
        let pitch = mapped.RowPitch as usize;
        // SAFETY: the Y plane is Height rows of `pitch` bytes, mapped until
        // Unmap below.
        let luma = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize)
        };
        let number = capture::read_frame_number(luma, pitch, decoded.width, decoded.height);
        // SAFETY: mapped above; `luma` is not used past here.
        unsafe { self.context.Unmap(staging, 0) };
        number
    }
}

fn staging(device: &ID3D11Device, width: u32, height: u32) -> ID3D11Texture2D {
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
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: a full description and a live out parameter.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("staging texture");
    texture.expect("staging texture")
}

pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Median, 95th percentile and maximum, in ms.
pub fn spread(values: &[Duration]) -> (f64, f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort();
    let at = |q: f64| ms(sorted[((sorted.len() - 1) as f64 * q).round() as usize]);
    (at(0.5), at(0.95), ms(sorted[sorted.len() - 1]))
}
