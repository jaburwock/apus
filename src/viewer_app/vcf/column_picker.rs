//! Modal column picker for the variant table.
//!
//! The picker's state is a [`ListCursor`] the viewer holds. `VCFViewer` owns the declared column
//! names and visibility flags, rebuilding the entries so the overlay cannot drift from the table.
//! Picker indices use declared-column order, including hidden and fixed columns; the table's
//! cursor and horizontal offset use the filtered visible list, with the mapping kept in the
//! viewer's `visible_column_indices`.
//!
//! While open, the picker consumes the table's keys. The last visible column cannot be hidden.
//! When the list overflows the popover, its title reports the 1-based visible range and total so
//! a real file's longer list is discoverable without spending an entry row on a scrollbar.

use ratatui::{Frame, layout::Rect, style::Style, text::Line};

use crate::command::protocol::{CommandOutcome, ListMove, TableCommand};
use crate::keys::{Keymap, Scope};
use crate::list_cursor::ListCursor;
use crate::popover::{ListTitle, Popover};
use crate::style::Text;

/// Draws the picker over `area`, or nothing if the area is too small.
///
/// `entries` is one `(name, checked)` per declared column, in table order.
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    cursor: &mut ListCursor,
    entries: &[(String, bool)],
    keymap: &Keymap,
) -> Option<Rect> {
    // Narrow because the content is a checkbox and a column name, not a table.
    let popover = Popover::centred(area, 50, 70)?;
    let checked = entries.iter().filter(|(_, checked)| *checked).count();
    let title = format!("Columns [{checked}/{}]", entries.len());
    let hint = keymap.hints(
        Scope::ColumnPicker,
        [
            CommandOutcome::Table(TableCommand::ToggleColumn),
            CommandOutcome::Table(TableCommand::ShowAllColumns),
            CommandOutcome::Table(TableCommand::ShowOnlyColumn),
            CommandOutcome::List(ListMove::Close),
        ],
        " · ",
    );
    popover.draw_list(
        frame,
        cursor,
        entries.len(),
        ListTitle::WithRange(&title),
        &hint,
        |index, selected| {
            let (name, shown) = &entries[index];
            let mark = if *shown { 'x' } else { ' ' };
            let pointer = if selected { '>' } else { ' ' };
            let fg = if *shown { Text::PRIMARY_FG() } else { Text::DIMMED_FG() };
            Line::styled(format!("{pointer} [{mark}] {name}"), Style::default().fg(fg))
        },
    );
    Some(popover.bounds())
}
