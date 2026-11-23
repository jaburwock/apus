//! Relative-size bars for contigs, drawn as braille dots or Unicode 16 octant blocks.
//!
//! Both glyph sets are a 2x4 grid per cell, so the fill arithmetic is shared and only the mask
//! construction differs — see [`painter::octant`] for why the two bit orders are not interchangeable.

use ratatui::style::Color;

use crate::basepair::BasePair;
use crate::viewer_app::painter::{self, BRAILLE_LEFT_COLUMN, BRAILLE_RIGHT_COLUMN, GlyphSet, octant};

use super::layout::CONTIG_CELL_WIDTH;

/// Maps a contig length to a non-empty normalized fill level.
pub(super) fn dot_fill_level(length: BasePair, longest: BasePair, max_levels: u8) -> u8 {
    if longest.as_u64() == 0 {
        return 1;
    }
    let ratio = length.as_f64() / longest.as_f64();
    let level = (ratio * max_levels as f64).round();
    level.clamp(1.0, max_levels as f64) as u8
}

/// Splits a fill level across rows, topmost first.
pub(super) fn split_fill_level(level: u8, num_rows: usize) -> Vec<u8> {
    let max_level = 4 * num_rows as u8;
    let level = level.min(max_level);
    if num_rows <= 1 {
        return vec![level];
    }
    let top = level.min(4);
    let bottom = level.saturating_sub(4);
    vec![top, bottom]
}

/// Returns the per-column mask for a contig bar, blanking the outer dot-columns of the outer cells
/// so the bar reads as centred with a gap to its neighbours.
fn column_mask(col: u16) -> u8 {
    let mut mask = 0xFFu8;
    if col == 0 {
        mask &= !BRAILLE_LEFT_COLUMN;
    }
    if col == CONTIG_CELL_WIDTH - 1 {
        mask &= !BRAILLE_RIGHT_COLUMN;
    }
    mask
}

/// The octant counterpart of [`column_mask`] — the same centred look in the other bit order.
fn octant_column_mask(col: u16) -> u8 {
    let mut mask = 0xFFu8;
    if col == 0 {
        mask &= !octant::LEFT_COLUMN;
    }
    if col == CONTIG_CELL_WIDTH - 1 {
        mask &= !octant::RIGHT_COLUMN;
    }
    mask
}

/// Draws one contig's relative-size bar in whichever glyph set the caller asked for.
pub(super) fn draw_bar(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    rows_y: &[u16],
    fill_level: u8,
    bg: Color,
    fg: Color,
    glyphs: GlyphSet,
) {
    let per_row_fill = split_fill_level(fill_level, rows_y.len());
    for (&y, &filled) in rows_y.iter().zip(per_row_fill.iter()) {
        for col in 0..CONTIG_CELL_WIDTH {
            let column_mask = match glyphs {
                GlyphSet::Braille => column_mask(col),
                GlyphSet::Octant => octant_column_mask(col),
            };
            let ch = glyphs.glyph(glyphs.fill_mask(filled) & column_mask);
            if let Some(cell) = buf.cell_mut((x + col, y)) {
                painter::paint_cell(cell, &ch.to_string(), fg, bg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_level_is_monotonic_and_bounded_for_valid_lengths() {
        let lengths = [0, 1, 3, 5, 8].map(BasePair);
        let levels: Vec<_> = lengths
            .into_iter()
            .map(|length| dot_fill_level(length, BasePair(8), 8))
            .collect();
        assert!(levels.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(levels.first(), Some(&1));
        assert_eq!(levels.last(), Some(&8));
        assert_eq!(dot_fill_level(BasePair(0), BasePair(0), 8), 1);
    }

    #[test]
    fn split_fill_level_two_rows() {
        assert_eq!(split_fill_level(8, 2), vec![4, 4]);
        assert_eq!(split_fill_level(5, 2), vec![4, 1]);
        assert_eq!(split_fill_level(0, 2), vec![0, 0]);
    }

    #[test]
    fn split_fill_level_one_row() {
        assert_eq!(split_fill_level(3, 1), vec![3]);
        assert_eq!(split_fill_level(4, 1), vec![4]);
    }
}
