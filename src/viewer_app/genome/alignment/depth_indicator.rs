//! Draws lane numbers over the pileup without reserving a pane.
//!
//! The top row shows its logical lane number. Rows below it are labelled at a fixed cadence.
//!
//! The indicator is drawn after reduction directly over the alignment `Rect`: its label has an
//! opaque backing because it must remain legible over reads. Drawing over the pileup preserves
//! genomic-column alignment with the other tracks (invariant 9).

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::style::{Chrome, Surface};
use crate::viewer_app::painter;

const LANE_LABEL_STEP: usize = 10;

/// Marks the row the number belongs to, since the label draws over the reads with no pane of
/// its own. Box-drawing rather than a hyphen: it fills the cell, so it reads as a rule pointing
/// at the lane instead of as punctuation in the read beneath it.
const LANE_RULE: char = '─';

/// The label for a zero-based lane index.
///
/// The `+ 1` is a display convention, so the top of an unscrolled pileup reads as lane 1. Neither
/// the word "lane" nor a unit is spelled out: an earlier `<n>X` borrowed coverage's notation for
/// something that is not a depth — a lane is a stacking row, and two reads in one lane are not
/// two-fold coverage — while spelling out `Lane <n>` cost seven columns of reads on every
/// labelled row. The rule plus the bare number is three columns at the widths that occur.
fn lane_label(lane: usize) -> String {
    format!("{LANE_RULE} {}", lane + 1)
}

/// Draws lane numbers over the pileup's left edge.
///
/// `lane_logical_indices` holds one entry per framebuffer lane (`area.height * lanes_per_row`);
/// synthetic expansion sub-lanes are `None`. Either half of a packed row can trigger a cadence
/// marker. The top row instead uses its first lane, or that lane's parent for an expansion.
pub(super) fn draw_depth_indicator(
    frame: &mut Frame,
    area: Rect,
    top_logical_lane: Option<usize>,
    lane_logical_indices: &[Option<usize>],
    lanes_per_row: usize,
) {
    let buffer = frame.buffer_mut();
    let style = Style::default()
        .fg(Chrome::MARKER_FG())
        .bg(Surface::LABEL_BG())
        .add_modifier(Modifier::BOLD);
    for (row_y, row_lanes) in (area.top()..area.bottom()).zip(lane_logical_indices.chunks_exact(lanes_per_row)) {
        let lane = if row_y == area.top() {
            top_logical_lane
        } else {
            row_lanes
                .iter()
                .flatten()
                .copied()
                .find(|lane| lane.is_multiple_of(LANE_LABEL_STEP))
        };
        if let Some(lane) = lane {
            painter::paint_text(buffer, area, i32::from(area.x), row_y, &lane_label(lane), style);
        }
    }
}
