//! Resolves the shared coordinate centline to a pileup framebuffer position.
//!
//! The pixel plane's horizontal resolution is exactly `bp_per_column`, so a
//! content-relative pixel index and a bp offset from `left` are the same
//! number. That equivalence is what lets the painter mix the two freely.

use ratatui::layout::Rect;

use crate::viewer_app::painter::RIGHT_HALF_BLOCK;
use crate::viewer_app::viewport::ViewPosition;

use super::super::coordinate::marks::centline_mark;

/// Returns the content-relative framebuffer pixel occupied by the centline.
///
/// The half-cell comes from `centline_mark` rather than being re-derived here:
/// two independent parity rules is exactly how the mark and the pileup drifted
/// apart before (history 2026-08-13).
pub(super) fn centline_px(area: Rect, origin_x: u16, bp_per_column: u64, position: ViewPosition) -> Option<usize> {
    let (mark_x, glyph) = centline_mark(area, bp_per_column, position)?;
    let mark_col_content = mark_x.checked_sub(origin_x)?;
    let px = usize::from(mark_col_content) * bp_per_column as usize;
    Some(if glyph == RIGHT_HALF_BLOCK { px + 1 } else { px })
}
