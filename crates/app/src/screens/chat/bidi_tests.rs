// Arabic, English and mixed text in the chat and the composer: which way
// each message runs, where its letters and the caret land, and what a
// selection copies.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use eframe::egui::epaint::WHITE_UV;
use eframe::egui::epaint::text::cursor::CCursor;
use eframe::egui::epaint::text::{Glyph, PlacedRow, Row};
use eframe::egui::text::{CCursorRange, CharIndex, LayoutJob};
use eframe::egui::text_edit::TextEditState;
use eframe::egui::text_selection::visuals::{RowVertexIndices, paint_text_selection};
use eframe::egui::{
    Align, Context, Event, FullOutput, Galley, Key, Modifiers, OutputCommand, PlatformOutput,
    PointerButton, Pos2, RawInput, Rect, Response, Shape, TextFormat, Ui, UiBuilder, Visuals, pos2,
    vec2,
};
use proptest::prelude::*;
use room::view::{ChatLine, LineKind};

use super::{Chat, body, body_galley, system_galley};
use crate::theme::{self, ASH, CHALK, INK, SIDE};
use crate::{controls, messages};

const COLUMN: f32 = 300.0;
const ARABIC: &str = "مرحبا بكم في الغرفة";
const ENGLISH: &str = "see you in there";
const MIXED: &str = "مرحبا hello 123 عالم";
// Two rows in the chat column.
const LONG_ARABIC: &str = "مرحبا بكم في الغرفة، سنبدأ اللعب بعد خمس دقائق فلا تتأخروا علينا";

// One frame in the panel's smallest window, 360 by 640, with these events
// and modifiers. Returns what `run` gave, the painted text and the output.
fn frame<R>(
    ctx: &Context,
    events: Vec<Event>,
    modifiers: Modifiers,
    run: impl FnOnce(&mut Ui) -> R,
) -> (R, Vec<(Pos2, Arc<Galley>)>, PlatformOutput) {
    // The modifiers held go first, as a change of modifiers, which is how
    // egui takes them.
    let mut all = vec![Event::ModifiersChanged(modifiers)];
    all.extend(events);
    // A second apart, so two clicks at one place are never a double click.
    let time = ctx.input(|input| input.time) + 1.0;
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 640.0))),
        time: Some(time),
        events: all,
        ..RawInput::default()
    };
    let mut out = None;
    let mut run = Some(run);
    let mut output: FullOutput = ctx.run_ui(input, |ui| {
        if let Some(run) = run.take() {
            out = Some(run(ui));
        }
    });
    let texts = output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            Shape::Text(text) => Some((text.pos, Arc::clone(&text.galley))),
            _ => None,
        })
        .collect();
    let platform = std::mem::take(&mut output.platform_output);
    output.drop_without_applying_deltas();
    (out.expect("the frame ran"), texts, platform)
}

fn copied(output: &PlatformOutput) -> Option<&str> {
    output.commands.iter().find_map(|command| match command {
        OutputCommand::CopyText(text) => Some(text.as_str()),
        _ => None,
    })
}

// A message in a chat column COLUMN wide, 12 from the left and 40 down.
fn message(ui: &mut Ui, text: &str) -> Response {
    let column = Rect::from_min_size(pos2(SIDE, 40.0), vec2(COLUMN, 400.0));
    ui.scope_builder(UiBuilder::new().max_rect(column), |ui| {
        body(ui, text, COLUMN)
    })
    .inner
}

// Where a galley shown by a label is drawn: its right end is the label's
// right edge when it is right-aligned.
fn galley_pos(response: &Response, galley: &Galley) -> Pos2 {
    if galley.job.halign == Align::RIGHT {
        response.rect.right_top()
    } else {
        response.rect.left_top()
    }
}

// The screen point of cursor position `index` on the first row.
fn cursor_point(origin: Pos2, galley: &Galley, index: usize) -> Pos2 {
    let row = &galley.rows[0];
    let x = origin.x + row.pos.x + row.x_offset(CharIndex(index));
    pos2(x, origin.y + row.pos.y + row.height() / 2.0)
}

// The screen x where the glyph for char `index` of the first row starts.
fn glyph_x(origin: Pos2, galley: &Galley, index: usize) -> f32 {
    let row = &galley.rows[0];
    origin.x + row.pos.x + row.glyphs[index].pos.x
}

fn chars(text: &str, from: usize, to: usize) -> String {
    text.chars().skip(from).take(to - from).collect()
}

#[test]
fn a_message_runs_the_way_its_first_letter_does() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let (aligned, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        [
            ARABIC,
            MIXED,
            "123 مرحبا",
            "(مرحبا) hello",
            "שלום friends",
            ENGLISH,
            "hello مرحبا",
            "123 456",
            "",
        ]
        .map(|text| body_galley(ui, text, COLUMN).job.halign)
    });
    assert_eq!(
        aligned,
        [
            Align::RIGHT,
            Align::RIGHT,
            Align::RIGHT,
            Align::RIGHT,
            Align::RIGHT,
            Align::LEFT,
            Align::LEFT,
            Align::LEFT,
            Align::LEFT,
        ]
    );
}

