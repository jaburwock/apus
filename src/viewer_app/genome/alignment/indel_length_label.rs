//! Draws the centreline's indel-size overlay after pileup reduction.
//!
//! The pileup returns one optional [`IndelLabel`] per terminal row only when the centreline lands
//! on an insertion flank, deletion, or reference skip. This module owns the small, opaque label
//! that sits in the next terminal column: insertions and deletions use their operation's palette
//! so the label reads as a continuation of the adjacent `I`/`D` signal, while skips use the
//! neutral label surface over the plain track background.

use ratatui::{Frame, layout::Rect, style::Style};

use crate::style::{Indel, Surface, Text};
use crate::viewer_app::painter;

use super::pileup::IndelLabel;

/// Draws formatted indel and reference-skip sizes beside the centreline.
pub(super) fn draw_indel_length_labels(
    frame: &mut Frame,
    area: Rect,
    centline_col: u16,
    indels: &[Option<IndelLabel>],
) {
    let buffer = frame.buffer_mut();
    let start_x = i32::from(centline_col) + 1;

    for (row_y, indel) in (area.top()..area.bottom()).zip(indels) {
        let Some(indel) = indel else { continue };
        let (size, fg, bg) = match *indel {
            IndelLabel::Insertion(size) => (size, Text::ON_DATA_FG(), Indel::INSERTION()),
            IndelLabel::Deletion(size) => (size, Text::ON_DATA_FG(), Indel::DELETION()),
            IndelLabel::Skip(size) => (size, Text::PRIMARY_FG(), Surface::LABEL_BG()),
        };
        let label = format!("_{}bp", size.format_with_commas());
        painter::paint_text(buffer, area, start_x, row_y, &label, Style::default().fg(fg).bg(bg));
    }
}
