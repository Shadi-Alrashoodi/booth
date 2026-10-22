// The pointer over a shared window, which Windows.Graphics.Capture leaves
// out of the pictures: where it is, from GetCursorInfo against what DWM
// composes of the window, and its shape read from the cursor handle, laid
// out as Desktop Duplication hands a monitor's pointer, so the room and the
// viewer take it as they are. It shows only while it is over the window
// itself, so a window share never says where the pointer is elsewhere.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, RGBQUAD, ReleaseDC,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GA_ROOT, GetAncestor, GetCursorInfo,
    GetIconInfo, HICON, ICONINFO, WindowFromPoint,
};

use crate::convert::Plan;
use crate::{CursorKind, CursorShape, CursorUpdate};

// How often the pointer is looked at while a window is shared: about a
// 240 Hz monitor's refresh, as Desktop Duplication reports a monitor's.
// Windows.Graphics.Capture says nothing when it moves.
pub(crate) const POINTER_EVERY: Duration = Duration::from_millis(4);
// Each update is one datagram, so the last one is said once more this long
// after it, and a hidden pointer again every HIDDEN_AGAIN: a lost one would
// leave a pointer drawn on a picture it left.
const AGAIN_AFTER: Duration = Duration::from_millis(100);
const HIDDEN_AGAIN: Duration = Duration::from_secs(1);
// The shape goes again for a new scale only once the scale held this long:
// each step of a window being dragged has its own, and shapes are big.
const SCALE_SETTLES: Duration = Duration::from_millis(200);

thread_local! {
    static PHYSICAL: Cell<bool> = const { Cell::new(false) };
}

// What GetCursorInfo and the window say this time round.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Seen {
    // The hot spot, in physical desktop pixels.
    at: (i32, i32),
    showing: bool,
    cursor: isize,
    // What DWM composes of the window, the pictures' rectangle: left, top,
    // right and bottom.
    bounds: Option<(i32, i32, i32, i32)>,
    // Nothing else is in front of the window at the pointer.
    over: bool,
}

