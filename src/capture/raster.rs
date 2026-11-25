//! CPU cell-grid rasterizer using `fontdue` and shared procedural pixel-snapped geometry.
//!
//! Renders a settled [`CellGrid`] directly into an opaque `Rgba8` pixel buffer without any GPU,
//! window system, or system font installation. Backgrounds, procedural blocks/octants/braille/box
//! lines, underline/strikethrough rules, dimming, and optical weight boost match the native GPU
//! renderer's integer pixel geometry and ANSI-256 palette.

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::grid::color::{DIM_ALPHA, RGB_LUT};
use crate::grid::procedural::{self, PixelRect};
use crate::grid::{CellFlags, CellGrid, CellMetrics, FIRA_CODE_BOLD, FIRA_CODE_REGULAR, fallback_char};

/// Upper bound on either output image axis in pixels.
pub const MAX_CAPTURE_DIMENSION: u32 = 16_384;

/// Upper bound on total RGBA framebuffer allocation (256 MiB, 64M pixels).
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;

/// Validates output image dimensions before allocating an RGBA buffer.
pub fn validate_dimensions(width: u32, height: u32) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err("capture width and height must be non-zero".to_string());
    }
    if width > MAX_CAPTURE_DIMENSION || height > MAX_CAPTURE_DIMENSION {
        return Err(format!(
            "capture dimensions {width}x{height} exceed maximum {MAX_CAPTURE_DIMENSION} px per axis"
        ));
    }
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|w| {
            usize::try_from(height)
                .ok()
                .and_then(|h| w.checked_mul(h)?.checked_mul(4))
        })
        .ok_or_else(|| format!("capture dimensions {width}x{height} overflow buffer size"))?;
    if bytes > MAX_CAPTURE_BYTES {
        return Err(format!(
            "capture dimensions {width}x{height} require {bytes} bytes, exceeding limit of {MAX_CAPTURE_BYTES}"
        ));
    }
    Ok(bytes)
}

struct CachedGlyph {
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    bitmap: Vec<u8>,
}

struct FontCache {
    regular: Font,
    bold: Font,
    size_px: f32,
    glyphs: HashMap<(char, bool), CachedGlyph>,
}

impl FontCache {
    fn new(size_px: f32) -> Result<Self, String> {
        let settings = FontSettings {
            scale: size_px,
            ..FontSettings::default()
        };
        let regular = Font::from_bytes(FIRA_CODE_REGULAR, settings)
            .map_err(|e| format!("failed to parse embedded Fira Code Regular font: {e}"))?;
        let bold = Font::from_bytes(FIRA_CODE_BOLD, settings)
            .map_err(|e| format!("failed to parse embedded Fira Code Bold font: {e}"))?;
        Ok(Self {
            regular,
            bold,
            size_px,
            glyphs: HashMap::new(),
        })
    }

    fn rasterize(&mut self, ch: char, bold: bool) -> &CachedGlyph {
        let mapped = fallback_char(ch);
        self.glyphs.entry((mapped, bold)).or_insert_with(|| {
            let (font, gid) = if bold {
                let gid = self.bold.lookup_glyph_index(mapped);
                if gid != 0 {
                    (&self.bold, gid)
                } else {
                    let reg_gid = self.regular.lookup_glyph_index(mapped);
                    if reg_gid != 0 {
                        (&self.regular, reg_gid)
                    } else {
                        (&self.bold, 0)
                    }
                }
            } else {
                (&self.regular, self.regular.lookup_glyph_index(mapped))
            };
            let (metrics, bitmap) = font.rasterize_indexed(gid, self.size_px);
            CachedGlyph {
                xmin: metrics.xmin,
                ymin: metrics.ymin,
                width: metrics.width,
                height: metrics.height,
                bitmap,
            }
        })
    }
}

