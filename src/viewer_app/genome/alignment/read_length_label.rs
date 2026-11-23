//! Draws the centreline's read-length overlay after pileup reduction.
//!
//! The pileup supplies one optional read span per terminal row when the centreline intersects a
//! visible read. Like the indel label, it draws on the finished terminal buffer because a
//! multi-cell string has no useful representation in the framebuffer's reduction.

use ratatui::{Frame, layout::Rect, style::Style};

use crate::basepair::BasePair;
use crate::style::{Surface, Text};
use crate::viewer_app::painter;

/// Draws formatted read lengths beside the centreline.
pub(super) fn draw_read_length_labels(frame: &mut Frame, area: Rect, centline_col: u16, lengths: &[Option<BasePair>]) {
    let buffer = frame.buffer_mut();
    let start_x = i32::from(centline_col) + 1;
    let style = Style::default().fg(Text::PRIMARY_FG()).bg(Surface::LABEL_BG());

    for (row_y, length) in (area.top()..area.bottom()).zip(lengths) {
        let Some(length) = length else { continue };
        let label = format!("<{}bp>", length.as_u64());
        painter::paint_text(buffer, area, start_x, row_y, &label, style);
    }
}