// Right-aligned in the column when it starts in Arabic, left-aligned when
// it starts in English, and in both the words in reading order.
#[test]
fn arabic_english_and_mixed_messages_sit_in_reading_order() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let (shown, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        [ARABIC, ENGLISH, MIXED, "hello مرحبا"].map(|text| {
            let galley = body_galley(ui, text, COLUMN);
            (message(ui, text), galley)
        })
    });
    let right = SIDE + COLUMN;
    let [
        (arabic, arabic_galley),
        (english, english_galley),
        (mixed, mixed_galley),
        (tail, tail_galley),
    ] = shown;

    assert_eq!(arabic.rect.right(), right);
    assert!(arabic.rect.left() > SIDE + 100.0, "{:?}", arabic.rect);
    assert_eq!(english.rect.left(), SIDE);
    assert_eq!(mixed.rect.right(), right);
    assert_eq!(tail.rect.left(), SIDE);

    // The first word of the Arabic message is its right end.
    let origin = galley_pos(&arabic, &arabic_galley);
    let first = glyph_x(origin, &arabic_galley, 0);
    let second_word = glyph_x(origin, &arabic_galley, 6);
    assert!(second_word < first, "{second_word} {first}");
    assert!(right - 15.0 < first && first < right, "{first}");

    // "مرحبا hello 123 عالم": مرحبا on the right, then "hello 123" reading
    // left to right, then عالم on the left.
    let origin = galley_pos(&mixed, &mixed_galley);
    let x = |i| glyph_x(origin, &mixed_galley, i);
    assert!(x(19) < x(16) && x(16) < x(6), "عالم is left of hello");
    assert!(
        x(6) < x(10) && x(10) < x(12) && x(12) < x(14),
        "hello 123 reads left to right"
    );
    assert!(
        x(14) < x(4) && x(4) < x(0),
        "مرحبا is right of 123 and reads right to left"
    );

    // "hello مرحبا": hello first on the left, the Arabic word after it,
    // read from its right end.
    let origin = galley_pos(&tail, &tail_galley);
    let x = |i| glyph_x(origin, &tail_galley, i);
    assert!(x(0) < x(4) && x(4) < x(10) && x(10) < x(6));

    let english_origin = galley_pos(&english, &english_galley);
    assert_eq!(glyph_x(english_origin, &english_galley, 0), SIDE);
}

// An Arabic message whose first letter carries a vowel mark, on one row
// and on two. The mark has no width of its own, and the layout once took it
// for the end of the row, which came out a letter short: the first letter,
// at the right end, stuck out of the column and out of the label a friend
// clicks to select the message. Every letter now lies inside both, and the
// first one ends at the column's edge.
//
// The last message comes out 300.24 wide once its letters are put on whole
// pixels in visual order, so it cannot fit the column. Its first letter may
// end up to half a pixel past the right edge, as it always could, but its
// last letter must not start a pixel left of the column, which it did while
// the row was rounded out to 301.
#[test]
fn a_right_aligned_arabic_message_stays_inside_the_column() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let right = SIDE + COLUMN;
    for (text, rows, past_right) in [
        ("بِسْمِ اللَّهِ الرَّحْمَنِ", 1, 0.0),
        (
            "بِسْمِ اللَّهِ الرَّحْمَنِ الرَّحِيمِ، سَنَبْدَأُ اللَّعِبَ بَعْدَ خَمْسِ دَقَائِقَ فَلَا تَتَأَخَّرُوا",
            2,
            0.0,
        ),
        (
            "خمس بعد اللَّهِ بَعْدَ الرَّحْمَنِ 1440p 9:30 ok بعد تتأخروا مرحبا",
            1,
            0.5,
        ),
    ] {
        let show = |ui: &mut Ui| (message(ui, text), body_galley(ui, text, COLUMN));
        let ((response, galley), _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, show);
        assert_eq!(galley.rows.len(), rows, "{text}");
        let origin = galley_pos(&response, &galley);
        let clickable = response.interact_rect;
        for placed in &galley.rows {
            for glyph in placed
                .glyphs
                .iter()
                .filter(|glyph| !glyph.chr.is_whitespace())
            {
                let left = origin.x + placed.pos.x + glyph.pos.x;
                let end = origin.x + placed.pos.x + glyph.max_x();
                assert!(
                    SIDE <= left && end <= right + past_right,
                    "{text}: {:?} at {left}..{end} is outside the column",
                    glyph.chr
                );
                assert!(
                    clickable.left() <= left && end <= clickable.right() + past_right,
                    "{text}: {:?} at {left}..{end} is outside the label, {clickable:?}",
                    glyph.chr
                );
            }
        }
        let first = &galley.rows[0].glyphs[0];
        assert_eq!(Some(first.chr), text.chars().next());
        let first_end = origin.x + galley.rows[0].pos.x + first.max_x();
        assert!(
            right - 1.0 < first_end && first_end <= right + past_right,
            "{text}: {first_end}"
        );
    }
}

// The caret can go to every place in each message, each place is on the
// row, a click there puts it back, and the end of a right-to-left line
// that ends in digits is just after the last digit.
#[test]
fn caret_places_click_back() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let (galleys, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        [ARABIC, ENGLISH, MIXED, "مرحبا 123", "hello مرحبا"]
            .map(|text| body_galley(ui, text, COLUMN))
    });
    for galley in &galleys {
        let text = galley.text();
        assert_eq!(galley.rows.len(), 1, "{text}");
        let row = &galley.rows[0];
        let (left, right) = (row.pos.x, row.pos.x + row.size.x);
        for index in 0..=text.chars().count() {
            let cursor = CCursor {
                index: CharIndex(index),
                prefer_next_row: false,
            };
            let place = galley.pos_from_cursor(cursor);
            assert!(
                left - 1.0 <= place.min.x && place.min.x <= right + 1.0,
                "{text} {index}: {} is off {left}..{right}",
                place.min.x
            );
            let back = galley.cursor_from_pos(place.center().to_vec2());
            let again = galley.pos_from_cursor(back);
            assert_eq!(
                again.min.x, place.min.x,
                "{text} {index} came back as {back:?}"
            );
        }
    }

    let digits = &galleys[3];
    let row = &digits.rows[0];
    let three = row.glyphs.last().expect("the 3");
    assert_eq!(three.chr, '3');
    let end = digits.pos_from_cursor(digits.end());
    assert_eq!(end.min.x, row.pos.x + three.max_x());
    assert!(end.min.x < row.pos.x + row.size.x / 2.0, "{end:?}");
}

