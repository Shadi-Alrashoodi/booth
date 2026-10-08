use eframe::egui::emath::GuiRounding;
use eframe::egui::{
    Color32, CursorIcon, Mesh, Painter, Rect, Response, Sense, Ui, WidgetInfo, WidgetType, pos2,
    vec2,
};
use room::view::{LinkState, PathWord, Strip, TracePoint};
use stats::Thresholds;

use crate::controls;
use crate::theme::{self, ASH, BAD, SAGE, SIDE, WARN};

pub const HEIGHT: f32 = 24.0;

const TRACE_SAMPLES: usize = 120;
const TRACE_HEIGHT: f32 = 16.0;
const TRACE_TOP_MS: f32 = 100.0;
// Between words, the numbers in their slots.
const GAP: f32 = 12.0;

// Counts every sample the trace has ever shown, so that with Windows
// animations off each one can be written at a fixed column (total mod 120)
// instead of the whole line moving left.
#[derive(Default)]
pub struct Sweep {
    total: u64,
    last: Vec<TracePoint>,
}

impl Sweep {
    pub fn update(&mut self, trace: &[TracePoint]) {
        self.total += appended(&self.last, trace) as u64;
        self.last.clear();
        self.last.extend_from_slice(trace);
    }
}

// The trace is a sliding window: the new one is the old one with some samples
// gone from the front and the same number or more added at the back. The
// smallest shift that lines the two up is the right one unless the window is
// periodic, and then every shift draws the same picture anyway.
fn appended(old: &[TracePoint], new: &[TracePoint]) -> usize {
    for shift in 0..=old.len() {
        let kept = &old[shift..];
        if kept.len() <= new.len() && new[..kept.len()] == *kept {
            return new.len() - kept.len();
        }
    }
    new.len()
}

pub fn show(ui: &mut Ui, strip: Option<&Strip>, sweep: &Sweep, scrolling: bool) -> Response {
    // With nobody connected a click still opens the stats, but the strip is
    // not a Tab stop: a focus ring around nothing reads as broken.
    let sense = match strip.map(|strip| strip.state) {
        None => Sense::hover(),
        Some(LinkState::Alone | LinkState::Closed) => Sense::CLICK,
        Some(_) => Sense::click(),
    };
    let (rect, mut response) = ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), sense);
    let Some(strip) = strip else {
        return response;
    };
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Stats"));
    response = response.on_hover_cursor(CursorIcon::PointingHand);

    let painter = ui.painter_at(rect);
    let slot = trace_slot(rect);

    // The window's edge would cut a ring outside the strip, so it moves in
    // onto the strip's own edge, clear of the trace slot, and is painted
    // first so the data wins wherever rounding makes the two meet.
    if response.has_focus() && controls::keyboard_focus(ui) {
        controls::ring_within(&painter, rect, rect);
    }

    let words = words(strip);
    let widths: Vec<(Part, f32)> = words
        .iter()
        .map(|word| (word.part, slot_width(&painter, word)))
        .collect();
    let kept = fit(&widths, slot.left() - GAP - (rect.left() + SIDE));
    let mut x = rect.left() + SIDE;
    for (word, (_, width)) in words.into_iter().zip(widths) {
        if !kept.contains(&word.part) {
            continue;
        }
        let galley = painter.layout_no_wrap(word.text, theme::mono_caption(), word.color);
        // Every word starts at the left of its slot, so each reading has a
        // fixed place and the first sits on the gutter; a short value leaves
        // the rest of its slot empty after it.
        let y = (rect.center().y - galley.size().y / 2.0).round();
        painter.galley(pos2(x, y), galley, word.color);
        x += width + GAP;
    }

    let mut pixels = Pixels::new(&painter);
    let empty = match strip.state {
        LinkState::Live | LinkState::Reconnecting => {
            empty_columns(strip.trace.len(), sweep.total, scrolling)
        }
        LinkState::Connecting | LinkState::Lost | LinkState::Alone | LinkState::Closed => {
            [true; TRACE_SAMPLES]
        }
    };
    draw_baseline(&mut pixels, slot, &empty);
    match strip.state {
        LinkState::Live => draw_trace(&mut pixels, slot, &strip.trace, sweep, scrolling, None),
        LinkState::Reconnecting => {
            draw_trace(&mut pixels, slot, &strip.trace, sweep, scrolling, Some(ASH))
        }
        LinkState::Connecting | LinkState::Lost | LinkState::Alone | LinkState::Closed => {}
    }
    pixels.paint(&painter);
    response
}

