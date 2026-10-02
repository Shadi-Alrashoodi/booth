#![expect(clippy::unwrap_used)] // TODO(emilk): remove unwraps

use core::{iter, ops::Range};
use std::sync::Arc;

use emath::{Align, GuiRounding as _, NumExt as _, Pos2, Rect, Vec2, pos2, vec2};

use crate::{
    Color32, Mesh, Stroke, Vertex,
    stroke::PathStroke,
    text::{
        ByteIndex, ByteRange,
        font::{StyledMetrics, UvRect, is_cjk, is_cjk_break_allowed},
        fonts::FontFaceKey,
    },
};

use super::{
    ByteRangeExt as _, FontsImpl, Galley, Glyph, LayoutJob, LayoutSection, PlacedRow, Row,
    RowVisuals, VariationCoords,
    font::{Font, FontFace, ShapedGlyph},
    text_layout_types::joins_previous,
};

// ----------------------------------------------------------------------------

/// Returns `true` if the character is a Unicode combining mark (categories Mn, Mc, Me).
///
/// These characters modify the preceding base character and should not be
/// rendered as standalone replacement glyphs when the shaper can't handle them.
#[inline]
fn is_combining_mark(c: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// Represents GUI scale and convenience methods for rounding to pixels.
#[derive(Clone, Copy)]
struct PointScale {
    pub pixels_per_point: f32,
}

impl PointScale {
    #[inline(always)]
    pub fn new(pixels_per_point: f32) -> Self {
        Self { pixels_per_point }
    }

    #[inline(always)]
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    #[inline(always)]
    pub fn round_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).round() / self.pixels_per_point
    }

    #[inline(always)]
    pub fn floor_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).floor() / self.pixels_per_point
    }

    #[inline(always)]
    pub fn ceil_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).ceil() / self.pixels_per_point
    }
}

// ----------------------------------------------------------------------------

/// Temporary storage before line-wrapping.
#[derive(Clone)]
struct Paragraph {
    /// Start of the next glyph to be added. In screen-space / physical pixels.
    pub cursor_x_px: f32,

    /// This is included in case there are no glyphs
    pub section_index_at_start: u32,

    pub glyphs: Vec<Glyph>,

    /// In case of an empty paragraph ("\n"), use this as height.
    pub empty_paragraph_height: f32,

    /// Bidi level of the paragraph (UAX #9), 1 when its first strong
    /// character is right-to-left.
    pub bidi_level: u8,
}

impl Paragraph {
    pub fn from_section_index(section_index_at_start: u32) -> Self {
        Self {
            cursor_x_px: 0.0,
            section_index_at_start,
            glyphs: vec![],
            empty_paragraph_height: 0.0,
            bidi_level: 0,
        }
    }
}

/// Layout text into a [`Galley`].
///
/// In most cases you should use [`crate::FontsView::layout_job`] instead
/// since that memoizes the input, making subsequent layouting of the same text much faster.
pub fn layout(fonts: &mut FontsImpl, pixels_per_point: f32, job: Arc<LayoutJob>) -> Galley {
    profiling::function_scope!();

    job.debug_sanity_check();

    if job.wrap.max_rows == 0 {
        // Early-out: no text
        return Galley {
            job,
            rows: Default::default(),
            rect: Rect::ZERO,
            mesh_bounds: Rect::NOTHING,
            num_vertices: 0,
            num_indices: 0,
            pixels_per_point,
            elided: true,
            intrinsic_size: Vec2::ZERO,
        };
    }

    // For most of this we ignore the y coordinate:

    let bidi = BidiLevels::new(&job.text);

    let mut paragraphs = vec![Paragraph::from_section_index(0)];
    {
        let mut shape_buffer = fonts.take_shape_buffer();
        for (section_index, section) in job.sections.iter().enumerate() {
            let mut font = fonts.font(&section.format.font_id.family);
            shape_buffer = layout_section(
                &mut font,
                shape_buffer,
                pixels_per_point,
                &job,
                section_index as u32,
                bidi.as_ref(),
                &mut paragraphs,
            );
        }
        fonts.return_shape_buffer(shape_buffer);
    }

    let point_scale = PointScale::new(pixels_per_point);

    let intrinsic_size = calculate_intrinsic_size(point_scale, &job, &paragraphs);
    let paragraph_levels: Vec<u8> = paragraphs.iter().map(|p| p.bidi_level).collect();

    let mut elided = false;
    let mut rows = rows_from_paragraphs(paragraphs, &job, pixels_per_point, &mut elided);
    if elided && let Some(last_placed) = rows.last_mut() {
        let last_row = Arc::make_mut(&mut last_placed.row);
        replace_last_glyph_with_overflow_character(fonts, pixels_per_point, &job, last_row);
        if let Some(last) = last_row.glyphs.last() {
            last_row.size.x = last.max_x();
        }
    }

    // Up to here every glyph sits at its logical position. Now place
    // right-to-left text where a reader expects it. Each paragraph's rows
    // follow each other, and only its last one ends with a newline:
    if bidi.is_some() {
        let mut paragraph = 0;
        for placed_row in &mut rows {
            let paragraph_level = paragraph_levels.get(paragraph).copied().unwrap_or(0);
            if placed_row.ends_with_newline {
                paragraph += 1;
            }
            let row = Arc::make_mut(&mut placed_row.row);
            reset_trailing_whitespace(row, paragraph_level);
            if row.glyphs.iter().any(Glyph::is_rtl) {
                reorder_row_visually(point_scale, &job, row);
                // A row cut from a longer line, or cut short, was given the
                // width up to its last glyph in logical order. A right-to-left
                // mark there has no width and sits at the left edge of its
                // letter, which reaches further. And placed on whole pixels
                // in visual order, a glyph can end a little past the line.
                row.size.x = row
                    .glyphs
                    .iter()
                    .map(Glyph::max_x)
                    .fold(row.size.x, f32::max);
            }
        }
    }

    let justify = job.justify && job.wrap.max_width.is_finite();

    if justify || job.halign != Align::LEFT {
        let num_rows = rows.len();
        for (i, placed_row) in rows.iter_mut().enumerate() {
            let is_last_row = i + 1 == num_rows;
            let justify_row = justify && !placed_row.ends_with_newline && !is_last_row;
            halign_and_justify_row(
                point_scale,
                placed_row,
                job.halign,
                job.wrap.max_width,
                justify_row,
                job.keep_trailing_whitespace,
            );
        }
    }

    // Calculate the Y positions and tessellate the text:
    galley_from_rows(point_scale, job, rows, elided, intrinsic_size)
}

/// Shared context for emitting shaped glyphs into a [`Paragraph`].
struct ShapingContext {
    pixels_per_point: f32,
    font_size: f32,
    line_height: f32,
    extra_letter_spacing: f32,
    section_index: u32,
    font_metrics: StyledMetrics,
    is_first_glyph_in_section: bool,
    prev_cluster: Option<u32>,

    /// Bidi embedding level of the run being emitted.
    bidi_level: u8,
}

impl ShapingContext {
    fn glyph(
        &self,
        chr: char,
        physical_x: i32,
        advance_width_px: f32,
        face_metrics: &StyledMetrics,
        uv_rect: UvRect,
    ) -> Glyph {
        Glyph {
            chr,
            pos: pos2(physical_x as f32 / self.pixels_per_point, f32::NAN),
            advance_width: advance_width_px / self.pixels_per_point,
            line_height: self.line_height,
            font_face_height: face_metrics.row_height,
            font_face_ascent: face_metrics.ascent,
            font_height: self.font_metrics.row_height,
            font_ascent: self.font_metrics.ascent,
            uv_rect,
            bidi_level: self.bidi_level,
            section_index: self.section_index,
            first_vertex: 0,
        }
    }
}

/// Produced by [`segment_into_runs`] for text shaping.
#[derive(Debug)]
struct TextRun {
    /// Which font face should shape this run.
    font_key: FontFaceKey,

    /// Byte range within the section text.
    byte_range: ByteRange,

    /// Bidi embedding level (UAX #9) shared by every char in the run.
    /// Odd means right-to-left.
    bidi_level: u8,
}

/// Emit shaped glyphs from a [`harfrust::GlyphBuffer`] into a [`Paragraph`].
///
/// When a cluster maps multiple characters to fewer glyphs (e.g. flag emojis,
/// ligatures), zero-width "continuation" glyphs are emitted for the extra
/// characters so that `glyphs.len() == char_count`, an invariant that all
/// cursor and selection code relies on.
///
/// The glyphs are emitted in _logical_ order with increasing x, whatever the
/// run's direction; a right-to-left row is put in visual order later, by
/// [`reorder_row_visually`], once the rows are known.
fn layout_shaped_run(
    font: &mut Font<'_>,
    run: &TextRun,
    run_text: &str,
    glyph_buffer: &harfrust::GlyphBuffer,
    face_metrics: &StyledMetrics,
    ctx: &mut ShapingContext,
    paragraph: &mut Paragraph,
) {
    let px_scale = face_metrics.px_scale_factor;

    // Reset cluster tracking: cluster values are byte offsets within run_text,
    // so they are not comparable across runs.
    ctx.prev_cluster = None;

    // Track how many glyphs we emit per cluster so we can add zero-width
    // continuation glyphs when a cluster has more chars than glyphs.
    let mut cluster_start_byte: usize = 0;
    let mut cluster_end_byte = run_text.len();
    let mut cluster_glyph_count: usize = 0;

    // The shaper returns a right-to-left run in visual order, where a base
    // comes after its marks. Walked backwards it is in logical order, clusters
    // and the glyphs inside them alike, so the continuation bookkeeping below
    // holds for both directions and a mark follows its base, which is what
    // `reorder_row_visually` moves together.
    let infos = glyph_buffer.glyph_infos();
    let positions = glyph_buffer.glyph_positions();
    let rtl = run.bidi_level % 2 == 1;
    let glyph_index = |k: usize| if rtl { infos.len() - 1 - k } else { k };

    // A zero-width glyph of a right-to-left run is placed from the pen of
    // the base walked just before it: in the shaper's order it came first and
    // shared that pen, and its offset is relative to it.
    let mut base_pen_px = paragraph.cursor_x_px;

    for k in 0..infos.len() {
        let (info, pos) = (&infos[glyph_index(k)], &positions[glyph_index(k)]);
        let glyph_id = skrifa::GlyphId::new(info.glyph_id);
        let cluster = info.cluster;
        let mut advance_width_px = pos.x_advance as f32 * px_scale;
        let x_offset_px = pos.x_offset as f32 * px_scale;
        let y_offset_px = -(pos.y_offset as f32 * px_scale); // harfrust Y+ up → screen Y+ down

        // Apply extra_letter_spacing only at cluster boundaries,
        // never between glyphs within the same cluster (e.g. base + mark).
        let is_new_cluster = ctx.prev_cluster.is_none_or(|pc| pc != cluster);
        if is_new_cluster {
            if ctx.prev_cluster.is_some() {
                emit_continuation_glyphs(
                    ctx,
                    paragraph,
                    run_text,
                    cluster_start_byte..cluster as usize,
                    cluster_glyph_count,
                    face_metrics,
                );
            }
            if !ctx.is_first_glyph_in_section {
                paragraph.cursor_x_px += ctx.extra_letter_spacing * ctx.pixels_per_point;
            }
            cluster_start_byte = cluster as usize;
            cluster_end_byte = (k + 1..infos.len())
                .map(|next| infos[glyph_index(next)].cluster)
                .find(|&next| next != cluster)
                .map_or(run_text.len(), |next| next as usize);
            cluster_glyph_count = 0;
            ctx.is_first_glyph_in_section = false;
        }
        ctx.prev_cluster = Some(cluster);

        // The n-th glyph of a cluster stands for its n-th char, as the
        // continuation glyphs assume: a base and its marks each keep their own.
        let chr = run_text
            .get(cluster_start_byte..cluster_end_byte)
            .and_then(|s| s.chars().nth(cluster_glyph_count))
            .or_else(|| run_text.get(cluster as usize..)?.chars().next())
            .unwrap_or('\u{FFFD}'); // Unicode Replacement Character

        // Tab is a layout concept, not a glyph: the shaper doesn't know about tab stops.
        // Override the advance width using the font's configured tab size.
        if chr == '\t' {
            let tweak = font.fonts_by_id.get(&run.font_key).map(|ff| ff.tweak());
            let tab_size = tweak.map_or(4.0, |t| t.tab_size);
            let (_, space_info) = font.glyph_info(' ', face_metrics);
            let space_width_px = space_info.advance_width_unscaled.0 * px_scale;
            advance_width_px = tab_size * space_width_px;
        }

        // Thin space (U+2009) and narrow no-break space (U+202F):
        // override the shaper's advance width with the configured fraction of a space.
        if chr == '\u{2009}' || chr == '\u{202F}' {
            let tweak = font.fonts_by_id.get(&run.font_key).map(|ff| ff.tweak());
            let thin_space_width = tweak.map_or(0.5, |t| t.thin_space_width);
            let (_, space_info) = font.glyph_info(' ', face_metrics);
            let space_width_px = space_info.advance_width_unscaled.0 * px_scale;
            advance_width_px = thin_space_width * space_width_px;
        }

        let glyph = if glyph_id == skrifa::GlyphId::NOTDEF {
            // The shaper couldn't map this character. Drop combining marks
            // (Unicode category M) and duplicate NOTDEF glyphs within the same
            // cluster; only the first base character gets a replacement glyph.
            if is_combining_mark(chr) || !is_new_cluster {
                continue;
            }

            // Use the fallback font face (not run.font_key which returned NOTDEF).
            let fallback_key = font.resolve_face(chr);
            let fallback_metrics = font
                .fonts_by_id
                .get(&fallback_key)
                .map(|ff| {
                    ff.styled_metrics(ctx.pixels_per_point, ctx.font_size, &Default::default())
                })
                .unwrap_or_default();
            let (_, glyph_info) = font.glyph_info(chr, &fallback_metrics);
            let advance_width_px =
                glyph_info.advance_width_unscaled.0 * fallback_metrics.px_scale_factor;
            let (glyph_alloc, physical_x) =
                if let Some(ff) = font.fonts_by_id.get_mut(&fallback_key) {
                    ff.allocate_glyph(
                        font.atlas,
                        &fallback_metrics,
                        &ShapedGlyph {
                            glyph_id: glyph_info.id.unwrap_or(skrifa::GlyphId::NOTDEF),
                            h_pos: paragraph.cursor_x_px,
                            is_cjk: is_cjk(chr),
                        },
                    )
                } else {
                    Default::default()
                };

            base_pen_px = paragraph.cursor_x_px;
            paragraph.cursor_x_px += advance_width_px;

            ctx.glyph(
                chr,
                physical_x,
                advance_width_px,
                &fallback_metrics,
                glyph_alloc.uv_rect,
            )
        } else {
            let pen_px = if rtl && advance_width_px == 0.0 {
                base_pen_px
            } else {
                base_pen_px = paragraph.cursor_x_px;
                paragraph.cursor_x_px
            };
            // In a right-to-left run the shaper's x offset goes into the image
            // offset, as the y offset does, so the glyph itself sits on its pen
            // and the row can be mirrored block by block without letters
            // drifting apart at their joins.
            let h_pos = if rtl { pen_px } else { pen_px + x_offset_px };
            let (mut glyph_alloc, physical_x) =
                if let Some(ff) = font.fonts_by_id.get_mut(&run.font_key) {
                    ff.allocate_glyph(
                        font.atlas,
                        face_metrics,
                        &ShapedGlyph {
                            glyph_id,
                            h_pos,
                            is_cjk: is_cjk(chr),
                        },
                    )
                } else {
                    Default::default()
                };

            // Apply shaper y_offset; this varies per glyph instance so it
            // is not part of the cached ShapedGlyph / GlyphAllocation.
            glyph_alloc.uv_rect.offset.y += y_offset_px / ctx.pixels_per_point;
            if rtl {
                glyph_alloc.uv_rect.offset.x += x_offset_px / ctx.pixels_per_point;
            }

            paragraph.cursor_x_px += advance_width_px;

            ctx.glyph(
                chr,
                physical_x,
                advance_width_px,
                face_metrics,
                glyph_alloc.uv_rect,
            )
        };
        paragraph.glyphs.push(glyph);
        cluster_glyph_count += 1;
    }

    // Emit continuation glyphs for the last cluster in the run.
    if ctx.prev_cluster.is_some() {
        emit_continuation_glyphs(
            ctx,
            paragraph,
            run_text,
            cluster_start_byte..run_text.len(),
            cluster_glyph_count,
            face_metrics,
        );
    }
}

