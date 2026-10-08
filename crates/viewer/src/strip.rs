// The strip along the viewer's bottom edge: the panel's strip with the video
// numbers and the present path added, drawn with Direct2D over the back
// buffer. The 24 point band in window tone with no line over it, Plex Mono
// at 12 points for every word and number, the 16 point sides, the 12 point
// gaps, the trace's geometry, the colours and the way numbers keep their
// slots are the panel's (crates/app/src/strip.rs), in points scaled by the
// window's DPI and rounded to whole pixels the way egui rounds them.

use stats::{Level, Thresholds, TraceSample};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    ID2D1DeviceContext, ID2D1SolidColorBrush,
};

use crate::error::ViewerError;
use crate::palette::{self, AMBER, ASH, BAD, Colour, WARN, WINDOW};
use crate::text::Text;

pub(crate) const HEIGHT: f32 = 24.0;
const TRACE_SAMPLES: usize = 120;
const TRACE_HEIGHT: f32 = 16.0;
const TRACE_TOP_MS: f32 = 100.0;
// Between words, the numbers in their slots.
const GAP: f32 = 12.0;
const SIDE: f32 = 16.0;
const FONT_SIZE: f32 = 12.0;

// The strip's numbers as the caller has them: the link's from the room
// (room::view::Strip, field for field) and the video's from the decode
// loop. Levels are worked out by the caller, as the room does for the
// panel, from the thresholds in crates/stats.
#[derive(Clone, Debug)]
pub struct Strip {
    pub state: LinkState,
    pub rtt_ms: Option<f32>,
    pub rtt_level: Level,
    pub jitter_ms: Option<f32>,
    pub jitter_level: Level,
    pub loss_pct: Option<f32>,
    pub loss_level: Level,
    pub path: Option<PathWord>,
    // Oldest first, at most 120: one per ping.
    pub trace: Vec<TraceSample>,
    pub encode_ms: Option<f32>,
    pub encode_level: Level,
    pub decode_ms: Option<f32>,
    pub decode_level: Level,
    // Capture to display.
    pub end_to_end_ms: Option<f32>,
    pub end_to_end_level: Level,
    // While this PC controls the share: the release key as the strip
    // prints it, "Ctrl+Shift+End".
    pub controlling: Option<String>,
    // The shared PC has an administrator window in front, which drops
    // everything sent to it, so control is paused.
    pub control_paused: bool,
}

impl Default for Strip {
    fn default() -> Strip {
        Strip {
            state: LinkState::default(),
            rtt_ms: None,
            rtt_level: Level::Good,
            jitter_ms: None,
            jitter_level: Level::Good,
            loss_pct: None,
            loss_level: Level::Good,
            path: None,
            trace: Vec::new(),
            encode_ms: None,
            encode_level: Level::Good,
            decode_ms: None,
            decode_level: Level::Good,
            end_to_end_ms: None,
            end_to_end_level: Level::Good,
            controlling: None,
            control_paused: false,
        }
    }
}

const PAUSED: &str = "The shared PC has an administrator window in front. Control is paused.";
// What stays of it in a strip too narrow for the whole sentence.
const PAUSED_SHORT: &str = "Control is paused.";

// room::view::LinkState, one to one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    // Nobody at the other end. The strip stays empty.
    #[default]
    Alone,
    Connecting,
    Live,
    Reconnecting,
    Lost,
    Closed,
}

// room::view::PathWord, plus the word for booth.exe --loopback, where the
// video never leaves the PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathWord {
    Lan,
    Direct,
    Loop,
}

// How the last frame reached the screen: flip when DWM stepped aside and
// the display scanned the swap chain's buffer itself, composed when DWM
// copied it into the desktop first, which costs up to a refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentPath {
    Flip,
    Composed,
}

pub(crate) fn band_height(dpi: u32) -> u32 {
    (HEIGHT * scale(dpi)).round() as u32
}

fn scale(dpi: u32) -> f32 {
    dpi.max(96) as f32 / 96.0
}

// Counts every sample the trace has ever shown, so that with Windows
// animations off each one can be written at a fixed column (total mod 120)
// instead of the whole line moving left.
#[derive(Default)]
pub(crate) struct Sweep {
    total: u64,
    last: Vec<TraceSample>,
}