// The fixed 120 by 16 px drawing area at the right edge, centred in the
// 24 px band, which leaves four rows above and below it for the focus ring.
// The bottom row, drawn down from its own top (row_span), can reach a device
// pixel into the four under it, still clear of the ring.
// It is placed from the band's top, which is on a whole device pixel, and not
// rounded in points: at 150 percent that rounding would push the bottom row,
// the one a good link draws on, half a pixel into the ring.
fn trace_slot(rect: Rect) -> Rect {
    Rect::from_min_size(
        pos2(
            rect.right() - SIDE - TRACE_SAMPLES as f32,
            rect.top() + (HEIGHT - TRACE_HEIGHT) / 2.0,
        ),
        vec2(TRACE_SAMPLES as f32, TRACE_HEIGHT),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    State,
    RoundTrip,
    Jitter,
    Loss,
    Path,
}

impl Part {
    // The widest value each number normally takes. Its slot is that wide
    // whatever it shows, so going from <1 ms to 1 ms, or from 9 to 10, does
    // not push the words after it along. The path word's slot is as wide as
    // "relayed", the longest it will be once a relay comes, so LAN and direct
    // leave whatever follows them in the same place, as in the viewer.
    fn widest(self) -> &'static [&'static str] {
        match self {
            Part::RoundTrip => &["100 ms", "<1 ms"],
            Part::Jitter => &["\u{b1}99"],
            Part::Loss => &["10.0%"],
            Part::Path => &["relayed"],
            Part::State => &[],
        }
    }
}

// What goes first when the strip is too narrow for everything. The state
// word and the path word always stay: the strip is the one place state
// shows, and "relayed" is the reason a link is slow.
const DROP_ORDER: [Part; 3] = [Part::Jitter, Part::Loss, Part::RoundTrip];

struct Word {
    part: Part,
    text: String,
    color: Color32,
    // Whether the word gets the room its widest value needs. A live number
    // does, and the path word always does. The last numbers shown while
    // reconnecting do not: the round trip and jitter stay as they were until
    // a packet comes back, only loss moves, and at the narrowest window that
    // room is what keeps a number on screen at all.
    slot: bool,
}

fn slot_width(painter: &Painter, word: &Word) -> f32 {
    let measure = |text: &str| {
        painter
            .layout_no_wrap(text.to_owned(), theme::mono_caption(), word.color)
            .size()
            .x
    };
    let widest = if word.slot { word.part.widest() } else { &[] };
    widest
        .iter()
        .map(|text| measure(text))
        .fold(measure(&word.text), f32::max)
        .ceil()
}

fn fit(widths: &[(Part, f32)], room: f32) -> Vec<Part> {
    let mut kept: Vec<Part> = widths.iter().map(|(part, _)| *part).collect();
    let used = |kept: &[Part]| {
        let shown: Vec<f32> = widths
            .iter()
            .filter(|(part, _)| kept.contains(part))
            .map(|(_, width)| *width)
            .collect();
        shown.iter().sum::<f32>() + GAP * shown.len().saturating_sub(1) as f32
    };
    for part in DROP_ORDER {
        if used(&kept) <= room {
            break;
        }
        kept.retain(|kept| *kept != part);
    }
    kept
}