fn seen(hwnd: isize) -> Seen {
    // Physical pixels whatever the process chose, as DWM's bounds are.
    if !PHYSICAL.replace(true) {
        // SAFETY: a constant context; this thread makes no windows.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }
    let mut info = CURSORINFO {
        cbSize: mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a live CURSORINFO with its size filled in. It fails on the
    // secure desktop, which reads as no pointer.
    let read = unsafe { GetCursorInfo(&mut info) }.is_ok();
    let showing = read
        && info.flags.0 & CURSOR_SHOWING.0 != 0
        && info.flags.0 & CURSOR_SUPPRESSED.0 == 0
        && !info.hCursor.is_invalid();
    let at = (info.ptScreenPos.x, info.ptScreenPos.y);
    let bounds = if showing { frame_bounds(hwnd) } else { None };
    let inside = bounds.is_some_and(|(left, top, right, bottom)| {
        (left..right).contains(&at.0) && (top..bottom).contains(&at.1)
    });
    // SAFETY: plain queries; another thread's window is never sent a
    // message here, so a program that hangs cannot hold this up.
    let over = inside
        && unsafe { GetAncestor(WindowFromPoint(POINT { x: at.0, y: at.1 }), GA_ROOT) }.0 as isize
            == hwnd;
    Seen {
        at,
        showing,
        cursor: info.hCursor.0 as isize,
        bounds,
        over,
    }
}

// What Windows composes of a window, without the shadow and the invisible
// borders: left, top, right and bottom.
pub(crate) fn frame_bounds(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    let mut bounds = RECT::default();
    // SAFETY: a live RECT of the size passed; a gone window fails.
    unsafe {
        DwmGetWindowAttribute(
            HWND(hwnd as *mut _),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut bounds as *mut RECT as *mut _,
            mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some((bounds.left, bounds.top, bounds.right, bounds.bottom))
}

// Where the pointer's image goes in the frame, its top left in frame pixels
// as Desktop Duplication gives a monitor's; None when it is hidden: not
// showing, not over the window, or over the black around a picture smaller
// than its frame.
fn place(seen: &Seen, plan: &Plan, hotspot: (i32, i32)) -> Option<(i32, i32)> {
    if !seen.showing || !seen.over {
        return None;
    }
    let (left, top, right, bottom) = seen.bounds?;
    if !(left..right).contains(&seen.at.0) || !(top..bottom).contains(&seen.at.1) {
        return None;
    }
    let (scale_x, scale_y) = plan.scale();
    let on_picture =
        |at: i32, start: i32, scale: f32, side: u32| (((at - start) as f32 * scale) as u32) < side;
    if !on_picture(seen.at.0, left, scale_x, plan.picture.0)
        || !on_picture(seen.at.1, top, scale_y, plan.picture.1)
    {
        return None;
    }
    Some((
        ((seen.at.0 - hotspot.0 - left) as f32 * scale_x).round() as i32,
        ((seen.at.1 - hotspot.1 - top) as f32 * scale_y).round() as i32,
    ))
}

// What went out last.
#[derive(Clone, Debug, PartialEq)]
struct Sent {
    x: i32,
    y: i32,
    visible: bool,
    // The shape the watchers have, and the scale it went with in the
    // thousandths the room sends.
    shape: Option<(CursorShape, u16)>,
}

fn milli(scale: f32) -> u16 {
    (scale * 1000.0).round().clamp(0.0, f32::from(u16::MAX)) as u16
}

// The pointer of one shared window, from one look to the next.
pub(crate) struct Pointer {
    hwnd: isize,
    due: Instant,
    // The handle read last and the shape it had, kept when a read fails so
    // a broken cursor costs no reads every look.
    read: Option<(isize, Option<CursorShape>)>,
    sent: Option<Sent>,
    again_at: Option<Instant>,
    // A scale the shape has not gone with yet, and since when.
    new_scale: Option<(u16, Instant)>,
}

impl Pointer {
    pub(crate) fn new(hwnd: isize) -> Pointer {
        Pointer {
            hwnd,
            due: Instant::now(),
            read: None,
            sent: None,
            again_at: None,
            new_scale: None,
        }
    }

    pub(crate) fn due(&self) -> Instant {
        self.due
    }

    // The update for the watchers, if anything changed since the last.
    pub(crate) fn poll(&mut self, plan: &Plan) -> Option<CursorUpdate> {
        let now = Instant::now();
        self.due = now + POINTER_EVERY;
        let seen = seen(self.hwnd);
        if seen.showing
            && self
                .read
                .as_ref()
                .is_none_or(|(handle, _)| *handle != seen.cursor)
        {
            let old = self.read.take().and_then(|(_, shape)| shape);
            self.read = Some((seen.cursor, shape(seen.cursor).or(old)));
        }
        let read = self.read.as_ref().and_then(|(_, shape)| shape.clone());
        let hotspot = read
            .as_ref()
            .map_or((0, 0), |shape| (shape.hotspot_x, shape.hotspot_y));
        let placed = place(&seen, plan, hotspot);
        self.update(placed, plan.scale().0, read, now)
    }

    // The pointer is not over the window any more, as when it is minimized.
    pub(crate) fn hide(&mut self) -> Option<CursorUpdate> {
        self.update(None, 1.0, None, Instant::now())
    }

    fn update(
        &mut self,
        placed: Option<(i32, i32)>,
        scale: f32,
        shape: Option<CursorShape>,
        now: Instant,
    ) -> Option<CursorUpdate> {
        let Some(sent) = &self.sent else {
            // The first update always goes, so a source opened again clears
            // a pointer the watchers still show.
            return Some(self.send(placed, scale, shape, now));
        };
        let again = self.again_at.is_some_and(|at| now >= at);
        let Some((x, y)) = placed else {
            return (sent.visible || again).then(|| self.send(None, scale, None, now));
        };
        let scale_milli = milli(scale);
        let had = sent.shape.as_ref();
        let new_shape = shape
            .as_ref()
            .is_some_and(|shape| had.is_none_or(|(had, _)| had != shape));
        let rescaled = had.is_some_and(|(_, milli)| *milli != scale_milli);
        let moved = (x, y, true) != (sent.x, sent.y, sent.visible);
        let settled = rescaled && {
            let since = match self.new_scale {
                Some((milli, since)) if milli == scale_milli => since,
                _ => now,
            };
            self.new_scale = Some((scale_milli, since));
            now.saturating_duration_since(since) >= SCALE_SETTLES
        };
        if !rescaled {
            self.new_scale = None;
        }
        if new_shape || settled {
            Some(self.send(placed, scale, shape, now))
        } else if moved || again {
            Some(self.send(placed, scale, None, now))
        } else {
            None
        }
    }

    // What goes out now, and what the watchers then have. A shape goes only
    // with a visible pointer.
    fn send(
        &mut self,
        placed: Option<(i32, i32)>,
        scale: f32,
        shape: Option<CursorShape>,
        now: Instant,
    ) -> CursorUpdate {
        let last = self.sent.take();
        let (x, y) = placed.unwrap_or(last.as_ref().map_or((0, 0), |sent| (sent.x, sent.y)));
        let shape = shape.filter(|_| placed.is_some());
        let repeat = last.as_ref().is_some_and(|sent| {
            (sent.x, sent.y, sent.visible) == (x, y, placed.is_some()) && shape.is_none()
        });
        self.again_at = match (repeat, placed) {
            (false, _) => Some(now + AGAIN_AFTER),
            (true, None) => Some(now + HIDDEN_AGAIN),
            (true, Some(_)) => None,
        };
        if shape.is_some() {
            self.new_scale = None;
        }
        let kept = match &shape {
            Some(shape) => Some((shape.clone(), milli(scale))),
            None => last.and_then(|sent| sent.shape),
        };
        self.sent = Some(Sent {
            x,
            y,
            visible: placed.is_some(),
            shape: kept,
        });
        CursorUpdate {
            x,
            y,
            visible: placed.is_some(),
            scale,
            shape,
        }
    }
}

// Wider or taller than this is no pointer Windows draws, and more than the
// room sends.
const LARGEST_SIDE: u32 = 256;

// The shape of the cursor `cursor` (an HCURSOR as a number), or None for one
// that is gone or not a pointer the room can send.
pub(crate) fn shape(cursor: isize) -> Option<CursorShape> {
    let mut info = ICONINFO::default();
    // SAFETY: a handle Windows reported; one that is gone fails. Fills a
    // live ICONINFO.
    unsafe { GetIconInfo(HICON(cursor as *mut c_void), &mut info) }.ok()?;
    // GetIconInfo makes copies of both bitmaps, which are ours to delete.
    let bitmaps = Bitmaps(info.hbmMask, info.hbmColor);
    let (width, rows) = size_of_bitmap(bitmaps.0)?;
    if width == 0 || width > LARGEST_SIDE || rows == 0 || rows > 2 * LARGEST_SIDE {
        return None;
    }
    let dc = ScreenDc::get()?;
    let mask = read_bits(&dc, bitmaps.0, width, rows, 1)?;
    let color = if bitmaps.1.is_invalid() {
        None
    } else {
        // A colour bitmap of another size than its mask is no cursor
        // Windows draws either.
        if size_of_bitmap(bitmaps.1)? != (width, rows) {
            return None;
        }
        Some(read_bits(&dc, bitmaps.1, width, rows, 32)?)
    };
    let hotspot = (!info.fIcon.as_bool()).then_some((info.xHotspot, info.yHotspot));
    shape_from_bits(width, rows, &mask, color.as_deref(), hotspot)
}

// The shape from a cursor's bits: `mask` is `rows` rows of 1 bit a pixel,
// each row whole 32-bit words, a 1 keeping the screen; `color`, when there
// is a colour bitmap, the same rows of 32-bit BGRA. A cursor with no colour
// bitmap is an AND mask over an XOR mask of the same size, as Desktop
// Duplication's monochrome pointers are. `hotspot` is None for an icon,
// whose hot spot is its centre.
fn shape_from_bits(
    width: u32,
    rows: u32,
    mask: &[u8],
    color: Option<&[u8]>,
    hotspot: Option<(u32, u32)>,
) -> Option<CursorShape> {
    let mask_pitch = width.div_ceil(32) * 4;
    if width == 0 || mask.len() != (mask_pitch * rows) as usize {
        return None;
    }
    let (kind, pitch, bytes, tall) = match color {
        None => {
            if rows < 2 || !rows.is_multiple_of(2) {
                return None;
            }
            (CursorKind::Monochrome, mask_pitch, mask.to_vec(), rows / 2)
        }
        Some(color) => {
            if color.len() != (width * 4 * rows) as usize {
                return None;
            }
            if color.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0) {
                (CursorKind::Color, width * 4, color.to_vec(), rows)
            } else {
                // Drawn as the screen ANDed with the mask, then XORed with
                // the colour: where the mask keeps the screen the colour is
                // XORed onto it, elsewhere it replaces it.
                let mut bytes = color.to_vec();
                for (index, pixel) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (index as u32 % width, index as u32 / width);
                    let byte = mask[(y * mask_pitch + x / 8) as usize];
                    pixel[3] = if byte & (0x80 >> (x % 8)) != 0 {
                        0xff
                    } else {
                        0
                    };
                }
                (CursorKind::MaskedColor, width * 4, bytes, rows)
            }
        }
    };
    if tall > LARGEST_SIDE {
        return None;
    }
    let (hotspot_x, hotspot_y) = hotspot.unwrap_or((width / 2, tall / 2));
    Some(CursorShape {
        kind,
        width,
        height: rows,
        pitch,
        hotspot_x: hotspot_x.min(width - 1) as i32,
        hotspot_y: hotspot_y.min(tall - 1) as i32,
        bytes,
    })
}

fn size_of_bitmap(bitmap: HBITMAP) -> Option<(u32, u32)> {
    let mut info = BITMAP::default();
    // SAFETY: a bitmap of ours and a live BITMAP of the size passed.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some(&mut info as *mut BITMAP as *mut c_void),
        )
    };
    if got == 0 {
        return None;
    }
    Some((
        u32::try_from(info.bmWidth).ok()?,
        u32::try_from(info.bmHeight).ok()?,
    ))
}

