//! Thin Ratatui [`Backend`] adapter over [`CellGrid`].
//!
//! This is the sole module in `apus-desktop` that translates `ratatui::style::Color` and
//! `Modifier`. Because Apus authors only `Color::Indexed(u8)` and paints the frame ground with
//! `Surface::APP_BG()`, every cell is reduced at ingest to plain `u8` ANSI-256 foreground and
//! background indices so the entire GPU renderer below stays `ratatui`-free.

use std::io;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell as RatatuiCell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

use crate::color::{FALLBACK_BG, FALLBACK_FG};
use crate::grid::{Cell, CellFlags, CellGrid};

/// Ratatui backend backed by an in-memory ANSI-256 [`CellGrid`].
pub struct GridBackend {
    grid: CellGrid,
    pixel_size: Size,
}

impl GridBackend {
    pub fn new(cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> Self {
        Self {
            grid: CellGrid::new(cols, rows),
            pixel_size: Size::new(pixel_width, pixel_height),
        }
    }

    pub fn grid(&self) -> &CellGrid {
        &self.grid
    }

    pub fn resize(&mut self, cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) {
        self.grid.resize(cols, rows);
        self.pixel_size = Size::new(pixel_width, pixel_height);
    }
}

fn color_index(color: Color, fallback: u8) -> u8 {
    match color {
        Color::Indexed(idx) => idx,
        _ => fallback,
    }
}

fn convert_cell(cell: &RatatuiCell) -> Cell {
    let mut fg = color_index(cell.fg, FALLBACK_FG);
    let mut bg = color_index(cell.bg, FALLBACK_BG);
    let modifier = cell.modifier;

    if modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut flags = CellFlags::NONE;
    if modifier.contains(Modifier::BOLD) {
        flags = flags.union(CellFlags::BOLD);
    }
    if modifier.contains(Modifier::DIM) {
        flags = flags.union(CellFlags::DIM);
    }
    if modifier.contains(Modifier::UNDERLINED) {
        flags = flags.union(CellFlags::UNDERLINED);
    }
    if modifier.contains(Modifier::CROSSED_OUT) {
        flags = flags.union(CellFlags::CROSSED_OUT);
    }

    let symbol = if modifier.contains(Modifier::HIDDEN) {
        ' '
    } else {
        cell.symbol().chars().next().unwrap_or(' ')
    };

    Cell { symbol, fg, bg, flags }
}

impl Backend for GridBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a RatatuiCell)>,
    {
        for (x, y, cell) in content {
            self.grid.set(x, y, convert_cell(cell));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.grid.set_cursor_visible(false);
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.grid.set_cursor_visible(true);
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (x, y) = self.grid.cursor();
        Ok(Position::new(x, y))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let pos = position.into();
        self.grid.set_cursor(pos.x, pos.y);
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.grid.clear();
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if clear_type == ClearType::All {
            self.grid.clear();
        }
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        Ok(Size::new(self.grid.cols(), self.grid.rows()))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: Size::new(self.grid.cols(), self.grid.rows()),
            pixels: self.pixel_size,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn backend_extracts_indexed_colours_and_applies_modifiers_at_ingest() {
        let mut backend = GridBackend::new(4, 2, 32, 32);
        let mut normal = RatatuiCell::new("A");
        normal.set_style(
            Style::default()
                .fg(Color::Indexed(196))
                .bg(Color::Indexed(235))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        );
        let mut reversed = RatatuiCell::new("B");
        reversed.set_style(
            Style::default()
                .fg(Color::Indexed(46))
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::REVERSED | Modifier::DIM),
        );
        let mut hidden = RatatuiCell::new("X");
        hidden.set_style(
            Style::default()
                .fg(Color::Indexed(21))
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::HIDDEN),
        );

        backend
            .draw([(0, 0, &normal), (1, 0, &reversed), (2, 0, &hidden)].into_iter())
            .unwrap();

        let cells = backend.grid().cells();
        assert_eq!(cells[0].symbol, 'A');
        assert_eq!(cells[0].fg, 196);
        assert_eq!(cells[0].bg, 235);
        assert!(cells[0].flags.contains(CellFlags::BOLD));
        assert!(cells[0].flags.contains(CellFlags::UNDERLINED));

        assert_eq!(cells[1].symbol, 'B');
        assert_eq!(cells[1].fg, 236);
        assert_eq!(cells[1].bg, 46);
        assert!(cells[1].flags.contains(CellFlags::DIM));

        assert_eq!(cells[2].symbol, ' ');
        assert_eq!(cells[2].bg, 237);
    }
}