impl Sweep {
    pub(crate) fn update(&mut self, trace: &[TraceSample]) {
        self.total += appended(&self.last, trace) as u64;
        self.last.clear();
        self.last.extend_from_slice(trace);
    }
}

// The trace is a sliding window: the new one is the old one with some samples
// gone from the front and the same number or more added at the back. The
// smallest shift that lines the two up is the right one unless the window is
// periodic, and then every shift draws the same picture anyway.
fn appended(old: &[TraceSample], new: &[TraceSample]) -> usize {
    for shift in 0..=old.len() {
        let kept = &old[shift..];
        if kept.len() <= new.len() && new[..kept.len()] == *kept {
            return new.len() - kept.len();
        }
    }
    new.len()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    State,
    RoundTrip,
    Jitter,
    Loss,
    Path,
    Control,
    Paused,
    Encode,
    Decode,
    EndToEnd,
    Present,
}

impl Part {
    // In ash, with the one space before the value that follows it.
    fn label(self) -> Option<&'static str> {
        match self {
            Part::Encode => Some("enc "),
            Part::Decode => Some("dec "),
            Part::EndToEnd => Some("e2e "),
            _ => None,
        }
    }

    // The widest value each number normally takes. Its slot is that wide
    // whatever it shows, so going from 9 to 10 does not push the words after
    // it along. The path word's slot is as wide as "relayed", the longest it
    // will be once a relay comes, so LAN, direct and loop leave enc, dec and
    // e2e in the same place from one session to the next.
    fn widest(self) -> &'static [&'static str] {
        match self {
            Part::RoundTrip | Part::EndToEnd => &["100 ms", "<1 ms"],
            Part::Jitter => &["\u{b1}99"],
            Part::Loss => &["10.0%"],
            Part::Encode | Part::Decode => &["99.9 ms"],
            Part::Path => &["relayed"],
            Part::State | Part::Control | Part::Paused | Part::Present => &[],
        }
    }
}

// What goes first when the strip is too narrow for everything. The state
// word, the path word, the control words and the present path always stay:
// the strip is the one place state shows, "relayed" is the reason a link is
// slow, the control words say this PC's keys go to another, and "composed"
// is the reason a windowed viewer is a frame behind. When they still do not
// fit, the control words take their short forms.
const DROP_ORDER: [Part; 6] = [
    Part::Jitter,
    Part::Loss,
    Part::Encode,
    Part::Decode,
    Part::RoundTrip,
    Part::EndToEnd,
];

#[derive(Clone, Debug, PartialEq)]
struct Word {
    part: Part,
    text: String,
    colour: Colour,
    // Whether the word gets the room its widest value needs. A live number
    // does, and the path word always does. The last numbers shown while
    // reconnecting do not.
    slot: bool,
    // What it says when even the words that always stay do not fit.
    short: Option<String>,
}

fn words(strip: &Strip, present: Option<PresentPath>) -> Vec<Word> {
    let mut words = Vec::new();
    let mut push = |part, text: String, colour, slot| {
        words.push(Word {
            part,
            text,
            colour,
            slot,
            short: None,
        })
    };
    // With no numbers the control words still show: this PC's keys go to
    // another whatever the link does.
    let stale = match strip.state {
        LinkState::Alone => {
            control_words(strip, &mut words);
            return words;
        }
        LinkState::Closed => {
            push(Part::State, String::from("closed"), ASH, false);
            control_words(strip, &mut words);
            return words;
        }
        LinkState::Connecting => {
            push(Part::State, String::from("connecting"), ASH, false);
            control_words(strip, &mut words);
            return words;
        }
        LinkState::Lost => {
            push(Part::State, String::from("lost"), BAD, false);
            control_words(strip, &mut words);
            return words;
        }
        LinkState::Reconnecting => {
            push(Part::State, String::from("reconnecting"), WARN, false);
            true
        }
        LinkState::Live => false,
    };
    let colour = |level| if stale { ASH } else { palette::level(level) };
    if let Some(ms) = strip.rtt_ms {
        push(
            Part::RoundTrip,
            milliseconds(ms),
            colour(strip.rtt_level),
            !stale,
        );
    }
    if let Some(ms) = strip.jitter_ms {
        push(
            Part::Jitter,
            format!("\u{b1}{ms:.0}"),
            colour(strip.jitter_level),
            !stale,
        );
    }
    if let Some(pct) = strip.loss_pct {
        push(
            Part::Loss,
            format!("{pct:.1}%"),
            colour(strip.loss_level),
            !stale,
        );
    }
    if let Some(path) = strip.path {
        let word = match path {
            PathWord::Lan => "LAN",
            PathWord::Direct => "direct",
            PathWord::Loop => "loop",
        };
        // A word, not a reading: colour in the strip is for the numbers.
        push(Part::Path, word.to_string(), ASH, true);
    }
    control_words(strip, &mut words);
    let mut push = |part, text: String, colour, slot| {
        words.push(Word {
            part,
            text,
            colour,
            slot,
            short: None,
        })
    };
    if let Some(ms) = strip.encode_ms {
        push(Part::Encode, tenths(ms), colour(strip.encode_level), !stale);
    }
    if let Some(ms) = strip.decode_ms {
        push(Part::Decode, tenths(ms), colour(strip.decode_level), !stale);
    }
    if let Some(ms) = strip.end_to_end_ms {
        push(
            Part::EndToEnd,
            milliseconds(ms),
            colour(strip.end_to_end_level),
            !stale,
        );
    }
    // Measured here, never stale. Flip is a word like the path word. Composed
    // is in warn, as relayed is: the path that is slower than it could be,
    // and F11 is the way out.
    match present {
        Some(PresentPath::Flip) => push(Part::Present, "flip".to_string(), ASH, false),
        Some(PresentPath::Composed) => push(Part::Present, "composed".to_string(), WARN, false),
        None => {}
    }
    words
}