fn words(strip: &Strip) -> Vec<Word> {
    let mut words = Vec::new();
    let mut push = |part, text: String, color, slot| {
        words.push(Word {
            part,
            text,
            color,
            slot,
        })
    };
    let stale = match strip.state {
        LinkState::Alone => return words,
        LinkState::Closed => {
            push(Part::State, String::from("closed"), ASH, false);
            return words;
        }
        LinkState::Connecting => {
            push(Part::State, String::from("connecting"), ASH, false);
            return words;
        }
        LinkState::Lost => {
            push(Part::State, String::from("lost"), BAD, false);
            return words;
        }
        LinkState::Reconnecting => {
            push(Part::State, String::from("reconnecting"), WARN, false);
            true
        }
        LinkState::Live => false,
    };
    let color = |level| {
        if stale {
            ASH
        } else {
            theme::level_color(level)
        }
    };
    if let Some(ms) = strip.rtt_ms {
        push(
            Part::RoundTrip,
            round_trip(ms),
            color(strip.rtt_level),
            !stale,
        );
    }
    if let Some(ms) = strip.jitter_ms {
        push(
            Part::Jitter,
            format!("\u{b1}{ms:.0}"),
            color(strip.jitter_level),
            !stale,
        );
    }
    if let Some(pct) = strip.loss_pct {
        push(
            Part::Loss,
            format!("{pct:.1}%"),
            color(strip.loss_level),
            !stale,
        );
    }
    // A word, not a reading: colour in the strip is for the numbers, and a
    // sage word beside them gives them nothing to stand out against.
    if let Some(path) = strip.path {
        push(Part::Path, path_word(path).to_owned(), ASH, true);
    }
    words
}

pub fn round_trip(ms: f32) -> String {
    if ms < 0.5 {
        String::from("<1 ms")
    } else {
        format!("{ms:.0} ms")
    }
}

pub fn path_word(path: PathWord) -> &'static str {
    match path {
        PathWord::Lan => "LAN",
        PathWord::Direct => "direct",
    }
}

// Where sample `i` of the `count` shown goes in the slot. Scrolling, the
// newest is always at the right edge. With Windows animations off each sample
// keeps the column it was first drawn in and the newest overwrites the
// oldest, so nothing on screen moves.
fn column(i: usize, count: usize, total: u64, scrolling: bool) -> usize {
    if scrolling {
        TRACE_SAMPLES - count + i
    } else {
        let first = total.saturating_sub(count as u64);
        ((first + i as u64) % TRACE_SAMPLES as u64) as usize
    }
}

// The columns no sample of the current trace is drawn in. A lost ping has a
// column and stays a gap: loss is shown as data.
fn empty_columns(len: usize, total: u64, scrolling: bool) -> [bool; TRACE_SAMPLES] {
    let count = len.min(TRACE_SAMPLES);
    let mut empty = [true; TRACE_SAMPLES];
    for i in 0..count {
        empty[column(i, count, total, scrolling)] = false;
    }
    empty
}

// The flat ash line along the bottom row, wherever there is no sample to
// draw: the whole slot while connecting, lost, closed or alone, and the
// columns a new link has not reached yet, so the trace grows over it instead
// of starting as a stray dash.
fn draw_baseline(pixels: &mut Pixels, slot: Rect, empty: &[bool; TRACE_SAMPLES]) {
    let bottom_row = TRACE_HEIGHT - 1.0;
    let (top, bottom) = row_span(slot.top(), bottom_row, bottom_row, pixels.per_point);
    let mut start = 0;
    for run in empty.chunk_by(|a, b| a == b) {
        let end = start + run.len();
        if run[0] {
            pixels.fill(
                Rect::from_min_max(
                    pos2(slot.left() + start as f32, top),
                    pos2(slot.left() + end as f32, bottom),
                ),
                ASH,
            );
        }
        start = end;
    }
}

// Where rows `upper` to `lower` of the slot go, in points on whole device
// pixels. Each row is as many device pixels tall as one point rounds to, from
// the row's own top down: two at 150 percent, so the trace scales with the
// display as every other stroke does, where one device pixel alone came
// close to vanishing on a cheap monitor.
fn row_span(slot_top: f32, upper: f32, lower: f32, per_point: f32) -> (f32, f32) {
    let rows = per_point.round().max(1.0);
    let top = ((slot_top + upper) * per_point).round();
    let bottom = ((slot_top + lower) * per_point).round() + rows;
    (top / per_point, bottom / per_point)
}