struct Canvas {
    width: i32,
    height: i32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32, clear_rgb: [u8; 3]) -> Result<Self, String> {
        let bytes = validate_dimensions(width, height)?;
        let pixel_count = bytes / 4;
        let mut rgba = Vec::with_capacity(bytes);
        let clear = [clear_rgb[0], clear_rgb[1], clear_rgb[2], 255];
        for _ in 0..pixel_count {
            rgba.extend_from_slice(&clear);
        }
        Ok(Self {
            width: width as i32,
            height: height as i32,
            rgba,
        })
    }

    fn fill_rect_opaque(&mut self, rect: PixelRect, rgb: [u8; 3]) {
        let x0 = rect.x0.clamp(0, self.width);
        let y0 = rect.y0.clamp(0, self.height);
        let x1 = rect.x1.clamp(0, self.width);
        let y1 = rect.y1.clamp(0, self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let pixel = [rgb[0], rgb[1], rgb[2], 255];
        let stride = self.width as usize * 4;
        for y in y0..y1 {
            let row = y as usize * stride;
            for x in x0..x1 {
                let off = row + x as usize * 4;
                self.rgba[off..off + 4].copy_from_slice(&pixel);
            }
        }
    }

    fn fill_rect(&mut self, rect: PixelRect, rgb: [u8; 3], alpha: f32) {
        if alpha >= 1.0 {
            self.fill_rect_opaque(rect, rgb);
            return;
        }
        if alpha <= 0.0 {
            return;
        }
        let x0 = rect.x0.clamp(0, self.width);
        let y0 = rect.y0.clamp(0, self.height);
        let x1 = rect.x1.clamp(0, self.width);
        let y1 = rect.y1.clamp(0, self.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let stride = self.width as usize * 4;
        for y in y0..y1 {
            let row = y as usize * stride;
            for x in x0..x1 {
                blend_pixel(&mut self.rgba[row + x as usize * 4..], rgb, alpha);
            }
        }
    }

    fn blend_glyph(&mut self, glyph: &CachedGlyph, dst_x0: i32, dst_y0: i32, rgb: [u8; 3], coverage_lut: &[f32; 256]) {
        if glyph.width == 0 || glyph.height == 0 {
            return;
        }
        let gx_start = (-dst_x0).max(0) as usize;
        let gy_start = (-dst_y0).max(0) as usize;
        let gx_end = (self.width - dst_x0).clamp(0, glyph.width as i32) as usize;
        let gy_end = (self.height - dst_y0).clamp(0, glyph.height as i32) as usize;
        if gx_start >= gx_end || gy_start >= gy_end {
            return;
        }
        let stride = self.width as usize * 4;
        for gy in gy_start..gy_end {
            let dst_row = (dst_y0 + gy as i32) as usize * stride;
            let src_row = gy * glyph.width;
            for gx in gx_start..gx_end {
                let cov = glyph.bitmap[src_row + gx];
                if cov == 0 {
                    continue;
                }
                let alpha = coverage_lut[usize::from(cov)];
                let off = dst_row + (dst_x0 + gx as i32) as usize * 4;
                blend_pixel(&mut self.rgba[off..], rgb, alpha);
            }
        }
    }
}

#[inline]
fn blend_pixel(dst: &mut [u8], fg: [u8; 3], alpha: f32) {
    let inv = 1.0 - alpha;
    dst[0] = (f32::from(fg[0]) * alpha + f32::from(dst[0]) * inv)
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[1] = (f32::from(fg[1]) * alpha + f32::from(dst[1]) * inv)
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[2] = (f32::from(fg[2]) * alpha + f32::from(dst[2]) * inv)
        .round()
        .clamp(0.0, 255.0) as u8;
    dst[3] = 255;
}

fn build_coverage_lut(base_alpha: f32, weight_boost: bool) -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let mut cov = i as f32 / 255.0;
        if weight_boost {
            cov = cov.sqrt();
        }
        *slot = cov * base_alpha;
    }
    lut
}