// "controlling, {panic key} releases" in amber after the path word, and the
// paused sentence after it while an administrator window on the sharer's PC
// drops what is sent. The release key is left out then, so the sentence has
// the room.
fn control_words(strip: &Strip, words: &mut Vec<Word>) {
    let Some(key) = &strip.controlling else {
        return;
    };
    let controlling = String::from("controlling");
    let (text, short) = if strip.control_paused {
        (controlling, None)
    } else {
        (format!("controlling, {key} releases"), Some(controlling))
    };
    words.push(Word {
        part: Part::Control,
        text,
        colour: AMBER,
        slot: false,
        short,
    });
    if strip.control_paused {
        words.push(Word {
            part: Part::Paused,
            text: PAUSED.to_string(),
            colour: WARN,
            slot: false,
            short: Some(PAUSED_SHORT.to_string()),
        });
    }
}

fn milliseconds(ms: f32) -> String {
    if ms < 0.5 {
        String::from("<1 ms")
    } else {
        format!("{ms:.0} ms")
    }
}

// Encode and decode, with their unit like every other reading: "2.1 ms".
fn tenths(ms: f32) -> String {
    if ms < 99.95 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

fn fit(widths: &[(Part, f32)], room: f32) -> Vec<Part> {
    let mut kept: Vec<Part> = widths.iter().map(|(part, _)| *part).collect();
    for part in DROP_ORDER {
        if used(widths, &kept) <= room {
            break;
        }
        kept.retain(|kept| *kept != part);
    }
    kept
}

fn used(widths: &[(Part, f32)], kept: &[Part]) -> f32 {
    let shown: Vec<f32> = widths
        .iter()
        .filter(|(part, _)| kept.contains(part))
        .map(|(_, width)| *width)
        .collect();
    shown.iter().sum::<f32>() + GAP * shown.len().saturating_sub(1) as f32
}

// A word placed in the strip, in points from the strip's left edge.
#[derive(Clone, Debug, PartialEq)]
struct Placed {
    word: Word,
    x: f32,
    width: f32,
}

// `measure` answers in points.
fn place(
    words: Vec<Word>,
    strip_width: f32,
    measure: &mut dyn FnMut(&str) -> Result<f32, ViewerError>,
) -> Result<Vec<Placed>, ViewerError> {
    let room = strip_width - SIDE - TRACE_SAMPLES as f32 - GAP - SIDE;
    let mut words = words;
    let mut widths = measured(&words, measure)?;
    let mut kept = fit(&widths, room);
    if used(&widths, &kept) > room && words.iter().any(|word| word.short.is_some()) {
        for word in &mut words {
            if let Some(short) = word.short.take() {
                word.text = short;
            }
        }
        widths = measured(&words, measure)?;
        kept = fit(&widths, room);
    }
    let mut x = SIDE;
    let mut placed = Vec::with_capacity(kept.len());
    for (word, (_, width)) in words.into_iter().zip(widths) {
        if !kept.contains(&word.part) {
            continue;
        }
        placed.push(Placed { word, x, width });
        x += width + GAP;
    }
    Ok(placed)
}

fn measured(
    words: &[Word],
    measure: &mut dyn FnMut(&str) -> Result<f32, ViewerError>,
) -> Result<Vec<(Part, f32)>, ViewerError> {
    let mut widths = Vec::with_capacity(words.len());
    for word in words {
        let label = match word.part.label() {
            Some(label) => measure(label)?,
            None => 0.0,
        };
        let mut value = measure(&word.text)?;
        if word.slot {
            for widest in word.part.widest() {
                value = value.max(measure(widest)?);
            }
        }
        widths.push((word.part, (label + value).ceil()));
    }
    Ok(widths)
}

// egui's row for a 12 point font: ascent and descent rounded to 1/32 of a
// point, the row rounded to whole pixels, centred in the band and rounded
// to whole points. Returns the baseline in pixels below the band's top.
fn baseline(metrics: crate::text::Metrics, scale: f32) -> f32 {
    let per_unit = FONT_SIZE / metrics.units_per_em;
    let ui = |value: f32| (value * 32.0).round() / 32.0;
    let pixel = |points: f32| (points * scale).round() / scale;
    let ascent = ui(metrics.ascent * per_unit);
    let descent = ui(metrics.descent * per_unit);
    let gap = ui(metrics.line_gap.max(0.0) * per_unit);
    let row = pixel(ascent + descent + gap);
    let top = (HEIGHT / 2.0 - row / 2.0).round();
    (top * scale).round() + (ascent * scale).round()
}

// Where rows `upper` to `lower` of the trace slot go, in whole pixels, as
// the panel places them: each row as many pixels tall as one point rounds
// to, from the row's own top down, so the trace is two pixels at 150 percent
// like every other stroke scaled with the display.
fn row_span(slot_top: f32, upper: f32, lower: f32, scale: f32) -> (f32, f32) {
    let rows = scale.round().max(1.0);
    let top = (slot_top + upper * scale).round();
    (top, (slot_top + lower * scale).round() + rows)
}

pub(crate) struct Painter {
    context: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    text: Text,
}

// The part of the frame that changes with the window rather than the video.
pub(crate) struct Look {
    pub size: (u32, u32),
    pub dpi: u32,
    pub scrolling: bool,
    pub present: Option<PresentPath>,
    pub band: Band,
}

// Where the strip goes: a band under the picture, or with the setting on, in
// fullscreen, hidden until the mouse moves. The picture then has the whole
// monitor, so a share the monitor's size shows one to one, and the strip
// that the mouse brings back goes over its bottom edge rather than moving
// the picture up under the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Band {
    Below,
    Over,
    Hidden,
}