// A drag over part of a message and Ctrl+C copy the characters between the
// two ends in the order they were written, and the highlight is exactly
// those letters, one rectangle per run of them on the row, in ink.
#[test]
fn selection_copies_and_marks() {
    // Char ranges whose ends are on distinct places in each message, and
    // how many runs the selected letters make on the row.
    let cases = [
        (ARABIC, 0, 5, 1),
        (ARABIC, 6, 19, 1),
        (ARABIC, 2, 9, 1),
        (ENGLISH, 4, 7, 1),
        (MIXED, 0, 11, 2),
        (MIXED, 6, 15, 1),
        (MIXED, 3, 18, 1),
        ("hello مرحبا", 2, 8, 2),
    ];
    for (text, from, to, runs) in cases {
        let ctx = Context::default();
        theme::apply(&ctx);
        let show = |ui: &mut Ui| (message(ui, text), body_galley(ui, text, COLUMN));
        let ((response, galley), _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, show);
        let origin = galley_pos(&response, &galley);
        let (start, end) = (
            cursor_point(origin, &galley, from),
            cursor_point(origin, &galley, to),
        );

        frame(&ctx, press(start, true), Modifiers::NONE, show);
        frame(&ctx, vec![Event::PointerMoved(end)], Modifiers::NONE, show);
        let (_, painted, _) = frame(&ctx, press(end, false), Modifiers::NONE, show);
        let (_, _, output) = frame(&ctx, vec![Event::Copy], Modifiers::NONE, show);
        assert_eq!(
            copied(&output),
            Some(chars(text, from, to).as_str()),
            "{text}"
        );

        let (_, shown) = painted
            .iter()
            .find(|(_, galley)| galley.text() == text)
            .expect("the message was painted");
        assert_eq!(check_highlight(shown, &(from..to)), [runs], "{text}");
    }
}

// The highlight's rectangles on a painted row, in the row's coordinates:
// the quads in the selection colour that are not glyphs and cover anything.
fn highlight(row: &Row) -> Vec<Rect> {
    let corners: Vec<Pos2> = row
        .visuals
        .mesh
        .vertices
        .iter()
        .filter(|vertex| vertex.color == ASH && vertex.uv == WHITE_UV)
        .map(|vertex| vertex.pos)
        .collect();
    corners
        .chunks(4)
        .map(Rect::from_points)
        .filter(|rect| rect.width() > 0.0)
        .collect()
}

// Glyphs sit on whole pixels and their advances are not whole, so two
// neighbours overlap, or leave a gap, of up to a pixel.
const PIXEL: f32 = 1.0;

fn covers(rect: &Rect, glyph: &Glyph) -> bool {
    rect.left() <= glyph.pos.x + PIXEL && glyph.max_x() <= rect.right() + PIXEL
}

fn reaches_into(rect: &Rect, glyph: &Glyph) -> bool {
    rect.left() + PIXEL < glyph.max_x() && glyph.pos.x + PIXEL < rect.right()
}

// A galley painted with chars `selected` selected, each row checked as
// check_row_highlight does. Returns how many rectangles each row has.
fn check_highlight(galley: &Galley, selected: &Range<usize>) -> Vec<usize> {
    let mut start = 0;
    let mut counts = Vec::new();
    for placed in &galley.rows {
        counts.push(check_row_highlight(galley.text(), placed, start, selected));
        start += placed.char_count_including_newline().0;
    }
    counts
}

// A painted row of `text` that starts at char `start`, with chars
// `selected` selected: every selected letter inside a rectangle and no
// other letter reaching into one; each rectangle the row's height, holding
// a selected letter or the newline after the row, and apart from the
// others; exactly the selected chars in ink. On a row with right-to-left
// text a letter with a selected mark on it counts as selected for the
// rectangles. Returns how many rectangles the row has.
fn check_row_highlight(
    text: &str,
    placed: &PlacedRow,
    start: usize,
    selected: &Range<usize>,
) -> usize {
    let row = &placed.row;
    let rects = highlight(row);
    let chosen = |i: usize| selected.contains(&(start + i));
    let mut lit: Vec<bool> = (0..row.glyphs.len()).map(chosen).collect();
    if row.glyphs.iter().any(Glyph::is_rtl) {
        for i in (1..row.glyphs.len()).rev() {
            let (glyph, before) = (&row.glyphs[i], &row.glyphs[i - 1]);
            if glyph.advance_width <= 0.0 && glyph.bidi_level == before.bidi_level {
                lit[i - 1] |= lit[i];
            }
        }
    }
    for (i, glyph) in row.glyphs.iter().enumerate() {
        let at = start + i;
        if glyph.advance_width > 0.0 {
            if lit[i] {
                assert!(
                    rects.iter().any(|rect| covers(rect, glyph)),
                    "{text:?} {at} {:?} is not highlighted",
                    glyph.chr
                );
            } else {
                assert!(
                    !rects.iter().any(|rect| reaches_into(rect, glyph)),
                    "{text:?} {at} {:?} is highlighted",
                    glyph.chr
                );
            }
        }
        let end = row
            .glyphs
            .get(i + 1)
            .map_or(row.visuals.glyph_vertex_range.end, |next| {
                next.first_vertex as usize
            });
        let ink = if chosen(i) { INK } else { CHALK };
        for vertex in &row.visuals.mesh.vertices[glyph.first_vertex as usize..end] {
            assert_eq!(vertex.color, ink, "{text:?} {at} {:?}", glyph.chr);
        }
    }
    let newline = placed.ends_with_newline && chosen(row.glyphs.len());
    for (n, rect) in rects.iter().enumerate() {
        assert_eq!((rect.top(), rect.bottom()), (0.0, row.size.y), "{text:?}");
        let holds = row
            .glyphs
            .iter()
            .enumerate()
            .any(|(i, glyph)| lit[i] && glyph.advance_width > 0.0 && covers(rect, glyph));
        assert!(
            holds || newline,
            "{text:?}: {rect:?} holds no selected letter"
        );
        for other in &rects[n + 1..] {
            assert!(
                rect.right() < other.left() || other.right() < rect.left(),
                "{text:?}: {rect:?} touches {other:?}"
            );
        }
    }
    rects.len()
}

