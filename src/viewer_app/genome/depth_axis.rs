use std::fmt::Display;

use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::style::{Chrome, Surface};
use crate::viewer_app::painter::paint_text;

/// Anchors the ticks to the full chart height even when the pane is clipped.
pub(super) fn draw_depth_axis(buffer: &mut Buffer, area: Rect, chart_rows: u16, max: impl Display, mid: impl Display) {
    let style = Style::default().fg(Chrome::MARKER_FG()).bg(Surface::LABEL_BG());
    let width = usize::from(area.intersection(*buffer.area()).width);
    let x = i32::from(area.x);
    for y in area.top()..area.bottom() {
        let label = match y - area.top() {
            0 if area.height == 1 => Some(format!("╶{max}")),
            0 => Some(format!("┌{max}")),
            row if row == chart_rows / 2 => Some(format!("├{mid}")),
            row if row == chart_rows - 1 => Some("└0".to_string()),
            _ => None,
        };
        let text = label
            .as_deref()
            .filter(|label| label.chars().count() <= width)
            .unwrap_or("│");
        paint_text(buffer, area, x, y, text, style);
    }
}
