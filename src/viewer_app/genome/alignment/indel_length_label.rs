//! Draws the centreline's indel-size overlay after pileup reduction.
//!
//! The pileup returns one optional [`IndelLabel`] per terminal row only when the centreline lands
//! on an insertion flank or deletion. This module owns the small, opaque label that sits in the
//! next terminal column and uses the operation's palette so it reads as a continuation of the
//! adjacent `I`/`D` signal without duplicating that glyph.

use ratatui::{Frame, layout::Rect, style::Style};

use crate::style::{Indel, Text};
use crate::viewer_app::painter;

use super::pileup::IndelLabel;

/// Draws formatted indel sizes beside the centreline.
pub(super) fn draw_indel_length_labels(
    frame: &mut Frame,
    area: Rect,
    centline_col: u16,
    indels: &[Option<IndelLabel>],
) {
    let buffer = frame.buffer_mut();
    let start_x = i32::from(centline_col) + 1;
    let fg = Text::ON_DATA_FG();

    for (row_y, indel) in (area.top()..area.bottom()).zip(indels) {
        let Some(indel) = indel else { continue };
        let (size, bg) = match *indel {
            IndelLabel::Insertion(size) => (size, Indel::INSERTION()),
            IndelLabel::Deletion(size) => (size, Indel::DELETION()),
        };
        let label = format!("_{}bp", size.format_with_commas());
        painter::paint_text(buffer, area, start_x, row_y, &label, Style::default().fg(fg).bg(bg));
    }
}