fn cursor(index: usize) -> CCursor {
    CCursor {
        index: CharIndex(index),
        prefer_next_row: false,
    }
}

// The galley with chars `selected` selected, painted by this egui and by
// 0.36.2 as released.
fn paint_both(
    ctx: &Context,
    galley: &Arc<Galley>,
    selected: &Range<usize>,
) -> (Arc<Galley>, Arc<Galley>) {
    let visuals = ctx.global_style().visuals.clone();
    let range = CCursorRange::two(cursor(selected.start), cursor(selected.end));
    let (now, _) = paint_with(paint_text_selection, galley, &visuals, &range);
    let (before, _) = paint_with(paint_text_selection_0_36_2, galley, &visuals, &range);
    (now, before)
}

// From inside an Arabic line to inside the English line under it. The
// Arabic row is highlighted from where the selection starts to its left
// end, where its paragraph ends and the selected newline shows, and the
// English row from its left end.
#[test]
fn selection_arabic_into_english() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let text = format!("{ARABIC}\n{ENGLISH}");
    let (galley, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        body_galley(ui, &text, COLUMN)
    });
    assert_eq!(galley.rows.len(), 2);
    let selected = 6..24;
    let (now, _) = paint_both(&ctx, &galley, &selected);
    assert_eq!(check_highlight(&now, &selected), [1, 1]);

    let arabic = &now.rows[0].row;
    let rect = highlight(arabic)[0];
    let left_end = arabic
        .glyphs
        .iter()
        .map(|glyph| glyph.pos.x)
        .fold(f32::MAX, f32::min);
    assert!(rect.left() < left_end, "{rect:?} {left_end}");
    // Up to the space after مرحبا, which is not selected.
    assert_eq!(rect.right(), arabic.glyphs[5].pos.x);
    let english = &now.rows[1].row;
    assert_eq!(highlight(english)[0].left(), 0.0);
}

// An English line that ends in Arabic with vowel marks, selected on into
// the line under it. The selected newline shows past the right end of the
// letters, and never over the unselected بِسْمِ.
#[test]
fn selected_newline() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let text = "see you all at nine tonight if the patch is out by then بِسْمِ اللَّهِ\nok";
    let (galley, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        body_galley(ui, text, COLUMN)
    });
    assert_eq!(galley.rows.len(), 3);
    let ends = galley.rows[1].row.glyphs.iter().map(Glyph::max_x);
    let letters_end = ends.fold(0.0, f32::max);
    // اللَّهِ with the newline after بِسْمِ, then all the Arabic with it.
    for (selected, runs) in [(63..72, [0, 2, 1]), (56..72, [0, 1, 1])] {
        let (now, _) = paint_both(&ctx, &galley, &selected);
        assert_eq!(check_highlight(&now, &selected), runs, "{selected:?}");
        let row = &now.rows[1].row;
        let newline = highlight(row).last().copied().expect("the newline");
        assert_eq!(newline.right(), letters_end + row.height() / 2.0);
    }
}

// All of a two-row Arabic message, as a drag from its first letter to its
// last selects it: 0.36.2 highlighted nothing, and every letter turned ink
// on the panel. Both rows are highlighted end to end, and a selection from
// inside the first row to inside the second covers the rest of the first
// and the start of the second.
#[test]
fn a_selection_over_two_arabic_rows_highlights_both() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let text = LONG_ARABIC;
    let all = 0..text.chars().count();
    let show = |ui: &mut Ui| (message(ui, text), body_galley(ui, text, COLUMN));
    let ((response, galley), _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, show);
    assert_eq!(galley.rows.len(), 2);

    let (_, before) = paint_both(&ctx, &galley, &all);
    let highlighted: f32 = before
        .rows
        .iter()
        .flat_map(|placed| highlight(&placed.row))
        .map(|rect| rect.width())
        .sum();
    assert!(highlighted < 1.0, "0.36.2 highlighted {highlighted} points");

    let origin = galley_pos(&response, &galley);
    let point = |index| origin + galley.pos_from_cursor(cursor(index)).center().to_vec2();
    let (start, end) = (point(all.start), point(all.end));
    frame(&ctx, press(start, true), Modifiers::NONE, show);
    frame(&ctx, vec![Event::PointerMoved(end)], Modifiers::NONE, show);
    let (_, painted, _) = frame(&ctx, press(end, false), Modifiers::NONE, show);
    let (_, _, output) = frame(&ctx, vec![Event::Copy], Modifiers::NONE, show);
    assert_eq!(copied(&output), Some(text));
    let (_, shown) = painted
        .iter()
        .find(|(_, galley)| galley.text() == text)
        .expect("the message was painted");
    assert_eq!(check_highlight(shown, &all), [1, 1]);

    let selected = 4..all.end - 4;
    let (now, _) = paint_both(&ctx, &galley, &selected);
    assert_eq!(check_highlight(&now, &selected), [1, 1]);
}