impl Painter {
    // `brush` is any solid brush made on `context`; its colour is set for
    // each thing drawn.
    pub(crate) fn new(
        context: &ID2D1DeviceContext,
        brush: ID2D1SolidColorBrush,
    ) -> Result<Painter, ViewerError> {
        Ok(Painter {
            context: context.clone(),
            brush,
            text: Text::new()?,
        })
    }

    // Between the context's BeginDraw and EndDraw, with the target set.
    pub(crate) fn draw(
        &mut self,
        strip: &Strip,
        sweep: &Sweep,
        look: &Look,
    ) -> Result<(), ViewerError> {
        let scale = scale(look.dpi);
        let (width, height) = (look.size.0 as f32, look.size.1 as f32);
        let band = band_height(look.dpi) as f32;
        let top = height - band;
        if top < 0.0 {
            return Ok(());
        }
        // SAFETY: plain calls on the live context this painter was made with.
        unsafe {
            self.context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
            self.context
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        }
        // No line over it: the band is told apart from the picture by its
        // tone, as the panel's strip is from the chat above it.
        self.fill(0.0, top, width, height, WINDOW);

        self.text.set_size(FONT_SIZE * scale)?;
        let text = &mut self.text;
        let placed = place(
            words(strip, look.present),
            width / scale,
            &mut |word: &str| Ok(text.line(word)?.width / scale),
        )?;
        let baseline = top + baseline(self.text.metrics(), scale);
        // Every word starts at the left of its slot, the value right after
        // its label's space, so each reading has a fixed place and a short
        // value leaves the rest of its slot empty after it.
        for placed in &placed {
            let word = &placed.word;
            let mut left = placed.x * scale;
            if let Some(label) = word.part.label() {
                self.text_at(label, left, baseline, ASH)?;
                left += self.text.line(label)?.width;
            }
            self.text_at(&word.text, left, baseline, word.colour)?;
        }

        let slot_left = width - (SIDE + TRACE_SAMPLES as f32) * scale;
        let slot_top = top + ((HEIGHT - TRACE_HEIGHT) / 2.0) * scale;
        let empty = match strip.state {
            LinkState::Live | LinkState::Reconnecting => {
                empty_columns(strip.trace.len(), sweep.total, look.scrolling)
            }
            LinkState::Connecting | LinkState::Lost | LinkState::Alone | LinkState::Closed => {
                [true; TRACE_SAMPLES]
            }
        };
        self.baseline(&empty, scale, slot_left, slot_top);
        match strip.state {
            LinkState::Live => self.trace(strip, sweep, look, slot_left, slot_top, None),
            LinkState::Reconnecting => {
                self.trace(strip, sweep, look, slot_left, slot_top, Some(ASH))
            }
            LinkState::Connecting | LinkState::Lost | LinkState::Alone | LinkState::Closed => {}
        }
        Ok(())
    }