// A BITMAPINFO with room for the two colours GDI writes for 1 bit a pixel:
// the windows crate's own has room for one.
#[repr(C)]
struct Dib {
    header: BITMAPINFOHEADER,
    colors: [RGBQUAD; 2],
}

// `rows` rows of `bitmap` top down at `bits` a pixel, each row whole 32-bit
// words. For 1 bit a pixel a 1 is the colour table's second entry, which
// GDI makes white, so a 1 keeps the screen in a mask.
fn read_bits(dc: &ScreenDc, bitmap: HBITMAP, width: u32, rows: u32, bits: u16) -> Option<Vec<u8>> {
    let mut dib = Dib {
        header: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(rows as i32),
            biPlanes: 1,
            biBitCount: bits,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        colors: [RGBQUAD::default(); 2],
    };
    let pitch = (width * u32::from(bits)).div_ceil(32) * 4;
    let mut bytes = vec![0u8; (pitch * rows) as usize];
    // SAFETY: a bitmap of ours that no DC has selected, a buffer of `rows`
    // rows of `pitch` bytes, and a header with room for its colour table.
    let lines = unsafe {
        GetDIBits(
            dc.0,
            bitmap,
            0,
            rows,
            Some(bytes.as_mut_ptr() as *mut c_void),
            &mut dib as *mut Dib as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        )
    };
    if lines != rows as i32 {
        return None;
    }
    let white =
        |color: RGBQUAD| color.rgbRed == 255 && color.rgbGreen == 255 && color.rgbBlue == 255;
    if bits == 1 && white(dib.colors[0]) {
        bytes.iter_mut().for_each(|byte| *byte = !*byte);
    }
    Some(bytes)
}

