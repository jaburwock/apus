//! Shared framing for centred lists and corner-anchored overlays.
//!
//! A popover draws over a viewer and reserves none of its width (invariant 9).

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Clear, Paragraph},
};

use crate::list_cursor::ListCursor;
use crate::style::{Selection, Surface, Text};

/// Centred lists need room for their rows and bottom-border hint.
const MIN_WIDTH: u16 = 24;
const MIN_HEIGHT: u16 = 6;

pub enum ListTitle<'a> {
    Plain(&'a str),
    WithRange(&'a str),
}

/// The occupied list rows in the last draw, excluding borders and trailing empty space.
#[derive(Clone, Copy)]
pub(crate) struct VisibleRows {
    outer: Rect,
    area: Rect,
    first: usize,
}

impl VisibleRows {
    pub(crate) fn bounds(self) -> Rect {
        self.outer
    }

    pub(crate) fn index_at(self, cell: Position) -> Option<usize> {
        self.area
            .contains(cell)
            .then(|| self.first + usize::from(cell.y - self.area.y))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "The genome legend uses only the bottom-right corner")
)]
pub enum PopoverAnchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A popover placed in an area, before it is drawn.
///
/// Placing comes first so a caller can size its content to [`Popover::inner`] before choosing a
/// title or formatting rows.
pub struct Popover {
    outer: Rect,
    /// The area inside the border.
    pub inner: Rect,
}

impl Popover {
    /// Uses the caller's placement and size, which can differ from the centred list minimums.
    pub fn from_rect(outer: Rect) -> Self {
        Self {
            outer,
            inner: Block::bordered().inner(outer),
        }
    }

    pub fn bounds(&self) -> Rect {
        self.outer
    }

    /// Centres a popover at `width_pct` by `height_pct` of `area`, or `None` below the list minimum.
    pub fn centred(area: Rect, width_pct: u16, height_pct: u16) -> Option<Self> {
        let [outer] = Layout::vertical([Constraint::Percentage(height_pct)])
            .flex(Flex::Center)
            .areas(area);
        let [outer] = Layout::horizontal([Constraint::Percentage(width_pct)])
            .flex(Flex::Center)
            .areas(outer);
        (outer.width >= MIN_WIDTH && outer.height >= MIN_HEIGHT).then(|| Self::from_rect(outer))
    }

    /// Places a framed popover flush against a corner if the content and its border fit.
    pub fn anchored(bounds: Rect, anchor: PopoverAnchor, content_width: u16, content_height: u16) -> Option<Self> {
        let width = content_width.checked_add(2)?;
        let height = content_height.checked_add(2)?;
        if bounds.width < width || bounds.height < height {
            return None;
        }

        let (x, y) = match anchor {
            PopoverAnchor::TopLeft => (bounds.x, bounds.y),
            PopoverAnchor::TopRight => (bounds.right() - width, bounds.y),
            PopoverAnchor::BottomLeft => (bounds.x, bounds.bottom() - height),
            PopoverAnchor::BottomRight => (bounds.right() - width, bounds.bottom() - height),
        };
        Some(Self::from_rect(Rect::new(x, y, width, height)))
    }

    /// Clears the popover and draws its border, with an optional top title and bottom hint.
    pub fn draw_frame(&self, frame: &mut Frame, title: Option<&str>, hint: &str) {
        frame.render_widget(Clear, self.outer);
        let mut block = Block::bordered().style(Style::default().fg(Text::PRIMARY_FG()).bg(Surface::POPOVER_BG()));
        if let Some(title) = title {
            block = block.title(format!(" {title} "));
        }
        if !hint.is_empty() {
            block = block.title_bottom(format!(" {hint} "));
        }
        frame.render_widget(block, self.outer);
    }

    pub fn draw_content(
        &self,
        frame: &mut Frame,
        title: Option<&str>,
        hint: &str,
        draw_inner: impl FnOnce(&mut Frame, Rect),
    ) {
        self.draw_frame(frame, title, hint);
        draw_inner(frame, self.inner);
    }

    /// Draws the list and returns its hit-test geometry.
    pub fn draw_list<'a>(
        &self,
        frame: &mut Frame,
        cursor: &mut ListCursor,
        total: usize,
        title: ListTitle<'_>,
        hint: &str,
        mut format_row: impl FnMut(usize, bool) -> Line<'a>,
    ) -> VisibleRows {
        let rows = cursor.window(usize::from(self.inner.height), total);
        match title {
            ListTitle::WithRange(title) if rows.len() < total => {
                let title = format!("{title} {}-{}/{total}", rows.start + 1, rows.end);
                self.draw_frame(frame, Some(&title), hint);
            }
            ListTitle::Plain(title) | ListTitle::WithRange(title) => self.draw_frame(frame, Some(title), hint),
        }

        let focus = Style::default().bg(Selection::FOCUS_BG());
        let lines: Vec<Line> = rows
            .clone()
            .map(|index| {
                let selected = index == cursor.selected();
                let line = format_row(index, selected);
                if selected { line.patch_style(focus) } else { line }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), self.inner);
        VisibleRows {
            outer: self.outer,
            area: Rect {
                height: rows.len() as u16,
                ..self.inner
            },
            first: rows.start,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn anchored_popovers_fit_their_border_at_each_corner() {
        let bounds = Rect::new(13, 17, 20, 11);
        for (anchor, x, y) in [
            (PopoverAnchor::TopLeft, 13, 17),
            (PopoverAnchor::TopRight, 26, 17),
            (PopoverAnchor::BottomLeft, 13, 23),
            (PopoverAnchor::BottomRight, 26, 23),
        ] {
            let popover = Popover::anchored(bounds, anchor, 5, 3).expect("popover fits");
            assert_eq!(popover.outer, Rect::new(x, y, 7, 5), "{anchor:?}");
            assert_eq!(popover.inner, Rect::new(x + 1, y + 1, 5, 3), "{anchor:?}");
        }

        for (width, height) in [(19, 9), (18, 10), (u16::MAX, 9), (18, u16::MAX)] {
            assert!(Popover::anchored(bounds, PopoverAnchor::TopRight, width, height).is_none());
        }
    }

    #[test]
    fn popover_hit_targets_match_the_rendered_rows_after_resize() {
        let _palette = crate::style::palette::lock_for_test();
        let labels = ["G", "Y", "F", "I", "S", "T", "X", "M", "P", "D", "Z", "U", "V", "C"];
        let mut cursor = ListCursor::at(10);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test terminal");

        for height in [10, 6] {
            terminal.backend_mut().resize(40, height);
            let mut visible = None;
            terminal
                .draw(|frame| {
                    let popover = Popover::centred(frame.area(), 100, 100).expect("popover fits");
                    visible = Some(popover.draw_list(
                        frame,
                        &mut cursor,
                        labels.len(),
                        ListTitle::Plain("Rows"),
                        "",
                        |index, _| Line::from(labels[index]),
                    ));
                })
                .expect("draw popover");

            let visible = visible.unwrap();
            let buffer = terminal.backend().buffer();
            for row in 1..height - 1 {
                let index = visible.index_at(Position::new(1, row)).unwrap();
                assert_eq!(buffer[(1, row)].symbol(), labels[index]);
                assert_eq!(visible.index_at(Position::new(0, row)), None);
            }
            assert_eq!(visible.index_at(Position::new(1, height - 1)), None);
        }
    }
}
