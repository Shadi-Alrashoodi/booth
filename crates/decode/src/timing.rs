// How long the GPU takes over each picture. FFmpeg's call returns once the
// work is queued, and the video engine finishes the picture a couple of
// milliseconds later. Two timestamps on the device's immediate context, one
// just before FFmpeg gets the access unit and one after the picture is done,
// give that time without the CPU ever waiting: they are read with DONOTFLUSH
// on a later call, and a measurement that is not in after a lap of the ring
// is dropped.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BOX, D3D11_QUERY, D3D11_QUERY_DATA_TIMESTAMP_DISJOINT,
    D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP, D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Query,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

// Decodes a measurement may trail by before its queries are used again. At
// 120 fps one comes in a decode or two later; eight also cover a run of
// frames the reassembler lets go together while the GPU is busy.
const RING: usize = 8;

// Measurements kept for a caller that has not taken them.
const MOST_KEPT: usize = 64;

// One macroblock of the picture. Any copy out of the picture would wait for
// the video engine; this one costs next to nothing.
const CORNER: u32 = 16;

/// The GPU's time for one decoded picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuTime {
    /// The access unit it was, as [`crate::Decoded::unit`] numbers them.
    pub unit: u64,
    /// From just before FFmpeg got the access unit to the picture being
    /// finished, including any wait for the video engine.
    pub took: Duration,
    /// When the first timestamp went to the GPU, on this PC's clock. The
    /// GPU takes it at once unless it is busy with other work, so the
    /// picture was done at `began + took` or later.
    pub began: Instant,
}

pub(crate) struct Timing {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    lock: ID3D11Multithread,
    sets: Vec<QuerySet>,
    next: usize,
    // The set begun for the decode under way.
    open: Option<usize>,
    // The picture's format, and the texture its corner is copied into, or
    // None when Direct3D would not make one in that format.
    corner: Option<(DXGI_FORMAT, Option<ID3D11Texture2D>)>,
    done: VecDeque<GpuTime>,
}

struct QuerySet {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
    unit: u64,
    began: Instant,
    waiting: bool,
}

