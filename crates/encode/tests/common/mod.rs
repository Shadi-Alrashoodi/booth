// What the encoder tests and the example share: a D3D11 device on the NVIDIA
// GPU, and a moving NV12 test pattern uploaded from the CPU. Nothing here
// touches the screen: every picture is made up, since a test must never read
// back, save or show a real capture.

#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Foundation::{HMODULE, TRUE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIFactory1,
};
use windows::core::{BOOL, Interface};

use encode::annexb::{self, ListChange, MemoryOp, Pps, SliceHeader, Sps, hevc};

pub const NVIDIA: u32 = 0x10de;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub name: String,
}

// The GPU tests take turns: a consumer card allows only a few encode
// sessions at once, and encode times mean nothing while another test encodes.
static TURN: Mutex<()> = Mutex::new(());

pub fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A device on the first NVIDIA adapter, or None with the reason printed.
pub fn nvidia() -> Option<Gpu> {
    // SAFETY: plain DXGI calls; EnumAdapters1 fails past the last adapter.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }.expect("adapter description");
        if desc.VendorId == NVIDIA {
            let adapter: IDXGIAdapter = adapter.cast().expect("IDXGIAdapter");
            return Some(device_on(Some(&adapter), false, &desc.Description));
        }
    }
    println!("skipped: no NVIDIA GPU on this PC, so there is no NVENC to test");
    None
}

/// A device on the first hardware GPU of any vendor, so the Media Foundation
/// tests also run on an AMD or Intel PC.
pub fn hardware() -> Option<Gpu> {
    // SAFETY: plain DXGI calls; EnumAdapters1 fails past the last adapter.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }.expect("adapter description");
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            let adapter: IDXGIAdapter = adapter.cast().expect("IDXGIAdapter");
            return Some(device_on(Some(&adapter), false, &desc.Description));
        }
    }
    None
}

/// Windows' software rasterizer, a GPU no encoder is written for.
pub fn warp() -> Gpu {
    device_on(None, true, &[])
}

fn device_on(adapter: Option<&IDXGIAdapter>, warp: bool, description: &[u16]) -> Gpu {
    let mut device = None;
    let mut context = None;
    let driver = if warp {
        D3D_DRIVER_TYPE_WARP
    } else {
        D3D_DRIVER_TYPE_UNKNOWN
    };
    // SAFETY: the out pointers are valid for the call.
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .expect("D3D11 device");
    let len = description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(description.len());
    Gpu {
        device: device.expect("device"),
        context: context.expect("context"),
        name: String::from_utf16_lossy(&description[..len]),
    }
}

pub fn texture(gpu: &Gpu, width: u32, height: u32, format: DXGI_FORMAT) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // What the capture shader's output textures carry.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` is valid and the out pointer is valid for the call.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.expect("texture");
    texture.expect("texture")
}

fn staging(gpu: &Gpu, width: u32, height: u32) -> ID3D11Texture2D {
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
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: as in texture().
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .expect("staging texture");
    texture.expect("staging texture")
}

/// A pool of three NV12 textures, like the capture pool, filled with frame
/// after frame of the pattern.
pub struct Frames {
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    pool: Vec<(ID3D11Texture2D, ID3D11Texture2D)>,
    query: ID3D11Query,
    picture: Vec<u8>,
    next: usize,
}

impl Frames {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Frames {
        let pool = (0..3)
            .map(|_| {
                (
                    texture(gpu, width, height, DXGI_FORMAT_NV12),
                    staging(gpu, width, height),
                )
            })
            .collect();
        let mut query = None;
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        // SAFETY: valid description and out pointer.
        unsafe { gpu.device.CreateQuery(&desc, Some(&mut query)) }.expect("event query");
        Frames {
            context: gpu.context.clone(),
            width,
            height,
            pool,
            query: query.expect("event query"),
            picture: vec![0; (width * height * 3 / 2) as usize],
            next: 0,
        }
    }