fn draw_trace(
    pixels: &mut Pixels,
    slot: Rect,
    trace: &[TracePoint],
    sweep: &Sweep,
    scrolling: bool,
    only: Option<Color32>,
) {
    let thresholds = Thresholds::default();
    let count = trace.len().min(TRACE_SAMPLES);
    let trace = &trace[trace.len() - count..];
    let mut previous: Option<(usize, f32)> = None;
    for (i, point) in trace.iter().enumerate() {
        let column = column(i, count, sweep.total, scrolling);
        let TracePoint::Rtt(ms) = *point else {
            previous = None;
            continue;
        };
        let row = ((1.0 - (ms / TRACE_TOP_MS).clamp(0.0, 1.0)) * (TRACE_HEIGHT - 1.0)).round();
        let color = only.unwrap_or(match thresholds.rtt_level(ms) {
            stats::Level::Good => SAGE,
            stats::Level::Warn => WARN,
            stats::Level::Bad => BAD,
        });
        // Joined to the sample before with a vertical run, so a jump reads as
        // one jagged line rather than scattered dots.
        let (upper, lower) = match previous {
            Some((col, prev_row)) if col + 1 == column => (row.min(prev_row), row.max(prev_row)),
            _ => (row, row),
        };
        let (top, bottom) = row_span(slot.top(), upper, lower, pixels.per_point);
        pixels.fill(
            Rect::from_min_max(
                pos2(slot.left() + column as f32, top),
                pos2(slot.left() + column as f32 + 1.0, bottom),
            ),
            color,
        );
        previous = Some((column, row));
    }
}

// The trace is drawn as one mesh of quads snapped to whole pixels. egui
// feathers the edges of a filled rect, and on a sample one pixel wide the
// feathering is most of the sample, so a flat line came out dotted.
struct Pixels {
    mesh: Mesh,
    per_point: f32,
}

impl Pixels {
    fn new(painter: &Painter) -> Pixels {
        Pixels {
            mesh: Mesh::default(),
            per_point: painter.pixels_per_point(),
        }
    }

    fn fill(&mut self, rect: Rect, color: Color32) {
        self.mesh
            .add_colored_rect(rect.round_to_pixels(self.per_point), color);
    }

