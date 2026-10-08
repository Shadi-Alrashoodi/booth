// The chat between the people list and the strip: the lines, and the
// composer under them.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use eframe::egui::containers::scroll_area::ScrollAreaOutput;
use eframe::egui::text::LayoutJob;
use eframe::egui::{
    Align, Frame, Galley, Label, Layout, Margin, Rect, Response, ScrollArea, Sense, TextFormat, Ui,
    UiBuilder, pos2, vec2,
};
use room::view::{ChatLine, LineKind, Person};
use room::{ChatRefused, Room};

use crate::controls;
use crate::messages;
use crate::theme::{self, ASH, BAD, CHALK, CONTROL_HEIGHT, HALF_STEP, PANEL, SIDE, WARN};

// A line of chat is at most about 70 characters wide. An ordinary sentence
// gives the width of an ordinary character.
const LINE_CHARS: f32 = 70.0;
const SAMPLE: &str = "the quick brown fox jumps over the lazy dog";
const AUTHOR_GAP: f32 = 16.0;
const MESSAGE_GAP: f32 = 8.0;
const BODY_LINE: f32 = 18.0;
const NAME_TO_TIME: f32 = 8.0;
// The first line keeps clear of the people list, the last of the composer.
const ENDS: i8 = 16;
// Far above what one message may be, far below a document pasted by mistake.
const DRAFT_CHARS: usize = 2048;
// One formatted time per minute that has a line in it.
const TIMES_KEPT: usize = 4096;
// Names seen in one room. A friend's program can rename itself with every
// packet; past this the count starts over from what the room holds now.
const NAMES_KEPT: usize = 4096;
const ELLIPSIS: char = '\u{2026}';
// Unicode's first strong isolate and its end, and the left-to-right mark:
// they draw nothing and only steer which way text runs.
const ISOLATE: char = '\u{2068}';
const END_ISOLATE: char = '\u{2069}';
const LTR_MARK: char = '\u{200E}';

pub struct Chat {
    draft: String,
    error: Option<&'static str>,
    // Focus goes to the composer when the room opens.
    focus_pending: bool,
    // How tall the composer and the line under it were on the last frame.
    // The chat is laid out before them, so Tab reaches the composer in
    // reading order; a frame where the composer grew is laid out again.
    bottom: f32,
    times: Times,
    heights: Heights,
    names: Names,
}

impl Chat {
    pub fn new() -> Chat {
        Chat {
            draft: String::new(),
            error: None,
            focus_pending: true,
            bottom: CONTROL_HEIGHT + SIDE,
            times: Times::default(),
            heights: Heights::default(),
            names: Names::default(),
        }
    }

