use core::ops::Range;
use std::sync::Arc;

use emath::Pos2;
use epaint::{
    Color32, Stroke,
    text::{
        CharIndex, Glyph, Row, RowVisuals,
        cursor::{CCursor, LayoutCursor},
    },
};

use crate::{
    Galley, Painter, Rect, Ui, Visuals, pos2, text_selection::text_cursor_state::cursor_rect, vec2,
};

use super::CCursorRange;

#[derive(Clone, Debug)]
pub struct RowVertexIndices {
    pub row: usize,
    pub vertex_indices: [u32; 6],
}

/// Adds text selection rectangles to the galley.
///
/// A row with right-to-left text gets one rectangle per run of selected
/// glyphs that sit next to each other on the row: there the chars between
/// two carets need not be one stretch of the row.
pub fn paint_text_selection(
    galley: &mut Arc<Galley>,
    visuals: &Visuals,
    cursor_range: &CCursorRange,
    mut new_vertex_indices: Option<&mut Vec<RowVertexIndices>>,
) {
    if cursor_range.is_empty() {
        return;
    }

    // We need to modify the galley (add text selection painting to it),
    // and so we need to clone it if it is shared:
    let galley: &mut Galley = Arc::make_mut(galley);

    let background_color = visuals.selection.bg_fill;
    let text_color = visuals.selection.stroke.color;

    let [min, max] = cursor_range.sorted_cursors();
    let min = galley.layout_from_cursor(min);
    let max = galley.layout_from_cursor(max);

    for ri in min.row..=max.row {
        if galley.rows[ri].glyphs.iter().any(Glyph::is_rtl) {
            let glyph_count = galley.rows[ri].glyphs.len();
            let first = if ri == min.row {
                min.column.0.min(glyph_count)
            } else {
                0
            };
            let end = if ri == max.row {
                max.column.0.min(glyph_count)
            } else {
                glyph_count
            };
            let newline = (ri != max.row && galley.rows[ri].ends_with_newline)
                .then(|| paragraph_is_rtl(galley, ri));

            let row = Arc::make_mut(&mut galley.rows[ri].row);
            for rect in bidi_selection_rects(row, &(first..end), newline) {
                let selection_triangles =
                    insert_selection_rect(&mut row.visuals, rect, background_color);
                if let Some(new_vertex_indices) = &mut new_vertex_indices {
                    new_vertex_indices.push(RowVertexIndices {
                        row: ri,
                        vertex_indices: selection_triangles,
                    });
                }
            }
            recolor_glyphs(row, first..end, text_color);
            row.visuals.mesh_bounds = row.visuals.mesh.calc_bounds();
            continue;
        }

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
                row.height() / 2.0 // visualize that we select the newline
            } else {
                0.0
            };
            row.size.x + newline_size
        };

        let rect = Rect::from_min_max(pos2(left, 0.0), pos2(right, row.size.y));
        let mesh = &mut row.visuals.mesh;

        if !row.glyphs.is_empty() {
            // Change color of the selected text:
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

        let selection_triangles = insert_selection_rect(&mut row.visuals, rect, background_color);

        row.visuals.mesh_bounds = row.visuals.mesh.calc_bounds();

        if let Some(new_vertex_indices) = &mut new_vertex_indices {
            new_vertex_indices.push(RowVertexIndices {
                row: ri,
                vertex_indices: selection_triangles,
            });
        }
    }
}

/// Puts `rect` into the row's mesh behind its glyphs and returns the six
/// vertex indices of its two triangles.
fn insert_selection_rect(row_visuals: &mut RowVisuals, rect: Rect, color: Color32) -> [u32; 6] {
    let mesh = &mut row_visuals.mesh;

    // Time to insert the selection rectangle into the row mesh.
    // It should be on top (after) of any background in the galley,
    // but behind (before) any glyphs. The row visuals has this information:
    let glyph_index_start = row_visuals.glyph_index_start;

    // Start by appending the selection rectangle to end of the mesh, as two triangles (= 6 indices):
    let num_indices_before = mesh.indices.len();
    mesh.add_colored_rect(rect, color);
    assert_eq!(
        num_indices_before + 6,
        mesh.indices.len(),
        "We expect exactly 6 new indices"
    );

    // Copy out the new triangles:
    let selection_triangles = [
        mesh.indices[num_indices_before],
        mesh.indices[num_indices_before + 1],
        mesh.indices[num_indices_before + 2],
        mesh.indices[num_indices_before + 3],
        mesh.indices[num_indices_before + 4],
        mesh.indices[num_indices_before + 5],
    ];

    // Move every old triangle forwards by 6 indices to make room for the new triangle:
    for i in (glyph_index_start..num_indices_before).rev() {
        mesh.indices.swap(i, i + 6);
    }
    // Put the new triangle in place:
    mesh.indices[glyph_index_start..glyph_index_start + 6].clone_from_slice(&selection_triangles);

    selection_triangles
}