/// Emit zero-width continuation glyphs when a cluster has more characters than
/// shaped glyphs.
///
/// This preserves the invariant `glyphs.len() == char_count` that all cursor
/// and text-selection code depends on. Continuation glyphs have
/// [`UvRect::default()`] so [`tessellate_glyphs`] skips them entirely.
fn emit_continuation_glyphs(
    ctx: &ShapingContext,
    paragraph: &mut Paragraph,
    run_text: &str,
    cluster_bytes: Range<usize>,
    cluster_glyph_count: usize,
    face_metrics: &StyledMetrics,
) {
    let Some(cluster_text) = run_text.get(cluster_bytes) else {
        return;
    };
    let char_count = cluster_text.chars().count();
    if char_count <= cluster_glyph_count {
        return;
    }

    let physical_x = paragraph.cursor_x_px.round() as i32;

    for chr in cluster_text.chars().skip(cluster_glyph_count) {
        paragraph
            .glyphs
            .push(ctx.glyph(chr, physical_x, 0.0, face_metrics, UvRect::default()));
    }
}

// Ignores the Y coordinate.
#[must_use]
fn layout_section(
    font: &mut Font<'_>,
    mut shape_buffer: harfrust::UnicodeBuffer,
    pixels_per_point: f32,
    job: &LayoutJob,
    section_index: u32,
    bidi: Option<&BidiLevels>,
    out_paragraphs: &mut Vec<Paragraph>,
) -> harfrust::UnicodeBuffer {
    let section = &job.sections[section_index as usize];
    let LayoutSection {
        leading_space,
        byte_range,
        format,
    } = section;

    let font_size = format.font_id.size;
    let font_metrics = font.styled_metrics(pixels_per_point, font_size, &format.coords);
    let line_height = section
        .format
        .line_height
        .unwrap_or(font_metrics.row_height);
    let extra_letter_spacing = section.format.extra_letter_spacing;

    let mut paragraph = out_paragraphs.last_mut().unwrap();
    if paragraph.glyphs.is_empty() {
        paragraph.empty_paragraph_height = line_height;
    }
    paragraph.cursor_x_px += leading_space * pixels_per_point;

    let section_text = &job.text[byte_range.as_usize()];
    let mut ctx = ShapingContext {
        pixels_per_point,
        font_size,
        line_height,
        extra_letter_spacing,
        section_index,
        font_metrics,
        is_first_glyph_in_section: paragraph.glyphs.is_empty(),
        prev_cluster: None,
        bidi_level: 0,
    };
    let mut runs = Vec::new();

    // Where the current segment starts in `job.text`, to look up its bidi levels.
    let mut segment_start = byte_range.start.0;

    // Process each paragraph segment (split on newlines, which the shaper can't handle).
    for (seg_idx, segment) in SplitOrWhole::new(section_text, job.break_on_newline).enumerate() {
        let segment_levels = bidi.and_then(|bidi| {
            bidi.levels
                .get(segment_start..segment_start + segment.len())
        });
        let paragraph_level = bidi.map_or(0, |bidi| bidi.paragraph_level(segment_start));
        segment_start += segment.len() + 1; // and the `\n` it was split on

        if 0 < seg_idx {
            paragraph = out_paragraphs.push_mut(Paragraph::from_section_index(section_index));
            paragraph.empty_paragraph_height = line_height;
            ctx.is_first_glyph_in_section = true;
        }
        if paragraph.glyphs.is_empty() {
            paragraph.bidi_level = paragraph_level;
        }

        if segment.is_empty() {
            continue;
        }

        segment_into_runs(font, segment, segment_levels, &mut runs);

        let num_runs = runs.len();
        for (run_idx, run) in runs.iter().enumerate() {
            let run_text = &segment[run.byte_range.as_usize()];
            ctx.bidi_level = run.bidi_level;
            let Some(font_face) = font.fonts_by_id.get(&run.font_key) else {
                continue;
            };

            let face_metrics =
                font_face.styled_metrics(pixels_per_point, font_size, &format.coords);

            // Set buffer flags for paragraph boundary context.
            let mut flags = harfrust::BufferFlags::empty();
            if run_idx == 0 {
                flags |= harfrust::BufferFlags::BEGINNING_OF_TEXT;
            }
            if run_idx + 1 == num_runs {
                flags |= harfrust::BufferFlags::END_OF_TEXT;
            }

            let glyph_buffer = shape_text(
                font_face,
                run_text,
                run.bidi_level % 2 == 1,
                &format.coords,
                shape_buffer,
                flags,
            );

            layout_shaped_run(
                font,
                run,
                run_text,
                &glyph_buffer,
                &face_metrics,
                &mut ctx,
                paragraph,
            );

            shape_buffer = glyph_buffer.clear();
        }
    }

    shape_buffer
}

/// Iterator that either splits on `'\n'` or yields the whole string once.
/// Avoids `Box<dyn Iterator>` and `Vec<&str>` allocation.
enum SplitOrWhole<'a> {
    Split(core::str::Split<'a, char>),
    Whole(iter::Once<&'a str>),
}

impl<'a> SplitOrWhole<'a> {
    fn new(text: &'a str, split: bool) -> Self {
        if split {
            Self::Split(text.split('\n'))
        } else {
            Self::Whole(iter::once(text))
        }
    }
}

impl<'a> Iterator for SplitOrWhole<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Split(iter) => iter.next(),
            Self::Whole(iter) => iter.next(),
        }
    }
}

/// Calculate the intrinsic size of the text.
///
/// The result is eventually passed to `Response::intrinsic_size`.
/// This works by calculating the size of each `Paragraph` (instead of each `Row`).
fn calculate_intrinsic_size(
    point_scale: PointScale,
    job: &LayoutJob,
    paragraphs: &[Paragraph],
) -> Vec2 {
    let mut intrinsic_size = Vec2::ZERO;
    for (idx, paragraph) in paragraphs.iter().enumerate() {
        // Use the precise cursor position instead of `last_glyph.max_x()`,
        // because glyph positions are pixel-snapped but the cursor tracks
        // the exact subpixel advance. This makes sure that when two galleys are
        // placed side-by-side, the gap matches what it would be within a
        // single galley.
        let width = paragraph.cursor_x_px / point_scale.pixels_per_point;
        intrinsic_size.x = f32::max(intrinsic_size.x, width);

        let mut height = paragraph
            .glyphs
            .iter()
            .map(|g| g.line_height)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
            .unwrap_or(paragraph.empty_paragraph_height);
        if idx == 0 {
            height = f32::max(height, job.first_row_min_height);
        }
        intrinsic_size.y += point_scale.round_to_pixel(height);
    }
    intrinsic_size
}

// Ignores the Y coordinate.
fn rows_from_paragraphs(
    paragraphs: Vec<Paragraph>,
    job: &LayoutJob,
    pixels_per_point: f32,
    elided: &mut bool,
) -> Vec<PlacedRow> {
    let num_paragraphs = paragraphs.len();

    let mut rows = vec![];

    for (i, paragraph) in paragraphs.into_iter().enumerate() {
        if job.wrap.max_rows <= rows.len() {
            *elided = true;
            break;
        }

        let is_last_paragraph = (i + 1) == num_paragraphs;

        if paragraph.glyphs.is_empty() {
            rows.push(PlacedRow {
                pos: pos2(0.0, f32::NAN),
                row: Arc::new(Row {
                    section_index_at_start: paragraph.section_index_at_start,
                    glyphs: vec![],
                    visuals: Default::default(),
                    size: vec2(0.0, paragraph.empty_paragraph_height),
                }),
                ends_with_newline: !is_last_paragraph,
            });
        } else {
            // Use precise cursor position for width instead of pixel-snapped
            // `last_glyph.max_x()`, so that side-by-side galleys have the same
            // spacing as characters within a single galley.
            let paragraph_width = paragraph.cursor_x_px / pixels_per_point;
            if paragraph_width <= job.effective_wrap_width() {
                // Early-out optimization: the whole paragraph fits on one row.
                rows.push(PlacedRow {
                    pos: pos2(0.0, f32::NAN),
                    row: Arc::new(Row {
                        section_index_at_start: paragraph.section_index_at_start,
                        glyphs: paragraph.glyphs,
                        visuals: Default::default(),
                        size: vec2(paragraph_width, 0.0),
                    }),
                    ends_with_newline: !is_last_paragraph,
                });
            } else {
                line_break(&paragraph, job, &mut rows, elided);
                let placed_row = rows.last_mut().unwrap();
                placed_row.ends_with_newline = !is_last_paragraph;
            }
        }
    }

    rows
}

