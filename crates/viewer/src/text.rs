// The strip's face, IBM Plex Sans Medium, loaded from the same file the
// panel embeds, through DirectWrite's in-memory loader: nothing is
// installed and nothing is read from disk. The factory is isolated, so the
// loader belongs to this viewer alone and goes away with it.

use std::collections::HashMap;

use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_ISOLATED, DWRITE_FONT_METRICS1, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_LINE_METRICS, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory5, IDWriteFontCollection1,
    IDWriteInMemoryFontFileLoader, IDWriteTextFormat, IDWriteTextLayout,
};
use windows::core::{HSTRING, w};

use crate::error::ViewerError;

const MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Medium.ttf");

// Laid out strings are kept between frames; the strip shows a few dozen
// different ones at most, so this is only a guard.
const CACHE_LIMIT: usize = 512;

// The face's vertical metrics in font units, for placing the baseline the
// way egui does.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Metrics {
    pub units_per_em: f32,
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

pub(crate) struct Line {
    pub layout: IDWriteTextLayout,
    // In pixels: the advance including trailing spaces, and the baseline's
    // distance from the layout's top.
    pub width: f32,
    pub baseline: f32,
}

pub(crate) struct Text {
    factory: IDWriteFactory5,
    loader: IDWriteInMemoryFontFileLoader,
    collection: IDWriteFontCollection1,
    family: HSTRING,
    weight: DWRITE_FONT_WEIGHT,
    metrics: Metrics,
    size: f32,
    format: Option<IDWriteTextFormat>,
    lines: HashMap<String, Line>,
}

impl Text {
    pub(crate) fn new() -> Result<Text, ViewerError> {
        let step = "load the strip's font";
        let fail = |err: windows::core::Error| ViewerError::windows(step, &err);
        // SAFETY: plain factory calls; the font data is static and copied by
        // the loader, since no owner object is passed; every result is
        // checked.
        unsafe {
            let factory: IDWriteFactory5 =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED).map_err(fail)?;
            let loader = factory.CreateInMemoryFontFileLoader().map_err(fail)?;
            factory.RegisterFontFileLoader(&loader).map_err(fail)?;
            let file = loader
                .CreateInMemoryFontFileReference(
                    &factory,
                    MEDIUM.as_ptr() as *const _,
                    MEDIUM.len() as u32,
                    None,
                )
                .map_err(fail)?;
            let builder = factory.CreateFontSetBuilder().map_err(fail)?;
            builder.AddFontFile(&file).map_err(fail)?;
            let set = builder.CreateFontSet().map_err(fail)?;
            let collection = factory
                .CreateFontCollectionFromFontSet(&set)
                .map_err(fail)?;
            if collection.GetFontFamilyCount() == 0 {
                return Err(ViewerError::other(format!(
                    "could not {step}: DirectWrite found no font in IBMPlexSans-Medium.ttf"
                )));
            }
            // Whatever DirectWrite calls the family and the weight, they are
            // read back rather than assumed, so the one face in the
            // collection is the one every format picks.
            let family = collection.GetFontFamily(0).map_err(fail)?;
            let names = family.GetFamilyNames().map_err(fail)?;
            let length = names.GetStringLength(0).map_err(fail)?;
            let mut name = vec![0u16; length as usize + 1];
            names.GetString(0, &mut name).map_err(fail)?;
            name.truncate(length as usize);
            let font = family.GetFont(0).map_err(fail)?;
            let mut metrics = DWRITE_FONT_METRICS1::default();
            font.GetMetrics(&mut metrics);
            Ok(Text {
                weight: font.GetWeight(),
                metrics: Metrics {
                    units_per_em: metrics.Base.designUnitsPerEm.max(1) as f32,
                    ascent: metrics.Base.ascent as f32,
                    descent: metrics.Base.descent as f32,
                    line_gap: metrics.Base.lineGap as f32,
                },
                family: HSTRING::from_wide(&name),
                factory,
                loader,
                collection,
                size: 0.0,
                format: None,
                lines: HashMap::new(),
            })
        }
    }

    pub(crate) fn metrics(&self) -> Metrics {
        self.metrics
    }

    // Text is laid out at this size, in pixels, until it changes.
    pub(crate) fn set_size(&mut self, size: f32) -> Result<(), ViewerError> {
        if self.format.is_some() && self.size == size {
            return Ok(());
        }
        self.lines.clear();
        self.format = None;
        // SAFETY: plain calls on live objects with a static locale name.
        let format = unsafe {
            let format = self
                .factory
                .CreateTextFormat(
                    &self.family,
                    &self.collection,
                    self.weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )
                .map_err(|err| ViewerError::windows("set up the strip's text", &err))?;
            format
                .SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)
                .map_err(|err| ViewerError::windows("set up the strip's text", &err))?;
            format
        };
        self.format = Some(format);
        self.size = size;
        Ok(())
    }

    pub(crate) fn line(&mut self, text: &str) -> Result<&Line, ViewerError> {
        if !self.lines.contains_key(text) {
            if self.lines.len() >= CACHE_LIMIT {
                self.lines.clear();
            }
            let line = self.lay_out(text)?;
            self.lines.insert(text.to_string(), line);
        }
        Ok(&self.lines[text])
    }

    fn lay_out(&self, text: &str) -> Result<Line, ViewerError> {
        let Some(format) = &self.format else {
            return Err(ViewerError::other(
                "could not lay out the strip's text: no size was set",
            ));
        };
        let wide: Vec<u16> = text.encode_utf16().collect();
        let fail =
            |err: windows::core::Error| ViewerError::windows("lay out the strip's text", &err);
        // SAFETY: plain calls on live objects; the metrics are live out
        // parameters and one line's room is passed for a one-line layout.
        unsafe {
            let layout = self
                .factory
                .CreateTextLayout(&wide, format, 100_000.0, 1_000.0)
                .map_err(fail)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics).map_err(fail)?;
            let mut lines = [DWRITE_LINE_METRICS::default()];
            let mut count = 0;
            layout
                .GetLineMetrics(Some(&mut lines), &mut count)
                .map_err(fail)?;
            Ok(Line {
                layout,
                width: metrics.widthIncludingTrailingWhitespace,
                baseline: lines[0].baseline,
            })
        }
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        self.lines.clear();
        self.format = None;
        // SAFETY: registered in new() on this factory.
        let _ = unsafe { self.factory.UnregisterFontFileLoader(&self.loader) };
    }
}
