//! Pixel and glyph planes share cell geometry: glyphs are cell-strided and
//! indexed from pixel coordinates. `reduce_into` clears reused output first.
//! Pixel colors stay concrete because reduction chooses fg/bg from pixels;
//! `Color::Reset` cannot represent either pixel channel.

use ratatui::style::{Color, Modifier};

const UNCOVERED: char = ' ';
const FULL_BLOCK: char = '\u{2588}';
const HALF_BLOCK_TOP: char = '\u{2580}';
const HALF_BLOCK_BOTTOM: char = '\u{2584}';
const LEFT_HALF_BLOCK: char = '\u{258c}';
const RIGHT_HALF_BLOCK: char = '\u{2590}';

const QUADRANT_GLYPHS: [char; 16] = [
    UNCOVERED,
    '\u{2598}',
    '\u{259d}',
    HALF_BLOCK_TOP,
    '\u{2596}',
    LEFT_HALF_BLOCK,
    '\u{259e}',
    '\u{2599}',
    '\u{2597}',
    '\u{259f}',
    RIGHT_HALF_BLOCK,
    '\u{259c}',
    HALF_BLOCK_BOTTOM,
    '\u{259b}',
    '\u{259a}',
    FULL_BLOCK,
];

/// Cell-local glyph overlay. `None` background preserves the reduced pixel background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    pub ch: char,
    pub fg: Color,
    pub bg: Option<Color>,
    pub bold: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReducedCell {
    pub symbol: char,
    pub fg: Color,
    pub bg: Color,
    pub modifier: Modifier,
}

impl ReducedCell {
    /// An uncovered reduced cell.
    pub(crate) fn uncovered(fallback: Color) -> Self {
        Self {
            symbol: UNCOVERED,
            fg: fallback,
            bg: fallback,
            modifier: Modifier::empty(),
        }
    }
}

/// Both planes are positional: a write replaces whatever an earlier write left at that coordinate,
/// so painter's order governs overlaps *between* the planes too. The glyph plane was once an
/// append list swept after the pixel pass, which made a glyph beat a pixel regardless of order and
/// caused two rendering defects. Don't reintroduce one.
pub struct Framebuffer {
    pixels: Vec<Option<Color>>,
    /// Cell-strided, `cell_cols * cell_rows` long — **not** the pixel stride.
    glyphs: Vec<Option<Glyph>>,
    width: usize,
    height: usize,
    scale_x: usize,
    scale_y: usize,
}

impl Framebuffer {
    /// Creates a virtual-pixel buffer with the given cell scale.
    pub fn new(width: usize, height: usize, scale_x: usize, scale_y: usize) -> Self {
        let (scale_x, scale_y) = (scale_x.max(1), scale_y.max(1));
        Self {
            pixels: vec![None; width * height],
            glyphs: vec![None; (width / scale_x) * (height / scale_y)],
            width,
            height,
            scale_x,
            scale_y,
        }
    }

    #[cfg(test)]
    pub fn width(&self) -> usize {
        self.width
    }

    #[cfg(test)]
    pub fn height(&self) -> usize {
        self.height
    }

    /// Terminal-cell columns.
    pub fn cell_cols(&self) -> usize {
        self.width / self.scale_x
    }

    /// Terminal-cell rows.
    pub fn cell_rows(&self) -> usize {
        self.height / self.scale_y
    }