/// Renders `grid` into an opaque `Rgba8` buffer of `width × height` pixels on the CPU.
pub fn render_grid(
    grid: &CellGrid,
    metrics: CellMetrics,
    width: u32,
    height: u32,
    weight_boost: bool,
) -> Result<Vec<u8>, String> {
    let clear_rgb = RGB_LUT[usize::from(grid.dominant_bg())];
    let mut canvas = Canvas::new(width, height, clear_rgb)?;
    let mut fonts = FontCache::new(metrics.size_px)?;

    let cols = usize::from(grid.cols());
    let rows = usize::from(grid.rows());
    let cells = grid.cells();
    let cw = metrics.width_px.round().max(1.0) as i32;
    let ch = metrics.height_px.round().max(1.0) as i32;
    let baseline_px = metrics.baseline_px.round() as i32;

    // Pass 1: cell backgrounds, coalesced into horizontal runs per row.
    for r in 0..rows {
        let y0 = r as i32 * ch;
        let y1 = y0 + ch;
        let row = &cells[r * cols..(r + 1) * cols];
        let mut c = 0usize;
        while c < cols {
            let bg_idx = row[c].bg;
            let mut end = c + 1;
            while end < cols && row[end].bg == bg_idx {
                end += 1;
            }
            let bg = RGB_LUT[usize::from(bg_idx)];
            canvas.fill_rect_opaque(
                PixelRect {
                    x0: c as i32 * cw,
                    y0,
                    x1: end as i32 * cw,
                    y1,
                },
                bg,
            );
            c = end;
        }
    }

    // Precompute coverage LUTs once per frame for full and dimmed cells.
    let full_lut = build_coverage_lut(1.0, weight_boost);
    let dim_lut = build_coverage_lut(DIM_ALPHA, weight_boost);

    // Pass 2: decorations, procedural grid glyphs, and rasterized text glyphs.
    for r in 0..rows {
        let y0 = r as i32 * ch;
        let row = &cells[r * cols..(r + 1) * cols];
        for (c, cell) in row.iter().enumerate() {
            let x0 = c as i32 * cw;
            let is_dim = cell.flags.contains(CellFlags::DIM);
            let alpha = if is_dim { DIM_ALPHA } else { 1.0 };
            let fg = RGB_LUT[usize::from(cell.fg)];

            if cell.flags.contains(CellFlags::UNDERLINED) {
                canvas.fill_rect(procedural::underline_rect(x0, y0, metrics), fg, alpha);
            }
            if cell.flags.contains(CellFlags::CROSSED_OUT) {
                canvas.fill_rect(procedural::strikethrough_rect(x0, y0, metrics), fg, alpha);
            }

            if cell.symbol == ' ' {
                continue;
            }

            if let Some(mask) = procedural::octant_mask(cell.symbol) {
                procedural::octant_rects(mask, x0, y0, cw, ch, |rect| {
                    canvas.fill_rect(rect, fg, alpha);
                });
            } else if let Some(mask) = procedural::braille_mask(cell.symbol) {
                procedural::braille_rects(mask, x0, y0, cw, ch, |rect| {
                    canvas.fill_rect(rect, fg, alpha);
                });
            } else if let Some(arms) = procedural::box_drawing_arms(cell.symbol) {
                procedural::box_drawing_rects(arms, x0, y0, cw, ch, |rect| {
                    canvas.fill_rect(rect, fg, alpha);
                });
            } else {
                let glyph = fonts.rasterize(cell.symbol, cell.flags.contains(CellFlags::BOLD));
                if glyph.width > 0 && glyph.height > 0 {
                    let dst_x0 = x0 + glyph.xmin;
                    let dst_y0 = y0 + baseline_px - glyph.height as i32 - glyph.ymin;
                    let lut = if is_dim { &dim_lut } else { &full_lut };
                    canvas.blend_glyph(glyph, dst_x0, dst_y0, fg, lut);
                }
            }
        }
    }

    Ok(canvas.rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Cell, FIRA_CODE_METRICS};

    fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let off = ((y * width + x) * 4) as usize;
        [rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3]]
    }

    #[test]
    fn fontdue_metrics_match_shared_fira_code_metrics() {
        let regular = Font::from_bytes(FIRA_CODE_REGULAR, FontSettings::default()).unwrap();
        for size_px in [12.0f32, 15.0, 24.0] {
            let line = regular.horizontal_line_metrics(size_px).unwrap();
            let advance = regular.metrics('M', size_px).advance_width;
            let expected = FIRA_CODE_METRICS.cell_metrics(size_px);
            let got = crate::grid::FontMetrics {
                ascent_em: line.ascent / size_px,
                descent_em: line.descent / size_px,
                line_gap_em: line.line_gap / size_px,
                advance_em: advance / size_px,
            }
            .cell_metrics(size_px);
            assert_eq!(got, expected, "cell metrics mismatch at {size_px}px");
        }
    }

    #[test]
    fn seeded_half_cell_boundaries_keep_exact_integer_edges_at_odd_cell_size() {
        // 12pt at 1x gives odd 7x15 px cells: left half = 4 px [0..4), right = 3 px [4..7);
        // top half = 8 px [0..8), bottom = 7 px [8..15). Every column and row must use the same
        // sub-cell edges so coverage and alignment marks never drift across the grid.
        let metrics = FIRA_CODE_METRICS.cell_metrics(12.0);
        assert_eq!((metrics.width_px, metrics.height_px), (7.0, 15.0));

        let cols = 5u16;
        let rows = 3u16;
        let width = u32::from(cols) * 7;
        let height = u32::from(rows) * 15;
        let mut grid = CellGrid::new(cols, rows);

        // Row 0: upper half block `▀`, fg=196 (red), bg=21 (blue).
        // Row 1: lower half block `▄`, fg=46 (green), bg=226 (yellow).
        // Row 2: left half block `▌`, fg=201 (magenta), bg=51 (cyan).
        for c in 0..cols {
            grid.set(
                c,
                0,
                Cell {
                    symbol: '▀',
                    fg: 196,
                    bg: 21,
                    flags: CellFlags::NONE,
                },
            );
            grid.set(
                c,
                1,
                Cell {
                    symbol: '▄',
                    fg: 46,
                    bg: 226,
                    flags: CellFlags::NONE,
                },
            );
            grid.set(
                c,
                2,
                Cell {
                    symbol: '▌',
                    fg: 201,
                    bg: 51,
                    flags: CellFlags::NONE,
                },
            );
        }

        let rgba = render_grid(&grid, metrics, width, height, false).unwrap();
        let rgba_of = |idx: u8| {
            let [r, g, b] = RGB_LUT[usize::from(idx)];
            [r, g, b, 255]
        };

        for c in 0..u32::from(cols) {
            let x0 = c * 7;
            // Row 0: y in [0..8) is fg(196), y in [8..15) is bg(21).
            for dy in 0..15 {
                let expected = if dy < 8 { rgba_of(196) } else { rgba_of(21) };
                for dx in 0..7 {
                    assert_eq!(
                        pixel_at(&rgba, width, x0 + dx, dy),
                        expected,
                        "row 0 col {c} dx={dx} dy={dy}"
                    );
                }
            }
            // Row 1: y in [0..8) is bg(226), y in [8..15) is fg(46).
            for dy in 0..15 {
                let expected = if dy < 8 { rgba_of(226) } else { rgba_of(46) };
                for dx in 0..7 {
                    assert_eq!(
                        pixel_at(&rgba, width, x0 + dx, 15 + dy),
                        expected,
                        "row 1 col {c} dx={dx} dy={dy}"
                    );
                }
            }
            // Row 2: x in [0..4) is fg(201), x in [4..7) is bg(51).
            for dy in 0..15 {
                for dx in 0..7 {
                    let expected = if dx < 4 { rgba_of(201) } else { rgba_of(51) };
                    assert_eq!(
                        pixel_at(&rgba, width, x0 + dx, 30 + dy),
                        expected,
                        "row 2 col {c} dx={dx} dy={dy}"
                    );
                }
            }
        }
    }

    #[test]
    fn text_flags_weight_boost_and_fallback_glyph_affect_rendered_pixels() {
        let metrics = FIRA_CODE_METRICS.cell_metrics(12.0);
        let cw = metrics.width_px as u32;
        let ch = metrics.height_px as u32;
        let mut grid = CellGrid::new(4, 1);
        // White fg (231) on black bg (16) so luminance directly reflects coverage & dim.
        grid.set(
            0,
            0,
            Cell {
                symbol: 'A',
                fg: 231,
                bg: 16,
                flags: CellFlags::NONE,
            },
        );
        grid.set(
            1,
            0,
            Cell {
                symbol: 'A',
                fg: 231,
                bg: 16,
                flags: CellFlags::DIM,
            },
        );
        grid.set(
            2,
            0,
            Cell {
                symbol: ' ',
                fg: 231,
                bg: 16,
                flags: CellFlags::UNDERLINED.union(CellFlags::CROSSED_OUT),
            },
        );
        // `▸` is absent from Fira Code and must map to `▶`.
        grid.set(
            3,
            0,
            Cell {
                symbol: '▸',
                fg: 231,
                bg: 16,
                flags: CellFlags::BOLD,
            },
        );

        let normal = render_grid(&grid, metrics, cw * 4, ch, false).unwrap();
        let boosted = render_grid(&grid, metrics, cw * 4, ch, true).unwrap();

        let sum_cell = |buf: &[u8], col: u32| -> u64 {
            let mut sum = 0u64;
            for y in 0..ch {
                for x in (col * cw)..((col + 1) * cw) {
                    sum += u64::from(pixel_at(buf, cw * 4, x, y)[0]);
                }
            }
            sum
        };

        let normal_a = sum_cell(&normal, 0);
        let dim_a = sum_cell(&normal, 1);
        let boosted_a = sum_cell(&boosted, 0);
        let fallback_arrow = sum_cell(&normal, 3);

        assert!(normal_a > 0, "glyph 'A' must paint non-zero foreground pixels");
        assert!(dim_a > 0 && dim_a < normal_a, "DIM cell must blend dimmer than normal");
        assert!(
            boosted_a > normal_a,
            "weight_boost sqrt(coverage) must thicken antialiased strokes"
        );
        assert!(fallback_arrow > 0, "fallback glyph `▸` -> `▶` must render");

        // Cell 2 has underline and strikethrough rules on a blank symbol.
        let ul = procedural::underline_rect((2 * cw) as i32, 0, metrics);
        let st = procedural::strikethrough_rect((2 * cw) as i32, 0, metrics);
        assert_eq!(
            pixel_at(&normal, cw * 4, ul.x0 as u32, ul.y0 as u32),
            [255, 255, 255, 255]
        );
        assert_eq!(
            pixel_at(&normal, cw * 4, st.x0 as u32, st.y0 as u32),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn validate_dimensions_rejects_zero_and_oversized_allocations() {
        assert!(validate_dimensions(0, 100).is_err());
        assert!(validate_dimensions(100, 0).is_err());
        assert!(validate_dimensions(MAX_CAPTURE_DIMENSION + 1, 16).is_err());
        assert!(validate_dimensions(16, MAX_CAPTURE_DIMENSION + 1).is_err());
        assert!(validate_dimensions(MAX_CAPTURE_DIMENSION, MAX_CAPTURE_DIMENSION).is_err());
        assert_eq!(validate_dimensions(980, 600).unwrap(), 980 * 600 * 4);
    }
}