// On rows with no right-to-left letter, the highlight, the ink and the
// rectangles handed back are exactly 0.36.2's, for every selection of a set
// of texts at two widths, and on the left-to-right rows of messages that
// also have Arabic ones.
#[test]
fn left_to_right_rows_as_in_0_36_2() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let visuals = ctx.global_style().visuals.clone();
    let texts = [
        ENGLISH,
        "see you in there, all of you, at nine tonight if the patch is out",
        "one\ntwo\n\nthree ",
        "cafe\u{301} au lait",
        "a\tb  c",
        "123 456",
        "x",
        "",
        "مرحبا بكم\nsee you in there\nعالم",
        "hello\nمرحبا\n123",
    ];
    let mut compared = 0;
    for text in texts {
        for width in [COLUMN, 90.0] {
            let (galley, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
                body_galley(ui, text, width)
            });
            let len = text.chars().count();
            let ends = (0..=len).flat_map(|from| (from..=len).map(move |to| (from, to)));
            for ((from, to), prefer_next_row) in ends.flat_map(|pair| [(pair, false), (pair, true)])
            {
                let at = |index| CCursor {
                    index: CharIndex(index),
                    prefer_next_row,
                };
                let range = CCursorRange::two(at(from), at(to));
                let case = format!("{text:?} {width} {from}..{to} {prefer_next_row}");
                compared += compare_left_to_right_rows(&galley, &visuals, &range, &case);
            }
        }
    }
    assert!(compared > 10_000, "{compared}");
}

// Paints `range` with this egui and with 0.36.2 and checks that every row
// with no right-to-left letter came out the same. Returns how many did.
fn compare_left_to_right_rows(
    galley: &Arc<Galley>,
    visuals: &Visuals,
    range: &CCursorRange,
    case: &str,
) -> usize {
    let (now, now_rects) = paint_with(paint_text_selection, galley, visuals, range);
    let (before, before_rects) = paint_with(paint_text_selection_0_36_2, galley, visuals, range);
    let mut compared = 0;
    for (ri, (row, old)) in now.rows.iter().zip(&before.rows).enumerate() {
        if row.glyphs.iter().any(Glyph::is_rtl) {
            continue;
        }
        let on_row = |rects: &[(usize, [u32; 6])]| {
            rects
                .iter()
                .filter(|(r, _)| *r == ri)
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(row, old, "{case}");
        assert_eq!(on_row(&now_rects), on_row(&before_rects), "{case}");
        compared += 1;
    }
    compared
}

type SelectionPainter =
    fn(&mut Arc<Galley>, &Visuals, &CCursorRange, Option<&mut Vec<RowVertexIndices>>);

// The galley painted by `paint` with `range` selected, and the rectangles
// handed back as (row, vertex indices).
fn paint_with(
    paint: SelectionPainter,
    galley: &Arc<Galley>,
    visuals: &Visuals,
    range: &CCursorRange,
) -> (Arc<Galley>, Vec<(usize, [u32; 6])>) {
    let mut painted = Arc::clone(galley);
    let mut rects = Vec::new();
    paint(&mut painted, visuals, range, Some(&mut rects));
    let rects = rects
        .into_iter()
        .map(|rect| (rect.row, rect.vertex_indices))
        .collect();
    (painted, rects)
}

// The name line still goes name, time, fingerprint from the left, and a line
// from the room still reads as an English sentence, whatever script the name
// is in. Left alone, an Arabic name followed by a number would pull the time
// in front of it, and one at the start of a sentence would turn it around.
#[test]
fn name_and_room_lines_left_to_right() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let said = ChatLine {
        author: [7; 32],
        name: String::from("سارة 2"),
        text: String::from("hi"),
        at_unix_ms: 1_790_284_323_456,
        mine: false,
        kind: LineKind::Said,
    };
    let started = ChatLine {
        text: String::from("سارة 2 started sharing"),
        kind: LineKind::System,
        ..said.clone()
    };
    let mut chat = Chat::new();
    // The time as Windows would write it, whatever this PC's settings.
    chat.times
        .0
        .insert(said.at_unix_ms / 60_000, Some(String::from("21:15")));
    let ((name, room), _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
        (
            chat.name_galley(ui, &said, COLUMN),
            system_galley(ui, &started, Some("21:15"), COLUMN),
        )
    });

    // The isolate, "سارة 2", its end, a space, then the time.
    assert_eq!(name.text(), "\u{2068}سارة 2\u{2069} 21:15");
    let row = &name.rows[0].row;
    let name_right = row.glyphs[1..7]
        .iter()
        .map(|g| g.max_x())
        .fold(0.0, f32::max);
    let time_left = row.glyphs[9..]
        .iter()
        .map(|g| g.pos.x)
        .fold(f32::MAX, f32::min);
    assert!(name_right <= time_left + 0.5, "{name_right} {time_left}");
    assert!(
        row.glyphs[6].pos.x < row.glyphs[1].pos.x,
        "the 2 is read after سارة"
    );

    // The mark, "21:15", a space, "سارة 2", " started sharing".
    let row = &room.rows[0].row;
    let x = |i: usize| row.glyphs[i].pos.x;
    let text: Vec<char> = room.text().chars().collect();
    assert_eq!((text[1], text[7], text[14]), ('2', 'س', 's'));
    assert!(x(1) < x(7) && x(7) < x(14), "{} {} {}", x(1), x(7), x(14));
}

// The panel's sentences that start with a friend's name, and lines that give
// a name and then a number, as the stats panel does, keep reading as English
// when the name is Arabic: the name on the left, the rest after it.
#[test]
fn arabic_name_in_an_english_line() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let name = "سارة 2";
    for line in [
        messages::wants_control(name),
        messages::controlling_this_pc(name),
        format!("{}, 10 ms (2 frames)", messages::isolated(name)),
        format!("{} 10.5%", messages::isolated(name)),
    ] {
        let (galley, _, _) = frame(&ctx, Vec::new(), Modifiers::NONE, |ui| {
            let job = LayoutJob::simple(line.clone(), theme::body(), CHALK, f32::INFINITY);
            ui.painter().layout_job(job)
        });
        // The isolate, the six chars of the name, its end, then the rest.
        let row = &galley.rows[0].row;
        let name_right = row.glyphs[1..7]
            .iter()
            .map(|g| g.max_x())
            .fold(0.0, f32::max);
        let rest_left = row.glyphs[8..]
            .iter()
            .map(|g| g.pos.x)
            .fold(f32::MAX, f32::min);
        assert!(
            name_right <= rest_left + 0.5,
            "{line:?}: {name_right} {rest_left}"
        );
    }
}