fn line_break(
    paragraph: &Paragraph,
    job: &LayoutJob,
    out_rows: &mut Vec<PlacedRow>,
    elided: &mut bool,
) {
    let wrap_width = job.effective_wrap_width();

    // Keeps track of good places to insert row break if we exceed `wrap_width`.
    let mut row_break_candidates = RowBreakCandidates::default();

    let mut first_row_indentation = paragraph.glyphs[0].pos.x;
    let mut row_start_x = 0.0;
    let mut row_start_idx = 0;

    for i in 0..paragraph.glyphs.len() {
        if job.wrap.max_rows <= out_rows.len() {
            *elided = true;
            break;
        }

        let potential_row_width = paragraph.glyphs[i].max_x() - row_start_x;

        if wrap_width < potential_row_width {
            // Row break:

            if first_row_indentation > 0.0
                && !row_break_candidates.has_good_candidate(job.wrap.break_anywhere)
            {
                // Allow the first row to be completely empty, because we know there will be more space on the next row:
                // TODO(emilk): this records the height of this first row as zero, though that is probably fine since first_row_indentation usually comes with a first_row_min_height.
                out_rows.push(PlacedRow {
                    pos: pos2(0.0, f32::NAN),
                    row: Arc::new(Row {
                        section_index_at_start: paragraph.section_index_at_start,
                        glyphs: vec![],
                        visuals: Default::default(),
                        size: Vec2::ZERO,
                    }),
                    ends_with_newline: false,
                });
                row_start_x += first_row_indentation;
                first_row_indentation = 0.0;
            } else if let Some(last_kept_index) = row_break_candidates.get(job.wrap.break_anywhere)
            {
                let glyphs: Vec<Glyph> = paragraph.glyphs[row_start_idx..=last_kept_index]
                    .iter()
                    .copied()
                    .map(|mut glyph| {
                        glyph.pos.x -= row_start_x;
                        glyph
                    })
                    .collect();

                let section_index_at_start = glyphs[0].section_index;
                let paragraph_max_x = glyphs.last().unwrap().max_x();

                out_rows.push(PlacedRow {
                    pos: pos2(0.0, f32::NAN),
                    row: Arc::new(Row {
                        section_index_at_start,
                        glyphs,
                        visuals: Default::default(),
                        size: vec2(paragraph_max_x, 0.0),
                    }),
                    ends_with_newline: false,
                });

                // Start a new row:
                row_start_idx = last_kept_index + 1;
                row_start_x = paragraph.glyphs[row_start_idx].pos.x;
                row_break_candidates.forget_before_idx(row_start_idx);
            } else {
                // Found no place to break, so we have to overrun wrap_width.
            }
        }

        row_break_candidates.add(i, &paragraph.glyphs[i..]);
    }

    if row_start_idx < paragraph.glyphs.len() {
        // Final row of text:

        if job.wrap.max_rows <= out_rows.len() {
            *elided = true; // can't fit another row
        } else {
            let paragraph_min_x = paragraph.glyphs[row_start_idx].pos.x - row_start_x;
            let paragraph_max_x = paragraph.glyphs.last().unwrap().max_x() - row_start_x;

            let glyphs: Vec<Glyph> = paragraph.glyphs[row_start_idx..]
                .iter()
                .copied()
                .map(|mut glyph| {
                    glyph.pos.x -= row_start_x + paragraph_min_x;
                    glyph
                })
                .collect();

            let section_index_at_start = glyphs[0].section_index;

            out_rows.push(PlacedRow {
                pos: pos2(paragraph_min_x, 0.0),
                row: Arc::new(Row {
                    section_index_at_start,
                    glyphs,
                    visuals: Default::default(),
                    size: vec2(paragraph_max_x - paragraph_min_x, 0.0),
                }),
                ends_with_newline: false,
            });
        }
    }
}

/// Trims the last glyphs in the row and replaces it with an overflow character (e.g. `…`).
///
/// Called before we have any Y coordinates.
fn replace_last_glyph_with_overflow_character(
    fonts: &mut FontsImpl,
    pixels_per_point: f32,
    job: &LayoutJob,
    row: &mut Row,
) {
    let Some(overflow_character) = job.wrap.overflow_character else {
        return;
    };

    let mut section_index = row
        .glyphs
        .last()
        .map(|g| g.section_index)
        .unwrap_or(row.section_index_at_start);
    loop {
        let section = &job.sections[section_index as usize];
        let extra_letter_spacing = section.format.extra_letter_spacing;
        let mut font = fonts.font(&section.format.font_id.family);
        let font_size = section.format.font_id.size;

        let font_id = font.resolve_face(overflow_character);
        let font_face_metrics = font
            .fonts_by_id
            .get(&font_id)
            .map(|f| f.styled_metrics(pixels_per_point, font_size, &section.format.coords))
            .unwrap_or_default();
        let (_, glyph_info) = font.glyph_info(overflow_character, &font_face_metrics);
        let mut font_face = font.fonts_by_id.get_mut(&font_id);

        let overflow_glyph_x = if let Some(prev_glyph) = row.glyphs.last() {
            // A right-to-left mark has no width and sits at the start of its
            // letter, so after one the overflow character goes where the
            // letter ends, or it is drawn over the letter.
            let end = if prev_glyph.is_rtl() {
                let mut letter = row.glyphs.len() - 1;
                while joins_previous(&row.glyphs, letter) {
                    letter -= 1;
                }
                row.glyphs[letter..]
                    .iter()
                    .map(Glyph::max_x)
                    .fold(prev_glyph.max_x(), f32::max)
            } else {
                prev_glyph.max_x()
            };
            end + extra_letter_spacing
        } else {
            0.0 // TODO(emilk): heed paragraph leading_space
        };
        let bidi_level = row.glyphs.last().map_or(0, |glyph| glyph.bidi_level);

        let advance_width_px =
            glyph_info.advance_width_unscaled.0 * font_face_metrics.px_scale_factor;
        let replacement_glyph_width = advance_width_px / pixels_per_point;

        // Check if we're within width budget:
        if overflow_glyph_x + replacement_glyph_width <= job.effective_wrap_width()
            || row.glyphs.is_empty()
        {
            // we are done

            let (replacement_glyph_alloc, physical_x) = font_face
                .as_mut()
                .map(|f| {
                    f.allocate_glyph(
                        font.atlas,
                        &font_face_metrics,
                        &ShapedGlyph {
                            glyph_id: glyph_info.id.unwrap_or(skrifa::GlyphId::NOTDEF),
                            h_pos: overflow_glyph_x * pixels_per_point,
                            is_cjk: is_cjk(overflow_character),
                        },
                    )
                })
                .unwrap_or_default();

            let font_metrics =
                font.styled_metrics(pixels_per_point, font_size, &section.format.coords);
            let line_height = section
                .format
                .line_height
                .unwrap_or(font_metrics.row_height);

            row.glyphs.push(Glyph {
                chr: overflow_character,
                pos: pos2(physical_x as f32 / pixels_per_point, f32::NAN),
                advance_width: advance_width_px / pixels_per_point,
                line_height,
                font_face_height: font_face_metrics.row_height,
                font_face_ascent: font_face_metrics.ascent,
                font_height: font_metrics.row_height,
                font_ascent: font_metrics.ascent,
                uv_rect: replacement_glyph_alloc.uv_rect,
                bidi_level,
                section_index,
                first_vertex: 0, // filled in later
            });
            return;
        }

        // We didn't fit - pop the last glyph and try again.
        if let Some(last_glyph) = row.glyphs.pop() {
            section_index = last_glyph.section_index;
        } else {
            section_index = row.section_index_at_start;
        }
    }
}

/// Horizontally aligned the text on a row.
///
/// Ignores the Y coordinate.
fn halign_and_justify_row(
    point_scale: PointScale,
    placed_row: &mut PlacedRow,
    halign: Align,
    wrap_width: f32,
    justify: bool,
    keep_trailing_whitespace: bool,
) {
    #![expect(clippy::useless_let_if_seq)] // False positive

    let row = Arc::make_mut(&mut placed_row.row);

    if row.glyphs.is_empty() {
        return;
    }

    let num_leading_spaces = row
        .glyphs
        .iter()
        .take_while(|glyph| glyph.chr.is_whitespace())
        .count();

    let glyph_range = if num_leading_spaces == row.glyphs.len() {
        // There is only whitespace
        (0, row.glyphs.len())
    } else if keep_trailing_whitespace {
        (num_leading_spaces, row.glyphs.len())
    } else {
        let num_trailing_spaces = row
            .glyphs
            .iter()
            .rev()
            .take_while(|glyph| glyph.chr.is_whitespace())
            .count();

        (num_leading_spaces, row.glyphs.len() - num_trailing_spaces)
    };
    let num_glyphs_in_range = glyph_range.1 - glyph_range.0;
    assert!(num_glyphs_in_range > 0, "Should have at least one glyph");

    // Glyphs are indexed in logical order but placed in visual order
    // (see `reorder_row_visually`), so measure and shift them by x:
    let order = visual_order(row);
    let kept = |i: &usize| glyph_range.0 <= *i && *i < glyph_range.1;
    let first_kept = order.iter().position(kept).unwrap_or(0);
    let last_kept = order.iter().rposition(kept).unwrap_or(0);

    // A right-to-left mark has no width and sits at the left edge of its
    // letter, and sorts after it, so the glyph placed last can end a letter
    // short of the row. Such a row is measured over all its kept glyphs.
    let has_rtl = row.glyphs.iter().any(Glyph::is_rtl);
    let (original_min_x, original_max_x) = if has_rtl {
        row.glyphs[glyph_range.0..glyph_range.1].iter().fold(
            (f32::INFINITY, f32::NEG_INFINITY),
            |(min_x, max_x), glyph| (min_x.min(glyph.pos.x), max_x.max(glyph.max_x())),
        )
    } else {
        (
            row.glyphs[order[first_kept]].logical_rect().min.x,
            row.glyphs[order[last_kept]].logical_rect().max.x,
        )
    };
    let original_width = original_max_x - original_min_x;

    let target_width = if justify && num_glyphs_in_range > 1 {
        wrap_width
    } else {
        original_width
    };

    // The row is placed on a whole pixel and its glyphs end on fractions of
    // one, so a right-aligned row can reach up to half a pixel past the edge
    // it is aligned to. A row with right-to-left text gets a box of whole
    // pixels instead, which ends on that edge, so the first letter of a
    // right-to-left line stays inside it. Not when that box is wider than
    // the line, as a mirrored row placed on whole pixels can be: rounding
    // the galley cuts its rect back to the wrap width on the right, the rect
    // then ends a pixel short of the edge the row is aligned to, and a label
    // placed by that edge draws the row a pixel left of itself.
    let whole_pixels = point_scale.ceil_to_pixel(target_width);
    let box_width = if has_rtl && whole_pixels <= wrap_width {
        whole_pixels
    } else {
        target_width
    };
    let (target_min_x, target_max_x) = match halign {
        Align::LEFT => (0.0, box_width),
        Align::Center => (-box_width / 2.0, box_width / 2.0),
        Align::RIGHT => (-box_width, 0.0),
    };

    let num_spaces_in_range = row.glyphs[glyph_range.0..glyph_range.1]
        .iter()
        .filter(|glyph| glyph.chr.is_whitespace())
        .count();

    let mut extra_x_per_glyph = if num_glyphs_in_range == 1 {
        0.0
    } else {
        (target_width - original_width) / (num_glyphs_in_range as f32 - 1.0)
    };
    extra_x_per_glyph = extra_x_per_glyph.at_least(0.0); // Don't contract

    let mut extra_x_per_space = 0.0;
    if 0 < num_spaces_in_range && num_spaces_in_range < num_glyphs_in_range {
        // Add an integral number of pixels between each glyph,
        // and add the balance to the spaces:

        extra_x_per_glyph = point_scale.floor_to_pixel(extra_x_per_glyph);

        extra_x_per_space = (target_width
            - original_width
            - extra_x_per_glyph * (num_glyphs_in_range as f32 - 1.0))
            / (num_spaces_in_range as f32);
    }

    placed_row.pos.x = point_scale.round_to_pixel(target_min_x);
    let mut translate_x = -original_min_x - extra_x_per_glyph * first_kept as f32;

    for &i in &order {
        let glyph = &mut row.glyphs[i];
        glyph.pos.x += translate_x;
        glyph.pos.x = point_scale.round_to_pixel(glyph.pos.x);
        translate_x += extra_x_per_glyph;
        if glyph.chr.is_whitespace() {
            translate_x += extra_x_per_space;
        }
    }

    // Note we ignore the leading/trailing whitespace here!
    row.size.x = target_max_x - target_min_x;
}