    /// Frame `n` of the pattern in the next texture of the pool, already on
    /// the GPU, so the encoder's time does not include the upload.
    pub fn frame(&mut self, n: u64) -> ID3D11Texture2D {
        draw(n, self.width, self.height, &mut self.picture);
        let (input, staging) = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();

        let (w, h) = (self.width as usize, self.height as usize);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture is CPU-writable and not mapped yet.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
        }
        .expect("map");
        let pitch = mapped.RowPitch as usize;
        assert!(
            mapped.DepthPitch as usize >= pitch * h * 3 / 2,
            "the mapped NV12 texture is smaller than its two planes"
        );
        // SAFETY: an NV12 staging texture maps as `h` rows of luma then h / 2
        // rows of interleaved chroma, each `pitch` bytes apart, which the
        // assert above holds against the size the driver reports.
        let memory =
            unsafe { std::slice::from_raw_parts_mut(mapped.pData as *mut u8, pitch * h * 3 / 2) };
        for row in 0..h * 3 / 2 {
            memory[row * pitch..row * pitch + w]
                .copy_from_slice(&self.picture[row * w..(row + 1) * w]);
        }
        // SAFETY: mapped above.
        unsafe {
            self.context.Unmap(staging, 0);
            self.context.CopyResource(input, staging);
            self.context.End(&self.query);
        }
        let mut done = BOOL(0);
        while done != TRUE {
            // SAFETY: `done` is the BOOL an event query writes. Until the
            // GPU is done it returns S_FALSE and leaves it alone.
            unsafe {
                self.context.GetData(
                    &self.query,
                    Some(&mut done as *mut BOOL as *mut _),
                    size_of::<BOOL>() as u32,
                    0,
                )
            }
            .expect("event query");
        }
        input.clone()
    }
}

const SCROLL: usize = 256;
const CANVAS_W: usize = 2560 + SCROLL;
const CANVAS_H: usize = 1440 + SCROLL;
const BOX: (usize, usize) = (320, 180);
// A block of text scrolling up like a chat log, two new rows every frame.
const TEXT: (usize, usize) = (640, 360);
const TEXT_ROWS: usize = 4096;
// A block of faint noise, new every frame, like film grain in a game. It is
// what lets the rate control spend or save bits at any bitrate: the noise
// costs a lot at a low QP and next to nothing at a high one.
const NOISE: (usize, usize) = (640, 360);
const NOISE_CANVAS: (usize, usize) = (2048, 1024);
const NOISE_LEVELS: u64 = 13;

struct Canvas {
    luma: Vec<u8>,
    chroma: Vec<u8>,
    text: Vec<u8>,
    noise: Vec<u8>,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Text-like detail: 5x7 dot glyphs in 6x8 cells, bright on dark.
fn glyph(x: usize, y: usize, seed: u64) -> u8 {
    let (cx, cy, px, py) = (x / 6, y / 8, x % 6, y % 8);
    let bits = splitmix(seed ^ (((cy as u64) << 32) | cx as u64));
    if px < 5 && py < 7 && (bits >> (py * 5 + px)) & 1 == 1 {
        235
    } else {
        24
    }
}

fn canvas() -> &'static Canvas {
    static CANVAS: OnceLock<Canvas> = OnceLock::new();
    CANVAS.get_or_init(|| {
        let mut luma = vec![0; CANVAS_W * CANVAS_H];
        for y in 0..CANVAS_H {
            for x in 0..CANVAS_W {
                let t = (x + y) % 438;
                let triangle = if t < 219 { t } else { 437 - t };
                let mut v = (16 + triangle) as u8;
                if y % 96 >= 64 && y % 96 < 80 {
                    v = glyph(x, y, 1);
                }
                if x % 64 == 0 {
                    v = 235;
                }
                if y % 48 == 0 {
                    v = 16;
                }
                luma[y * CANVAS_W + x] = v;
            }
        }
        let mut chroma = vec![0; CANVAS_W * CANVAS_H / 2];
        for y in 0..CANVAS_H / 2 {
            for x in 0..CANVAS_W / 2 {
                chroma[y * CANVAS_W + 2 * x] = (16 + x * 224 / (CANVAS_W / 2)) as u8;
                chroma[y * CANVAS_W + 2 * x + 1] = (16 + y * 224 / (CANVAS_H / 2)) as u8;
            }
        }
        let mut text = vec![0; TEXT.0 * TEXT_ROWS];
        for y in 0..TEXT_ROWS {
            for x in 0..TEXT.0 {
                text[y * TEXT.0 + x] = glyph(x, y, 2);
            }
        }
        let noise = (0..NOISE_CANVAS.0 * NOISE_CANVAS.1)
            .map(|i| (128 - NOISE_LEVELS / 2 + splitmix(i as u64) % NOISE_LEVELS) as u8)
            .collect();
        Canvas {
            luma,
            chroma,
            text,
            noise,
        }
    })
}