fn press(at: Pos2, pressed: bool) -> Vec<Event> {
    vec![
        Event::PointerMoved(at),
        Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        },
    ]
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

// One frame of the composer, focused, in the smallest window.
fn composer(
    ctx: &Context,
    draft: &mut String,
    events: Vec<Event>,
    modifiers: Modifiers,
) -> (Response, Vec<(Pos2, Arc<Galley>)>, PlatformOutput) {
    frame(ctx, events, modifiers, |ui| {
        let response = controls::composer(ui, "composer", draft, "Message", 2048);
        response.request_focus();
        response
    })
}

// The typed text as the composer painted it: where it is, and the galley.
fn typed(painted: &[(Pos2, Arc<Galley>)], draft: &str) -> (Pos2, Arc<Galley>) {
    painted
        .iter()
        .find(|(_, galley)| galley.text() == draft)
        .cloned()
        .expect("the draft was painted")
}

fn caret(ctx: &Context, response: &Response) -> usize {
    let state = TextEditState::load(ctx, response.id).expect("the composer has state");
    state.cursor.char_range().expect("a caret").primary.index.0
}

// A click at `from`, then a Shift+click at `to`, then Ctrl+C: what it copied.
fn select_and_copy(ctx: &Context, draft: &mut String, from: Pos2, to: Pos2) -> Option<String> {
    composer(ctx, draft, press(from, true), Modifiers::NONE);
    composer(ctx, draft, press(from, false), Modifiers::NONE);
    composer(ctx, draft, press(to, true), Modifiers::SHIFT);
    composer(ctx, draft, press(to, false), Modifiers::SHIFT);
    let (_, _, output) = composer(ctx, draft, vec![Event::Copy], Modifiers::NONE);
    copied(&output).map(str::to_owned)
}

// The composer moves what is typed to the right edge once it starts with an
// Arabic letter and back when it does not, the caret after digits typed at
// the end of an Arabic line follows them, and a click, a selection and
// Ctrl+C work on the mixed text.
#[test]
fn the_composer_follows_what_is_typed() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut draft = String::new();
    composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    let (field, _, _) = composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);

    let arabic = vec![Event::Text(String::from("مرحبا"))];
    composer(&ctx, &mut draft, arabic, Modifiers::NONE);
    // The side is chosen from the text a frame starts with.
    let (_, painted, _) = composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    let (pos, galley) = typed(&painted, "مرحبا");
    let text_right = pos.x + galley.rect.right();
    assert!(
        (field.rect.right() - text_right).abs() <= 1.0,
        "{text_right} {:?}",
        field.rect
    );

    let digits = vec![Event::Text(String::from(" 123"))];
    composer(&ctx, &mut draft, digits, Modifiers::NONE);
    let (response, painted, _) = composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    assert_eq!(caret(&ctx, &response), 9);
    let (pos, galley) = typed(&painted, "مرحبا 123");
    let row = &galley.rows[0];
    let three = row.glyphs.last().expect("the 3");
    let at = |index: usize| {
        let x = pos.x + row.pos.x + row.x_offset(CharIndex(index));
        pos2(x, pos.y + row.pos.y + row.height() / 2.0)
    };
    let end = CCursor {
        index: CharIndex(9),
        prefer_next_row: false,
    };
    let caret_x = pos.x + galley.pos_from_cursor(end).min.x;
    assert_eq!(caret_x, pos.x + row.pos.x + three.max_x());
    assert!(
        caret_x < pos.x + row.pos.x + row.glyphs[4].pos.x,
        "{caret_x}"
    );

    // A click just right of "123" puts the caret after it.
    composer(&ctx, &mut draft, press(at(9), true), Modifiers::NONE);
    let (response, _, _) = composer(&ctx, &mut draft, press(at(9), false), Modifiers::NONE);
    assert_eq!(caret(&ctx, &response), 9);

    // From the right end to the space is the Arabic word, from the space to
    // after the digits is " 123", each copied as it was typed.
    assert_eq!(
        select_and_copy(&ctx, &mut draft, at(0), at(5)).as_deref(),
        Some("مرحبا")
    );
    assert_eq!(
        select_and_copy(&ctx, &mut draft, at(5), at(9)).as_deref(),
        Some(" 123")
    );
    assert_eq!(
        select_and_copy(&ctx, &mut draft, at(2), at(7)).as_deref(),
        Some("حبا 1")
    );

    // Ctrl+A and Ctrl+C take all of it, in the order it was typed.
    let select_all = vec![key(Key::A, Modifiers::COMMAND), Event::Copy];
    let (_, _, output) = composer(&ctx, &mut draft, select_all, Modifiers::COMMAND);
    assert_eq!(copied(&output), Some("مرحبا 123"));

    // A draft that starts in English is on the left.
    let mut english = String::from("ok مرحبا");
    composer(&ctx, &mut english, Vec::new(), Modifiers::NONE);
    let (field, painted, _) = composer(&ctx, &mut english, Vec::new(), Modifiers::NONE);
    let (pos, galley) = typed(&painted, "ok مرحبا");
    let text_left = pos.x + galley.rect.left();
    assert!((text_left - field.rect.left()).abs() <= 1.0, "{text_left}");
}

// In the composer a click and a Shift+click highlight exactly the letters
// they select. The end of the Arabic word with the start of "hello 123" is
// two runs on the row, apart because the rest of the English sits between.
#[test]
fn the_composer_highlights_what_is_selected() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut draft = String::from("مرحبا hello 123");
    composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    let (_, painted, _) = composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    let (pos, galley) = typed(&painted, &draft);
    let row = &galley.rows[0];
    let at = |index: usize| {
        let x = pos.x + row.pos.x + row.x_offset(CharIndex(index));
        pos2(x, pos.y + row.pos.y + row.height() / 2.0)
    };
    for (from, to, runs) in [(2, 9, 2), (0, 5, 1), (6, 15, 1), (0, 15, 1), (3, 13, 2)] {
        let copied = select_and_copy(&ctx, &mut draft, at(from), at(to));
        assert_eq!(copied.as_deref(), Some(chars(&draft, from, to).as_str()));
        let (_, painted, _) = composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
        let (_, shown) = typed(&painted, &draft);
        assert_eq!(check_highlight(&shown, &(from..to)), [runs], "{from}..{to}");
    }
}

