//! Cell painting, bounded text overlays, and sub-cell chart glyphs.
//!
//! A terminal cell is one character, but a chart often needs finer steps than whole rows. Both
//! vocabularies here divide a cell into the same 2x4 grid, so a fill level computed once renders in
//! either: [`GlyphSet::Braille`] is portable, [`GlyphSet::Octant`] is sharper but Unicode 16 (2024)
//! and renders tofu on an older font, which is why octants sit behind a setting and braille is the
//! default. Callers resolve the vocabulary once at a renderer boundary and then speak in masks.
//!
//! **The 2x4 topology is shared; the bit order is not.** Braille numbers its dots down each column
//! with the bottom row in the high bits, octants read left-to-right then top-to-bottom. A mask
//! built for one is a different picture in the other, so build masks from [`octant`] or
//! [`BRAILLE_ROW_BITS`] and hand them to the matching constructor rather than reusing a literal.
//!
//! The half blocks are the other half of the vocabulary: where a braille or octant cell subdivides
//! *vertically*, `LEFT_HALF_BLOCK`/`RIGHT_HALF_BLOCK` subdivide horizontally, letting one cell show
//! two colours side by side — a foreground glyph over a background, not two foregrounds.
//!
//! [`paint_cell`] sets symbol, fg and bg together, since a sub-cell glyph is only legible when both
//! channels are known: the lit dots take the fg and the unlit ground takes the bg. Never cross-
//! assign the two, and where a track's own background may vary underneath, read the existing
//! `cell.bg` and pass it back rather than flattening it.

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Style},
};

/// Fills the left half; the background shows through the right.
pub(crate) const LEFT_HALF_BLOCK: &str = "\u{258c}";
/// Fills the right half; the background shows through the left.
pub(crate) const RIGHT_HALF_BLOCK: &str = "\u{2590}";

/// Braille bits for each row, from top to bottom, with both dot columns lit.
const BRAILLE_ROW_BITS: [u8; 4] = [0x09, 0x12, 0x24, 0xC0];

const BRAILLE_BASE: u32 = 0x2800;

/// The terminal-cell vocabulary selected by a chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyphSet {
    Braille,
    Octant,
}

/// Masks for the two braille dot columns.
pub(crate) const BRAILLE_LEFT_COLUMN: u8 = 0x01 | 0x02 | 0x04 | 0x40;
pub(crate) const BRAILLE_RIGHT_COLUMN: u8 = 0x08 | 0x10 | 0x20 | 0x80;

/// Full-height rule used for the `Close`-zoom centline.
pub(crate) const VERTICAL_RULE: char = '\u{2502}';

/// Bit positions of the octant 2x4 cell grid, LSB first, reading left-to-right then top-to-bottom:
///
/// ```text
///   0 1
///   2 3
///   4 5
///   6 7
/// ```
///
/// The same 2x4 topology braille uses, so a fill level carries over to either glyph set — but the
/// bit *order* does not. Braille's historical dot numbering runs down each column and puts its
/// bottom row in the high bits, so a mask built for [`braille_char`] means something else here.
/// Build the mask from these constants and hand it to the one you mean.
pub(crate) mod octant {
    /// Both cells of one row, top row first.
    pub(crate) const ROWS: [u8; 4] = [0x03, 0x0C, 0x30, 0xC0];
    /// All four cells of the left and right half-columns.
    pub(crate) const LEFT_COLUMN: u8 = 0x55;
    pub(crate) const RIGHT_COLUMN: u8 = 0xAA;
}

/// Builds an octant character from a 2x4 cell mask; see [`octant`] for the bit layout.
///
/// Delegates to ratatui's table rather than computing a codepoint. Unicode 16 encoded only the
/// masks that had no glyph already — the space, half, quarter and full blocks kept their existing
/// codepoints instead of being duplicated — so the mapping is not `base + mask` arithmetic, and a
/// local copy of the 256 entries would be a second thing to keep right.
///
/// **These are Unicode 16 (2024).** A font without them renders tofu, which is why every caller
/// reaches this behind a setting rather than by default.
pub(crate) fn octant_char(mask: u8) -> char {
    ratatui::symbols::pixel::OCTANTS[mask as usize]
}

/// Builds a braille character from filled rows and a column mask.
pub(crate) fn braille_char(filled_rows: u8, mask: u8) -> char {
    let filled_rows = filled_rows.min(4);
    let bits = BRAILLE_ROW_BITS[..filled_rows as usize]
        .iter()
        .fold(0u8, |acc, b| acc | b)
        & mask;
    char::from_u32(BRAILLE_BASE + bits as u32).unwrap_or(' ')
}

impl GlyphSet {
    /// Resolves the vocabulary once at a renderer boundary.
    pub(crate) fn from_octants(octants: bool) -> Self {
        if octants { Self::Octant } else { Self::Braille }
    }

    /// Builds a full-cell mask for `filled` rows from the top.
    pub(crate) fn fill_mask(self, filled: u8) -> u8 {
        let rows = match self {
            Self::Braille => &BRAILLE_ROW_BITS,
            Self::Octant => &octant::ROWS,
        };
        rows.iter()
            .take(filled.min(4) as usize)
            .fold(0u8, |mask, row| mask | row)
    }

    /// Converts a full-cell mask into this vocabulary's character.
    pub(crate) fn glyph(self, mask: u8) -> char {
        match self {
            Self::Braille => braille_char(4, mask),
            Self::Octant => octant_char(mask),
        }
    }
}

/// Sets a cell's symbol and both color channels.
pub fn paint_cell(cell: &mut Cell, symbol: &str, fg: Color, bg: Color) {
    cell.set_symbol(symbol);
    cell.set_style(Style::default().fg(fg).bg(bg));
}

/// Paints single-column characters at an absolute position, within the pane and buffer.
///
/// A start left of the pane skips the hidden prefix so the visible suffix keeps its anchor.
pub(crate) fn paint_text(buffer: &mut Buffer, area: Rect, start_x: i32, row_y: u16, text: &str, style: Style) {
    let area = area.intersection(*buffer.area());
    let first_x = start_x.max(i32::from(area.left()));
    if !(area.top()..area.bottom()).contains(&row_y) || first_x >= i32::from(area.right()) {
        return;
    }
    let skipped = first_x.abs_diff(start_x) as usize;
    for (x, ch) in (first_x as u16..area.right()).zip(text.chars().skip(skipped)) {
        buffer[(x, row_y)].set_char(ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GlyphSet::glyph` passes a saturating row count so the mask alone decides the cell — the
    /// property that lets `fill_mask` own fill order and `glyph` own nothing but the lookup. If
    /// braille's rows ever stop covering all eight dots, `glyph` would silently drop bits.
    #[test]
    fn a_braille_glyph_is_its_mask_offset_from_the_braille_base() {
        for mask in 0..=u8::MAX {
            assert_eq!(
                GlyphSet::Braille.glyph(mask),
                char::from_u32(BRAILLE_BASE + u32::from(mask)).unwrap(),
                "mask {mask:#04x} must survive the glyph lookup unchanged"
            );
        }
    }

    #[test]
    fn fill_masks_are_monotone_for_both_vocabularies() {
        for glyphs in [GlyphSet::Braille, GlyphSet::Octant] {
            let mut previous = 0;
            for filled in 0..=4 {
                let mask = glyphs.fill_mask(filled);
                assert_eq!(mask.count_ones(), 2 * u32::from(filled));
                assert_eq!(previous & mask, previous);
                previous = mask;
            }
        }
    }
}