/// Frame `n` of the moving pattern as NV12 (BT.709 limited range values): a
/// scrolling canvas of gradients, one-pixel lines and text-like rows, a box
/// sliding across, a block of text scrolling up, and faint noise that is new
/// every frame.
pub fn draw(n: u64, width: u32, height: u32, out: &mut [u8]) {
    let c = canvas();
    let (w, h) = (width as usize, height as usize);
    assert!(
        w <= 2560 && h <= 1440 && w >= 1280 && h >= 720,
        "the pattern covers 1280x720 to 2560x1440"
    );
    let n = n as usize;
    let dx = 2 * back_and_forth(n * 2, SCROLL / 2);
    let dy = 2 * back_and_forth(n, SCROLL / 2);
    let (luma, chroma) = out.split_at_mut(w * h);

    for y in 0..h {
        let src = (y + dy) * CANVAS_W + dx;
        luma[y * w..(y + 1) * w].copy_from_slice(&c.luma[src..src + w]);
    }
    for y in 0..h / 2 {
        let src = (y + dy / 2) * CANVAS_W + dx;
        chroma[y * w..(y + 1) * w].copy_from_slice(&c.chroma[src..src + w]);
    }

    let bx = back_and_forth(n * 16, w - BOX.0) & !1;
    let by = (h / 3) & !1;
    for y in by..by + BOX.1 {
        luma[y * w + bx..y * w + bx + BOX.0].fill(180);
    }
    for y in by / 2..(by + BOX.1) / 2 {
        for x in (bx..bx + BOX.0).step_by(2) {
            chroma[y * w + x] = 90;
            chroma[y * w + x + 1] = 200;
        }
    }

    let (tx, ty) = (w - TEXT.0 - 32, h - TEXT.1 - 32);
    let scrolled = (n * 2) % (TEXT_ROWS - TEXT.1);
    for y in 0..TEXT.1 {
        let src = (scrolled + y) * TEXT.0;
        luma[(ty + y) * w + tx..(ty + y) * w + tx + TEXT.0]
            .copy_from_slice(&c.text[src..src + TEXT.0]);
    }

    let (nx, ny) = (32, h - NOISE.1 - 32);
    let jump = splitmix(n as u64);
    let sx = (jump as usize) % (NOISE_CANVAS.0 - NOISE.0);
    let sy = ((jump >> 32) as usize) % (NOISE_CANVAS.1 - NOISE.1);
    for y in 0..NOISE.1 {
        let src = (sy + y) * NOISE_CANVAS.0 + sx;
        luma[(ny + y) * w + nx..(ny + y) * w + nx + NOISE.0]
            .copy_from_slice(&c.noise[src..src + NOISE.0]);
    }
}

/// Median, 95th percentile and maximum.
pub fn spread(values: &[f64]) -> (f64, f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95), sorted[sorted.len() - 1])
}

// Positions go back and forth rather than wrapping: a jump back to the start
// would be a scene cut, which no rate control fits into one frame.
fn back_and_forth(step: usize, range: usize) -> usize {
    let t = step % (2 * range);
    if t < range { t } else { 2 * range - 1 - t }
}

/// Which earlier frames each frame of a stream may predict from, worked out
/// from the slice headers alone the way a decoder builds reference list 0
/// (H.264 8.2.4 and 8.2.5). It shows what an encoder actually did about a
/// loss, where the frame type alone only shows that it still predicts.
pub struct References {
    sps: Option<Sps>,
    pps: Option<Pps>,
    // Short-term reference frames held: (frame index, frame_num).
    held: Vec<(u64, u32)>,
}