// Shift+Right from between ب and its kasra selects the kasra alone, which
// only the keyboard can do. The kasra turns ink like any selected char, so
// the highlight goes under ب, which it is drawn on, or it would vanish on
// the panel.
#[test]
fn mark_highlighted_on_its_letter() {
    let ctx = Context::default();
    theme::apply(&ctx);
    let mut draft = String::from("بِسْمِ");
    composer(&ctx, &mut draft, Vec::new(), Modifiers::NONE);
    let to_kasra = vec![
        key(Key::Home, Modifiers::NONE),
        key(Key::ArrowRight, Modifiers::NONE),
    ];
    composer(&ctx, &mut draft, to_kasra, Modifiers::NONE);
    let over_kasra = vec![key(Key::ArrowRight, Modifiers::SHIFT)];
    composer(&ctx, &mut draft, over_kasra, Modifiers::SHIFT);
    let (_, painted, output) = composer(&ctx, &mut draft, vec![Event::Copy], Modifiers::NONE);
    assert_eq!(copied(&output), Some("\u{650}"));
    let (_, shown) = typed(&painted, &draft);
    assert_eq!(check_highlight(&shown, &(1..2)), [1]);
    let row = &shown.rows[0].row;
    assert!(covers(&highlight(row)[0], &row.glyphs[0]));
}

// What a friend can send: any mix of Arabic, Hebrew, Latin, digits,
// brackets, combining marks, joiners, emoji and control characters, and
// now and then any character at all.
const PIECES: &[&str] = &[
    "مرحبا",
    "عالم",
    "لا",
    "شكراً",
    "بِسْمِ",
    "שלום",
    "hello",
    "123",
    "١٢٣",
    "4.5%",
    " ",
    "\n",
    "\t",
    "(",
    ")",
    "[",
    "]",
    "\u{64E}",
    "\u{301}",
    "\u{200D}",
    "\u{200C}",
    "\u{200F}",
    "\u{202E}",
    "\u{2067}",
    "\u{2069}",
    "\u{1F600}",
    "\u{1F469}\u{200D}\u{1F4BB}",
    "\u{1F1EF}\u{1F1F5}",
    "\u{7}",
    "\u{FEFF}",
    "ـ",
];

fn chat_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        8 => prop::sample::select(PIECES).prop_map(str::to_owned),
        1 => any::<char>().prop_map(String::from),
    ];
    prop::collection::vec(piece, 0..24).prop_map(|pieces| pieces.concat())
}

// One glyph per character, and every caret place on its row. In a row with
// right-to-left text, which this port lays out, a click at a caret's x also
// gives back a caret drawn at that x. Left-to-right rows keep egui's own
// rule, where a caret between a letter and a combining accent the font
// draws over it sits inside the letter, so there only the index is checked.
//
// Plex has no glyph of its own for some rarer letters, among them the dotted
// ones of Arabic transliteration, and the shaper builds each from a letter
// and its accent: two glyphs for one char, as in egui 0.36.2, which puts the
// caret one place off after such a letter. Text with one of those skips the
// count.
fn check_rows(galley: &Galley, pieced: bool) -> Result<(), TestCaseError> {
    let text = galley.text();
    let counted: usize = galley
        .rows
        .iter()
        .map(|row| row.char_count_including_newline().0)
        .sum();
    if !pieced {
        prop_assert_eq!(counted, text.chars().count(), "{:?}", text);
    }
    for row in &galley.rows {
        let glyph_left = row.glyphs.iter().map(|g| g.pos.x).fold(0.0, f32::min);
        let glyph_right = row
            .glyphs
            .iter()
            .map(|g| g.max_x())
            .fold(row.size.x, f32::max);
        let bidi = row.glyphs.iter().any(|g| g.is_rtl());
        for index in 0..=row.glyphs.len() {
            let x = row.x_offset(CharIndex(index));
            prop_assert!(
                glyph_left - 1.0 <= x && x <= glyph_right + 1.0,
                "{:?} {}: {} is off {}..{}",
                text,
                index,
                x,
                glyph_left,
                glyph_right
            );
            let back = row.char_at(x);
            prop_assert!(back.0 <= row.glyphs.len());
            if bidi {
                prop_assert_eq!(row.x_offset(back), x, "{:?} {}", text, index);
            }
        }
    }
    Ok(())
}

// Whether the fonts draw this char from more than one glyph.
fn pieced(ui: &Ui, c: char) -> bool {
    let galley = ui
        .painter()
        .layout_no_wrap(c.to_string(), theme::body(), CHALK);
    galley
        .rows
        .iter()
        .map(|row| row.glyphs.len())
        .sum::<usize>()
        > 1
}

// Laying out needs fonts, and a context gets them on its first frame; one
// context serves every case.
fn shared_context() -> &'static Context {
    static CONTEXT: OnceLock<Context> = OnceLock::new();
    CONTEXT.get_or_init(|| {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx
    })
}