struct Bitmaps(HBITMAP, HBITMAP);

impl Drop for Bitmaps {
    fn drop(&mut self) {
        for bitmap in [self.0, self.1] {
            if !bitmap.is_invalid() {
                // SAFETY: a bitmap GetIconInfo made for us, deleted once.
                let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
            }
        }
    }
}

// The screen's device context, which GetDIBits reads through.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Option<ScreenDc> {
        // SAFETY: no window: the whole screen's DC, released in Drop.
        let dc = unsafe { GetDC(None) };
        (!dc.is_invalid()).then_some(ScreenDc(dc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: the DC GetDC gave, released once.
        unsafe { ReleaseDC(None, self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use windows::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection};
    use windows::Win32::System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, IDC_ARROW, IDC_IBEAM, LoadCursorW,
    };

    use super::*;

    // GDI's object count is the whole process's, so the tests that make
    // bitmaps take turns.
    static GDI: Mutex<()> = Mutex::new(());

    fn seen_at(at: (i32, i32), bounds: (i32, i32, i32, i32)) -> Seen {
        Seen {
            at,
            showing: true,
            cursor: 1,
            bounds: Some(bounds),
            over: true,
        }
    }

    // As Desktop Duplication places a monitor's pointer: the image's top
    // left in frame pixels, the hot spot inside the window's bounds.
    #[test]
    fn placed_like_a_monitors_pointer() {
        let plan = Plan::new(640, 360, crate::Rotation::Identity, 4096, 1440);
        let bounds = (100, 50, 740, 410);
        let place_at = |at, hotspot| place(&seen_at(at, bounds), &plan, hotspot);
        assert_eq!(place_at((300, 150), (0, 0)), Some((200, 100)));
        assert_eq!(place_at((100, 50), (0, 0)), Some((0, 0)));
        assert_eq!(place_at((739, 409), (0, 0)), Some((639, 359)));
        assert_eq!(place_at((102, 52), (4, 4)), Some((-2, -2)));
        for outside in [(740, 150), (300, 410), (99, 150), (300, 49)] {
            assert_eq!(place_at(outside, (0, 0)), None, "{outside:?}");
        }
        let hidden = Seen {
            showing: false,
            ..seen_at((300, 150), bounds)
        };
        let covered = Seen {
            over: false,
            ..seen_at((300, 150), bounds)
        };
        let gone = Seen {
            bounds: None,
            ..seen_at((300, 150), bounds)
        };
        for seen in [hidden, covered, gone] {
            assert_eq!(place(&seen, &plan, (0, 0)), None, "{seen:?}");
        }
        // Scaled into the box, and kept in a frame of another size.
        let scaled = Plan::new(1280, 720, crate::Rotation::Identity, 640, 360);
        assert_eq!(
            place(&seen_at((400, 200), (0, 0, 1280, 720)), &scaled, (2, 2)),
            Some((199, 99))
        );
        let framed = Plan::new(1000, 600, crate::Rotation::Identity, 4096, 1440).framed(
            4096,
            1440,
            (800, 480),
        );
        assert_eq!(
            place(&seen_at((500, 300), (0, 0, 1000, 600)), &framed, (0, 0)),
            Some((400, 240))
        );
    }

    fn arrow() -> CursorShape {
        CursorShape {
            kind: CursorKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![255; 16],
        }
    }

    // Updates go on change only, each said once more a moment later, a
    // hidden pointer again every second, a shape only when it is new or its
    // scale has held a while.
    #[test]
    fn updates_go_on_change() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pointer = Pointer::new(0);
        let first = pointer.update(None, 1.0, None, at(0)).unwrap();
        assert!(!first.visible && first.shape.is_none());
        assert_eq!(pointer.update(None, 1.0, None, at(50)), None);
        assert!(pointer.update(None, 1.0, None, at(100)).is_some());
        assert_eq!(pointer.update(None, 1.0, None, at(500)), None);
        assert!(pointer.update(None, 1.0, None, at(1100)).is_some());

        let shown = pointer
            .update(Some((10, 20)), 1.0, Some(arrow()), at(2000))
            .unwrap();
        assert!(shown.visible && shown.shape == Some(arrow()));
        assert_eq!(
            pointer.update(Some((10, 20)), 1.0, Some(arrow()), at(2050)),
            None
        );
        let again = pointer
            .update(Some((10, 20)), 1.0, Some(arrow()), at(2100))
            .unwrap();
        assert!(again.visible && again.shape.is_none());
        assert_eq!(
            pointer.update(Some((10, 20)), 1.0, Some(arrow()), at(3000)),
            None
        );
        let moved = pointer
            .update(Some((11, 20)), 1.0, Some(arrow()), at(3010))
            .unwrap();
        assert_eq!((moved.x, moved.y, moved.shape), (11, 20, None));
        // The same bitmap with another hot spot is another shape.
        let other = CursorShape {
            hotspot_x: 1,
            ..arrow()
        };
        let reshaped = pointer.update(Some((11, 20)), 1.0, Some(other.clone()), at(3020));
        assert_eq!(
            reshaped.and_then(|update| update.shape),
            Some(other.clone())
        );
        let repeat = pointer.update(Some((11, 20)), 1.0, Some(other.clone()), at(3150));
        assert_eq!(repeat.map(|update| update.shape), Some(None));
        // A new scale sends the shape again once it held a while.
        assert_eq!(
            pointer.update(Some((11, 20)), 0.5, Some(other.clone()), at(3200)),
            None
        );
        assert_eq!(
            pointer.update(Some((11, 20)), 0.5, Some(other.clone()), at(3399)),
            None
        );
        let rescaled = pointer
            .update(Some((11, 20)), 0.5, Some(other.clone()), at(3400))
            .unwrap();
        assert_eq!((rescaled.scale, rescaled.shape), (0.5, Some(other)));
        // Hidden: once, at the last place shown, never where it is now.
        let hidden = pointer.update(None, 0.5, None, at(4000)).unwrap();
        assert_eq!((hidden.x, hidden.y, hidden.visible), (11, 20, false));
        assert_eq!(pointer.update(None, 0.5, None, at(4010)), None);
    }

    // One look at the pointer is a few system calls, done up to 250 times a
    // second while someone watches a window.
    #[test]
    fn a_look_is_cheap() {
        // SAFETY: a plain query.
        let desktop = unsafe { windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow() };
        let mut took: Vec<Duration> = (0..1000)
            .map(|_| {
                let start = Instant::now();
                seen(desktop.0 as isize);
                start.elapsed()
            })
            .collect();
        took.sort();
        let (median, p99) = (took[500], took[990]);
        println!("a look at the pointer: median {median:?}, 99th {p99:?}");
        assert!(median < Duration::from_millis(1), "{median:?}");
    }

    // A 32-wide pointer: one 32-bit word a row.
    fn mask_rows(rows: &[u32]) -> Vec<u8> {
        rows.iter().flat_map(|row| row.to_be_bytes()).collect()
    }

    #[test]
    fn monochrome_bits_pass_through() {
        let mask = mask_rows(&[0xffff_0000; 64]);
        let shape = shape_from_bits(32, 64, &mask, None, Some((3, 5))).unwrap();
        assert_eq!(shape.kind, CursorKind::Monochrome);
        assert_eq!((shape.width, shape.height, shape.pitch), (32, 64, 4));
        assert_eq!((shape.hotspot_x, shape.hotspot_y), (3, 5));
        assert_eq!(shape.bytes, mask);
        // 20 wide is still a whole word a row.
        let narrow = shape_from_bits(20, 40, &[0; 160], None, None).unwrap();
        assert_eq!(
            (narrow.pitch, narrow.hotspot_x, narrow.hotspot_y),
            (4, 10, 10)
        );
        // The hot spot is on the picture: half the mask's height.
        let clamped = shape_from_bits(32, 64, &mask, None, Some((3, 40))).unwrap();
        assert_eq!((clamped.hotspot_x, clamped.hotspot_y), (3, 31));
    }

    #[test]
    fn colour_with_alpha_is_kept_and_without_it_takes_the_mask() {
        let mut color = vec![0u8; 32 * 32 * 4];
        color[..4].copy_from_slice(&[50, 100, 200, 128]);
        let mask = mask_rows(&[0x8000_0000; 32]);
        let shape = shape_from_bits(32, 32, &mask, Some(&color), Some((40, 40))).unwrap();
        assert_eq!(shape.kind, CursorKind::Color);
        assert_eq!((shape.pitch, shape.height), (128, 32));
        assert_eq!((shape.hotspot_x, shape.hotspot_y), (31, 31));
        assert_eq!(shape.bytes, color);

        let mut opaque = vec![0u8; 32 * 32 * 4];
        opaque[..4].copy_from_slice(&[50, 100, 200, 0]);
        let shape = shape_from_bits(32, 32, &mask, Some(&opaque), None).unwrap();
        assert_eq!(shape.kind, CursorKind::MaskedColor);
        // The first pixel of each row keeps the screen and is XORed; the
        // rest replace it.
        assert_eq!(&shape.bytes[..8], &[50, 100, 200, 0xff, 0, 0, 0, 0]);
        assert_eq!(shape.bytes[128 + 3], 0xff);
        assert_eq!(shape.bytes[128 + 7], 0);
    }

    #[test]
    fn broken_bits_are_no_shape() {
        let mask = mask_rows(&[0; 64]);
        assert!(shape_from_bits(0, 64, &[], None, None).is_none());
        assert!(shape_from_bits(32, 63, &mask[..252], None, None).is_none());
        assert!(shape_from_bits(32, 64, &mask[..200], None, None).is_none());
        let color = vec![1u8; 32 * 31 * 4];
        assert!(shape_from_bits(32, 32, &mask[..128], Some(&color), None).is_none());
        let tall = mask_rows(&[0; 600]);
        assert!(shape_from_bits(32, 600, &tall, None, None).is_none());
    }

    // An icon made from known bitmaps, given to `test` and destroyed after.
    fn with_cursor(mask: HBITMAP, color: HBITMAP, hotspot: (u32, u32), test: impl FnOnce(isize)) {
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: hotspot.0,
            yHotspot: hotspot.1,
            hbmMask: mask,
            hbmColor: color,
        };
        // SAFETY: two live bitmaps of ours, which the call copies.
        let icon = unsafe { CreateIconIndirect(&info) }.unwrap();
        test(icon.0 as isize);
        // SAFETY: the icon and the bitmaps made here, each freed once.
        unsafe {
            let _ = DestroyIcon(icon);
            for bitmap in [mask, color] {
                if !bitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                }
            }
        }
    }

    fn mono_bitmap(width: i32, height: i32, bits: &[u8]) -> HBITMAP {
        // SAFETY: `bits` holds height rows of whole 16-bit words, which is
        // what CreateBitmap reads for 1 bit a pixel.
        unsafe { CreateBitmap(width, height, 1, 1, Some(bits.as_ptr() as *const c_void)) }
    }

    fn color_bitmap(width: i32, height: i32, pixels: &[u8]) -> HBITMAP {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: a full header for a top-down 32-bit section; `bits` is a
        // live out parameter.
        let bitmap =
            unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.unwrap();
        // SAFETY: the section has width * height 32-bit pixels at `bits`.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
        }
        bitmap
    }

    #[test]
    fn cursors_read_back_as_made() {
        let _turn = GDI.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mask = mask_rows(&(0..64).map(|row| 0x0f0f_0000 | row).collect::<Vec<_>>());
        with_cursor(
            mono_bitmap(32, 64, &mask),
            HBITMAP::default(),
            (3, 5),
            |cursor| {
                let shape = shape(cursor).unwrap();
                assert_eq!(shape.kind, CursorKind::Monochrome);
                assert_eq!((shape.width, shape.height), (32, 64));
                assert_eq!((shape.hotspot_x, shape.hotspot_y), (3, 5));
                assert_eq!(shape.bytes, mask);
            },
        );

        let mut pixels = vec![0u8; 32 * 32 * 4];
        pixels[..4].copy_from_slice(&[50, 100, 200, 128]);
        let empty = mask_rows(&[0; 32]);
        with_cursor(
            mono_bitmap(32, 32, &empty),
            color_bitmap(32, 32, &pixels),
            (0, 0),
            |cursor| {
                let shape = shape(cursor).unwrap();
                assert_eq!(shape.kind, CursorKind::Color);
                // Straight alpha, as the viewer blends it.
                assert_eq!(&shape.bytes[..4], &[50, 100, 200, 128]);
            },
        );

        let mut opaque = vec![0u8; 32 * 32 * 4];
        opaque[..4].copy_from_slice(&[50, 100, 200, 0]);
        let keep_first = mask_rows(&[0x8000_0000; 32]);
        with_cursor(
            mono_bitmap(32, 32, &keep_first),
            color_bitmap(32, 32, &opaque),
            (1, 1),
            |cursor| {
                let shape = shape(cursor).unwrap();
                assert_eq!(shape.kind, CursorKind::MaskedColor);
                assert_eq!(&shape.bytes[..8], &[50, 100, 200, 0xff, 0, 0, 0, 0]);
            },
        );
    }

    #[test]
    fn system_cursors_read() {
        let _turn = GDI.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (name, id) in [("arrow", IDC_ARROW), ("I-beam", IDC_IBEAM)] {
            // SAFETY: a system cursor, shared and never freed.
            let cursor = unsafe { LoadCursorW(None, id) }.unwrap();
            let shape = shape(cursor.0 as isize).unwrap();
            println!(
                "{name}: {:?} {}x{}, hot spot {},{}",
                shape.kind, shape.width, shape.height, shape.hotspot_x, shape.hotspot_y
            );
            let tall = match shape.kind {
                CursorKind::Monochrome => shape.height / 2,
                _ => shape.height,
            };
            assert!((16..=LARGEST_SIDE).contains(&shape.width), "{name}");
            assert!((16..=LARGEST_SIDE).contains(&tall), "{name}");
            assert!((shape.hotspot_x as u32) < shape.width && (shape.hotspot_y as u32) < tall);
            assert_eq!(
                (shape.pitch * shape.height) as usize,
                shape.bytes.len(),
                "{name}"
            );
        }
    }

    #[test]
    fn reading_shapes_leaks_nothing() {
        let _turn = GDI.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: a system cursor, shared and never freed.
        let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap().0 as isize;
        // SAFETY: plain queries on this process.
        let count = || unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        shape(cursor);
        let before = count();
        for _ in 0..200 {
            shape(cursor).unwrap();
        }
        let after = count();
        assert!(
            after <= before + 8,
            "GDI objects {before} before, {after} after"
        );
    }
}