/// The selection rectangles of a row with right-to-left text: the glyphs
/// `selected` by char index, merged into runs that sit next to each other
/// on the row, one rectangle each. `newline` is set when the selection goes
/// on past the newline that ends the row, to whether its paragraph reads
/// right to left.
fn bidi_selection_rects(row: &Row, selected: &Range<usize>, newline: Option<bool>) -> Vec<Rect> {
    // A glyph that takes no room, a mark or the rest of a cluster, is drawn
    // over the one before it and would only split a run. When it is
    // selected and its letter is not, the letter's place is highlighted,
    // or the mark, in the selected text color, would vanish on the panel.
    let mut placed: Vec<(f32, f32, bool)> = Vec::new();
    let mut letter: Option<usize> = None;
    for (i, glyph) in row.glyphs.iter().enumerate() {
        let is_selected = selected.contains(&i);
        if joins_previous(&row.glyphs, i) {
            if let Some(letter) = letter {
                placed[letter].2 |= is_selected;
            }
        } else if 0.0 < glyph.advance_width {
            letter = Some(placed.len());
            placed.push((glyph.pos.x, glyph.max_x(), is_selected));
        } else {
            letter = None;
        }
    }
    if let Some(right_to_left) = newline {
        // Half the row's height past the end the paragraph reads towards,
        // as egui shows it after a left-to-right row. That end is taken from
        // the glyphs, not the row's rect, so it sits where the letters end
        // whatever width the row was given.
        let width = row.height() / 2.0;
        let left_end = placed.iter().map(|&(left, _, _)| left).fold(0.0, f32::min);
        let right_end = placed
            .iter()
            .map(|&(_, right, _)| right)
            .fold(row.size.x, f32::max);
        placed.push(if right_to_left {
            (left_end - width, left_end, true)
        } else {
            (right_end, right_end + width, true)
        });
    }
    placed.sort_by(|a, b| a.0.total_cmp(&b.0));

    // Glyphs sit on whole pixels and their advances are not whole, so a run
    // ends where the next glyph on the row starts, which keeps it out of
    // that glyph and leaves no hairline before it.
    let rect = |left: f32, right: f32| Rect::from_min_max(pos2(left, 0.0), pos2(right, row.size.y));
    let mut rects = Vec::new();
    let mut run: Option<(f32, f32)> = None;
    for (left, right, is_selected) in placed {
        match (run, is_selected) {
            (None, true) => run = Some((left, right)),
            (Some((start, end)), true) => run = Some((start, end.max(right))),
            (Some((start, _)), false) => {
                rects.push(rect(start, left));
                run = None;
            }
            (None, false) => {}
        }
    }
    if let Some((start, end)) = run {
        rects.push(rect(start, end));
    }
    rects
}

/// Whether glyph `i` is drawn as one with the glyph before it: a glyph that
/// takes no room, of the same bidi level. The same rule as the vendored
/// epaint's, where it is private to the crate.
fn joins_previous(glyphs: &[Glyph], i: usize) -> bool {
    0 < i && glyphs[i].advance_width <= 0.0 && glyphs[i].bidi_level == glyphs[i - 1].bidi_level
}

/// Whether the paragraph that row `ri` ends reads right to left. Its bidi
/// level is the lowest of its glyphs': its first strong letter has it, and
/// so does the whitespace at the end of each of its rows (UAX #9 rule L1).
fn paragraph_is_rtl(galley: &Galley, ri: usize) -> bool {
    let start = galley.rows[..ri]
        .iter()
        .rposition(|placed_row| placed_row.ends_with_newline)
        .map_or(0, |previous| previous + 1);
    galley.rows[start..=ri]
        .iter()
        .flat_map(|placed_row| &placed_row.glyphs)
        .map(|glyph| glyph.bidi_level)
        .min()
        .is_some_and(|level| level % 2 == 1)
}