// Two caret places anywhere from the start of the text to past its end,
// each on either side of a row break.
fn selection() -> impl Strategy<Value = CCursorRange> {
    let end = (0..130usize, any::<bool>()).prop_map(|(index, prefer_next_row)| CCursor {
        index: CharIndex(index),
        prefer_next_row,
    });
    (end.clone(), end).prop_map(|(a, b)| CCursorRange::two(a, b))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Laid out as a message at two widths, as the composer lays it out, and
    // as a name cut to one row, it never panics, keeps one glyph per
    // character, and every caret place lies on its row. Selected anywhere,
    // it paints without a panic, rows with no right-to-left letter come out
    // as 0.36.2 paints them, and with one glyph per character the highlight
    // on the other rows is exactly the selected letters.
    #[test]
    fn any_chat_text(
        text in chat_text(),
        selections in prop::collection::vec(selection(), 1..4),
    ) {
        let ctx = shared_context();
        let ((name, galleys, any_pieced), _, _) = frame(ctx, Vec::new(), Modifiers::NONE, |ui| {
            let format = TextFormat::simple(theme::body(), CHALK);
            let mut composer_job = LayoutJob::single_section(text.clone(), format.clone());
            composer_job.wrap.max_width = 336.0;
            composer_job.keep_trailing_whitespace = true;
            if controls::right_to_left(&text) {
                composer_job.halign = Align::RIGHT;
            }
            let mut name_job = LayoutJob::single_section(text.clone(), format);
            name_job.wrap.max_width = 90.0;
            name_job.wrap.max_rows = 1;
            name_job.wrap.break_anywhere = true;
            let galleys = [
                body_galley(ui, &text, COLUMN),
                body_galley(ui, &text, 90.0),
                ui.painter().layout_job(composer_job),
            ];
            let any_pieced = text.chars().any(|c| pieced(ui, c));
            (ui.painter().layout_job(name_job), galleys, any_pieced)
        });
        prop_assert!(name.rows.len() <= 1);
        for galley in &galleys {
            check_rows(galley, any_pieced)?;
        }
        let visuals = ctx.global_style().visuals.clone();
        let len = text.chars().count();
        for range in &selections {
            // The name's cut-off row ends in a char that is not in the text.
            paint_with(paint_text_selection, &name, &visuals, range);
            let [min, max] = range.sorted_cursors();
            let selected = min.index.0.min(len)..max.index.0.min(len);
            for galley in &galleys {
                // Not check_row_highlight on those: 0.36.2 leaves the spaces
                // an aligned row starts with, which the layout hangs left of
                // the row, out of the highlight, and can lay a sliver over
                // them.
                compare_left_to_right_rows(galley, &visuals, range, &text);
                let (painted, _) = paint_with(paint_text_selection, galley, &visuals, range);
                let mut start = 0;
                for placed in &painted.rows {
                    if !any_pieced && placed.glyphs.iter().any(Glyph::is_rtl) {
                        check_row_highlight(&text, placed, start, &selected);
                    }
                    start += placed.char_count_including_newline().0;
                }
            }
        }
    }
}

// egui 0.36.2's selection painter as released, from its
// src/text_selection/visuals.rs, unchanged but for its name and with its
// comments left out: what rows with no right-to-left letter must still get.
fn paint_text_selection_0_36_2(
    galley: &mut Arc<Galley>,
    visuals: &Visuals,
    cursor_range: &CCursorRange,
    mut new_vertex_indices: Option<&mut Vec<RowVertexIndices>>,
) {
    if cursor_range.is_empty() {
        return;
    }

    let galley: &mut Galley = Arc::make_mut(galley);

    let background_color = visuals.selection.bg_fill;
    let text_color = visuals.selection.stroke.color;

    let [min, max] = cursor_range.sorted_cursors();
    let min = galley.layout_from_cursor(min);
    let max = galley.layout_from_cursor(max);

    for ri in min.row..=max.row {
        let placed_row = &mut galley.rows[ri];
        let row = Arc::make_mut(&mut placed_row.row);

        let left = if ri == min.row {
            row.x_offset(min.column)
        } else {
            0.0
        };
        let right = if ri == max.row {
            row.x_offset(max.column)
        } else {
            let newline_size = if placed_row.ends_with_newline {
                row.height() / 2.0
            } else {
                0.0
            };
            row.size.x + newline_size
        };

        let rect = Rect::from_min_max(pos2(left, 0.0), pos2(right, row.size.y));
        let mesh = &mut row.visuals.mesh;

        if !row.glyphs.is_empty() {
            let first_glyph_index = if ri == min.row { min.column.0 } else { 0 };
            let last_glyph_index = if ri == max.row {
                max.column.0
            } else {
                row.glyphs.len()
            };

            let first_vertex_index = row
                .glyphs
                .get(first_glyph_index)
                .map_or(row.visuals.glyph_vertex_range.end, |g| g.first_vertex as _);
            let last_vertex_index = row
                .glyphs
                .get(last_glyph_index)
                .map_or(row.visuals.glyph_vertex_range.end, |g| g.first_vertex as _);

            for vi in first_vertex_index..last_vertex_index {
                mesh.vertices[vi].color = text_color;
            }
        }

        let glyph_index_start = row.visuals.glyph_index_start;

        let num_indices_before = mesh.indices.len();
        mesh.add_colored_rect(rect, background_color);
        assert_eq!(
            num_indices_before + 6,
            mesh.indices.len(),
            "We expect exactly 6 new indices"
        );

        let selection_triangles = [
            mesh.indices[num_indices_before],
            mesh.indices[num_indices_before + 1],
            mesh.indices[num_indices_before + 2],
            mesh.indices[num_indices_before + 3],
            mesh.indices[num_indices_before + 4],
            mesh.indices[num_indices_before + 5],
        ];

        for i in (glyph_index_start..num_indices_before).rev() {
            mesh.indices.swap(i, i + 6);
        }
        mesh.indices[glyph_index_start..glyph_index_start + 6]
            .clone_from_slice(&selection_triangles);

        row.visuals.mesh_bounds = mesh.calc_bounds();

        if let Some(new_vertex_indices) = &mut new_vertex_indices {
            new_vertex_indices.push(RowVertexIndices {
                row: ri,
                vertex_indices: selection_triangles,
            });
        }
    }
}