impl References {
    pub fn new() -> References {
        References {
            sps: None,
            pps: None,
            held: Vec::new(),
        }
    }

    /// Reads one access unit and returns the frames its first slice may
    /// predict from, newest first, and its slice header.
    pub fn frame(&mut self, index: u64, data: &[u8]) -> (Vec<u64>, SliceHeader) {
        for nal in annexb::nal_units(data) {
            match nal.kind() {
                annexb::NAL_SPS => self.sps = annexb::parse_sps(&nal),
                annexb::NAL_PPS => self.pps = annexb::parse_pps(&nal),
                _ => {}
            }
        }
        let sps = self.sps.clone().expect("an SPS before the first slice");
        let pps = self.pps.clone().expect("a PPS before the first slice");
        let nal = annexb::nal_units(data)
            .find(|n| n.is_slice())
            .expect("a slice in every frame");
        let header =
            annexb::slice_header(&nal, &sps, &pps).expect("a slice header this reader can read");
        let max_frame_num = 1i64 << sps.log2_max_frame_num;
        let current = i64::from(header.frame_num);
        // PicNum of a held frame: frame_num counted back from the current
        // one across the wrap.
        let pic_num = |frame_num: u32| {
            let n = i64::from(frame_num);
            if n > current { n - max_frame_num } else { n }
        };

        if nal.kind() == annexb::NAL_IDR {
            self.held = vec![(index, header.frame_num)];
            return (Vec::new(), header);
        }

        let mut list: Vec<(u64, u32)> = self.held.clone();
        list.sort_by_key(|&(_, n)| std::cmp::Reverse(pic_num(n)));
        let mut predicted = current;
        for (at, change) in header.list0_changes.iter().enumerate() {
            let no_wrap = match *change {
                ListChange::Down(d) => (predicted - i64::from(d)).rem_euclid(max_frame_num),
                ListChange::Up(d) => (predicted + i64::from(d)).rem_euclid(max_frame_num),
                ListChange::LongTerm(_) => panic!("frame {index} uses a long-term reference"),
            };
            predicted = no_wrap;
            let wanted = if no_wrap > current {
                no_wrap - max_frame_num
            } else {
                no_wrap
            };
            let from = list
                .iter()
                .position(|&(_, n)| pic_num(n) == wanted)
                .unwrap_or_else(|| panic!("frame {index} moves a picture it does not hold"));
            let picture = list.remove(from);
            list.insert(at.min(list.len()), picture);
        }
        list.truncate(header.num_ref_idx_l0_active as usize);
        let usable = list.iter().map(|&(i, _)| i).collect();

        if nal.ref_idc() != 0 {
            match &header.memory_ops {
                None => {
                    if self.held.len() >= sps.max_num_ref_frames as usize {
                        let oldest = (0..self.held.len())
                            .min_by_key(|&i| pic_num(self.held[i].1))
                            .unwrap_or(0);
                        self.held.remove(oldest);
                    }
                }
                Some(ops) => {
                    for op in ops {
                        let MemoryOp::ForgetShortTerm(d) = *op else {
                            panic!("frame {index} uses memory operation {op:?}");
                        };
                        let gone = current - i64::from(d);
                        self.held.retain(|&(_, n)| pic_num(n) != gone);
                    }
                }
            }
            self.held.push((index, header.frame_num));
        }
        (usable, header)
    }
}

/// What [`References`] does for H.264, for HEVC: which earlier frames each
/// frame may predict from, from the slice headers alone, as a decoder works
/// out picture order counts (HEVC 8.3.1), keeps the pictures a frame's
/// reference picture set names (8.3.2) and builds reference list 0 (8.3.4).
pub struct HevcReferences {
    sps: Option<hevc::Sps>,
    pps: Option<hevc::Pps>,
    // Every picture the decoder holds: (frame index, picture order count).
    held: Vec<(u64, i64)>,
    // The count of the last picture a later one counts its own from.
    previous: i64,
}

/// One frame as a decoder sees it.
pub struct HevcFrame {
    /// Reference list 0, newest first: what the frame may predict from.
    pub usable: Vec<u64>,
    /// Every frame the decoder keeps from this one on, this one included.
    pub kept: Vec<u64>,
    pub header: hevc::SliceHeader,
}