    // The flat ash line along the slot's bottom row wherever there is no
    // sample to draw, as in the panel's strip.
    fn baseline(&self, empty: &[bool; TRACE_SAMPLES], scale: f32, slot_left: f32, slot_top: f32) {
        let bottom_row = TRACE_HEIGHT - 1.0;
        let (top, bottom) = row_span(slot_top, bottom_row, bottom_row, scale);
        let mut start = 0;
        for run in empty.chunk_by(|a, b| a == b) {
            let end = start + run.len();
            if run[0] {
                self.fill(
                    slot_left + start as f32 * scale,
                    top,
                    slot_left + end as f32 * scale,
                    bottom,
                    ASH,
                );
            }
            start = end;
        }
    }

    fn trace(
        &self,
        strip: &Strip,
        sweep: &Sweep,
        look: &Look,
        slot_left: f32,
        slot_top: f32,
        only: Option<Colour>,
    ) {
        let scale = scale(look.dpi);
        let thresholds = Thresholds::default();
        let count = strip.trace.len().min(TRACE_SAMPLES);
        let trace = &strip.trace[strip.trace.len() - count..];
        let mut previous: Option<(usize, f32)> = None;
        for (i, point) in trace.iter().enumerate() {
            let column = column(i, count, sweep.total, look.scrolling);
            let TraceSample::Rtt(ms) = *point else {
                previous = None;
                continue;
            };
            let row = ((1.0 - (ms / TRACE_TOP_MS).clamp(0.0, 1.0)) * (TRACE_HEIGHT - 1.0)).round();
            let colour = only.unwrap_or(palette::level(thresholds.rtt_level(ms)));
            // Joined to the sample before with a vertical run, so a jump reads
            // as one jagged line rather than scattered dots.
            let (upper, lower) = match previous {
                Some((col, prev_row)) if col + 1 == column => {
                    (row.min(prev_row), row.max(prev_row))
                }
                _ => (row, row),
            };
            let x = slot_left + column as f32 * scale;
            let (top, bottom) = row_span(slot_top, upper, lower, scale);
            self.fill(x, top, x + scale, bottom, colour);
            previous = Some((column, row));
        }
    }