    /// Returns the backing index for a pixel coordinate.
    ///
    /// Each axis is checked separately. A flat check against the backing length lets an over-wide
    /// `x` wrap onto the next row, and the reducer's `sx + 1` read on an odd-width buffer would then
    /// paint another row's pixel into the rightmost cell instead of leaving it uncovered.
    fn pixel_index(&self, x: usize, y: usize) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(y * self.width + x)
    }

    /// Returns the cell containing a pixel coordinate.
    fn cell_index(&self, px_x: usize, px_y: usize) -> Option<usize> {
        if px_x >= self.width || px_y >= self.height {
            return None;
        }
        let cols = self.cell_cols();
        let (cell_x, cell_y) = (px_x / self.scale_x, px_y / self.scale_y);
        if cell_x >= cols || cell_y >= self.cell_rows() {
            return None;
        }
        Some(cell_y * cols + cell_x)
    }

    pub fn pixel(&self, x: usize, y: usize) -> Option<Color> {
        self.pixel_index(x, y)
            .and_then(|index| self.pixels.get(index).copied().flatten())
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: Color) {
        if let Some(index) = self.pixel_index(x, y) {
            self.pixels[index] = Some(color);
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for row in y..y_end {
            for col in x..x_end {
                if let Some(index) = self.pixel_index(col, row) {
                    self.pixels[index] = Some(color);
                }
            }
        }
    }

    /// Writes a glyph at the containing cell; later writes replace earlier ones.
    pub fn set_glyph(&mut self, px_x: usize, px_y: usize, glyph: Glyph) {
        if let Some(idx) = self.cell_index(px_x, px_y) {
            self.glyphs[idx] = Some(glyph);
        }
    }

    /// Clears the glyph at the containing cell.
    pub fn clear_glyph(&mut self, px_x: usize, px_y: usize) {
        if let Some(idx) = self.cell_index(px_x, px_y) {
            self.glyphs[idx] = None;
        }
    }

    /// Returns the glyph at the cell containing the pixel.
    #[cfg(test)]
    pub fn glyph(&self, px_x: usize, px_y: usize) -> Option<Glyph> {
        self.cell_index(px_x, px_y).and_then(|idx| self.glyphs[idx])
    }

    #[cfg(test)]
    pub fn reduce(&self, scale_x: usize, scale_y: usize, fallback: Color) -> Vec<ReducedCell> {
        let mut cells = Vec::new();
        self.reduce_into(&mut cells, scale_x, scale_y, fallback);
        cells
    }

    /// Reduces into a caller-owned vec, clearing and resizing it first.
    pub fn reduce_into(&self, out: &mut Vec<ReducedCell>, scale_x: usize, scale_y: usize, fallback: Color) {
        let cols = self.width / scale_x;
        let rows = self.height / scale_y;

        out.clear();
        out.resize(cols * rows, ReducedCell::uncovered(fallback));

        for cell_y in 0..rows {
            for cell_x in 0..cols {
                let cell = &mut out[cell_y * cols + cell_x];

                if let Some(reduced) = self.reduce_one_cell(cell_x, cell_y, scale_x, scale_y, fallback) {
                    *cell = reduced;
                }

                if let Some(g) = self.glyph_at_cell(cell_x, cell_y) {
                    cell.symbol = g.ch;
                    cell.fg = g.fg;
                    if let Some(bg) = g.bg {
                        cell.bg = bg;
                    }
                    if g.bold {
                        cell.modifier.insert(Modifier::BOLD);
                    }
                }
            }
        }
    }

    /// Reduces one pixel-cell quad, if it contains painted pixels.
    fn reduce_one_cell(
        &self,
        cell_x: usize,
        cell_y: usize,
        scale_x: usize,
        scale_y: usize,
        fallback: Color,
    ) -> Option<ReducedCell> {
        let mut cell = ReducedCell::uncovered(fallback);
        match (scale_x, scale_y) {
            (1, 1) => {
                let color = self.pixel(cell_x, cell_y)?;
                cell.symbol = FULL_BLOCK;
                cell.fg = color;
                cell.bg = color;
            }
            (2, 1) => {
                let sx = cell_x * 2;
                let lp = self.pixel(sx, cell_y);
                let rp = self.pixel(sx + 1, cell_y);
                if lp.is_none() && rp.is_none() {
                    return None;
                }
                let l = lp.unwrap_or(fallback);
                let r = rp.unwrap_or(fallback);
                if l == r {
                    cell.symbol = FULL_BLOCK;
                    cell.fg = l;
                    cell.bg = fallback;
                } else {
                    cell.symbol = LEFT_HALF_BLOCK;
                    cell.fg = l;
                    cell.bg = r;
                }
            }
            (1, 2) => {
                let sy = cell_y * 2;
                let tp = self.pixel(cell_x, sy);
                let bp = self.pixel(cell_x, sy + 1);
                if tp.is_none() && bp.is_none() {
                    return None;
                }
                let t = tp.unwrap_or(fallback);
                let b = bp.unwrap_or(fallback);
                if t == b {
                    cell.symbol = FULL_BLOCK;
                    cell.fg = t;
                    cell.bg = fallback;
                } else {
                    cell.symbol = HALF_BLOCK_TOP;
                    cell.fg = t;
                    cell.bg = b;
                }
            }
            (2, 2) => {
                let sx = cell_x * 2;
                let sy = cell_y * 2;
                let ul = self.pixel(sx, sy).unwrap_or(fallback);
                let ur = self.pixel(sx + 1, sy).unwrap_or(fallback);
                let ll = self.pixel(sx, sy + 1).unwrap_or(fallback);
                let lr = self.pixel(sx + 1, sy + 1).unwrap_or(fallback);
                let colours = [ul, ur, ll, lr];
                let distinct: Vec<Color> = {
                    let mut seen = Vec::new();
                    for &c in &colours {
                        if !seen.contains(&c) {
                            seen.push(c);
                        }
                    }
                    seen
                };
                if distinct.len() == 1 {
                    cell.symbol = FULL_BLOCK;
                    cell.fg = distinct[0];
                    cell.bg = fallback;
                } else {
                    let majority = distinct[0];
                    let other = distinct.get(1).copied().unwrap_or(fallback);
                    let mut mask = 0u8;
                    for (i, &c) in colours.iter().enumerate() {
                        if c == majority {
                            mask |= 1 << i;
                        }
                    }
                    cell.symbol = QUADRANT_GLYPHS[mask as usize];
                    cell.fg = majority;
                    cell.bg = other;
                }
            }
            _ => {
                let color = self.pixel(cell_x, cell_y)?;
                cell.symbol = FULL_BLOCK;
                cell.fg = color;
                cell.bg = fallback;
            }
        }
        Some(cell)
    }

    /// Reads a glyph by cell coordinate for the reduction loop.
    fn glyph_at_cell(&self, cell_x: usize, cell_y: usize) -> Option<Glyph> {
        let cols = self.cell_cols();
        if cell_x >= cols || cell_y >= self.cell_rows() {
            return None;
        }
        self.glyphs[cell_y * cols + cell_x]
    }

    /// Resizes backing storage when dimensions or scales change.
    pub fn resize_if_needed(&mut self, width: usize, height: usize, scale_x: usize, scale_y: usize) {
        let (scale_x, scale_y) = (scale_x.max(1), scale_y.max(1));
        if self.width == width && self.height == height && self.scale_x == scale_x && self.scale_y == scale_y {
            return;
        }
        let wanted = width * height;
        if self.pixels.len() < wanted {
            self.pixels.resize(wanted, None);
        }
        self.width = width;
        self.height = height;
        self.scale_x = scale_x;
        self.scale_y = scale_y;
        let wanted_cells = self.cell_cols() * self.cell_rows();
        if self.glyphs.len() < wanted_cells {
            self.glyphs.resize(wanted_cells, None);
        }
    }

    /// Clears pixels and glyphs while retaining backing capacity.
    pub fn clear(&mut self) {
        self.pixels.fill(None);
        self.glyphs.fill(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const BG: Color = Color::Indexed(235);
    const A: Color = Color::Indexed(1);
    const B: Color = Color::Indexed(2);
    const C: Color = Color::Indexed(3);

    #[test]
    fn pixel_storage_preserves_coordinates_bounds_and_reduction_fallback() {
        let mut fb = Framebuffer::new(3, 2, 1, 1);
        fb.set_pixel(1, 0, A);
        fb.set_pixel(0, 1, B);
        fb.set_pixel(3, 0, C);

        assert_eq!(fb.pixel(1, 0), Some(A));
        assert_eq!(fb.pixel(0, 1), Some(B));
        assert_eq!(fb.pixel(3, 0), None);
        assert_eq!(fb.pixel(0, 2), None);

        let mut partial = Framebuffer::new(2, 1, 2, 1);
        partial.set_pixel(0, 0, C);
        let reduced = partial.reduce(2, 1, BG);
        assert_eq!((reduced[0].fg, reduced[0].bg), (C, BG));
    }

    #[test]
    fn an_odd_width_horizontal_cell_reduces_to_uncovered() {
        let mut fb = Framebuffer::new(3, 2, 1, 1);
        fb.set_pixel(0, 1, A);
        assert_eq!(fb.reduce_one_cell(1, 0, 2, 1, BG), None);
    }

    #[test]
    fn fill_rect_places_seeded_values_and_clips_to_bounds() {
        let mut fb = Framebuffer::new(5, 3, 1, 1);
        fb.fill_rect(1, 1, 6, 4, A);
        for y in 0..3 {
            for x in 0..5 {
                let expected = if x >= 1 && y >= 1 { Some(A) } else { None };
                assert_eq!(fb.pixel(x, y), expected, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn reduce_1x1_identity() {
        let mut fb = Framebuffer::new(3, 2, 1, 1);
        fb.set_pixel(0, 0, A);
        fb.set_pixel(2, 1, B);
        let cells = fb.reduce(1, 1, BG);
        assert_eq!(cells.len(), 6);
        assert_eq!(cells[0].symbol, FULL_BLOCK);
        assert_eq!(cells[0].fg, A);
        assert_eq!(cells[0].bg, A);
        assert_eq!(cells[1].symbol, UNCOVERED);
        assert_eq!(cells[1].fg, BG);
        assert_eq!(cells[5].symbol, FULL_BLOCK);
        assert_eq!(cells[5].fg, B);
    }

    #[test]
    fn reachable_reductions_preserve_distinct_pixel_assignments() {
        let mut fb = Framebuffer::new(4, 1, 2, 1);
        fb.set_pixel(0, 0, A);
        fb.set_pixel(1, 0, B);
        fb.set_pixel(2, 0, C);
        fb.set_pixel(3, 0, A);
        let cells = fb.reduce(2, 1, BG);
        assert_eq!((cells[0].fg, cells[0].bg), (A, B));
        assert_eq!((cells[1].fg, cells[1].bg), (C, A));

        let mut vertical = Framebuffer::new(1, 4, 1, 2);
        vertical.set_pixel(0, 0, B);
        vertical.set_pixel(0, 1, C);
        vertical.set_pixel(0, 2, A);
        let cells = vertical.reduce(1, 2, BG);
        assert_eq!((cells[0].fg, cells[0].bg), (B, C));
        assert_eq!((cells[1].fg, cells[1].bg), (A, BG));
    }

    #[test]
    fn a_second_glyph_at_one_coordinate_replaces_the_first() {
        let mut fb = Framebuffer::new(2, 1, 1, 1);
        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'A',
                fg: A,
                bg: Some(A),
                bold: true,
            },
        );
        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'B',
                fg: C,
                bg: None,
                bold: false,
            },
        );

        let g = fb.glyph(0, 0).expect("a glyph must be present");
        assert_eq!(g.ch, 'B', "the later write must win the cell");
        assert_eq!(g.fg, C, "every field comes from the later write, not just the char");
        assert_eq!(g.bg, None, "a later bg: None must clear the earlier Some");
        assert!(!g.bold, "a later non-bold must clear the earlier bold");

        let cells = fb.reduce(1, 1, BG);
        assert_eq!(cells[0].symbol, 'B');
        assert!(
            !cells[0].modifier.contains(Modifier::BOLD),
            "the discarded glyph's bold must not survive into the reduction"
        );
    }

    #[test]
    fn clear_glyph_removes_an_earlier_glyph() {
        let mut fb = Framebuffer::new(2, 1, 1, 1);
        fb.set_pixel(0, 0, A);
        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'X',
                fg: C,
                bg: None,
                bold: false,
            },
        );
        fb.clear_glyph(0, 0);

        assert_eq!(fb.glyph(0, 0), None);
        let cells = fb.reduce(1, 1, BG);
        assert_eq!(
            cells[0].symbol, FULL_BLOCK,
            "with its glyph cleared the cell falls back to the pixel plane"
        );
    }

    #[test]
    fn glyph_bg_none_inherits_the_reduced_pixel_bg_and_some_replaces_it() {
        let mut fb = Framebuffer::new(3, 1, 1, 1);
        fb.set_pixel(0, 0, A);
        fb.set_pixel(1, 0, A);
        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'N',
                fg: C,
                bg: None,
                bold: false,
            },
        );
        fb.set_glyph(
            1,
            0,
            Glyph {
                ch: 'S',
                fg: C,
                bg: Some(B),
                bold: false,
            },
        );
        fb.set_glyph(
            2,
            0,
            Glyph {
                ch: 'Z',
                fg: C,
                bg: None,
                bold: false,
            },
        );

        let cells = fb.reduce(1, 1, BG);
        assert_eq!(cells[0].bg, A, "bg: None must keep the pixel plane's colour");
        assert_eq!(cells[1].bg, B, "bg: Some must replace it");
        assert_eq!((cells[2].symbol, cells[2].bg), ('Z', BG));
    }

    #[test]
    fn glyph_writes_are_addressed_in_pixels_and_land_in_the_containing_cell() {
        let mut fb = Framebuffer::new(4, 1, 2, 1);
        assert_eq!(fb.cell_cols(), 2, "4 pixels at scale 2 is 2 cells");

        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'L',
                fg: C,
                bg: None,
                bold: false,
            },
        );
        fb.set_glyph(
            1,
            0,
            Glyph {
                ch: 'R',
                fg: C,
                bg: None,
                bold: false,
            },
        );
        fb.set_glyph(
            2,
            0,
            Glyph {
                ch: 'N',
                fg: C,
                bg: None,
                bold: false,
            },
        );

        let cells = fb.reduce(2, 1, BG);
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells[0].symbol, 'R',
            "both pixels of a cell address one glyph slot, so the later write wins"
        );
        assert_eq!(cells[1].symbol, 'N', "the next cell is untouched by either");
    }

    #[test]
    fn clear_blanks_the_glyph_plane() {
        let mut fb = Framebuffer::new(2, 1, 1, 1);
        fb.set_glyph(
            0,
            0,
            Glyph {
                ch: 'X',
                fg: C,
                bg: Some(B),
                bold: false,
            },
        );
        fb.set_glyph(
            0,
            1,
            Glyph {
                ch: 'Y',
                fg: A,
                bg: None,
                bold: false,
            },
        );
        assert_eq!(fb.glyph(0, 1), None, "an out-of-bounds write must be dropped");
        fb.clear();

        assert_eq!(fb.glyph(0, 0), None);
        let cells = fb.reduce(1, 1, BG);
        assert_eq!(cells[0].symbol, UNCOVERED, "a stale glyph must not survive clear");
        assert_eq!(cells[0].bg, BG, "nor its background");
    }

    #[test]
    fn resize_if_needed_regrows_the_glyph_plane_when_only_the_scale_changes() {
        let mut fb = Framebuffer::new(8, 2, 2, 1);
        assert_eq!(fb.cell_cols(), 4);

        fb.resize_if_needed(8, 2, 1, 1);
        assert_eq!(fb.cell_cols(), 8, "halving scale_x doubles the cell columns");

        fb.set_glyph(
            7,
            1,
            Glyph {
                ch: 'E',
                fg: C,
                bg: None,
                bold: false,
            },
        );
        assert_eq!(fb.glyph(7, 1).map(|g| g.ch), Some('E'));
    }

    #[test]
    fn reduce_into_resets_cells_left_uncovered_by_this_frame() {
        let mut fb = Framebuffer::new(2, 1, 1, 1);
        fb.set_pixel(0, 0, A);
        let mut out = Vec::new();
        fb.reduce_into(&mut out, 1, 1, BG);
        assert_eq!(out[0].symbol, FULL_BLOCK, "the painted frame inks cell 0");

        fb.clear();
        fb.reduce_into(&mut out, 1, 1, BG);
        assert_eq!(
            out[0].symbol, UNCOVERED,
            "stale glyph must not survive into an empty frame"
        );
        assert_eq!(out[0].fg, BG, "stale fg must not survive");
        assert_eq!(out[0].bg, BG, "stale bg must not survive");
    }

    #[test]
    fn resized_frames_keep_seeded_pixels_at_their_coordinates() {
        let rows = 4usize;
        let mut fb = Framebuffer::new(0, 0, 1, 1);
        let mut out: Vec<ReducedCell> = Vec::new();

        let frame = |fb: &mut Framebuffer, out: &mut Vec<ReducedCell>, width: usize| {
            fb.resize_if_needed(width, rows, 1, 1);
            fb.clear();
            fb.set_pixel(width / 2, 0, A);
            fb.set_pixel(width - 1, rows - 1, B);
            fb.reduce_into(out, 1, 1, BG);
            assert_eq!(out.len(), width * rows);
            assert_eq!(out[width / 2].fg, A, "the first seed must stay at its coordinate");
            assert_eq!(
                out[(rows - 1) * width + width - 1].fg,
                B,
                "the second seed must stay at its coordinate"
            );
            for (position, cell) in out.iter().enumerate() {
                if position != width / 2 && position != (rows - 1) * width + width - 1 {
                    assert_eq!(cell.symbol, UNCOVERED, "stale content at {position}");
                }
            }
        };

        for width in [8, 9, 10] {
            frame(&mut fb, &mut out, width);
        }
    }
}