    fn paint(self, painter: &Painter) {
        if !self.mesh.is_empty() {
            painter.add(self.mesh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtts(values: &[f32]) -> Vec<TracePoint> {
        values.iter().map(|&ms| TracePoint::Rtt(ms)).collect()
    }

    #[test]
    fn counts_samples_while_the_window_fills() {
        assert_eq!(appended(&[], &rtts(&[1.0, 2.0])), 2);
        assert_eq!(appended(&rtts(&[1.0, 2.0]), &rtts(&[1.0, 2.0, 3.0])), 1);
        assert_eq!(appended(&rtts(&[1.0, 2.0]), &rtts(&[1.0, 2.0])), 0);
    }

    #[test]
    fn counts_samples_once_the_window_slides() {
        let old = rtts(&[1.0, 2.0, 3.0, 4.0]);
        let new = rtts(&[3.0, 4.0, 5.0, 6.0]);
        assert_eq!(appended(&old, &new), 2);

        let old = vec![TracePoint::Rtt(1.0), TracePoint::Lost, TracePoint::Lost];
        let new = vec![TracePoint::Lost, TracePoint::Lost, TracePoint::Lost];
        assert_eq!(appended(&old, &new), 1);
    }

    #[test]
    fn a_new_link_counts_as_all_new() {
        assert_eq!(appended(&rtts(&[1.0, 2.0, 3.0]), &rtts(&[9.0])), 1);
    }

    #[test]
    fn scrolling_keeps_the_newest_at_the_right_edge() {
        assert_eq!(column(0, 1, 1, true), TRACE_SAMPLES - 1);
        assert_eq!(column(119, 120, 500, true), TRACE_SAMPLES - 1);
        assert_eq!(column(0, 120, 500, true), 0);
    }

    #[test]
    fn overwriting_keeps_each_sample_in_its_column() {
        // Filling up: left to right from the first column.
        assert_eq!(column(0, 5, 5, false), 0);
        assert_eq!(column(4, 5, 5, false), 4);
        // Five samples past full: the newest has wrapped to column 4 and the
        // oldest still shown sits just right of it.
        assert_eq!(column(119, 120, 125, false), 4);
        assert_eq!(column(0, 120, 125, false), 5);
        // The same sample stays put as more arrive.
        assert_eq!(column(119, 120, 125, false), column(118, 120, 126, false));
    }

    #[test]
    fn the_baseline_fills_only_columns_with_no_sample() {
        // Three samples scrolling in from the right.
        let empty = empty_columns(3, 3, true);
        assert!(empty[..TRACE_SAMPLES - 3].iter().all(|&e| e));
        assert!(empty[TRACE_SAMPLES - 3..].iter().all(|&e| !e));
        // A full trace leaves no room for it.
        assert!(empty_columns(120, 500, true).iter().all(|&e| !e));
        assert!(empty_columns(150, 500, false).iter().all(|&e| !e));
        // Overwriting in place, two samples into a fresh sweep.
        let empty = empty_columns(2, 2, false);
        assert!(!empty[0] && !empty[1] && empty[2] && empty[TRACE_SAMPLES - 1]);
    }

    #[test]
    fn sweep_keeps_a_running_total() {
        let mut sweep = Sweep::default();
        sweep.update(&rtts(&[1.0]));
        sweep.update(&rtts(&[1.0, 2.0]));
        sweep.update(&rtts(&[2.0, 3.0]));
        assert_eq!(sweep.total, 3);
    }

    // The strip sits on the window's bottom edge, so its top is on a whole
    // device pixel at every common scale. At each one every row a trace
    // sample can use is as many device pixels tall as a point rounds to, and
    // the focus ring leaves all of them alone, the bottom row most of all,
    // since that is where a good link draws.
    #[test]
    fn focus_ring_never_covers_the_trace() {
        for per_point in [1.0, 1.25, 1.5, 1.75, 2.0, 2.5] {
            let window_px = 961.0;
            let bottom = window_px / per_point;
            let rect = Rect::from_min_max(pos2(0.0, bottom - HEIGHT), pos2(360.0, bottom));
            let width = controls::thickness(theme::RING_WIDTH, per_point);
            let outer = rect.round_to_pixels(per_point);
            let ring = [
                Rect::from_min_max(outer.min, pos2(outer.right(), outer.top() + width)),
                Rect::from_min_max(pos2(outer.left(), outer.bottom() - width), outer.max),
            ];
            let slot = trace_slot(rect);
            let rows = per_point.round().max(1.0);
            for row in 0..TRACE_HEIGHT as usize {
                let (top, bottom) = row_span(slot.top(), row as f32, row as f32, per_point);
                let sample = Rect::from_min_max(pos2(slot.left(), top), pos2(slot.right(), bottom))
                    .round_to_pixels(per_point);
                let tall = sample.height() * per_point;
                assert!(
                    (tall - rows).abs() < 1e-3,
                    "{per_point}: row {row} is {tall} px"
                );
                for side in ring {
                    let across = side.right().min(sample.right()) - side.left().max(sample.left());
                    let down = side.bottom().min(sample.bottom()) - side.top().max(sample.top());
                    assert!(
                        across <= 1e-3 || down <= 1e-3,
                        "{per_point}: ring meets trace row {row}"
                    );
                }
            }
        }
    }

    #[test]
    fn narrow_strip_drop_order() {
        let reconnecting = [
            (Part::State, 80.0),
            (Part::RoundTrip, 42.0),
            (Part::Jitter, 22.0),
            (Part::Loss, 36.0),
            (Part::Path, 44.0),
        ];
        let all: f32 = 80.0 + 42.0 + 22.0 + 36.0 + 44.0 + 4.0 * GAP;
        assert_eq!(fit(&reconnecting, all).len(), 5);
        assert_eq!(
            fit(&reconnecting, all - 1.0),
            [Part::State, Part::RoundTrip, Part::Loss, Part::Path]
        );
        assert_eq!(
            fit(&reconnecting, 190.0),
            [Part::State, Part::RoundTrip, Part::Path]
        );
        assert_eq!(fit(&reconnecting, 100.0), [Part::State, Part::Path]);
    }
}
