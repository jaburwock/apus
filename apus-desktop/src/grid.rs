//! Ratatui-free terminal cell grid using plain `u8` ANSI-256 colour indices.

use crate::color::{FALLBACK_BG, FALLBACK_FG};

/// Style flags carried per cell into the GPU renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellFlags(u8);

impl CellFlags {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const UNDERLINED: Self = Self(1 << 2);
    pub const CROSSED_OUT: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// One cell in the rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub symbol: char,
    pub fg: u8,
    pub bg: u8,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            fg: FALLBACK_FG,
            bg: FALLBACK_BG,
            flags: CellFlags::NONE,
        }
    }
}

/// Row-major grid of cells with dimensions and tracked cursor state.
#[derive(Debug, Clone)]
pub struct CellGrid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor: (u16, u16),
    cursor_visible: bool,
}

impl CellGrid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); usize::from(cols) * usize::from(rows)],
            cursor: (0, 0),
            cursor_visible: false,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if self.cols == cols && self.rows == rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.cells.clear();
        self.cells
            .resize(usize::from(cols) * usize::from(rows), Cell::default());
        self.cursor = (
            self.cursor.0.min(cols.saturating_sub(1)),
            self.cursor.1.min(rows.saturating_sub(1)),
        );
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }

    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x < self.cols && y < self.rows {
            let idx = usize::from(y) * usize::from(self.cols) + usize::from(x);
            self.cells[idx] = cell;
        }
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.cursor
    }

    pub fn set_cursor(&mut self, x: u16, y: u16) {
        self.cursor = (x, y);
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    /// Returns the most frequent background index in the grid (typically `Surface::APP_BG()`),
    /// used to clear fractional pixels outside the integer cell grid.
    pub fn dominant_bg(&self) -> u8 {
        if self.cells.is_empty() {
            return FALLBACK_BG;
        }
        let mut counts = [0u32; 256];
        for cell in &self.cells {
            counts[usize::from(cell.bg)] += 1;
        }
        counts
            .iter()
            .enumerate()
            .max_by_key(|&(_, count)| count)
            .map_or(FALLBACK_BG, |(idx, _)| idx as u8)
    }
}