impl Timing {
    // None when Direct3D would not make the queries: the decoder still works
    // and reports no GPU times.
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        lock: &ID3D11Multithread,
    ) -> Option<Timing> {
        let query = |kind: D3D11_QUERY| {
            let desc = D3D11_QUERY_DESC {
                Query: kind,
                MiscFlags: 0,
            };
            let mut query = None;
            // SAFETY: a full description and a live out parameter.
            unsafe { device.CreateQuery(&desc, Some(&mut query)) }.ok()?;
            query
        };
        let mut sets = Vec::with_capacity(RING);
        for _ in 0..RING {
            sets.push(QuerySet {
                disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                begin: query(D3D11_QUERY_TIMESTAMP)?,
                end: query(D3D11_QUERY_TIMESTAMP)?,
                unit: 0,
                began: Instant::now(),
                waiting: false,
            });
        }
        Some(Timing {
            device: device.clone(),
            context: context.clone(),
            lock: lock.clone(),
            sets,
            next: 0,
            open: None,
            corner: None,
            done: VecDeque::with_capacity(MOST_KEPT),
        })
    }

    // Just before FFmpeg gets access unit `unit`.
    pub(crate) fn begin(&mut self, unit: u64) {
        let _locked = Locked::enter(&self.lock);
        self.poll();
        let slot = self.next;
        self.next = (self.next + 1) % self.sets.len();
        let set = &mut self.sets[slot];
        // A set still unread after a full lap is dropped, never waited for.
        set.waiting = false;
        set.unit = unit;
        // The flush sends the timestamp to the GPU now; it waits for
        // nothing. FFmpeg's decode goes to the video engine at once, but a
        // timestamp left in the context's buffer goes with the next flush,
        // after the picture is done, and the time read 0.003 ms at 1440p.
        //
        // It has a side effect, measured in the loopback on this PC's RTX
        // 4070 Ti SUPER: without it, the viewer's present returned only once
        // the video engine was done; with it, the present returns at once
        // and the GPU waits for the picture before it draws. Capture to
        // display, timed to the present returning, then left the decode out
        // (5.1 ms fell to 3.5 at 1440p), which is why the viewer times it to
        // the later of the present returning and began + took.
        //
        // SAFETY: the device's own immediate context and queries made on
        // the device; the lock keeps another thread's work from landing
        // between the calls.
        unsafe {
            self.context.Begin(&set.disjoint);
            self.context.End(&set.begin);
            self.context.Flush();
        }
        set.began = Instant::now();
        self.open = Some(slot);
    }

    // The picture from that access unit came back: slice `index` of
    // `texture`. The decode runs on the video engine, which a timestamp on
    // this context does not wait for, so the second timestamp would say
    // nothing on its own. A copy out of the picture has to wait until the
    // picture is done, and the timestamp after the copy lands then. The copy
    // stays on the GPU and is never read. The flush sends both now: the
    // viewer's present would a moment later, but a caller that does not
    // present would see the time stretch to its next flush (4.6 ms instead
    // of 1.7 at 1440p in tests/gpu_time.rs).
    pub(crate) fn finish(&mut self, texture: &ID3D11Texture2D, index: u32) {
        let Some(slot) = self.open.take() else {
            return;
        };
        let _locked = Locked::enter(&self.lock);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a getter on a live texture with a live out parameter.
        unsafe { texture.GetDesc(&mut desc) };
        let fits = index < desc.ArraySize && desc.Width >= CORNER && desc.Height >= CORNER;
        let corner = if fits { self.corner(desc.Format) } else { None };
        let set = &mut self.sets[slot];
        let Some(corner) = corner else {
            // SAFETY: closes the disjoint query begun in begin().
            unsafe { self.context.End(&set.disjoint) };
            return;
        };
        let area = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: CORNER,
            bottom: CORNER,
            back: 1,
        };
        // SAFETY: both textures are on this device and in the same format;
        // the slice is in the array and the box inside it, with even sides
        // as NV12 and P010 need. The queries were begun in begin().
        unsafe {
            self.context.CopySubresourceRegion(
                &corner,
                0,
                0,
                0,
                0,
                texture,
                index * desc.MipLevels.max(1),
                Some(&area),
            );
            self.context.End(&set.end);
            self.context.End(&set.disjoint);
            self.context.Flush();
        }
        set.waiting = true;
    }

    // The access unit gave no picture.
    pub(crate) fn abandon(&mut self) {
        let Some(slot) = self.open.take() else {
            return;
        };
        let _locked = Locked::enter(&self.lock);
        // SAFETY: closes the disjoint query begun in begin().
        unsafe { self.context.End(&self.sets[slot].disjoint) };
    }

    pub(crate) fn take(&mut self, into: &mut Vec<GpuTime>) {
        {
            let _locked = Locked::enter(&self.lock);
            self.poll();
        }
        into.extend(self.done.drain(..));
    }

    fn corner(&mut self, format: DXGI_FORMAT) -> Option<ID3D11Texture2D> {
        if let Some((made_for, corner)) = &self.corner
            && *made_for == format
        {
            return corner.clone();
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: CORNER,
            Height: CORNER,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: a full description and a live out parameter.
        let made = unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) };
        let corner = made.ok().and(texture);
        self.corner = Some((format, corner.clone()));
        corner
    }

    // Oldest first. The caller holds the lock.
    fn poll(&mut self) {
        for offset in 0..self.sets.len() {
            let slot = (self.next + offset) % self.sets.len();
            let set = &mut self.sets[slot];
            if !set.waiting {
                continue;
            }
            let Some(disjoint) =
                query_data::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(&self.context, &set.disjoint)
            else {
                continue;
            };
            let (Some(begin), Some(end)) = (
                query_data::<u64>(&self.context, &set.begin),
                query_data::<u64>(&self.context, &set.end),
            ) else {
                continue;
            };
            set.waiting = false;
            // A disjoint result means the GPU's clock changed speed in
            // between, and the ticks do not convert.
            if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || end < begin {
                continue;
            }
            let nanos = u128::from(end - begin) * 1_000_000_000 / u128::from(disjoint.Frequency);
            if self.done.len() == MOST_KEPT {
                self.done.pop_front();
            }
            self.done.push_back(GpuTime {
                unit: set.unit,
                took: Duration::from_nanos(nanos as u64),
                began: set.began,
            });
        }
    }
}

// The device's own multithread lock, held until dropped. It nests, so the
// protected calls made while it is held take it again without waiting. It
// keeps a reference of its own, so the timing's other fields stay free to
// change while it is held.
struct Locked(ID3D11Multithread);

impl Locked {
    fn enter(lock: &ID3D11Multithread) -> Locked {
        // SAFETY: a call on a live interface, undone by the drop below.
        unsafe { lock.Enter() };
        Locked(lock.clone())
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: entered in Locked::enter on this thread.
        unsafe { self.0.Leave() };
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