/// Calculate the Y positions and tessellate the text.
fn galley_from_rows(
    point_scale: PointScale,
    job: Arc<LayoutJob>,
    mut rows: Vec<PlacedRow>,
    elided: bool,
    intrinsic_size: Vec2,
) -> Galley {
    let mut first_row_min_height = job.first_row_min_height;
    let mut cursor_y = 0.0;

    for placed_row in &mut rows {
        let mut max_row_height = first_row_min_height.at_least(placed_row.height());
        let row = Arc::make_mut(&mut placed_row.row);

        first_row_min_height = 0.0;
        for glyph in &row.glyphs {
            max_row_height = max_row_height.at_least(glyph.line_height);
        }
        max_row_height = point_scale.round_to_pixel(max_row_height);

        // Now position each glyph vertically:
        for glyph in &mut row.glyphs {
            let format = &job.sections[glyph.section_index as usize].format;

            glyph.pos.y = glyph.font_face_ascent

                // Apply valign to the different in height of the entire row, and the height of this `Font`:
                + format.valign.to_factor() * (max_row_height - glyph.line_height)

                // When mixing different `FontImpl` (e.g. latin and emojis),
                // we always center the difference:
                + 0.5 * (glyph.font_height - glyph.font_face_height);

            glyph.pos.y = point_scale.round_to_pixel(glyph.pos.y);
        }

        placed_row.pos.y = cursor_y;
        row.size.y = max_row_height;

        cursor_y += max_row_height;
        cursor_y = point_scale.round_to_pixel(cursor_y); // TODO(emilk): it would be better to do the calculations in pixels instead.
    }

    let format_summary = format_summary(&job);

    let mut rect = Rect::ZERO;
    let mut mesh_bounds = Rect::NOTHING;
    let mut num_vertices = 0;
    let mut num_indices = 0;

    for placed_row in &mut rows {
        rect |= placed_row.rect();

        let row = Arc::make_mut(&mut placed_row.row);
        row.visuals = tessellate_row(point_scale, &job, &format_summary, row);

        mesh_bounds |= row.visuals.mesh_bounds.translate(placed_row.pos.to_vec2());
        num_vertices += row.visuals.mesh.vertices.len();
        num_indices += row.visuals.mesh.indices.len();

        row.section_index_at_start = u32::MAX; // No longer in use.
        for glyph in &mut row.glyphs {
            glyph.section_index = u32::MAX; // No longer in use.
        }
    }

    let mut galley = Galley {
        job,
        rows,
        elided,
        rect,
        mesh_bounds,
        num_vertices,
        num_indices,
        pixels_per_point: point_scale.pixels_per_point,
        intrinsic_size,
    };

    if galley.job.round_output_to_gui {
        galley.round_output_to_gui();
    }

    galley
}

#[derive(Default)]
struct FormatSummary {
    any_background: bool,
    any_underline: bool,
    any_strikethrough: bool,
}

fn format_summary(job: &LayoutJob) -> FormatSummary {
    let mut format_summary = FormatSummary::default();
    for section in &job.sections {
        format_summary.any_background |= section.format.background != Color32::TRANSPARENT;
        format_summary.any_underline |= section.format.underline != Stroke::NONE;
        format_summary.any_strikethrough |= section.format.strikethrough != Stroke::NONE;
    }
    format_summary
}

fn tessellate_row(
    point_scale: PointScale,
    job: &LayoutJob,
    format_summary: &FormatSummary,
    row: &mut Row,
) -> RowVisuals {
    if row.glyphs.is_empty() {
        return Default::default();
    }

    let mut mesh = Mesh::default();

    mesh.reserve_triangles(row.glyphs.len() * 2);
    mesh.reserve_vertices(row.glyphs.len() * 4);

    if format_summary.any_background {
        add_row_backgrounds(point_scale, job, row, &mut mesh);
    }

    let glyph_index_start = mesh.indices.len();
    let glyph_vertex_start = mesh.vertices.len();
    tessellate_glyphs(point_scale, job, row, &mut mesh);
    let glyph_vertex_end = mesh.vertices.len();

    if format_summary.any_underline {
        add_row_hline(point_scale, row, &mut mesh, |glyph| {
            let format = &job.sections[glyph.section_index as usize].format;
            let stroke = format.underline;
            let y = glyph.logical_rect().bottom();
            (stroke, y)
        });
    }

    if format_summary.any_strikethrough {
        add_row_hline(point_scale, row, &mut mesh, |glyph| {
            let format = &job.sections[glyph.section_index as usize].format;
            let stroke = format.strikethrough;
            let y = glyph.logical_rect().center().y;
            (stroke, y)
        });
    }

    let mesh_bounds = mesh.calc_bounds();

    RowVisuals {
        mesh,
        mesh_bounds,
        glyph_index_start,
        glyph_vertex_range: glyph_vertex_start..glyph_vertex_end,
    }
}

/// Create background for glyphs that have them.
/// Creates as few rectangular regions as possible.
fn add_row_backgrounds(point_scale: PointScale, job: &LayoutJob, row: &Row, mesh: &mut Mesh) {
    if row.glyphs.is_empty() {
        return;
    }

    let mut end_run = |start: Option<(Color32, Rect, f32)>, stop_x: f32| {
        if let Some((color, start_rect, expand)) = start {
            let rect = Rect::from_min_max(start_rect.left_top(), pos2(stop_x, start_rect.bottom()));
            let rect = rect.expand(expand);
            let rect = rect.round_to_pixels(point_scale.pixels_per_point());
            mesh.add_colored_rect(rect, color);
        }
    };

    let mut run_start = None;
    let mut last_rect = Rect::NAN;

    for i in visual_order(row) {
        let glyph = &row.glyphs[i];
        let format = &job.sections[glyph.section_index as usize].format;
        let color = format.background;
        let rect = glyph.logical_rect();

        if color == Color32::TRANSPARENT {
            end_run(run_start.take(), last_rect.right());
        } else if let Some((existing_color, start, expand)) = run_start {
            if existing_color == color
                && start.top() == rect.top()
                && start.bottom() == rect.bottom()
                && format.expand_bg == expand
            {
                // continue the same background rectangle
            } else {
                end_run(run_start.take(), last_rect.right());
                run_start = Some((color, rect, format.expand_bg));
            }
        } else {
            run_start = Some((color, rect, format.expand_bg));
        }

        last_rect = rect;
    }

    end_run(run_start.take(), last_rect.right());
}

