//! Cursor and window geometry for fixed-height rows.

use std::ops::Range;

/// Moves the top row by the least that makes `selected` visible in `rows` rows.
pub(crate) fn follow_row(offset: usize, selected: usize, rows: usize) -> usize {
    if rows == 0 {
        return offset;
    }
    offset.clamp(selected.saturating_sub(rows - 1), selected)
}

/// The cursor of a list drawn in a window shorter than itself.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ListCursor {
    selected: usize,
    /// First row drawn. It follows `selected`, never the reverse, so every row stays reachable.
    offset: usize,
}

impl ListCursor {
    pub const fn at(selected: usize) -> Self {
        Self { selected, offset: 0 }
    }

    pub fn selected(self) -> usize {
        self.selected
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index;
    }

    /// Moves up, clamping at the top.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves down, clamping at the last of `total` rows.
    pub fn down(&mut self, total: usize) {
        self.selected = (self.selected + 1).min(total.saturating_sub(1));
    }

    /// The rows of a `total`-row list to draw in a window of `rows`. Scrolls when the selection
    /// enters the outer quarters, keeping a full window even at either end of the list.
    pub fn window(&mut self, rows: usize, total: usize) -> Range<usize> {
        self.selected = self.selected.min(total.saturating_sub(1));
        if rows == 0 {
            self.offset = self.offset.min(self.selected);
            return 0..0;
        }
        if rows >= total {
            self.offset = 0;
            return 0..total;
        }
        let margin = rows / 4;
        let first = self.selected.saturating_sub(rows - 1 - margin);
        let last = self.selected.saturating_sub(margin);
        self.offset = self.offset.clamp(first, last).min(total - rows);
        self.offset..self.offset + rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_reachable_and_the_cursor_clamps_as_the_list_shrinks() {
        for total in 0_usize..12 {
            for rows in 0..8 {
                let mut cursor = ListCursor::default();
                let check_window = |cursor: &mut ListCursor, total: usize, expected: usize| {
                    let window = cursor.window(rows, total);
                    assert_eq!(cursor.selected(), expected, "total {total}, rows {rows}");
                    assert_eq!(window.len(), rows.min(total));
                    assert!(
                        window.start <= window.end && window.end <= total,
                        "window {window:?} outside {total} rows"
                    );
                    if rows > 0 && total > 0 {
                        assert!(
                            window.contains(&expected),
                            "row {expected} outside window {window:?} (total {total}, rows {rows})"
                        );
                    }
                };
                let check_move = |cursor: &mut ListCursor, expected: usize| {
                    assert_eq!(cursor.selected(), expected, "total {total}, rows {rows}");
                    check_window(cursor, total, expected);
                };

                let last = total.saturating_sub(1);
                cursor.up();
                check_move(&mut cursor, 0);
                for step in 1..total + 3 {
                    cursor.down(total);
                    check_move(&mut cursor, step.min(last));
                }
                for step in 1..total + 3 {
                    cursor.up();
                    check_move(&mut cursor, last.saturating_sub(step));
                }

                let mut cursor = ListCursor::at(total + 8);
                cursor.window(rows, total + 9);
                for remaining in (0..=total).rev() {
                    check_window(&mut cursor, remaining, remaining.saturating_sub(1));
                }
            }
        }
    }

    #[test]
    fn entering_either_outer_quarter_scrolls_before_the_edge() {
        let mut cursor = ListCursor::default();
        for _ in 0..5 {
            cursor.down(20);
        }
        assert_eq!(cursor.window(8, 20), 0..8);
        cursor.down(20);
        assert_eq!(cursor.window(8, 20), 1..9);

        for _ in 0..3 {
            cursor.up();
        }
        assert_eq!(cursor.window(8, 20), 1..9);
        cursor.up();
        assert_eq!(cursor.window(8, 20), 0..8);
    }

    #[test]
    fn growing_the_window_restores_all_rows_without_moving_the_selection() {
        let mut cursor = ListCursor::at(3);
        assert_eq!(cursor.window(2, 5), 2..4);
        assert_eq!(cursor.window(5, 5), 0..5);
        assert_eq!(cursor.selected(), 3);
    }
}