impl HevcReferences {
    pub fn new() -> HevcReferences {
        HevcReferences {
            sps: None,
            pps: None,
            held: Vec::new(),
            previous: 0,
        }
    }

    /// Reads one access unit. Panics where a decoder would find the stream
    /// broken: a reference it does not hold, or tools Booth's encoders do
    /// not use.
    pub fn frame(&mut self, index: u64, data: &[u8]) -> HevcFrame {
        for nal in annexb::nal_units(data) {
            match hevc::kind(&nal) {
                hevc::SPS => self.sps = hevc::parse_sps(&nal),
                hevc::PPS => self.pps = hevc::parse_pps(&nal),
                _ => {}
            }
        }
        let sps = self.sps.clone().expect("an SPS before the first slice");
        let pps = self.pps.clone().expect("a PPS before the first slice");
        let nal = annexb::nal_units(data)
            .find(hevc::is_slice)
            .expect("a slice in every frame");
        let header =
            hevc::slice_header(&nal, &sps, &pps).expect("a slice header this reader can read");
        assert_eq!(
            header.long_term_pics, 0,
            "frame {index} names long-term pictures"
        );

        let Some(set) = header.ref_pic_set.clone() else {
            self.held = vec![(index, 0)];
            self.previous = 0;
            return HevcFrame {
                usable: Vec::new(),
                kept: vec![index],
                header,
            };
        };

        // PicOrderCntMsb from the previous picture's count (8.3.1).
        let max_lsb = 1i64 << sps.log2_max_pic_order_cnt_lsb;
        let lsb = i64::from(header.pic_order_cnt_lsb);
        let previous_lsb = self.previous.rem_euclid(max_lsb);
        let mut msb = self.previous - previous_lsb;
        if lsb < previous_lsb && previous_lsb - lsb >= max_lsb / 2 {
            msb += max_lsb;
        } else if lsb > previous_lsb && lsb - previous_lsb > max_lsb / 2 {
            msb -= max_lsb;
        }
        let poc = msb + lsb;
        // Sub-layer non-reference pictures (the even types below 16) and
        // leading pictures do not move the count on; Booth's encoders make
        // neither.
        let kind = hevc::kind(&nal);
        let sub_layer_non_reference = kind < 16 && kind.is_multiple_of(2);
        if hevc::temporal_id(&nal) == 0 && !sub_layer_non_reference && !(6..=9).contains(&kind) {
            self.previous = poc;
        }

        let find = |delta: i32| {
            let wanted = poc + i64::from(delta);
            self.held
                .iter()
                .find(|&&(_, p)| p == wanted)
                .map(|&(i, _)| (i, wanted))
                .unwrap_or_else(|| {
                    panic!("frame {index} names picture order count {wanted}, which the decoder does not hold")
                })
        };
        let before: Vec<(u64, i64)> = set.before.iter().map(|&(d, _)| find(d)).collect();
        let after: Vec<(u64, i64)> = set.after.iter().map(|&(d, _)| find(d)).collect();
        let current: Vec<u64> = set
            .before
            .iter()
            .zip(&before)
            .chain(set.after.iter().zip(&after))
            .filter(|((_, used), _)| *used)
            .map(|(_, &(i, _))| i)
            .collect();

        let mut usable = Vec::new();
        if header.num_ref_idx_l0_active > 0 && !current.is_empty() {
            let active = header.num_ref_idx_l0_active as usize;
            let temp: Vec<u64> = current
                .iter()
                .copied()
                .cycle()
                .take(active.max(current.len()))
                .collect();
            for i in 0..active {
                let at = header.list0_entries.as_ref().map_or(i, |e| e[i] as usize);
                let frame = temp[at];
                if !usable.contains(&frame) {
                    usable.push(frame);
                }
            }
        }

        self.held = before.into_iter().chain(after).collect();
        self.held.push((index, poc));
        let kept = self.held.iter().map(|&(i, _)| i).collect();
        HevcFrame {
            usable,
            kept,
            header,
        }
    }
}