fn tessellate_glyphs(point_scale: PointScale, job: &LayoutJob, row: &mut Row, mesh: &mut Mesh) {
    for glyph in &mut row.glyphs {
        glyph.first_vertex = mesh.vertices.len() as u32;
        let uv_rect = glyph.uv_rect;
        if !uv_rect.is_nothing() {
            let mut left_top = glyph.pos + uv_rect.offset;
            left_top.x = point_scale.round_to_pixel(left_top.x);
            left_top.y = point_scale.round_to_pixel(left_top.y);

            let rect = Rect::from_min_max(left_top, left_top + uv_rect.size);
            let uv = Rect::from_min_max(
                pos2(uv_rect.min[0] as f32, uv_rect.min[1] as f32),
                pos2(uv_rect.max[0] as f32, uv_rect.max[1] as f32),
            );

            let format = &job.sections[glyph.section_index as usize].format;

            let color = format.color;

            if format.italics {
                let idx = mesh.vertices.len() as u32;
                mesh.add_triangle(idx, idx + 1, idx + 2);
                mesh.add_triangle(idx + 2, idx + 1, idx + 3);

                let top_offset = rect.height() * 0.25 * Vec2::X;

                mesh.vertices.push(Vertex {
                    pos: rect.left_top() + top_offset,
                    uv: uv.left_top(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.right_top() + top_offset,
                    uv: uv.right_top(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.left_bottom(),
                    uv: uv.left_bottom(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.right_bottom(),
                    uv: uv.right_bottom(),
                    color,
                });
            } else {
                mesh.add_rect_with_uv(rect, uv, color);
            }
        }
    }
}

/// Add a horizontal line over a row of glyphs with a stroke and y decided by a callback.
fn add_row_hline(
    point_scale: PointScale,
    row: &Row,
    mesh: &mut Mesh,
    stroke_and_y: impl Fn(&Glyph) -> (Stroke, f32),
) {
    let mut path = crate::tessellator::Path::default(); // reusing path to avoid re-allocations.

    let mut end_line = |start: Option<(Stroke, Pos2)>, stop_x: f32| {
        if let Some((stroke, start)) = start {
            let stop = pos2(stop_x, start.y);
            path.clear();
            path.add_line_segment([start, stop]);
            let feathering = 1.0 / point_scale.pixels_per_point();
            path.stroke_open(feathering, &PathStroke::from(stroke), mesh);
        }
    };

    let mut line_start = None;
    let mut last_right_x = f32::NAN;

    for i in visual_order(row) {
        let glyph = &row.glyphs[i];
        let (stroke, mut y) = stroke_and_y(glyph);
        stroke.round_center_to_pixel(point_scale.pixels_per_point, &mut y);

        if stroke.is_empty() {
            end_line(line_start.take(), last_right_x);
        } else if let Some((existing_stroke, start)) = line_start {
            if existing_stroke == stroke && start.y == y {
                // continue the same line
            } else {
                end_line(line_start.take(), last_right_x);
                line_start = Some((stroke, pos2(glyph.pos.x, y)));
            }
        } else {
            line_start = Some((stroke, pos2(glyph.pos.x, y)));
        }

        last_right_x = glyph.max_x();
    }

    end_line(line_start.take(), last_right_x);
}

// ----------------------------------------------------------------------------

/// Keeps track of good places to break a long row of text.
/// Will focus primarily on spaces, secondarily on things like `-`
#[derive(Clone, Copy, Default)]
struct RowBreakCandidates {
    /// Breaking at ` ` or other whitespace
    /// is always the primary candidate.
    space: Option<usize>,

    /// Logograms (single character representing a whole word) or kana (Japanese hiragana and katakana) are good candidates for line break.
    cjk: Option<usize>,

    /// Breaking anywhere before a CJK character is acceptable too.
    pre_cjk: Option<usize>,

    /// Breaking at a dash is a super-
    /// good idea.
    dash: Option<usize>,

    /// This is nicer for things like URLs, e.g. www.
    /// example.com.
    punctuation: Option<usize>,

    /// Breaking after just random character is some
    /// times necessary.
    any: Option<usize>,
}

impl RowBreakCandidates {
    fn add(&mut self, index: usize, glyphs: &[Glyph]) {
        let chr = glyphs[0].chr;
        const NON_BREAKING_SPACE: char = '\u{A0}';
        if chr.is_whitespace() && chr != NON_BREAKING_SPACE {
            self.space = Some(index);
        } else if is_cjk(chr) && (glyphs.len() == 1 || is_cjk_break_allowed(glyphs[1].chr)) {
            self.cjk = Some(index);
        } else if chr == '-' {
            self.dash = Some(index);
        } else if chr.is_ascii_punctuation() {
            self.punctuation = Some(index);
        } else if glyphs.len() > 1 && is_cjk(glyphs[1].chr) {
            self.pre_cjk = Some(index);
        }
        self.any = Some(index);
    }

    fn word_boundary(&self) -> Option<usize> {
        [self.space, self.cjk, self.pre_cjk]
            .into_iter()
            .max()
            .flatten()
    }

    fn has_good_candidate(&self, break_anywhere: bool) -> bool {
        if break_anywhere {
            self.any.is_some()
        } else {
            self.word_boundary().is_some()
        }
    }

    fn get(&self, break_anywhere: bool) -> Option<usize> {
        if break_anywhere {
            self.any
        } else {
            self.word_boundary()
                .or(self.dash)
                .or(self.punctuation)
                .or(self.any)
        }
    }

    fn forget_before_idx(&mut self, index: usize) {
        let Self {
            space,
            cjk,
            pre_cjk,
            dash,
            punctuation,
            any,
        } = self;
        if space.is_some_and(|s| s < index) {
            *space = None;
        }
        if cjk.is_some_and(|s| s < index) {
            *cjk = None;
        }
        if pre_cjk.is_some_and(|s| s < index) {
            *pre_cjk = None;
        }
        if dash.is_some_and(|s| s < index) {
            *dash = None;
        }
        if punctuation.is_some_and(|s| s < index) {
            *punctuation = None;
        }
        if any.is_some_and(|s| s < index) {
            *any = None;
        }
    }
}

// ----------------------------------------------------------------------------

/// Segment text into runs where each run uses a single font face.
///
/// Grapheme clusters are never split across runs: if a combining mark
/// falls back to a different font than its base character, it stays
/// with the base character's font (the shaper will handle it).
///
/// A run also never crosses a change of bidi embedding level (`levels`, one
/// per byte of `text`, or `None` for text without right-to-left characters),
/// so each run is shaped in one direction.
///
/// NOTE: Segmentation is by font face and direction, not by Unicode script.
/// A run may mix scripts (e.g. Latin + Cyrillic) when they share the same
/// font, which is acceptable for scripts with similar shaping rules.
///
/// Results are appended to `out` (which is cleared first) to allow
/// the caller to reuse the allocation across calls.
fn segment_into_runs(
    font: &mut Font<'_>,
    text: &str,
    levels: Option<&[u8]>,
    out: &mut Vec<TextRun>,
) {
    use unicode_segmentation::UnicodeSegmentation as _;

    out.clear();

    for (byte_offset, grapheme_str) in text.grapheme_indices(true) {
        let bidi_level = levels
            .and_then(|levels| levels.get(byte_offset))
            .copied()
            .unwrap_or(0);
        let byte_offset = ByteIndex(byte_offset);
        let byte_end = byte_offset + grapheme_str.len();

        let base_char = grapheme_str.chars().next().unwrap_or(' ');
        let mut font_key = font.resolve_face(base_char);

        if let Some(last_run) = out.last_mut()
            && last_run.bidi_level == bidi_level
        {
            // A space between right-to-left words stays in their run when
            // their face has one. Taken from the first face instead, it would
            // split the line into a run per word and one per space, each
            // shaped on its own, which costs several times the shaping.
            if grapheme_str == " "
                && bidi_level % 2 == 1
                && last_run.font_key != font_key
                && font
                    .fonts_by_id
                    .get_mut(&last_run.font_key)
                    .is_some_and(|face| face.glyph_id_resolution(' ').is_some())
            {
                font_key = last_run.font_key;
            }
            if last_run.font_key == font_key {
                last_run.byte_range.end = byte_end;
                continue;
            }
        }
        out.push(TextRun {
            font_key,
            byte_range: byte_offset..byte_end,
            bidi_level,
        });
    }
}

/// The resolved bidi embedding levels (UAX #9) of a job's text.
struct BidiLevels {
    /// One per byte of the text.
    levels: Vec<u8>,

    /// Where each bidi paragraph starts in the text, and its level, in order.
    paragraphs: Vec<(usize, u8)>,
}

impl BidiLevels {
    /// `None` when the text has no right-to-left character, which keeps
    /// the common case free of any bidi work.
    ///
    /// Paragraph direction is taken from the first strong character (rules P2/P3),
    /// the same as `dir="auto"` in a browser. A right-to-left paragraph whose
    /// text a left-to-right override turned all left-to-right still counts:
    /// rule L1 moves its trailing whitespace to the left end.
    fn new(text: &str) -> Option<Self> {
        if text.is_ascii() {
            return None;
        }
        let info = unicode_bidi::BidiInfo::new(text, None);
        let rtl_paragraph = info.paragraphs.iter().any(|p| p.level.is_rtl());
        (info.has_rtl() || rtl_paragraph).then(|| Self {
            levels: info.levels.iter().map(|level| level.number()).collect(),
            paragraphs: info
                .paragraphs
                .iter()
                .map(|paragraph| (paragraph.range.start, paragraph.level.number()))
                .collect(),
        })
    }

    /// The level of the paragraph that byte `at` of the text is in.
    fn paragraph_level(&self, at: usize) -> u8 {
        self.paragraphs
            .iter()
            .take_while(|(start, _)| *start <= at)
            .last()
            .map_or(0, |(_, level)| *level)
    }
}

/// UAX #9 rule L1: whitespace at the end of a row, and a tab with the
/// whitespace before it, take the paragraph's level. A space where a line
/// wraps then stays at the end of the line, instead of being drawn between
/// the runs it followed.
///
/// The formatting characters rule X9 removes are kept, as section 5.2 of
/// UAX #9 allows, with the level of the char before them. That holds after
/// the reset too, so one after a tab follows the tab.
fn reset_trailing_whitespace(row: &mut Row, paragraph_level: u8) {
    use unicode_bidi::BidiClass::{B, BN, FSI, LRE, LRI, LRO, PDF, PDI, RLE, RLI, RLO, S, WS};

    let mut reset = vec![false; row.glyphs.len()];
    let mut trailing = true;
    for (i, glyph) in row.glyphs.iter().enumerate().rev() {
        match unicode_bidi::bidi_class(glyph.chr) {
            B | S => {
                reset[i] = true;
                trailing = true;
            }
            WS | FSI | LRI | RLI | PDI | LRE | RLE | LRO | RLO | PDF | BN => reset[i] = trailing,
            _ => trailing = false,
        }
    }

    let mut previous = paragraph_level;
    for (glyph, reset) in row.glyphs.iter_mut().zip(reset) {
        if reset {
            glyph.bidi_level = paragraph_level;
        } else if matches!(
            unicode_bidi::bidi_class(glyph.chr),
            LRE | RLE | LRO | RLO | PDF | BN
        ) {
            glyph.bidi_level = previous;
        }
        previous = glyph.bidi_level;
    }
}

/// The glyph indices of `row` sorted by where the glyphs are placed.
///
/// The identity for left-to-right rows.
fn visual_order(row: &Row) -> Vec<usize> {
    let mut order: Vec<usize> = (0..row.glyphs.len()).collect();
    if row.glyphs.iter().any(Glyph::is_rtl) {
        order.sort_by(|&a, &b| row.glyphs[a].pos.x.total_cmp(&row.glyphs[b].pos.x));
    }
    order
}

/// Place the glyphs of a row in visual order (UAX #9 rule L2) by rewriting their x.
///
/// On entry `row.glyphs` is in logical order with increasing x, as
/// [`layout_shaped_run`] left it. It stays in logical order, so a glyph's
/// index is still its char index; only the positions move.
///
/// A block is one advancing glyph and the zero-width glyphs of the same
/// level after it (combining marks, the continuation glyphs of a cluster).
/// Blocks move as units so a mark stays over its base. A zero-width char of
/// another level, such as a direction mark, is a block of its own, and
/// still separates the runs on either side of it.
///
/// The room layout left between two blocks, a section's leading space or
/// letter spacing, moves as a space char there would: with both blocks when
/// they have one level, else with the lower one. So the gap a section starts
/// with stays between that section and the one before it, on whichever side
/// that boundary is drawn.
fn reorder_row_visually(point_scale: PointScale, job: &LayoutJob, row: &mut Row) {
    let glyphs = &mut row.glyphs;
    if glyphs.len() < 2 {
        return;
    }

    let mut blocks: Vec<Range<usize>> = Vec::new();
    for i in 0..glyphs.len() {
        match blocks.last_mut() {
            Some(last) if joins_previous(glyphs, i) => last.end = i + 1,
            _ => blocks.push(i..i + 1),
        }
    }
    let origins: Vec<f32> = blocks
        .iter()
        .map(|block| glyphs[block.start].pos.x)
        .collect();
    let gaps: Vec<f32> = blocks
        .iter()
        .map(|block| gap_before(job, glyphs, block.start))
        .collect();

    // A block that advances is as wide as the distance to the next one that
    // does, less the gaps on the way; the last ends where its glyphs end.
    // One that does not advance has no width: a joiner that rule L1 took out
    // of a right-to-left run still sits at the start of its letter, and
    // measuring to it would give that letter none. Rounding can put the next
    // block 1 px before this one; that must not pull the rest back over it.
    let mut widths = vec![0.0; blocks.len()];
    let mut next_start: Option<f32> = None;
    for (b, block) in blocks.iter().enumerate().rev() {
        if 0.0 < glyphs[block.start].advance_width {
            let end = next_start.unwrap_or_else(|| {
                glyphs[block.clone()]
                    .iter()
                    .map(Glyph::max_x)
                    .fold(origins[b], f32::max)
            });
            widths[b] = (end - origins[b]).max(0.0);
            next_start = Some(origins[b] - gaps[b]);
        } else if let Some(start) = &mut next_start {
            *start -= gaps[b];
        }
    }

    // What gets placed, in logical order: the gap before each block, if
    // there is one, then the block. Each as its level, its width and the
    // block it is, if any.
    let mut pieces: Vec<(u8, f32, Option<usize>)> = Vec::with_capacity(blocks.len());
    for (b, block) in blocks.iter().enumerate() {
        let level = glyphs[block.start].bidi_level;
        if 0 < b && gaps[b] != 0.0 {
            let before = glyphs[blocks[b - 1].start].bidi_level;
            pieces.push((level.min(before), gaps[b], None));
        }
        pieces.push((level, widths[b], Some(b)));
    }

    // L2: from the highest level down to the lowest odd level,
    // reverse every maximal sequence of pieces at that level or higher.
    let levels: Vec<u8> = pieces.iter().map(|&(level, _, _)| level).collect();
    let Some(lowest_odd) = levels.iter().copied().filter(|l| l % 2 == 1).min() else {
        return;
    };
    let highest = levels.iter().copied().max().unwrap_or(0);
    let mut order: Vec<usize> = (0..pieces.len()).collect();
    for level in (lowest_odd..=highest).rev() {
        let mut i = 0;
        while i < order.len() {
            if levels[order[i]] < level {
                i += 1;
                continue;
            }
            let start = i;
            while i < order.len() && level <= levels[order[i]] {
                i += 1;
            }
            order[start..i].reverse();
        }
    }

    let mut pen = origins[0];
    for &p in &order {
        let (_, width, block) = pieces[p];
        if let Some(b) = block {
            for glyph in &mut glyphs[blocks[b].clone()] {
                glyph.pos.x = point_scale.round_to_pixel(pen + (glyph.pos.x - origins[b]));
            }
        }
        pen += width;
    }
}

/// The room layout left before glyph `i` on top of the advance of the glyph
/// before it: the leading space of every section that starts there, and
/// letter spacing. Nothing before the first glyph of a row.
fn gap_before(job: &LayoutJob, glyphs: &[Glyph], i: usize) -> f32 {
    let Some(previous) = i.checked_sub(1).and_then(|previous| glyphs.get(previous)) else {
        return 0.0;
    };
    let section = glyphs[i].section_index as usize;
    let leading: f32 = job
        .sections
        .get(previous.section_index as usize + 1..=section)
        .map_or(0.0, |started| {
            started.iter().map(|section| section.leading_space).sum()
        });
    let spacing = job
        .sections
        .get(section)
        .map_or(0.0, |section| section.format.extra_letter_spacing);
    leading + spacing
}

/// Shape a text run and return the raw [`harfrust::GlyphBuffer`].
///
/// The caller should iterate `glyph_infos()` / `glyph_positions()` (both
/// `Copy` slices) and convert font units to pixels using `metrics.px_scale_factor`.
/// After iteration, recycle the buffer via `glyph_buffer.clear()`.
fn shape_text(
    font_face: &FontFace,
    text: &str,
    rtl: bool,
    coords: &VariationCoords,
    mut buffer: harfrust::UnicodeBuffer,
    flags: harfrust::BufferFlags,
) -> harfrust::GlyphBuffer {
    let font_ref = font_face.skrifa_font_ref();
    let tweak = font_face.tweak();

    // Build shaper with variable font instance if variation coordinates are set.
    let variations: Vec<harfrust::Variation> = iter::chain(tweak.coords.as_ref(), coords.as_ref())
        .map(|&(tag, value)| harfrust::Variation { tag, value })
        .collect();

    let instance = if variations.is_empty() {
        None
    } else {
        Some(harfrust::ShaperInstance::from_variations(
            font_ref, variations,
        ))
    };

    let shaper = font_face
        .shaper_data()
        .shaper(font_ref)
        .instance(instance.as_ref())
        .build();

    // A mark with no letter before it, such as a lone Arabic vowel sign at
    // the start of a line, gets no dotted circle to sit on: that circle
    // would be a glyph without a char, and the cursor code needs one glyph
    // per char.
    buffer.set_flags(flags | harfrust::BufferFlags::DO_NOT_INSERT_DOTTED_CIRCLE);
    buffer.push_str(text);
    // The direction is decided by the bidi algorithm, per run;
    // the shaper only guesses the script (and would guess the direction wrong
    // for digits inside right-to-left text).
    buffer.set_direction(if rtl {
        harfrust::Direction::RightToLeft
    } else {
        harfrust::Direction::LeftToRight
    });
    buffer.guess_segment_properties();

    shaper.shape(buffer, harfrust::ShapeOptions::new())
}

// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use core::iter;

    use super::{super::*, *};
    use crate::text::cursor::CCursor;

    #[test]
    fn test_zero_max_width() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let mut layout_job = LayoutJob::single_section("W".into(), TextFormat::default());
        layout_job.wrap.max_width = 0.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(galley.rows.len(), 1);
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_truncate_with_newline() {
        // No matter where we wrap, we should be appending the newline character.

        let pixels_per_point = 1.0;

        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let text_format = TextFormat {
            font_id: FontId::monospace(12.0),
            ..Default::default()
        };

        for text in ["Hello\nworld", "\nfoo"] {
            for break_anywhere in [false, true] {
                for max_width in [0.0, 5.0, 10.0, 20.0, f32::INFINITY] {
                    let mut layout_job =
                        LayoutJob::single_section(text.into(), text_format.clone());
                    layout_job.wrap.max_width = max_width;
                    layout_job.wrap.max_rows = 1;
                    layout_job.wrap.break_anywhere = break_anywhere;

                    let galley = layout(&mut fonts, pixels_per_point, layout_job.into());

                    assert!(galley.elided);
                    assert_eq!(galley.rows.len(), 1);
                    let row_text = galley.rows[0].text();
                    assert!(
                        row_text.ends_with('…'),
                        "Expected row to end with `…`, got {row_text:?} when line-breaking the text {text:?} with max_width {max_width} and break_anywhere {break_anywhere}.",
                    );
                }
            }
        }

        {
            let mut layout_job = LayoutJob::single_section("Hello\nworld".into(), text_format);
            layout_job.wrap.max_width = 50.0;
            layout_job.wrap.max_rows = 1;
            layout_job.wrap.break_anywhere = false;

            let galley = layout(&mut fonts, pixels_per_point, layout_job.into());

            assert!(galley.elided);
            assert_eq!(galley.rows.len(), 1);
            let row_text = galley.rows[0].text();
            assert_eq!(row_text, "Hello…");
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_cjk() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let mut layout_job = LayoutJob::single_section(
            "日本語とEnglishの混在した文章".into(),
            TextFormat::default(),
        );
        layout_job.wrap.max_width = 90.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["日本語と", "Englishの混在", "した文章"]
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_pre_cjk() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let mut layout_job = LayoutJob::single_section(
            "日本語とEnglishの混在した文章".into(),
            TextFormat::default(),
        );
        layout_job.wrap.max_width = 110.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["日本語とEnglish", "の混在した文章"]
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_truncate_width() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let mut layout_job =
            LayoutJob::single_section("# DNA\nMore text".into(), TextFormat::default());
        layout_job.wrap.max_width = f32::INFINITY;
        layout_job.wrap.max_rows = 1;
        layout_job.round_output_to_gui = false;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert!(galley.elided);
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["# DNA…"]
        );
        let row = &galley.rows[0];
        assert_eq!(row.pos, Pos2::ZERO);
        assert_eq!(row.rect().max.x, row.glyphs.last().unwrap().max_x());
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_truncate_with_pixels_per_point() {
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        for pixels_per_point in [
            0.33, 0.5, 0.67, 1.0, 1.25, 1.33, 1.5, 1.75, 2.0, 3.0, 4.0, 5.0,
        ] {
            for ch in ['W', 'A', 'n', 't', 'i'] {
                let target_width = 50.0;
                let text = (0..20).map(|_| ch).collect::<String>();

                let mut job = LayoutJob::single_section(text, TextFormat::default());
                job.wrap.max_width = target_width;
                job.wrap.max_rows = 1;
                let elided_galley = layout(&mut fonts, pixels_per_point, job.into());
                assert!(elided_galley.elided);

                let test_galley = layout(
                    &mut fonts,
                    pixels_per_point,
                    Arc::new(LayoutJob::single_section(
                        iter::chain(
                            (0..elided_galley.rows[0].char_count_excluding_newline().0).map(|_| ch),
                            iter::once('…'),
                        )
                        .collect::<String>(),
                        TextFormat::default(),
                    )),
                );

                assert!(elided_galley.size().x >= 0.0);
                assert!(elided_galley.size().x <= target_width);
                assert!(test_galley.size().x > target_width);
            }
        }
    }

    #[test]
    fn test_empty_row() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let font_id = FontId::default();
        let font_height = fonts
            .font(&font_id.family)
            .styled_metrics(pixels_per_point, font_id.size, &VariationCoords::default())
            .row_height;

        let job = LayoutJob::simple(String::new(), font_id, Color32::WHITE, f32::INFINITY);

        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1, "Expected one row");
        assert_eq!(
            galley.rows[0].row.glyphs.len(),
            0,
            "Expected no glyphs in the empty row"
        );
        assert_eq!(
            galley.size(),
            Vec2::new(0.0, font_height.round()),
            "Unexpected galley size"
        );
        assert_eq!(
            galley.intrinsic_size(),
            Vec2::new(0.0, font_height.round()),
            "Unexpected intrinsic size"
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_end_with_newline() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let font_id = FontId::default();
        let font_height = fonts
            .font(&font_id.family)
            .styled_metrics(pixels_per_point, font_id.size, &VariationCoords::default())
            .row_height;

        let job = LayoutJob::simple("Hi!\n".to_owned(), font_id, Color32::WHITE, f32::INFINITY);

        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 2, "Expected two rows");
        assert_eq!(
            galley.rows[1].row.glyphs.len(),
            0,
            "Expected no glyphs in the empty row"
        );
        assert_eq!(
            galley.size().round(),
            Vec2::new(17.0, font_height.round() * 2.0),
            "Unexpected galley size"
        );
        assert_eq!(
            galley.intrinsic_size().round(),
            Vec2::new(17.0, font_height.round() * 2.0),
            "Unexpected intrinsic size"
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_combining_diacritics() {
        // ɔ̃ = U+0254 (LATIN SMALL LETTER OPEN O) + U+0303 (COMBINING TILDE)
        // With text shaping, the combining tilde should NOT produce a separate
        // advance; it should be positioned above ɔ via GPOS anchors.
        // Note: the default fonts don't contain U+0254, so the replacement glyph
        // is used. The key test is that the combining mark does NOT add extra width.
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let job_combined = LayoutJob::simple(
            "ɔ\u{0303}".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley_combined = layout(&mut fonts, pixels_per_point, job_combined.into());

        let job_base = LayoutJob::simple(
            "ɔ".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley_base = layout(&mut fonts, pixels_per_point, job_base.into());

        let width_combined = galley_combined.size().x;
        let width_base = galley_base.size().x;

        assert!(
            (width_combined - width_base).abs() < 2.0,
            "Combining diacritic should not add significant width. \
             Base width: {width_base}, Combined width: {width_combined}"
        );

        let glyphs = &galley_combined.rows[0].row.glyphs;
        assert!(!glyphs.is_empty(), "Expected at least 1 glyph for ɔ̃");
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_shaping_basic_latin() {
        // Basic test: shaped Latin text should produce the same number of glyphs as characters.
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let job = LayoutJob::simple(
            "Hello".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        assert_eq!(galley.rows[0].row.glyphs.len(), 5);
        assert!(galley.size().x > 0.0);
    }

    #[test]
    fn test_shaping_empty_string() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let job = LayoutJob::simple(
            String::new(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        assert_eq!(galley.rows[0].row.glyphs.len(), 0);
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_shaping_multiple_newlines() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let job = LayoutJob::simple(
            "A\n\nB".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 3, "Expected 3 rows for 'A\\n\\nB'");
        assert_eq!(galley.rows[0].row.glyphs.len(), 1); // "A"
        assert_eq!(galley.rows[1].row.glyphs.len(), 0); // empty line
        assert_eq!(galley.rows[2].row.glyphs.len(), 1); // "B"
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_shaping_mixed_font_fallback() {
        // Text with both Latin and emoji should work without panicking,
        // even though they use different font faces.
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());

        let job = LayoutJob::simple(
            "Hi \u{1F389} bye".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        // "Hi " (3) + U+1F389 (1) + " bye" (4) = at least 8 glyphs
        assert!(
            galley.rows[0].row.glyphs.len() >= 8,
            "Expected >= 8 glyphs, got {}",
            galley.rows[0].row.glyphs.len()
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_gpos_kerning() {
        // GPOS kerning: pairs like "AV", "VA", "AT" should be tighter than
        // the sum of individual character widths. Without text shaping, egui
        // only uses the legacy `kern` table, so these pairs had diff ≈ 0.
        // With harfrust, GPOS kerning applies proper negative adjustments.
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let font_id = FontId::proportional(14.0);

        for pair in ["AV", "VA", "AT"] {
            let (pair_w, _, _) = measure_text(&mut fonts, pair, &font_id, pixels_per_point);
            let chars: Vec<char> = pair.chars().collect();
            let (w1, _, _) = measure_text(
                &mut fonts,
                &chars[0].to_string(),
                &font_id,
                pixels_per_point,
            );
            let (w2, _, _) = measure_text(
                &mut fonts,
                &chars[1].to_string(),
                &font_id,
                pixels_per_point,
            );
            let sum = w1 + w2;
            let kern_adjustment = sum - pair_w;

            assert!(
                kern_adjustment > 0.5,
                "GPOS kerning for '{pair}': expected pair to be noticeably tighter \
                 than sum of individuals. pair_width={pair_w:.2}, sum={sum:.2}, \
                 kern_adjustment={kern_adjustment:.2} (should be > 0.5)",
            );
        }
    }

    /// Regression test for <https://github.com/emilk/egui/issues/8087>.
    ///
    /// Multi-codepoint grapheme clusters (flag emojis, combining marks) must
    /// produce exactly as many glyphs as characters so that cursor positioning
    /// and text selection remain correct.
    #[test]
    #[cfg_attr(
        not(feature = "default_fonts"),
        ignore = "needs egui's default fonts, which Booth does not build"
    )]
    fn test_grapheme_cluster_glyph_count() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let font_id = FontId::default();

        // Each test case: (input text, expected char count)
        let cases: &[(&str, usize)] = &[
            // Flag emoji: two Regional Indicator codepoints → one visual glyph
            ("\u{1F1EF}\u{1F1F5}", 2), // the flag of Japan
            // Flag surrounded by ASCII
            ("A\u{1F1EB}\u{1F1F7}B", 4), // A, the flag of France, B
            // Base char + combining acute accent
            ("e\u{0301}", 2), // é as decomposed
            // Multiple combining marks
            ("o\u{0302}\u{0323}", 3), // ộ
            // Plain ASCII (sanity check)
            ("Hello", 5),
        ];

        for &(text, expected_chars) in cases {
            let job = LayoutJob::simple(
                text.to_owned(),
                font_id.clone(),
                Color32::WHITE,
                f32::INFINITY,
            );
            let galley = layout(&mut fonts, pixels_per_point, job.into());

            let total_glyphs: usize = galley.rows.iter().map(|r| r.row.glyphs.len()).sum();

            assert_eq!(
                total_glyphs,
                expected_chars,
                "Glyph count mismatch for {text:?}: \
                 expected {expected_chars} glyphs (one per char), got {total_glyphs}. \
                 Glyphs: {:?}",
                galley.rows[0]
                    .row
                    .glyphs
                    .iter()
                    .map(|g| (g.chr, g.advance_width))
                    .collect::<Vec<_>>(),
            );

            // Verify that Row::text() reconstructs the input text.
            let row_text: String = galley.rows.iter().map(|r| r.text()).collect();
            assert_eq!(row_text, text, "Row::text() mismatch for {text:?}");

            // Verify cursor round-trip: end cursor index == char count.
            assert_eq!(
                galley.end().index.0,
                expected_chars,
                "Galley::end().index mismatch for {text:?}",
            );
        }
    }

    /// Verify that cursor positioning round-trips correctly for text
    /// containing multi-codepoint grapheme clusters (regression test for #8087).
    #[test]
    fn test_grapheme_cluster_cursor_roundtrip() {
        let pixels_per_point = 1.0;
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default());
        let font_id = FontId::default();

        // "A" + flag emoji (2 codepoints) + "B" = 4 chars
        let text = "A\u{1F1EF}\u{1F1F5}B";
        let job = LayoutJob::simple(
            text.to_owned(),
            font_id.clone(),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        // Walking through every cursor index should produce valid positions.
        for i in 0..=galley.end().index.0 {
            let cursor = CCursor {
                index: CharIndex(i),
                prefer_next_row: false,
            };
            let rect = galley.pos_from_cursor(cursor);
            assert!(
                rect.is_finite(),
                "pos_from_cursor returned non-finite rect for index {i}",
            );

            // Round-trip: position → cursor → position should be stable.
            let cursor2 = galley.cursor_from_pos(Vec2::new(rect.center().x, rect.center().y));
            let rect2 = galley.pos_from_cursor(cursor2);
            assert!(
                (rect.min.x - rect2.min.x).abs() < 1.0,
                "Cursor round-trip unstable at index {i}: \
                 first={}, second={}, cursor2.index={}",
                rect.min.x,
                rect2.min.x,
                cursor2.index,
            );
        }
    }

    /// Plex Sans with Plex Sans Arabic after it, the fonts the app ships.
    /// The upstream tests use egui's default fonts, which are not built here.
    fn plex_fonts() -> FontsImpl {
        let mut definitions = FontDefinitions::empty();
        for (name, bytes) in [
            (
                "plex-sans",
                &include_bytes!("../../../../assets/fonts/IBMPlexSans-Regular.ttf")[..],
            ),
            (
                "plex-sans-arabic",
                &include_bytes!("../../../../assets/fonts/IBMPlexSansArabic-Regular.ttf")[..],
            ),
        ] {
            definitions
                .font_data
                .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
        }
        definitions.families.insert(
            FontFamily::Proportional,
            vec!["plex-sans".to_owned(), "plex-sans-arabic".to_owned()],
        );
        FontsImpl::new(TextOptions::default(), definitions)
    }

    fn layout_simple(fonts: &mut FontsImpl, text: &str) -> Arc<Galley> {
        let job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(13.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        Arc::new(layout(fonts, 1.0, job.into()))
    }

    fn x_of(galley: &Galley, chr: char) -> f32 {
        galley.rows[0]
            .row
            .glyphs
            .iter()
            .find(|g| g.chr == chr)
            .map(|g| g.pos.x)
            .unwrap_or_else(|| panic!("no glyph for {chr:?}"))
    }

    // Plex has no Hebrew, so those glyphs are replacement boxes; levels,
    // order, glyph count and cursor geometry do not depend on that.
    #[test]
    fn bidi_text_keeps_one_glyph_per_char_in_logical_order() {
        let mut fonts = plex_fonts();
        for text in [
            "שלום 42 עולם",
            "abc אבג 12",
            "אב",
            "١٢٣ עברית",
            "مرحبا hello 123 عالم",
            "شكراً",
            "لا بأس",
            "بّ\u{200D}ـ",
        ] {
            let galley = layout_simple(&mut fonts, text);
            let row = &galley.rows[0].row;
            assert_eq!(row.glyphs.len(), text.chars().count(), "{text:?}");
            assert_eq!(row.text(), text);
            assert_eq!(galley.end().index.0, text.chars().count());
        }
    }

    #[test]
    fn rtl_rows_are_placed_in_visual_order() {
        let mut fonts = plex_fonts();

        // A right-to-left paragraph: the first word ends up on the right,
        // and the number inside it still reads left to right.
        let galley = layout_simple(&mut fonts, "אב 12");
        let xs = ['1', '2', ' ', 'ב', 'א'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "visual order of \"אב 12\": {xs:?}");
        // The space takes the paragraph direction (UAX #9 N1/N2); the digits do not.
        for glyph in &galley.rows[0].row.glyphs {
            assert_eq!(
                glyph.is_rtl(),
                matches!(glyph.chr, 'א' | 'ב' | ' '),
                "{:?}",
                glyph.chr
            );
        }

        // A left-to-right paragraph with a Hebrew word in it:
        // the word is mirrored, the line is not.
        let galley = layout_simple(&mut fonts, "ab אב");
        let xs = ['a', 'b', ' ', 'ב', 'א'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "visual order of \"ab אב\": {xs:?}");

        // Plain text is untouched:
        let galley = layout_simple(&mut fonts, "ab 12");
        let xs = ['a', 'b', ' ', '1', '2'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "{xs:?}");

        // Arabic joins inside a word and the words read from the right.
        let galley = layout_simple(&mut fonts, "مرحبا عالم");
        let xs = ['ل', 'ع', ' ', 'ب', 'ح', 'ر'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "visual order of \"مرحبا عالم\": {xs:?}");
    }

    #[test]
    fn rtl_cursor_sits_on_the_right_side_of_its_glyph() {
        let mut fonts = plex_fonts();
        let galley = layout_simple(&mut fonts, "אב");
        let row = &galley.rows[0].row;
        let [alef, bet] = [&row.glyphs[0], &row.glyphs[1]];
        assert!(bet.pos.x < alef.pos.x, "bet is drawn left of alef");
        assert_eq!(row.x_offset(CharIndex(0)), alef.max_x());
        assert_eq!(row.x_offset(CharIndex(1)), bet.max_x());
        assert_eq!(row.x_offset(CharIndex(2)), bet.pos.x);

        for i in 0..=2 {
            let cursor = CCursor {
                index: CharIndex(i),
                prefer_next_row: false,
            };
            let rect = galley.pos_from_cursor(cursor);
            let back = galley.cursor_from_pos(rect.center().to_vec2());
            assert_eq!(back.index.0, i, "cursor round-trip at {i}");
        }
    }

    // The end of a right-to-left line that ends in digits is right after
    // the last digit, where the next one typed goes, not the right end of
    // the row, which is where the line starts.
    #[test]
    fn the_end_of_a_rtl_line_ending_in_digits_follows_the_last_digit() {
        let mut fonts = plex_fonts();
        let text = "مرحبا 123";
        let galley = layout_simple(&mut fonts, text);
        let row = &galley.rows[0].row;
        let three = row.glyphs.last().unwrap();
        assert_eq!(three.chr, '3');
        let end = CharIndex(text.chars().count());
        assert_eq!(row.x_offset(end), three.max_x());
        let start = row.x_offset(CharIndex(0));
        assert!((start - row.size.x).abs() <= 1.0, "{start} {}", row.size.x);
        assert!(row.x_offset(end) < start - 20.0, "{}", row.x_offset(end));
        // Before the first digit is the far left of the row.
        assert_eq!(row.x_offset(CharIndex(6)), 0.0);
        assert_eq!(row.char_at(three.max_x()), end);
        assert_eq!(row.char_at(row.size.x + 50.0), CharIndex(0));
    }

    // Where the level changes inside a line, the cursor goes with the higher
    // level: after "hello" or "123" inside Arabic it follows the last letter
    // or digit, and before them it leads the first.
    #[test]
    fn the_cursor_stays_with_the_run_it_was_typed_into() {
        let mut fonts = plex_fonts();
        let galley = layout_simple(&mut fonts, "مرحبا hello عالم");
        let row = &galley.rows[0].row;
        let glyph = |i: usize| &row.glyphs[i];
        assert_eq!((glyph(6).chr, glyph(10).chr), ('h', 'o'));
        assert_eq!(row.x_offset(CharIndex(6)), glyph(6).pos.x);
        assert_eq!(row.x_offset(CharIndex(11)), glyph(10).max_x());
        // Inside the Arabic words the cursor is on the right of its letter.
        assert_eq!(row.x_offset(CharIndex(0)), glyph(0).max_x());
        assert_eq!(row.x_offset(CharIndex(12)), glyph(12).max_x());
        assert_eq!(row.x_offset(CharIndex(16)), glyph(15).pos.x);

        // Digits after a Latin word join its run (UAX #9 W7), so the space
        // between them is left to right too.
        let galley = layout_simple(&mut fonts, "مرحبا hello 123 عالم");
        let row = &galley.rows[0].row;
        let glyph = |i: usize| &row.glyphs[i];
        assert_eq!(row.x_offset(CharIndex(11)), glyph(11).pos.x);
        assert_eq!(row.x_offset(CharIndex(15)), glyph(14).max_x());
        assert!(glyph(10).pos.x < glyph(11).pos.x && glyph(11).pos.x < glyph(12).pos.x);
    }

    // A mark is drawn over its own base, which in a right-to-left run the
    // shaper puts after it.
    #[test]
    fn a_mark_stays_on_its_base_in_rtl_text() {
        let mut fonts = plex_fonts();
        let galley = layout_simple(&mut fonts, "بَب شكراً");
        let row = &galley.rows[0].row;
        for (base, mark) in [(0, 1), (7, 8)] {
            let (base, mark) = (&row.glyphs[base], &row.glyphs[mark]);
            assert_eq!(mark.advance_width, 0.0, "{:?}", mark.chr);
            let drawn = mark.pos.x + mark.uv_rect.offset.x + mark.uv_rect.size.x / 2.0;
            assert!(
                base.pos.x - 1.0 <= drawn && drawn <= base.max_x() + 1.0,
                "{:?} centred at {drawn} is off {:?} at {}..{}",
                mark.chr,
                base.chr,
                base.pos.x,
                base.max_x()
            );
        }
        // Joined letters touch: every advancing glyph starts where the one
        // to its left ends.
        let mut placed: Vec<&Glyph> = row
            .glyphs
            .iter()
            .filter(|g| 0.0 < g.advance_width)
            .collect();
        placed.sort_by(|a, b| a.pos.x.total_cmp(&b.pos.x));
        for pair in placed.windows(2) {
            assert!(
                (pair[0].max_x() - pair[1].pos.x).abs() <= 1.0,
                "{:?} ends at {}, {:?} starts at {}",
                pair[0].chr,
                pair[0].max_x(),
                pair[1].chr,
                pair[1].pos.x
            );
        }
    }

    // Every cursor position lands on the row, and the x it lands on gives
    // back a position drawn at the same x.
    #[test]
    fn bidi_cursor_positions_round_trip_through_x() {
        let mut fonts = plex_fonts();
        for text in [
            "مرحبا hello 123 عالم",
            "hello مرحبا",
            "(مرحبا) [1, 2]",
            "لا شكراً 12.5%",
            "abc אב cd",
            "عالم abc\u{200F}def",
            "hello مرحبا\u{200D}",
        ] {
            let galley = layout_simple(&mut fonts, text);
            let row = &galley.rows[0].row;
            for i in 0..=row.glyphs.len() {
                let x = row.x_offset(CharIndex(i));
                assert!(
                    -1.0 <= x && x <= row.size.x + 1.0,
                    "{text:?} {i}: {x} outside 0..{}",
                    row.size.x
                );
                let back = row.char_at(x);
                assert_eq!(row.x_offset(back), x, "{text:?} {i} came back as {back:?}");
            }
        }
    }

    fn advancing_by_x(row: &Row) -> Vec<usize> {
        let mut placed: Vec<usize> = (0..row.glyphs.len())
            .filter(|&i| 0.0 < row.glyphs[i].advance_width)
            .collect();
        placed.sort_by(|&a, &b| row.glyphs[a].pos.x.total_cmp(&row.glyphs[b].pos.x));
        placed
    }

    // The order the advancing glyphs of a one-row text are drawn in, left to
    // right, and the order unicode-bidi gives them with rules L1 and L2.
    fn drawn_and_expected_order(fonts: &mut FontsImpl, text: &str) -> (Vec<usize>, Vec<usize>) {
        let galley = layout_simple(fonts, text);
        assert_eq!(galley.rows.len(), 1, "{text:?}");
        let row = &galley.rows[0].row;
        assert_eq!(row.glyphs.len(), text.chars().count(), "{text:?}");

        let info = unicode_bidi::BidiInfo::new(text, None);
        let paragraph = &info.paragraphs[0];
        let levels = info.reordered_levels_per_char(paragraph, paragraph.range.clone());
        let expected = unicode_bidi::BidiInfo::reorder_visual(&levels)
            .into_iter()
            .filter(|&i| 0.0 < row.glyphs[i].advance_width)
            .collect();
        (advancing_by_x(row), expected)
    }

    // Spaces, tabs and invisible direction marks or joiners next to a change
    // of direction, where the placing of blocks has to agree with UAX #9.
    #[test]
    fn rows_are_drawn_in_the_order_unicode_bidi_gives() {
        let mut fonts = plex_fonts();
        for text in [
            "مرحبا hello 123 عالم",
            // A right-to-left mark between two English words splits them.
            "عالم abc\u{200F}def",
            "4.5%\u{200F}\u{64E}\u{202D}.شغلﻻ,",
            // A joiner ending a line takes the paragraph's level (L1).
            "hello مرحبا\u{200D}",
            "مرحبا \tabc ",
            // A control char after a tab follows the tab's level, the
            // paragraph's: the box it shows as stays left of عالم.
            "abc مرحبا\t\u{7}عالم",
            // All left to right under an override, in a right-to-left paragraph.
            "12:30\u{202D}ﻻ$4\u{2068}\t$4",
        ] {
            let (drawn, expected) = drawn_and_expected_order(&mut fonts, text);
            assert_eq!(drawn, expected, "{text:?}");
        }

        let galley = layout_simple(&mut fonts, "عالم abc\u{200F}def");
        assert!(x_of(&galley, 'f') < x_of(&galley, 'a'));
    }

    // A space resolves to Plex Sans, the first face, but one between two
    // Arabic words stays in their run. Split there, each word and each space
    // would be shaped on its own, several times the work for a line.
    #[test]
    fn arabic_words_and_the_spaces_between_them_are_one_run() {
        let mut fonts = plex_fonts();
        let mut font = fonts.font(&FontFamily::Proportional);
        let mut runs = Vec::new();
        for (text, count) in [
            ("مرحبا بكم في الغرفة", 1),
            ("hello مرحبا بكم", 2),
            ("see you in there", 1),
        ] {
            let levels = BidiLevels::new(text).map(|bidi| bidi.levels);
            segment_into_runs(&mut font, text, levels.as_deref(), &mut runs);
            assert_eq!(runs.len(), count, "{text:?}: {runs:?}");
        }
    }

    // UAX #9 rule L1: the space where a line wraps ends its row, instead of
    // being drawn as a second gap between the English and the Arabic.
    #[test]
    fn the_space_where_a_line_wraps_ends_the_row() {
        let mut fonts = plex_fonts();
        let job = LayoutJob::simple(
            "hello مرحبا بكم في الغرفة الكبيرة".to_owned(),
            FontId::proportional(13.0),
            Color32::WHITE,
            90.0,
        );
        let galley = layout(&mut fonts, 1.0, job.into());
        assert!(1 < galley.rows.len());
        let row = &galley.rows[0].row;
        let placed = advancing_by_x(row);
        assert_eq!(placed.last(), Some(&(row.glyphs.len() - 1)));
        assert_eq!(row.glyphs.last().map(|g| g.chr), Some(' '));
        for pair in placed.windows(2) {
            let (left, right) = (&row.glyphs[pair[0]], &row.glyphs[pair[1]]);
            assert!(
                (left.max_x() - right.pos.x).abs() <= 1.0,
                "{:?} ends at {}, {:?} starts at {}",
                left.chr,
                left.max_x(),
                right.chr,
                right.pos.x
            );
        }
    }

    // The room a section starts with stays between it and the section before
    // it, on whichever side that boundary is drawn, and never inside a word.
    #[test]
    fn a_sections_leading_space_stays_between_the_sections() {
        let mut fonts = plex_fonts();
        let format = TextFormat::simple(FontId::proportional(13.0), Color32::WHITE);
        let mut two_sections = |first: &str, second: &str| {
            let mut job = LayoutJob::default();
            job.append(first, 0.0, format.clone());
            job.append(second, 20.0, format.clone());
            layout(&mut fonts, 1.0, job.into())
        };
        let near = |a: f32, b: f32| (a - b).abs() <= 1.0;

        let galley = two_sections("مرحبا", " عالم");
        let glyphs = &galley.rows[0].row.glyphs;
        assert!(near(glyphs[4].max_x(), glyphs[3].pos.x), "ا still joins ب");
        assert!(near(glyphs[5].max_x() + 20.0, glyphs[4].pos.x));

        let galley = two_sections("hello", "مرحبا");
        let glyphs = &galley.rows[0].row.glyphs;
        let arabic_left = glyphs[5..].iter().map(|g| g.pos.x).fold(f32::MAX, f32::min);
        assert!(near(glyphs[4].max_x() + 20.0, arabic_left));

        let galley = two_sections("مرحبا", "hello");
        let glyphs = &galley.rows[0].row.glyphs;
        assert!(near(glyphs[9].max_x() + 20.0, glyphs[4].pos.x));
    }

    // A joiner or mark with no width, at the start or end of a row or on a
    // level of its own, takes no room from the letters around it.
    #[test]
    fn zero_width_chars_never_put_glyphs_over_each_other() {
        let mut fonts = plex_fonts();
        for text in [
            "hello مرحبا\u{200D}",
            "مرحبا\u{200C} بكم\u{200D} في الغرفة",
            "شكرا\u{64B}\u{200C}",
            "<عالم\u{1F1EF}\u{1F1F5}\"!\u{1F1EF}\u{1F1F5}<\u{650} \u{1F469}\u{200D}\u{1F4BB}\u{1F600}#3\")؟",
        ] {
            for width in [f32::INFINITY, 120.0, 90.0, 60.0, 40.0] {
                let job = LayoutJob::simple(
                    text.to_owned(),
                    FontId::proportional(13.0),
                    Color32::WHITE,
                    width,
                );
                let galley = layout(&mut fonts, 1.0, job.into());
                for placed_row in &galley.rows {
                    let row = &placed_row.row;
                    for pair in advancing_by_x(row).windows(2) {
                        let (left, right) = (&row.glyphs[pair[0]], &row.glyphs[pair[1]]);
                        assert!(
                            left.max_x() <= right.pos.x + 1.0,
                            "{text:?} at {width}: {:?} ends at {}, {:?} starts at {}",
                            left.chr,
                            left.max_x(),
                            right.chr,
                            right.pos.x
                        );
                    }
                }
            }
        }
    }

    // `text` laid out `width` wide with `halign`, before egui rounds sizes to
    // 1/32 of a point, which it does to every row alike.
    fn layout_unrounded(
        fonts: &mut FontsImpl,
        text: &str,
        width: f32,
        halign: Align,
        pixels_per_point: f32,
    ) -> Galley {
        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(13.0),
            Color32::WHITE,
            width,
        );
        job.halign = halign;
        job.round_output_to_gui = false;
        layout(fonts, pixels_per_point, job.into())
    }

    // On each row with right-to-left text, every glyph that draws something
    // lies inside the row's rect, the row's rect without leading space and
    // the galley's rect, from where it starts to where its advance ends; and
    // the row is not a pixel wider than its glyphs reach, nor wider than its
    // line when they do not reach past it. Spaces draw nothing, and an
    // aligned row leaves the ones it starts or ends with outside its rect,
    // as egui does. Other rows are as egui 0.36.2 lays them out, where a
    // glyph put on a whole pixel can end up to half a pixel past the row.
    fn assert_rows_cover_their_glyphs(galley: &Galley, case: &str) {
        let pixel = 1.0 / galley.pixels_per_point;
        for placed in &galley.rows {
            if !placed.glyphs.iter().any(Glyph::is_rtl) {
                continue;
            }
            let drawn: Vec<&Glyph> = placed
                .glyphs
                .iter()
                .filter(|glyph| !glyph.chr.is_whitespace())
                .collect();
            for glyph in &drawn {
                let left = placed.pos.x + glyph.pos.x;
                let right = placed.pos.x + glyph.max_x();
                for (name, rect) in [
                    ("row", placed.rect()),
                    (
                        "row without leading space",
                        placed.rect_without_leading_space(),
                    ),
                    ("galley", galley.rect),
                ] {
                    assert!(
                        rect.left() <= left && right <= rect.right(),
                        "{case}: {:?} at {left}..{right} is outside the {name}, {}..{}",
                        glyph.chr,
                        rect.left(),
                        rect.right()
                    );
                }
            }
            let reach = placed.glyphs.iter().map(Glyph::max_x).fold(0.0, f32::max);
            assert!(
                placed.size.x < reach + pixel,
                "{case}: {} wide for glyphs reaching {reach}",
                placed.size.x
            );
            // Give or take float error in the glyphs' whole-pixel places.
            let line = galley.job.wrap.max_width.max(reach) + 0.001;
            assert!(
                placed.size.x <= line,
                "{case}: {} wide for glyphs reaching {reach} on a line {} wide",
                placed.size.x,
                galley.job.wrap.max_width
            );
        }
    }

    // A right-to-left row is drawn from its first letter at the right. With a
    // vowel mark on that letter, the mark, which has no width and sits at the
    // letter's left edge, was taken for the row's end, and the row came out
    // as much narrower as the letter is wide. Right-aligned, the letter then
    // stuck out past the edge.
    #[test]
    fn a_rtl_row_is_as_wide_as_its_first_letter_with_a_mark_on_it() {
        let mut fonts = plex_fonts();
        for text in [
            "بِسْمِ اللَّهِ الرَّحْمَنِ",
            "بِسْمِ",
            "بِ",
            "شُكْراً لَكُمْ hello",
            "بِسْمِ اللَّهِ الرَّحْمَنِ الرَّحِيمِ، سَنَبْدَأُ اللَّعِبَ بَعْدَ خَمْسِ دَقَائِقَ",
        ] {
            for halign in [Align::LEFT, Align::Center, Align::RIGHT] {
                for pixels_per_point in [1.0, 1.25, 1.5, 2.0] {
                    for width in [300.0, 120.0] {
                        let galley =
                            layout_unrounded(&mut fonts, text, width, halign, pixels_per_point);
                        let case = format!("{text:?} {halign:?} {width} at {pixels_per_point}");
                        assert_rows_cover_their_glyphs(&galley, &case);
                        if halign == Align::RIGHT {
                            assert!(galley.rect.right() <= 0.0, "{case}: {:?}", galley.rect);
                        }
                    }
                }
            }
        }

        // The first letter is the rightmost, and right-aligned it ends at
        // most a pixel short of the edge.
        let text = "بِسْمِ اللَّهِ الرَّحْمَنِ";
        let galley = layout_unrounded(&mut fonts, text, 300.0, Align::RIGHT, 1.0);
        let placed = &galley.rows[0];
        let first = &placed.glyphs[0];
        assert_eq!(first.chr, 'ب');
        let right = placed.pos.x + first.max_x();
        assert!(-1.0 < right && right <= 0.0, "{right}");
        let rightmost = placed.glyphs.iter().map(Glyph::max_x).fold(0.0, f32::max);
        assert_eq!(first.max_x(), rightmost);
    }

    // Placed on whole pixels in visual order, a right-to-left row can reach a
    // fraction of a point past the line it was broken to fit; this one ends
    // up 300.24 wide on a line 300 wide. Its box was rounded out to 301, and
    // egui's rounding of the galley cut the galley's rect back to 300 on the
    // right, so the rect ended a pixel short of the edge the row is aligned
    // to, where a right-aligned label puts its own right edge: the row's last
    // letter started a pixel left of the label. The box now stays as wide as
    // the glyphs, and the rect runs from where the row starts to that edge.
    #[test]
    fn a_rtl_row_past_its_line_starts_where_the_galley_does() {
        let mut fonts = plex_fonts();
        let text = "خمس بعد اللَّهِ بَعْدَ الرَّحْمَنِ 1440p 9:30 ok بعد تتأخروا مرحبا";
        for halign in [Align::LEFT, Align::Center, Align::RIGHT] {
            for pixels_per_point in [1.0, 1.25, 1.5, 2.0] {
                for width in [300.0, 299.5, 160.0] {
                    let galley =
                        layout_unrounded(&mut fonts, text, width, halign, pixels_per_point);
                    let case = format!("{halign:?} {width} at {pixels_per_point}");
                    assert_rows_cover_their_glyphs(&galley, &case);
                }
            }
        }

        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(13.0),
            Color32::WHITE,
            300.0,
        );
        job.halign = Align::RIGHT;
        let galley = layout(&mut fonts, 1.0, job.into());
        assert_eq!(galley.rows.len(), 1);
        let placed = &galley.rows[0];
        let left = placed
            .glyphs
            .iter()
            .map(|g| g.pos.x)
            .fold(f32::INFINITY, f32::min);
        let right = placed.glyphs.iter().map(Glyph::max_x).fold(0.0, f32::max);
        assert!(
            300.0 < right - left && right - left < 300.5,
            "{left}..{right}"
        );
        assert_eq!((galley.rect.left(), galley.rect.right()), (-300.0, 0.0));
        assert_eq!(placed.pos.x + left, galley.rect.left());
    }

    // English then Arabic with marks on one line: the Arabic is mirrored
    // where it stands, so the row ends at its first letter. A row cut from
    // the line was as wide as up to its last char, a mark, which sits at the
    // left edge of the last letter, and came out about that letter short.
    #[test]
    fn english_then_arabic_with_marks_is_as_wide_as_its_letters() {
        let mut fonts = plex_fonts();
        for text in [
            "see you all at nine tonight if the patch is out by then بِسْمِ اللَّهِ\nok",
            "the patch is out بِسْمِ اللَّهِ",
            "ok بِسْمِ اللَّهِ الرَّحْمَنِ ok بِسْمِ اللَّهِ الرَّحْمَنِ",
        ] {
            for halign in [Align::LEFT, Align::RIGHT] {
                for pixels_per_point in [1.0, 1.5] {
                    for width in [300.0, 160.0, 90.0, 60.0] {
                        let galley =
                            layout_unrounded(&mut fonts, text, width, halign, pixels_per_point);
                        let case = format!("{text:?} {halign:?} {width} at {pixels_per_point}");
                        assert_rows_cover_their_glyphs(&galley, &case);
                    }
                }
            }
        }

        let text = "see you all at nine tonight if the patch is out by then بِسْمِ اللَّهِ\nok";
        let galley = layout_unrounded(&mut fonts, text, 300.0, Align::LEFT, 1.0);
        assert_eq!(galley.rows.len(), 3);
        let row = &galley.rows[1].row;
        let ba = row.glyphs.iter().find(|glyph| glyph.chr == 'ب').unwrap();
        let rightmost = row.glyphs.iter().map(Glyph::max_x).fold(0.0, f32::max);
        assert_eq!(ba.max_x(), rightmost);
        assert!(rightmost <= row.size.x, "{rightmost} {}", row.size.x);
    }

    // The overflow character at the end of a cut right-to-left row goes after
    // the last letter kept. After a mark, which sits at the start of its
    // letter, it was put over that letter, and the letters before it were
    // pulled over each other when the row was mirrored.
    #[test]
    fn an_ellipsis_after_a_mark_does_not_cover_its_letter() {
        let mut fonts = plex_fonts();
        let text = "بِسْمِ اللَّهِ الرَّحْمَنِ";
        for width in (16..=78).map(|width| width as f32) {
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(13.0),
                Color32::WHITE,
                width,
            );
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            job.round_output_to_gui = false;
            let galley = layout(&mut fonts, 1.0, job.into());
            assert!(galley.elided);
            let row = &galley.rows[0].row;
            assert_eq!(row.glyphs.last().map(|glyph| glyph.chr), Some('…'));
            assert!(row.size.x <= width, "{width}: {}", row.size.x);
            assert_rows_cover_their_glyphs(&galley, &format!("cut at {width}"));
            for pair in advancing_by_x(row).windows(2) {
                let (left, right) = (&row.glyphs[pair[0]], &row.glyphs[pair[1]]);
                assert!(
                    left.max_x() <= right.pos.x + 1.0,
                    "{width}: {:?} ends at {}, {:?} starts at {}",
                    left.chr,
                    left.max_x(),
                    right.chr,
                    right.pos.x
                );
            }
        }
    }

    fn measure_text(
        fonts: &mut FontsImpl,
        text: &str,
        font_id: &FontId,
        pixels_per_point: f32,
    ) -> (f32, usize, Vec<(char, f32)>) {
        let job = LayoutJob::simple(
            text.to_owned(),
            font_id.clone(),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(fonts, pixels_per_point, job.into());
        let glyphs = &galley.rows[0].row.glyphs;
        let details: Vec<_> = glyphs.iter().map(|g| (g.chr, g.advance_width)).collect();
        (galley.size().x, glyphs.len(), details)
    }
}