    // The rest of the window: the lines, the composer under them, and the
    // line that says why a message was not sent.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        lines: &[Arc<ChatLine>],
        people: &[Person],
        room: &Room,
        enabled: bool,
    ) {
        self.names.note(people, lines);
        let height = (ui.available_height() - self.bottom).max(0.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        ui.painter().rect_filled(rect, 0, PANEL);
        let mut area = ui.new_child(UiBuilder::new().max_rect(rect));
        area.set_clip_rect(rect.intersect(ui.clip_rect()));
        self.history(&mut area, lines);

        // Still the chat's region: the field sits on its tone with the
        // gutter either side and 16 px down to the strip. Why a message was
        // not sent goes half a step under the field, as any field's error.
        let top = ui.cursor().top();
        Frame::new()
            .fill(PANEL)
            .inner_margin(Margin {
                left: SIDE as i8,
                right: SIDE as i8,
                top: 0,
                bottom: SIDE as i8,
            })
            .show(ui, |ui| {
                if let Some(text) = self.composer(ui, enabled) {
                    self.said(room.say(&text));
                }
                if let Some(error) = self.error {
                    ui.add_space(HALF_STEP);
                    controls::text(ui, error, theme::caption(), BAD);
                }
            });
        let bottom = ui.cursor().top() - top;
        if (bottom - self.bottom).abs() > 0.5 {
            self.bottom = bottom;
            ui.ctx().request_repaint();
        }
    }

    // Stuck to the newest line until someone scrolls up; then it stays
    // where they are until they scroll back down.
    fn history(&mut self, ui: &mut Ui, lines: &[Arc<ChatLine>]) -> ScrollAreaOutput<()> {
        ScrollArea::vertical()
            .id_salt("chat")
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show_viewport(ui, |ui, viewport| self.lines(ui, lines, viewport))
    }

    // An empty chat says nothing: the composer's "Message" is the verb.
    // Only the lines in view are laid out; the rest count by the height they
    // had when they came, so a full history costs a frame about what a short
    // one does. The panel repaints on every mouse move, next to a game.
    fn lines(&mut self, ui: &mut Ui, lines: &[Arc<ChatLine>], viewport: Rect) {
        if lines.is_empty() {
            return;
        }
        let sample = controls::text_width(ui, SAMPLE, theme::body());
        let most = LINE_CHARS * sample / SAMPLE.chars().count() as f32;
        let width = (ui.available_width() - 2.0 * SIDE).min(most).max(0.0);
        self.heights.update(ui, lines, width, &mut self.times);
        // Every name line is one row of the same height.
        let name_height = self.name_galley(ui, &lines[0], width).size().y;

        // The lines in view, and where the first of them starts, the space
        // above it included.
        let mut in_view: Option<(usize, f32)> = None;
        let mut end = 0;
        let mut y = f32::from(ENDS);
        for (i, (_, text_height)) in self.heights.lines.iter().enumerate() {
            let (gap, named) = spacing(lines, i);
            let top = y;
            y += gap + if named { name_height } else { 0.0 } + text_height;
            if y > viewport.min.y && top < viewport.max.y {
                in_view.get_or_insert((i, top));
                end = i + 1;
            }
        }
        ui.set_height(y + f32::from(ENDS));
        let Some((first, top)) = in_view else {
            return;
        };
        let origin = pos2(ui.max_rect().left() + SIDE, ui.max_rect().top() + top);
        let rect = Rect::from_min_size(origin, vec2(width, y - top));
        ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            for (i, line) in lines.iter().enumerate().take(end).skip(first) {
                let (gap, named) = spacing(lines, i);
                ui.add_space(gap);
                if named {
                    let name = self.name_galley(ui, line, width);
                    ui.add(Label::new(name).show_tooltip_when_elided(false));
                }
                if line.kind == LineKind::Said {
                    body(ui, &line.text, width);
                } else {
                    let time = self.times.get(line.at_unix_ms);
                    ui.add(Label::new(system_galley(ui, line, time, width)));
                }
            }
        });
    }

    // The name in Medium, then the time in ash, on one line. When another
    // key goes by the same name, the fingerprint follows in Plex Mono as on
    // the people rows, and the name is what gets cut to make room for it:
    // the fingerprint is what tells the two apart.
    fn name_galley(&mut self, ui: &Ui, line: &ChatLine, width: f32) -> Arc<Galley> {
        let format = |font_id, color| TextFormat {
            font_id,
            color,
            line_height: Some(BODY_LINE),
            ..TextFormat::default()
        };
        // A real space before the time and the fingerprint, so a screen
        // reader does not run them together; the rest of each gap is layout.
        let space = controls::text_width(ui, " ", theme::caption());
        let after_space = (NAME_TO_TIME - space).max(0.0);
        let time = self.times.get(line.at_unix_ms).map(str::to_owned);
        let fingerprint = self
            .names
            .shared(&line.name)
            .then(|| keys::fingerprint(&line.author));
        let tail = |job: &mut LayoutJob| {
            let parts = [
                (&time, theme::mono_caption()),
                (&fingerprint, theme::mono_caption()),
            ];
            for (text, font) in parts {
                if let Some(text) = text {
                    job.append(" ", 0.0, format(theme::caption(), ASH));
                    job.append(text, after_space, format(font, ASH));
                }
            }
        };
        let name = if fingerprint.is_some() {
            let mut alone = LayoutJob::default();
            tail(&mut alone);
            let taken = ui.painter().layout_job(alone).size().x.ceil();
            fit(ui, &line.name, width - taken)
        } else {
            line.name.clone()
        };
        // The name is isolated, so an Arabic name reads right to left in its
        // place and the line still goes name, time, fingerprint from the left.
        let mut job = LayoutJob::default();
        let isolated = format!("{ISOLATE}{name}{END_ISOLATE}");
        job.append(&isolated, 0.0, format(theme::medium(), CHALK));
        tail(&mut job);
        job.wrap.max_width = width;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        ui.painter().layout_job(job)
    }

    // The text to say when Enter was pressed in the composer. Disabled, it
    // keeps what was typed for when the host is back.
    fn composer(&mut self, ui: &mut Ui, enabled: bool) -> Option<String> {
        // egui fades a disabled scope to half, which would put the field and
        // its placeholder in colours the theme does not have, and the
        // placeholder under ash. Disabled, it still takes no keys.
        let response = ui
            .scope(|ui| {
                ui.visuals_mut().disabled_alpha = 1.0;
                ui.add_enabled_ui(enabled, |ui| {
                    controls::composer(
                        ui,
                        "composer",
                        &mut self.draft,
                        messages::MESSAGE_HINT,
                        DRAFT_CHARS,
                    )
                })
                .inner
            })
            .inner;
        if response.changed() {
            self.error = None;
        }
        if enabled && std::mem::take(&mut self.focus_pending) {
            response.request_focus();
        }
        // The field takes typed text by egui's own focus alone, and Enter
        // goes by the same rule, so the two never disagree about where a
        // key went. Response::has_focus also wants the window in front.
        let focused = ui.memory(|memory| memory.has_focus(response.id));
        (focused && controls::enter_pressed(ui)).then(|| self.draft.clone())
    }

    fn said(&mut self, result: Result<(), ChatRefused>) {
        match result {
            Ok(()) => {
                self.draft.clear();
                self.error = None;
            }
            // Kept in the composer, to be cut in two or to go once the host
            // is back.
            Err(why) => self.error = messages::chat_refused(why),
        }
    }
}