/// Gives the glyphs `selected` by char index the selected text color, glyph
/// by glyph. The glyphs are tessellated in char order, so a glyph's
/// vertices run up to the next glyph's first.
fn recolor_glyphs(row: &mut Row, selected: Range<usize>, color: Color32) {
    for i in selected {
        let first = row.glyphs[i].first_vertex as usize;
        let end = row
            .glyphs
            .get(i + 1)
            .map_or(row.visuals.glyph_vertex_range.end, |next| {
                next.first_vertex as usize
            });
        for vertex in &mut row.visuals.mesh.vertices[first..end] {
            vertex.color = color;
        }
    }
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn paint_ime_preedit_text_visuals(
    pos: Pos2,
    ui: &Ui,
    painter: &Painter,
    galley: &Arc<Galley>,
    row_height: f32,
    preedit_range: core::ops::Range<CCursor>,
    mut relative_active_range: Option<core::ops::Range<CCursor>>,
    time_since_last_interaction: f64,
) {
    /// Instead of implementing [`PartialOrd`] and [`Ord`] for [`CCursor`] to
    /// make [`std::ops::Range::is_empty`] available, we use this helper
    /// function instead.
    ///
    /// These traits are intentionally not implemented because
    /// [`CCursor::prefer_next_row`] makes it difficult to define a clear
    /// ordering between two [`CCursor`]s.
    fn is_cursor_range_empty(range: &core::ops::Range<CCursor>) -> bool {
        range.start.index == range.end.index
    }

    if is_cursor_range_empty(&preedit_range) {
        return;
    }

    if let Some(relative_active_range) = &mut relative_active_range
        && relative_active_range.end.index > preedit_range.end.index - preedit_range.start.index
    {
        relative_active_range.end.index = preedit_range.end.index - preedit_range.start.index;
    }

    let visuals = ui.visuals();
    let active_underline_stroke = visuals.ime_composition.active_underline_stroke;
    let inactive_underline_stroke = visuals.ime_composition.inactive_underline_stroke;

    if let Some(relative_active_range) = &relative_active_range
        && !is_cursor_range_empty(relative_active_range)
    {
        if relative_active_range.start.index > CharIndex::ZERO {
            paint_underlines(
                pos,
                painter,
                galley,
                galley.layout_from_cursor(preedit_range.start),
                galley.layout_from_cursor(preedit_range.start + relative_active_range.start.index),
                inactive_underline_stroke,
            );
        }

        paint_underlines(
            pos,
            painter,
            galley,
            galley.layout_from_cursor(preedit_range.start + relative_active_range.start.index),
            galley.layout_from_cursor(preedit_range.start + relative_active_range.end.index),
            active_underline_stroke,
        );

        if !is_cursor_range_empty(
            &(relative_active_range.end..(preedit_range.end - preedit_range.start.index)),
        ) {
            paint_underlines(
                pos,
                painter,
                galley,
                galley.layout_from_cursor(preedit_range.start + relative_active_range.end.index),
                galley.layout_from_cursor(preedit_range.end),
                inactive_underline_stroke,
            );
        }
    } else {
        paint_underlines(
            pos,
            painter,
            galley,
            galley.layout_from_cursor(preedit_range.start),
            galley.layout_from_cursor(preedit_range.end),
            inactive_underline_stroke,
        );
    }

    if let Some(relative_active_range) = relative_active_range
        && is_cursor_range_empty(&relative_active_range)
    {
        let active_cursor = preedit_range.start + relative_active_range.start.index;
        let cursor_rect = cursor_rect(galley, &active_cursor, row_height);

        paint_text_cursor(
            ui,
            painter,
            cursor_rect.translate(pos.to_vec2()),
            time_since_last_interaction,
        );
    }
}

fn paint_underlines(
    pos: Pos2,
    painter: &Painter,
    galley: &Arc<Galley>,
    min: LayoutCursor,
    max: LayoutCursor,
    stroke: Stroke,
) {
    for ri in min.row..=max.row {
        let placed_row = &galley.rows[ri];
        let row = &placed_row.row;

        let left = if ri == min.row {
            row.x_offset(min.column)
        } else {
            0.0
        };
        let right = if ri == max.row {
            row.x_offset(max.column)
        } else {
            row.size.x
        };

        let offset_y = placed_row.pos.y + row.size.y;

        painter.line_segment(
            [pos + vec2(left, offset_y), pos + vec2(right, offset_y)],
            stroke,
        );
    }
}

/// Paint one end of the selection, e.g. the primary cursor.
///
/// This will never blink.
pub fn paint_cursor_end(painter: &Painter, visuals: &Visuals, cursor_rect: Rect) {
    let stroke = visuals.text_cursor.stroke;

    let top = cursor_rect.center_top();
    let bottom = cursor_rect.center_bottom();

    painter.line_segment([top, bottom], stroke);

    if false {
        // Roof/floor:
        let extrusion = 3.0;
        let width = 1.0;
        painter.line_segment(
            [top - vec2(extrusion, 0.0), top + vec2(extrusion, 0.0)],
            (width, stroke.color),
        );
        painter.line_segment(
            [bottom - vec2(extrusion, 0.0), bottom + vec2(extrusion, 0.0)],
            (width, stroke.color),
        );
    }
}

/// Paint one end of the selection, e.g. the primary cursor, with blinking (if enabled).
pub fn paint_text_cursor(
    ui: &Ui,
    painter: &Painter,
    primary_cursor_rect: Rect,
    time_since_last_interaction: f64,
) {
    if ui.visuals().text_cursor.blink {
        let on_duration = ui.visuals().text_cursor.on_duration;
        let off_duration = ui.visuals().text_cursor.off_duration;
        let total_duration = on_duration + off_duration;

        let time_in_cycle = (time_since_last_interaction % (total_duration as f64)) as f32;

        let wake_in = if time_in_cycle < on_duration {
            // Cursor is visible
            paint_cursor_end(painter, ui.visuals(), primary_cursor_rect);
            on_duration - time_in_cycle
        } else {
            // Cursor is not visible
            total_duration - time_in_cycle
        };

        ui.request_repaint_after_secs(wake_in);
    } else {
        paint_cursor_end(painter, ui.visuals(), primary_cursor_rect);
    }
}
