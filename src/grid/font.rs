//! Embedded Fira Code faces and shared monospace cell geometry.
//!
//! Both CPU capture (`fontdue`) and GPU rendering (`apus-desktop`'s Slug engine) embed these exact
//! TTF slices and derive cell dimensions through [`FontMetrics::cell_metrics`] so terminal cell
//! counts, pixel dimensions, and baselines stay identical across renderers.

/// Embedded Fira Code Regular font bytes (SIL OFL 1.1).
pub const FIRA_CODE_REGULAR: &[u8] = include_bytes!("fonts/FiraCode-Regular.ttf");

/// Embedded Fira Code Bold font bytes (SIL OFL 1.1).
pub const FIRA_CODE_BOLD: &[u8] = include_bytes!("fonts/FiraCode-Bold.ttf");

/// Exact em-space metrics of the embedded `FiraCode-Regular.ttf` face (`upem = 1950`,
/// `ascender = 1800`, `descender = -600`, `line_gap = 0`, `'M'` advance = `1200`).
///
/// Verified against both `fontdue` (in `capture::raster`) and `ttf-parser` (in `apus-desktop`).
pub const FIRA_CODE_METRICS: FontMetrics = FontMetrics {
    ascent_em: 1800.0 / 1950.0,
    descent_em: -600.0 / 1950.0,
    line_gap_em: 0.0,
    advance_em: 1200.0 / 1950.0,
};

/// Metrics for a monospace font family in em units (y-up from baseline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub ascent_em: f32,
    pub descent_em: f32,
    pub line_gap_em: f32,
    pub advance_em: f32,
}

impl FontMetrics {
    /// Cell geometry in physical pixels for a given em size (`size_px`).
    ///
    /// Width and height are rounded to integer pixels so every terminal cell lands on exact
    /// physical pixel edges and adjacent block/box/background quads never seam.
    pub fn cell_metrics(self, size_px: f32) -> CellMetrics {
        let width_px = (self.advance_em * size_px).round().max(1.0);
        let raw_line_height = (self.ascent_em - self.descent_em + self.line_gap_em) * size_px;
        let height_px = raw_line_height.round().max(1.0);
        let leading = (height_px - (self.ascent_em - self.descent_em) * size_px) * 0.5;
        let baseline_px = (self.ascent_em * size_px + leading).round();
        CellMetrics {
            width_px,
            height_px,
            baseline_px,
            size_px,
        }
    }
}

/// Pixel-snapped cell geometry derived from [`FontMetrics`] and an em size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width_px: f32,
    pub height_px: f32,
    pub baseline_px: f32,
    pub size_px: f32,
}

/// Applies Apus's UI glyph fallback mapping for characters absent from Fira Code.
pub fn fallback_char(ch: char) -> char {
    // Fira Code lacks U+25B8 (`▸`, used in contig_selector.rs); map to `▶` (U+25B6).
    match ch {
        '\u{25B8}' => '\u{25B6}',
        other => other,
    }
}