// The time of day each line shows, asked of Windows once per minute that
// has a line in it, not once per line per frame.
#[derive(Default)]
struct Times(HashMap<u64, Option<String>>);

impl Times {
    fn get(&mut self, at_unix_ms: u64) -> Option<&str> {
        if self.0.len() >= TIMES_KEPT {
            self.0.clear();
        }
        self.0
            .entry(at_unix_ms / 60_000)
            .or_insert_with(|| messages::chat_time(at_unix_ms))
            .as_deref()
    }
}

// A line from the room puts the time first, and the sentence after it is in
// ash, or in warn when it is about a share that ran into
// something. It has no name line: the sentence names whom it is about.
fn system_galley(ui: &Ui, line: &ChatLine, time: Option<&str>, width: f32) -> Arc<Galley> {
    let format = |font_id, color| TextFormat {
        font_id,
        color,
        line_height: Some(BODY_LINE),
        ..TextFormat::default()
    };
    let color = if line.kind == LineKind::Problem {
        WARN
    } else {
        ASH
    };
    let mut job = LayoutJob::default();
    // The sentence is English, so the line reads left to right even when it
    // starts with an Arabic name, which would otherwise turn it around.
    job.append(&LTR_MARK.to_string(), 0.0, format(theme::caption(), ASH));
    let mut leading = 0.0;
    if let Some(time) = time {
        job.append(time, 0.0, format(theme::mono_caption(), ASH));
        // A real space, so a screen reader does not run the time into the
        // sentence; the rest of the gap is layout, as on a name line.
        job.append(" ", 0.0, format(theme::caption(), ASH));
        let space = controls::text_width(ui, " ", theme::caption());
        leading = (NAME_TO_TIME - space).max(0.0);
    }
    job.append(&line.text, leading, format(theme::body(), color));
    job.wrap.max_width = width;
    ui.painter().layout_job(job)
}

// The message text is a layout job of its own, so the name above it never
// decides which way it runs. A message whose first letter is Arabic or
// Hebrew is right-aligned, whatever follows; any other is left-aligned.
fn body_galley(ui: &Ui, text: &str, width: f32) -> Arc<Galley> {
    let format = TextFormat {
        font_id: theme::body(),
        color: CHALK,
        line_height: Some(BODY_LINE),
        valign: ui.text_valign(),
        ..TextFormat::default()
    };
    let mut job = LayoutJob::single_section(text.to_owned(), format);
    job.wrap.max_width = width;
    if controls::right_to_left(text) {
        job.halign = Align::RIGHT;
    }
    ui.painter().layout_job(job)
}