    // A rectangle with every edge on the nearest whole pixel, as egui's
    // round_to_pixels does it, so one-pixel samples stay solid.
    fn fill(&self, left: f32, top: f32, right: f32, bottom: f32, colour: Colour) {
        let rect = D2D_RECT_F {
            left: left.round(),
            top: top.round(),
            right: right.round(),
            bottom: bottom.round(),
        };
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return;
        }
        // SAFETY: plain calls on live objects with values that outlive them.
        unsafe {
            self.brush.SetColor(&colour.d2d());
            self.context.FillRectangle(&rect, &self.brush);
        }
    }

    fn text_at(
        &mut self,
        text: &str,
        left: f32,
        baseline: f32,
        colour: Colour,
    ) -> Result<(), ViewerError> {
        let line = self.text.line(text)?;
        // The origin goes in a transform: the layout call takes it as a
        // type from windows-numerics, which this crate does not name.
        let at: [f32; 6] = [1.0, 0.0, 0.0, 1.0, left.round(), baseline - line.baseline];
        // SAFETY: the transform is six floats laid out as D2D's Matrix3x2
        // (M11, M12, M21, M22, M31, M32) and outlives the call; the layout
        // and brush are live.
        unsafe {
            self.brush.SetColor(&colour.d2d());
            self.context
                .SetTransform(&at as *const [f32; 6] as *const _);
            self.context.DrawTextLayout(
                Default::default(),
                &line.layout,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            let identity: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
            self.context
                .SetTransform(&identity as *const [f32; 6] as *const _);
        }
        Ok(())
    }

    // For tests: where the words went, in pixels from the left edge.
    #[cfg(test)]
    pub(crate) fn word_spans(
        &mut self,
        strip: &Strip,
        look: &Look,
    ) -> Result<Vec<(String, f32, f32)>, ViewerError> {
        let scale = scale(look.dpi);
        self.text.set_size(FONT_SIZE * scale)?;
        let text = &mut self.text;
        let placed = place(
            words(strip, look.present),
            look.size.0 as f32 / scale,
            &mut |word: &str| Ok(text.line(word)?.width / scale),
        )?;
        Ok(placed
            .into_iter()
            .map(|p| (p.word.text, p.x * scale, (p.x + p.width) * scale))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::SAGE;

    fn rtts(values: &[f32]) -> Vec<TraceSample> {
        values.iter().map(|&ms| TraceSample::Rtt(ms)).collect()
    }

    fn live() -> Strip {
        Strip {
            state: LinkState::Live,
            rtt_ms: Some(4.0),
            jitter_ms: Some(0.4),
            loss_pct: Some(0.0),
            path: Some(PathWord::Lan),
            encode_ms: Some(2.1),
            decode_ms: Some(1.4),
            end_to_end_ms: Some(11.2),
            end_to_end_level: Level::Good,
            ..Strip::default()
        }
    }

    #[test]
    fn counts_samples_while_the_window_fills_and_slides() {
        assert_eq!(appended(&[], &rtts(&[1.0, 2.0])), 2);
        assert_eq!(appended(&rtts(&[1.0, 2.0]), &rtts(&[1.0, 2.0, 3.0])), 1);
        assert_eq!(
            appended(&rtts(&[1.0, 2.0, 3.0, 4.0]), &rtts(&[3.0, 4.0, 5.0, 6.0])),
            2
        );
    }

    #[test]
    fn scrolling_keeps_the_newest_at_the_right_edge() {
        assert_eq!(column(0, 1, 1, true), TRACE_SAMPLES - 1);
        assert_eq!(column(0, 120, 500, true), 0);
        assert_eq!(column(119, 120, 125, false), 4);
    }

    #[test]
    fn the_viewer_words_in_order() {
        let words = words(&live(), Some(PresentPath::Flip));
        let texts: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "4 ms", "\u{b1}0", "0.0%", "LAN", "2.1 ms", "1.4 ms", "11 ms", "flip"
            ]
        );
        let labels: Vec<Option<&str>> = words.iter().map(|word| word.part.label()).collect();
        assert_eq!(labels[4..7], [Some("enc "), Some("dec "), Some("e2e ")]);
        // Colour is for the numbers: the path word and flip are words, in
        // ash, and the labels before the video numbers are drawn in ash too.
        let colours: Vec<Colour> = words.iter().map(|word| word.colour).collect();
        assert_eq!(colours, [SAGE, SAGE, SAGE, ASH, SAGE, SAGE, SAGE, ASH]);
    }

    #[test]
    fn closed_says_so_and_alone_says_nothing() {
        let closed = Strip {
            state: LinkState::Closed,
            ..live()
        };
        let words = words(&closed, Some(PresentPath::Flip));
        assert_eq!(words.len(), 1);
        assert_eq!((words[0].text.as_str(), words[0].colour), ("closed", ASH));
        let alone = Strip {
            state: LinkState::Alone,
            ..live()
        };
        assert!(words_of(&alone).is_empty());
    }

    #[test]
    fn the_baseline_fills_only_columns_with_no_sample() {
        let empty = empty_columns(3, 3, true);
        assert!(empty[..TRACE_SAMPLES - 3].iter().all(|&e| e));
        assert!(empty[TRACE_SAMPLES - 3..].iter().all(|&e| !e));
        assert!(empty_columns(120, 500, true).iter().all(|&e| !e));
        let empty = empty_columns(2, 2, false);
        assert!(!empty[0] && !empty[1] && empty[2]);
    }

    // No network, so no round trip, jitter or trace: those words are left
    // out rather than shown as zero.
    #[test]
    fn the_loopback_says_loop_where_the_room_says_lan() {
        let strip = Strip {
            rtt_ms: None,
            jitter_ms: None,
            path: Some(PathWord::Loop),
            ..live()
        };
        let texts: Vec<String> = words(&strip, Some(PresentPath::Composed))
            .into_iter()
            .map(|word| word.text)
            .collect();
        assert_eq!(
            texts,
            ["0.0%", "loop", "2.1 ms", "1.4 ms", "11 ms", "composed"]
        );
    }

    #[test]
    fn composed_is_in_warn_and_stale_numbers_in_ash() {
        let mut strip = live();
        strip.state = LinkState::Reconnecting;
        strip.decode_level = Level::Bad;
        let words = words(&strip, Some(PresentPath::Composed));
        assert_eq!(words[0].text, "reconnecting");
        assert!(
            words[1..words.len() - 1]
                .iter()
                .all(|word| word.colour == ASH)
        );
        assert_eq!(words.last().map(|word| word.colour), Some(WARN));
    }

    #[test]
    fn levels_pick_the_panel_colours() {
        let mut strip = live();
        strip.rtt_level = Level::Warn;
        strip.end_to_end_level = Level::Bad;
        let words = words(&strip, None);
        assert_eq!(words[0].colour, WARN);
        assert_eq!(words[6].colour, BAD);
        assert_eq!(words[1].colour, SAGE);
    }

    #[test]
    fn connecting_shows_only_its_word() {
        let strip = Strip {
            state: LinkState::Connecting,
            ..live()
        };
        let words = words(&strip, Some(PresentPath::Flip));
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "connecting");
    }

    fn controlling(key: &str) -> Strip {
        Strip {
            controlling: Some(key.to_string()),
            ..live()
        }
    }

    #[test]
    fn controlling_follows_the_path_word() {
        let words = words(&controlling("Ctrl+Alt+F12"), Some(PresentPath::Flip));
        let texts: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "4 ms",
                "\u{b1}0",
                "0.0%",
                "LAN",
                "controlling, Ctrl+Alt+F12 releases",
                "2.1 ms",
                "1.4 ms",
                "11 ms",
                "flip"
            ]
        );
        assert_eq!(words[4].colour, AMBER);
        assert!(!words.iter().any(|word| word.part == Part::Paused));
    }

    #[test]
    fn control_is_paused_by_an_administrator_window() {
        let strip = Strip {
            control_paused: true,
            ..controlling("Ctrl+Shift+End")
        };
        let words = words(&strip, None);
        let control: Vec<(&str, Colour)> = words
            .iter()
            .filter(|word| matches!(word.part, Part::Control | Part::Paused))
            .map(|word| (word.text.as_str(), word.colour))
            .collect();
        assert_eq!(
            control,
            [
                ("controlling", AMBER),
                (
                    "The shared PC has an administrator window in front. Control is paused.",
                    WARN
                )
            ]
        );
        // Paused means nothing while this PC controls nothing.
        let watching = Strip {
            control_paused: true,
            ..live()
        };
        assert!(
            !words_of(&watching)
                .iter()
                .any(|text| text.contains("paused"))
        );
    }

    fn words_of(strip: &Strip) -> Vec<String> {
        words(strip, None)
            .into_iter()
            .map(|word| word.text)
            .collect()
    }

    #[test]
    fn the_control_words_stay_while_the_link_has_no_numbers() {
        for (state, first) in [
            (LinkState::Connecting, "connecting"),
            (LinkState::Lost, "lost"),
        ] {
            let strip = Strip {
                state,
                ..controlling("Ctrl+Shift+End")
            };
            assert_eq!(
                words_of(&strip),
                [first, "controlling, Ctrl+Shift+End releases"]
            );
        }
        let alone = Strip {
            state: LinkState::Alone,
            ..controlling("Ctrl+Shift+End")
        };
        assert_eq!(words_of(&alone), ["controlling, Ctrl+Shift+End releases"]);
    }

    #[test]
    fn a_narrow_strip_drops_numbers_first() {
        let paused = Strip {
            control_paused: true,
            ..controlling("Ctrl+Shift+End")
        };
        let texts = |width: f32, strip: &Strip| -> Vec<String> {
            place(words(strip, Some(PresentPath::Flip)), width, &mut measure)
                .unwrap()
                .into_iter()
                .map(|placed| placed.word.text)
                .collect()
        };
        let wide = texts(2000.0, &controlling("Ctrl+Shift+End"));
        assert!(wide.contains(&String::from("controlling, Ctrl+Shift+End releases")));
        assert_eq!(wide.len(), 9);
        // Room for the long words and nothing else: every number went. LAN
        // takes the slot "relayed" needs.
        let long = texts(
            164.0 + 7.0 * 7.0 + 36.0 * 7.0 + 4.0 * 7.0 + 24.0,
            &controlling("Ctrl+Shift+End"),
        );
        assert_eq!(
            long,
            ["LAN", "controlling, Ctrl+Shift+End releases", "flip"]
        );
        let short = texts(420.0, &controlling("Ctrl+Shift+End"));
        assert!(short.contains(&String::from("controlling")), "{short:?}");
        assert!(short.contains(&String::from("LAN")) && short.contains(&String::from("flip")));
        let short = texts(420.0, &paused);
        assert!(
            short.contains(&String::from("Control is paused.")),
            "{short:?}"
        );
    }

    // Every character 7 points wide, as a stand-in for the font.
    fn measure(text: &str) -> Result<f32, ViewerError> {
        Ok(text.chars().count() as f32 * 7.0)
    }

    #[test]
    fn numbers_keep_their_slots_as_digits_change() {
        let mut slow = live();
        slow.rtt_ms = Some(88.0);
        let fast = place(words(&live(), None), 1200.0, &mut measure).unwrap();
        let slow = place(words(&slow, None), 1200.0, &mut measure).unwrap();
        let starts = |placed: &[Placed]| placed.iter().map(|p| p.x).collect::<Vec<_>>();
        assert_eq!(starts(&fast), starts(&slow));
    }

    // Over a real link the path word changes between sessions; what follows
    // it stays put, 12 points after a slot as wide as "relayed".
    #[test]
    fn the_path_word_keeps_its_slot() {
        let starts = |path| {
            let strip = Strip {
                path: Some(path),
                ..live()
            };
            place(words(&strip, None), 1200.0, &mut measure)
                .unwrap()
                .iter()
                .map(|p| p.x)
                .collect::<Vec<_>>()
        };
        let lan = starts(PathWord::Lan);
        assert_eq!(lan, starts(PathWord::Direct));
        assert_eq!(lan, starts(PathWord::Loop));
        assert_eq!(lan[4] - lan[3], 7.0 * 7.0 + GAP);
    }

    #[test]
    fn trace_rows_are_whole_pixels_a_point_tall() {
        // At 150 percent a row is two pixels from its own top down, and the
        // bottom row ends a pixel past the slot, clear of the band's edge.
        let slot_top = 1000.0 + 4.0 * 1.5;
        assert_eq!(row_span(slot_top, 0.0, 0.0, 1.5), (1006.0, 1008.0));
        assert_eq!(row_span(slot_top, 15.0, 15.0, 1.5), (1029.0, 1031.0));
        assert_eq!(row_span(slot_top, 3.0, 15.0, 1.5), (1011.0, 1031.0));
        assert_eq!(row_span(1004.0, 15.0, 15.0, 1.0), (1019.0, 1020.0));
        assert_eq!(row_span(1008.0, 15.0, 15.0, 2.0), (1038.0, 1040.0));
    }

    #[test]
    fn a_narrow_viewer_drops_jitter_first() {
        let words_all = words(&live(), Some(PresentPath::Composed));
        let wide = place(words_all.clone(), 2000.0, &mut measure).unwrap();
        assert_eq!(wide.len(), 8);
        let narrow = place(words_all, 420.0, &mut measure).unwrap();
        let parts: Vec<Part> = narrow.iter().map(|p| p.word.part).collect();
        assert!(!parts.contains(&Part::Jitter));
        assert!(parts.contains(&Part::Path));
        assert!(parts.contains(&Part::Present));
    }

    #[test]
    fn the_band_scales_with_dpi() {
        assert_eq!(band_height(96), 24);
        assert_eq!(band_height(144), 36);
        assert_eq!(band_height(120), 30);
    }
}