// One message's text, against the right edge of the chat column when it
// reads right to left.
fn body(ui: &mut Ui, text: &str, width: f32) -> Response {
    let galley = body_galley(ui, text, width);
    if galley.job.halign == Align::RIGHT {
        ui.with_layout(Layout::top_down(Align::Max), |ui| {
            ui.add(body_label(galley))
        })
        .inner
    } else {
        ui.add(body_label(galley))
    }
}

// The message text is the one thing in the panel that can be selected and
// copied, so a link or an address a friend sends can be taken out. The theme
// keeps every other label unselectable.
fn body_label(galley: Arc<Galley>) -> Label {
    Label::new(galley).selectable(true)
}

// The longest start of the name that fits in `room` with an ellipsis after
// it, or the whole name when that fits.
fn fit(ui: &Ui, name: &str, room: f32) -> String {
    let width = |text: &str| controls::text_width(ui, text, theme::medium());
    if width(name) <= room {
        return name.to_owned();
    }
    let cut = |chars: usize| {
        let end = name
            .char_indices()
            .nth(chars)
            .map_or(name.len(), |(at, _)| at);
        format!("{}{ELLIPSIS}", &name[..end])
    };
    let (mut fits, mut over) = (0, name.chars().count());
    while over - fits > 1 {
        let mid = (fits + over) / 2;
        if width(&cut(mid)) <= room {
            fits = mid;
        } else {
            over = mid;
        }
    }
    cut(fits)
}

// 16 px between authors, 8 px between one author's messages, and the name
// and time only over the first of a run. Returns the space above
// line `i` and whether it gets the name line.
fn spacing(lines: &[Arc<ChatLine>], i: usize) -> (f32, bool) {
    let line = &lines[i];
    let said = line.kind == LineKind::Said;
    match i.checked_sub(1).and_then(|before| lines.get(before)) {
        None => (0.0, said),
        Some(before) if said && before.kind == LineKind::Said && before.author == line.author => {
            (MESSAGE_GAP, false)
        }
        // Lines from the room follow each other as one author's do, and
        // they never join a run of anyone's messages.
        Some(before) if !said && before.kind != LineKind::Said => (MESSAGE_GAP, false),
        Some(_) => (AUTHOR_GAP, said),
    }
}

// The height of each line's text at one width, oldest first. Lines only
// leave the history at the front and come at the back, so a frame lays out
// the new ones alone, and all of them again only when the width changes.
#[derive(Default)]
struct Heights {
    width: f32,
    lines: VecDeque<(Arc<ChatLine>, f32)>,
}

impl Heights {
    fn update(&mut self, ui: &Ui, lines: &[Arc<ChatLine>], width: f32, times: &mut Times) {
        if self.width != width {
            self.width = width;
            self.lines.clear();
        }
        while let Some((oldest, _)) = self.lines.front()
            && !lines
                .first()
                .is_some_and(|first| Arc::ptr_eq(first, oldest))
        {
            self.lines.pop_front();
        }
        let known = self.lines.len();
        if known > lines.len()
            || (known > 0 && !Arc::ptr_eq(&self.lines[known - 1].0, &lines[known - 1]))
        {
            self.lines.clear();
        }
        for line in &lines[self.lines.len()..] {
            let galley = if line.kind == LineKind::Said {
                body_galley(ui, &line.text, width)
            } else {
                system_galley(ui, line, times.get(line.at_unix_ms), width)
            };
            let height = galley.size().y;
            self.lines.push_back((Arc::clone(line), height));
        }
    }
}

// Which keys each name in the room has been seen with, on the people list
// or on a line of chat. Two people with the same name always show
// fingerprints. A name stays shared for the room, so a friend who takes
// the name of someone who left cannot pass for them in what is still on
// screen.
#[derive(Default)]
struct Names {
    keys: HashMap<String, Named>,
    // The newest line counted, to go on from on the next frame.
    newest: Option<Arc<ChatLine>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    One([u8; 32]),
    Shared,
}

impl Names {
    fn note(&mut self, people: &[Person], lines: &[Arc<ChatLine>]) {
        if self.keys.len() >= NAMES_KEPT {
            self.keys.clear();
            self.newest = None;
        }
        for person in people {
            self.add(&person.name, person.key);
        }
        let from = self
            .newest
            .as_ref()
            .and_then(|newest| lines.iter().rposition(|line| Arc::ptr_eq(line, newest)))
            .map_or(0, |at| at + 1);
        // A line from the room carries the name it is about as it was then,
        // which says nothing about who used which name.
        for line in lines[from..]
            .iter()
            .filter(|line| line.kind == LineKind::Said)
        {
            self.add(&line.name, line.author);
        }
        self.newest = lines.last().cloned();
    }

    fn add(&mut self, name: &str, key: [u8; 32]) {
        match self.keys.get_mut(name) {
            Some(named) if *named != Named::One(key) => *named = Named::Shared,
            Some(_) => {}
            None => {
                self.keys.insert(name.to_owned(), Named::One(key));
            }
        }
    }

    fn shared(&self, name: &str) -> bool {
        self.keys.get(name) == Some(&Named::Shared)
    }
}

#[cfg(test)]
mod bidi_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Context, Event, Key, Modifiers, MouseWheelUnit, RawInput, TouchPhase};
    use room::view::Level;

    fn line(author: u8, text: &str) -> Arc<ChatLine> {
        Arc::new(ChatLine {
            author: [author; 32],
            name: format!("person {author}"),
            text: text.to_owned(),
            at_unix_ms: 1_790_284_323_456,
            mine: false,
            kind: room::view::LineKind::Said,
        })
    }

    #[test]
    fn one_authors_run_has_one_name_line() {
        let lines = [
            line(1, "anyone got the key for the east door"),
            line(1, "the one by the stairs"),
            line(2, "on my way"),
            line(1, "thanks"),
            line(1, "see you there"),
            line(1, "bring snacks"),
        ];
        let spaced: Vec<(f32, bool)> = (0..lines.len()).map(|i| spacing(&lines, i)).collect();
        assert_eq!(
            spaced,
            [
                (0.0, true),
                (8.0, false),
                (16.0, true),
                (16.0, true),
                (8.0, false),
                (8.0, false),
            ]
        );
    }

    fn system(about: u8, text: &str, kind: LineKind) -> Arc<ChatLine> {
        Arc::new(ChatLine {
            kind,
            text: text.to_owned(),
            ..ChatLine::clone(&line(about, "unused"))
        })
    }

    // A line from the room has no name line, and it never joins a run of
    // messages, not even those of the person it is about. Lines
    // from the room follow each other as one author's messages do.
    #[test]
    fn lines_from_the_room_stand_apart_from_the_runs() {
        let lines = [
            line(1, "anyone got the key for the east door"),
            system(1, "person 1 started sharing", LineKind::System),
            system(
                2,
                "Could not start sharing: the host cannot be reached. Try again once it is back.",
                LineKind::Problem,
            ),
            line(1, "found it"),
            line(1, "on my way"),
            system(3, "person 3 stopped sharing", LineKind::System),
        ];
        let spaced: Vec<(f32, bool)> = (0..lines.len()).map(|i| spacing(&lines, i)).collect();
        assert_eq!(
            spaced,
            [
                (0.0, true),
                (16.0, false),
                (8.0, false),
                (16.0, true),
                (8.0, false),
                (16.0, false),
            ]
        );
        assert_eq!(spacing(&lines[1..], 0), (0.0, false));
    }

    // The name in a line from the room is whom it is about, as the room had
    // it then; it takes no part in telling two people with one name apart.
    #[test]
    fn lines_from_the_room_are_not_counted_as_names() {
        let mut names = Names::default();
        let started = Arc::new(ChatLine {
            name: String::from("Mara"),
            ..ChatLine::clone(&system(9, "Mara started sharing", LineKind::System))
        });
        names.note(&[person(1, "Mara")], &[named(1, "Mara"), started]);
        assert!(!names.shared("Mara"));
    }

    // Time first, then the sentence: in ash, or in warn for a problem with
    // a share, and wrapped like a message.
    #[test]
    fn a_line_from_the_room_starts_with_the_time() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let started = system(1, "Ines started sharing", LineKind::System);
        let failed = system(
            1,
            "Could not show Ines's screen: avcodec-62.dll was not found next to booth.exe. Unzip Booth again with all its files.",
            LineKind::Problem,
        );
        with_ui(&ctx, |ui| {
            let galley = system_galley(ui, &started, Some("21:15"), 300.0);
            assert_eq!(galley.text(), "\u{200E}21:15 Ines started sharing");
            let sections = &galley.job.sections;
            assert!(sections.iter().all(|section| section.format.color == ASH));
            assert_eq!(sections[0].format.font_id, theme::caption());
            assert_eq!(sections.last().unwrap().format.font_id, theme::body());
            assert_eq!(galley.size().y, BODY_LINE);

            let galley = system_galley(ui, &failed, Some("21:16"), 300.0);
            let last = galley.job.sections.last().unwrap();
            assert_eq!(last.format.color, WARN);
            assert_eq!(galley.job.sections[0].format.color, ASH);
            assert!(galley.size().y >= 2.0 * BODY_LINE, "{}", galley.size().y);

            // A time Windows would not give leaves the sentence alone.
            let alone = system_galley(ui, &started, None, 300.0);
            assert_eq!(alone.text(), "\u{200E}Ines started sharing");
        });
    }

    fn person(key: u8, name: &str) -> Person {
        Person {
            key: [key; 32],
            name: name.to_owned(),
            fingerprint: String::new(),
            rtt_ms: None,
            rtt_level: Level::Good,
            is_you: false,
            is_host: false,
            joined_by_invite: false,
            reconnecting: false,
            talking: false,
            sharing: false,
        }
    }

    fn named(author: u8, name: &str) -> Arc<ChatLine> {
        Arc::new(ChatLine {
            name: name.to_owned(),
            ..ChatLine::clone(&line(author, "hello"))
        })
    }

    // One frame with a Ui, for what needs fonts.
    fn with_ui(ctx: &Context, run: impl FnMut(&mut Ui)) {
        let mut run = run;
        ctx.run_ui(RawInput::default(), |ui| run(ui))
            .drop_without_applying_deltas();
    }

    // The theme turns selection off for every label; the message text turns
    // it back on for itself only.
    #[test]
    fn only_the_message_text_can_be_selected() {
        let ctx = Context::default();
        ctx.all_styles_mut(|style| style.interaction.selectable_labels = false);
        with_ui(&ctx, |ui| {
            let text = ui.add(body_label(body_galley(ui, "see you in there", 300.0)));
            let name = ui.add(Label::new("Mara"));
            assert!(text.sense.senses_drag(), "a drag over the text selects it");
            assert!(
                !name.sense.senses_drag(),
                "the name line stays unselectable"
            );
        });
    }

    // A friend who takes the host's name, or the name of someone who has
    // left, gets a fingerprint on every line under that name, and so does
    // the one they copy. A name only one key uses stands alone.
    #[test]
    fn two_keys_with_one_name_show_fingerprints() {
        let mut names = Names::default();
        let people = [person(1, "Mara"), person(2, "Ana")];
        let mut lines = vec![named(1, "Mara"), named(3, "Jonas")];
        names.note(&people, &lines);
        assert!(!names.shared("Mara") && !names.shared("Ana") && !names.shared("Jonas"));

        lines.push(named(4, "Mara"));
        names.note(&people, &lines);
        assert!(names.shared("Mara"));
        assert!(!names.shared("Ana"));
        // Jonas has left; someone new takes his name.
        let people = [person(1, "Mara"), person(2, "Ana"), person(5, "Jonas")];
        names.note(&people, &lines);
        assert!(names.shared("Jonas"));

        let ctx = Context::default();
        theme::apply(&ctx);
        let mut chat = Chat::new();
        chat.names = names;
        with_ui(&ctx, |ui| {
            let copied = chat.name_galley(ui, &lines[2], 300.0);
            let fingerprint = keys::fingerprint(&[4; 32]);
            let isolated = "\u{2068}Mara\u{2069} ";
            assert!(copied.text().starts_with(isolated), "{}", copied.text());
            assert!(copied.text().ends_with(&fingerprint), "{}", copied.text());
            let alone = chat.name_galley(ui, &named(2, "Ana"), 300.0);
            assert!(!alone.text().contains(&keys::fingerprint(&[2; 32])));
        });
    }

    // At the narrowest window a long name would push the fingerprint out of
    // the line, so the name is cut instead.
    #[test]
    fn the_name_is_cut_before_the_fingerprint() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let long = "Wolfgang Amadeus of the East Door";
        let mut chat = Chat::new();
        let lines = [named(1, long), named(2, long)];
        chat.names.note(&[], &lines);
        with_ui(&ctx, |ui| {
            let fingerprint = keys::fingerprint(&[2; 32]);
            let galley = chat.name_galley(ui, &lines[1], 240.0);
            let text = galley.text();
            assert!(text.ends_with(&fingerprint), "{text}");
            assert!(
                text.starts_with("\u{2068}Wolf") && text.contains(ELLIPSIS),
                "{text}"
            );
            assert!(galley.size().x <= 240.0, "{}", galley.size().x);
            assert!(!galley.elided, "{text}");
            // Room enough, nothing is cut.
            let wide = chat.name_galley(ui, &lines[1], 600.0);
            let isolated = format!("{ISOLATE}{long}{END_ISOLATE}");
            assert!(wide.text().starts_with(&isolated), "{}", wide.text());
        });
    }

    // Lines leave at the front and come at the back; the heights follow
    // them without laying out what was already measured, and start over
    // when the width changes.
    #[test]
    fn heights_follow_the_history() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let mut heights = Heights::default();
        let mut times = Times::default();
        let long = "on my way, bring the key for the east door ".repeat(6);
        let mut lines: Vec<Arc<ChatLine>> = (0..5).map(|n| line(1, &n.to_string())).collect();
        lines.push(line(2, &long));
        with_ui(&ctx, |ui| {
            heights.update(ui, &lines, 280.0, &mut times);
            let measured: Vec<f32> = heights.lines.iter().map(|(_, h)| *h).collect();
            assert_eq!(measured[..5], [BODY_LINE; 5]);
            assert!(measured[5] >= 3.0 * BODY_LINE, "{}", measured[5]);

            let first_kept = Arc::clone(&heights.lines[2].0);
            lines.drain(..2);
            lines.push(line(3, "late"));
            heights.update(ui, &lines, 280.0, &mut times);
            assert_eq!(heights.lines.len(), lines.len());
            assert!(Arc::ptr_eq(&heights.lines[0].0, &first_kept));
            for ((kept, _), line) in heights.lines.iter().zip(&lines) {
                assert!(Arc::ptr_eq(kept, line));
            }

            heights.update(ui, &lines, 1000.0, &mut times);
            let wide: Vec<f32> = heights.lines.iter().map(|(_, h)| *h).collect();
            assert!(wide[3] < measured[5], "{} {}", wide[3], measured[5]);

            // Another room's lines share nothing with these.
            let other: Vec<Arc<ChatLine>> = (0..3).map(|n| line(9, &n.to_string())).collect();
            heights.update(ui, &other, 1000.0, &mut times);
            assert_eq!(heights.lines.len(), 3);
            assert!(Arc::ptr_eq(&heights.lines[0].0, &other[0]));
        });
    }

    // One frame of the history alone in the smallest window, with this
    // input. Returns the scroll offset and the most it can be.
    fn scrolled(
        ctx: &Context,
        chat: &mut Chat,
        lines: &[Arc<ChatLine>],
        events: Vec<Event>,
    ) -> (f32, f32) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 640.0))),
            events,
            ..RawInput::default()
        };
        let mut out = None;
        ctx.run_ui(input, |ui| out = Some(chat.history(ui, lines)))
            .drop_without_applying_deltas();
        let out = out.expect("the frame ran");
        (
            out.state.offset.y,
            out.content_size.y - out.inner_rect.height(),
        )
    }

    // The chat stays at the newest line as lines come, until someone scrolls
    // up; then it stays where they are.
    #[test]
    fn scrolled_up_the_chat_stays_where_it_is() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let mut chat = Chat::new();
        let mut lines: Vec<Arc<ChatLine>> = (0..60)
            .map(|n| line(1 + n % 2, &format!("line {n}")))
            .collect();
        // egui spreads a wheel turn over a few frames.
        let settle = |chat: &mut Chat, lines: &[Arc<ChatLine>]| {
            let mut last = scrolled(&ctx, chat, lines, Vec::new());
            for _ in 0..100 {
                let now = scrolled(&ctx, chat, lines, Vec::new());
                if now == last {
                    return now;
                }
                last = now;
            }
            panic!("the chat never came to rest: {last:?}");
        };
        let (offset, most) = settle(&mut chat, &lines);
        assert!(most > 0.0, "{most}");
        assert_eq!(offset, most);

        lines.push(line(3, "a new line"));
        let (offset, grown) = settle(&mut chat, &lines);
        assert!(grown > most, "{grown} {most}");
        assert_eq!(offset, grown);

        let wheel = vec![
            Event::PointerMoved(pos2(100.0, 300.0)),
            Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: vec2(0.0, 200.0),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ];
        scrolled(&ctx, &mut chat, &lines, wheel);
        let (up, _) = settle(&mut chat, &lines);
        assert!(up < grown - 100.0, "{up} {grown}");

        lines.push(line(1, "one more while you read back"));
        let (held, more) = settle(&mut chat, &lines);
        assert!(more > grown, "{more} {grown}");
        assert_eq!(held, up);
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    // One frame of the composer on its own, with this input.
    fn frame(ctx: &Context, chat: &mut Chat, enabled: bool, events: Vec<Event>) -> Option<String> {
        let input = RawInput {
            events,
            ..RawInput::default()
        };
        let mut sent = None;
        ctx.run_ui(input, |ui| sent = chat.composer(ui, enabled))
            .drop_without_applying_deltas();
        sent
    }

    #[test]
    fn enter_sends_and_shift_enter_starts_a_new_line() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let mut chat = Chat::new();
        // The first frame asks for focus, which the next one has.
        assert_eq!(frame(&ctx, &mut chat, true, Vec::new()), None);
        let typed = vec![Event::Text(String::from("east door"))];
        assert_eq!(frame(&ctx, &mut chat, true, typed), None);
        let shift_enter = vec![key(Key::Enter, Modifiers::SHIFT)];
        assert_eq!(frame(&ctx, &mut chat, true, shift_enter), None);
        assert_eq!(chat.draft, "east door\n");
        let typed = vec![Event::Text(String::from("by the stairs"))];
        assert_eq!(frame(&ctx, &mut chat, true, typed), None);

        let enter = vec![key(Key::Enter, Modifiers::NONE)];
        let sent = frame(&ctx, &mut chat, true, enter);
        assert_eq!(sent.as_deref(), Some("east door\nby the stairs"));
        // Plain Enter never puts a line break in the draft.
        assert_eq!(chat.draft, "east door\nby the stairs");
        chat.said(Ok(()));
        assert_eq!(chat.draft, "");

        // Enter on a disabled composer sends nothing.
        chat.draft = String::from("still there?");
        let enter = vec![key(Key::Enter, Modifiers::NONE)];
        assert_eq!(frame(&ctx, &mut chat, false, enter), None);
        assert_eq!(chat.draft, "still there?");
    }

    #[test]
    fn a_refused_message_stays_with_the_sentence_under_it() {
        let ctx = Context::default();
        theme::apply(&ctx);
        let mut chat = Chat::new();
        frame(&ctx, &mut chat, true, Vec::new());
        chat.draft = "a".repeat(901);
        chat.said(Err(ChatRefused::TooLong));
        assert_eq!(chat.error, Some(messages::MESSAGE_TOO_LONG));
        assert_eq!(chat.draft.len(), 901);
        // Editing it takes the sentence away.
        let typed = vec![Event::Text(String::from("b"))];
        frame(&ctx, &mut chat, true, typed);
        assert_eq!(chat.draft.len(), 902);
        assert_eq!(chat.error, None);
        // Nothing to send is no error.
        chat.said(Err(ChatRefused::Empty));
        assert_eq!(chat.error, None);
    }
}
