//! The palette edit pane: a 2D hierarchical swatch sheet grouping related entries onto shared rows,
//! and a live colour picker that retints one entry from the ramp vocabulary or the full indexed
//! space while previewing its row's sibling swatches.
//!
//! App-global state, owned by `App` rather than copied into viewers (invariant 5) — a viewer never
//! reads it, and an edit reaches viewers through the palette itself rather than by the pane
//! touching one. It takes horizontal space from the viewer area rather than insetting a track
//! (invariant 9), and is keyboard-driven throughout (invariant 4).
//!
//! The pane reports its input scope and applies palette commands. App owns the bindings.

mod layout;

use std::ops::Range;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::palette::{self, REGISTRY, builtin::BUILT_INS, ramp};
use crate::{
    command::protocol::{CommandOutcome, Direction, ListMove, PaneCommand, ShellCommand},
    keys::{Keymap, Scope},
    list_cursor::ListCursor,
    style::{Chrome, Selection, Surface, Text},
};
use layout::{CLUSTER_GAP, SECTIONS, STRIP_INDENT, SWATCH_WIDTH, Swatch, SwatchRow};

/// Columns the pane occupies when open. Wide enough for the 12-swatch `Bases` row (6 adjacent
/// plain+tinted pairs with gaps and indent = 31 cols) and the 24-step grey ramp picker; viewers
/// keep the rest.
pub const PANE_WIDTH: u16 = 34;

/// Unselected swatch body on the sheet and picker sibling strip (`SWATCH_WIDTH` cells).
const SWATCH_BLANK: &str = "  ";

/// Selected swatch marker on the sheet and picker sibling strip (`SWATCH_WIDTH` cells).
///
/// Framing brackets sit at the cell edges so the swatch's background stays visible through the
/// centre while clearly marking which member of an adjacent cluster is focused.
const SWATCH_SELECTED: &str = "[]";

const _: () = assert!(SWATCH_BLANK.len() == SWATCH_WIDTH && SWATCH_SELECTED.len() == SWATCH_WIDTH);

/// Pinned header lines above the scrollable swatch sheet: hints, canonical name, description,
/// and a divider rule.
const SHEET_HEADER_ROWS: usize = 4;

/// Header lines drawn above the picker's colour grid: entry name, sibling swatch strip, and hints.
const PICKER_HEADER_ROWS: usize = 3;

/// What the pane is showing.
///
/// [`Mode::Menu`] is where `,` lands, because the two things behind it — retinting entries and
/// swapping the whole palette — are different enough operations that opening straight into either
/// would make the other hard to find.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum Mode {
    #[default]
    Menu,
    /// The built-in palettes, selected whole.
    ///
    /// Not applied live as the cursor moves, unlike the picker: selecting restores the defaults
    /// first and so discards hand edits, and nothing would restore them. The picker can preview
    /// because Esc puts back one colour.
    Select(ListCursor),
    /// The 2D hierarchical swatch sheet.
    List,
    /// Choosing a colour for the selected entry, applied live as the selection moves.
    Picker(Picker),
}

/// The menu's rows, in order. Each names the mode it opens; the pane needs no separate table of
/// labels, so a row cannot exist without a destination.
const MENU: &[(&str, Mode)] = &[
    ("Edit palette", Mode::List),
    ("Select palette", Mode::Select(ListCursor::at(0))),
];

/// Cells per row in the full-space grid.
///
/// Eight keeps the widest bucket to five rows, so a whole hue family is visible at once, and eight
/// three-column swatches fit `PANE_WIDTH` with room to spare.
const FULL_GRID_WIDTH: usize = 8;

/// Hue families the grid groups by. The achromatic bucket follows them, so there are eight in all.
///
/// Seven, matching the ramp hues the rest of the palette is authored from, so a colour found under
/// "orange" here is one an `orange(n)` entry could plausibly have been.
const HUE_BUCKETS: usize = 7;

/// The upper bound of each hue family, in degrees.
///
/// **Deliberately unequal**, because the named hues are not evenly spaced on the wheel: yellow and
/// orange sit barely 25° apart while green spans well over a hundred. Slicing the wheel into seven
/// equal arcs instead — which this first did — collects true oranges, the whole of yellow, and the
/// yellow-greens into one bucket, at which point the grouping stops meaning anything.
///
/// Red is the wrap-around case rather than an entry here: it owns both ends of the wheel, so an
/// angle past purple's bound has come back round to it.
const HUE_BOUNDS: [f64; HUE_BUCKETS] = [20.0, 45.0, 70.0, 160.0, 200.0, 260.0, 330.0];

/// Below this saturation a colour is grouped as grey rather than by its hue.
///
/// A hue angle is meaningless once there is almost no chroma to take it from: `(175, 175, 135)` has
/// a nominal hue of 60° but reads as a pale khaki, and sorting it among the yellows put visible greys
/// in the middle of a hue family. HSV saturation rather than L\*, since the question is "does this
/// have a colour", not "is it light".
const MIN_HUE_SATURATION: f64 = 0.25;

/// Every colour the palette can hold, in the order the full-space grid draws them, `None` for a pad.
///
/// **Grouped by hue, and sorted dark to light within each group.** The two axes a person actually
/// searches on are "roughly what colour" and "roughly how bright", so the layout answers them in
/// that order: find the family, then scan down it for the weight.
///
/// Each bucket starts on a fresh row, its last row padded out. That is what makes the grouping
/// legible — a bucket that flowed on from the previous one would put two families in one row and
/// leave the eye no boundary to catch. The padding is the cost, and it is small.
///
/// This deliberately makes **no cross-column claim**: cell 3 of the reds is not the same brightness
/// as cell 3 of the blues. It cannot be — some hues have no dark members and others no bright ones,
/// so a grid promising aligned rows is either lossy or mostly empty. Only the *within-bucket* order
/// is meaningful.
///
/// Greys come next-to-last, as one bucket: they are achromatic and so belong to no hue, and they are
/// what most surface and text entries are drawn from, which makes the end of the list a useful place
/// for them rather than a leftover.
///
/// The terminal's own 0–15 come last, in index order — see [`terminal_scheme_cells`].
fn full_grid_cells() -> Vec<Option<u8>> {
    let mut buckets: Vec<Vec<u8>> = vec![Vec::new(); HUE_BUCKETS + 1];
    for index in palette::FIRST_ADDRESSABLE_INDEX..=u8::MAX {
        buckets[hue_family(index)].push(index);
    }
    for bucket in &mut buckets {
        bucket.sort_by(|a, b| ramp::lstar(*a).total_cmp(&ramp::lstar(*b)));
    }
    buckets.push(terminal_scheme_cells());

    let mut cells = Vec::new();
    for bucket in &buckets {
        cells.extend(bucket.iter().copied().map(Some));
        // Pad to a row boundary so the next bucket starts fresh.
        let overhang = cells.len() % FULL_GRID_WIDTH;
        if overhang != 0 {
            cells.extend(std::iter::repeat_n(None, FULL_GRID_WIDTH - overhang));
        }
    }
    cells
}

/// The terminal's own scheme, 0–15, in index order.
///
/// **Sorted by index rather than by lightness like every other bucket**, because their colours are
/// not knowable here: the terminal decides what index 4 renders as, and `ramp::srgb` reports the
/// cube's origin for all sixteen. Sorting by a lightness that is really a placeholder would impose a
/// confident-looking order carrying no information. Index order is at least their real structure —
/// 0–7 the base colours, 8–15 their bright variants, so the two rows line up as a pair.
///
/// They are last because they are the exception the rest of the grid is defined against, and picking
/// one is opting out of everything the palette can promise: no contrast can be checked against a
/// value the terminal supplies, and the same entry looks different in another terminal. That is a
/// legitimate thing to want — it is the one way to reach a colour outside the 256-colour cube, and a
/// user who has tuned their terminal scheme can pull it in deliberately — but it is not the default
/// path through this grid.
fn terminal_scheme_cells() -> Vec<u8> {
    (0..palette::FIRST_ADDRESSABLE_INDEX).collect()
}

/// Which family an index is grouped under: a hue, or the trailing achromatic bucket.
///
/// Too little chroma to have a usable hue means grey, whatever the angle says — see
/// [`MIN_HUE_SATURATION`]. Otherwise the first bound the angle falls under wins; anything past the
/// last bound has wrapped around to red, which owns both ends of the wheel.
fn hue_family(index: u8) -> usize {
    let Some(degrees) = ramp::hue_degrees(index) else {
        return HUE_BUCKETS;
    };
    if ramp::saturation(index) < MIN_HUE_SATURATION {
        return HUE_BUCKETS;
    }
    HUE_BOUNDS.iter().position(|bound| degrees < *bound).unwrap_or(0)
}

/// Where `color` sits in the full-space grid, falling back to the origin.
///
/// Total in practice — the grid holds every index a palette entry can hold — so the fallback is
/// unreachable rather than a policy; it exists because `Color` admits spellings the palette does not.
fn full_grid_cell_of(color: Color) -> usize {
    full_grid_cells()
        .iter()
        .position(|cell| cell.map(Color::Indexed) == Some(color))
        .unwrap_or(0)
}

/// High-contrast foreground for a cursor marker drawn directly over a swatch background.
///
/// Picks dark or light grey from perceptual lightness so the marker stays legible over both
/// near-black and near-white swatches in light or dark themes. Terminal-owned indices 0–15 have
/// unknown RGB, so those fall back to `Text::PRIMARY_FG()`.
fn swatch_fg(color: Color) -> Color {
    match color {
        Color::Indexed(index) if index >= palette::FIRST_ADDRESSABLE_INDEX => {
            if ramp::lstar(index) > 55.0 {
                ramp::grey(1)
            } else {
                ramp::grey(22)
            }
        }
        _ => Text::PRIMARY_FG(),
    }
}

/// Formats a colour as its ramp spelling when on a ramp (e.g. `green(3)`), or its raw ANSI index.
fn format_color(color: Color) -> String {
    if let Some((axis, step)) = ramp::locate_color(color) {
        return format!("{}({step})", ramp::axes()[axis].name);
    }
    match color {
        Color::Indexed(index) => format!("index {index}"),
        _ => String::new(),
    }
}

/// 2D cursor and vertical scroll position over [`SECTIONS`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SheetCursor {
    row: usize,
    /// Desired column within a row; clamped on read so vertical moves across shorter rows
    /// preserve horizontal position until the user explicitly moves left or right.
    col: usize,
    /// First body line drawn when the sheet exceeds the available pane height.
    scroll: usize,
}

impl SheetCursor {
    fn row_spec(self) -> &'static SwatchRow {
        layout::row_at(self.row)
    }

    fn effective_col(self) -> usize {
        self.col.min(self.row_spec().len().saturating_sub(1))
    }

    fn swatch(self) -> Swatch {
        self.row_spec().swatch(self.effective_col())
    }

    fn entry(self) -> usize {
        self.swatch().entry
    }

    fn move_dir(&mut self, direction: Direction) {
        match direction {
            Direction::Up => self.row = self.row.saturating_sub(1),
            Direction::Down => self.row = (self.row + 1).min(layout::row_count().saturating_sub(1)),
            Direction::Left => self.col = self.effective_col().saturating_sub(1),
            Direction::Right => {
                let last = self.row_spec().len().saturating_sub(1);
                self.col = (self.effective_col() + 1).min(last);
            }
        }
    }

    /// Adjusts `scroll` minimally so the active row's lines (and its section heading when on a
    /// section's first row) fit within `body_rows`.
    fn window(&mut self, body_rows: usize) -> Range<usize> {
        let total = layout::total_body_lines();
        if body_rows == 0 {
            return 0..0;
        }
        if body_rows >= total {
            self.scroll = 0;
            return 0..total;
        }
        let (reveal_top, row_bottom) = layout::sheet_line_span(self.row);
        let max_offset = total - body_rows;
        let min_scroll = row_bottom.saturating_sub(body_rows - 1);
        let max_scroll = reveal_top.min(max_offset);
        if min_scroll > max_scroll {
            // Pane is shorter than the row + heading span: keep the swatch strip visible.
            self.scroll = min_scroll.min(max_offset);
        } else {
            self.scroll = self.scroll.clamp(min_scroll, max_scroll);
        }
        self.scroll..self.scroll + body_rows
    }
}

/// Which colour space the picker is choosing from.
///
/// Two reaches rather than one grid: the ramps are a curated vocabulary where a step carries meaning
/// (see `ramp.rs`), and the full space is every index the terminal has. Collapsing them would lose
/// the ramps' meaning; keeping the ramps as the only reach leaves any colour outside them unpickable,
/// which is the gap this closes.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Reach {
    /// The ramp grid: one row per hue, one column per step.
    Ramps { axis: usize, step: usize },
    /// Every index the palette can hold, as a position in [`full_grid_cells`].
    Full { cell: usize },
}

/// A position in one of the colour spaces, plus what to restore if the edit is abandoned.
///
/// Holding the *colour* rather than a position to revert to means Esc restores exactly what was
/// there, including a value neither reach can name.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Picker {
    reach: Reach,
    original: Color,
}

impl Picker {
    /// Opens on `original`'s own position, so a move starts from where the user already is.
    ///
    /// A colour on no ramp opens in [`Reach::Full`] on its own cell rather than jumping the cursor to
    /// an unrelated ramp position — which is what the palette's index storage makes reachable.
    fn opening_on(original: Color) -> Self {
        let reach = match ramp::locate_color(original) {
            Some((axis, step)) => Reach::Ramps { axis, step },
            None => Reach::Full {
                cell: full_grid_cell_of(original),
            },
        };
        Self { reach, original }
    }

    /// The colour at the current position.
    fn color(self) -> Color {
        match self.reach {
            Reach::Ramps { axis, step } => ramp::axes()[axis].at(step),
            Reach::Full { cell } => full_grid_cells()
                .get(cell)
                .copied()
                .flatten()
                .map_or(self.original, Color::Indexed),
        }
    }

    /// Swaps to the other colour space, landing on the current colour's position in it.
    ///
    /// Entering the full grid always finds the colour, since the grid holds every index. Leaving it
    /// usually will not: a colour picked from the full space is typically on no ramp, and there is no
    /// ramp position to preserve, so the cursor goes to the origin rather than to a guess.
    fn toggled_reach(self) -> Self {
        let reach = match self.reach {
            Reach::Ramps { .. } => Reach::Full {
                cell: full_grid_cell_of(self.color()),
            },
            Reach::Full { .. } => {
                let (axis, step) = ramp::locate_color(self.color()).unwrap_or((0, 0));
                Reach::Ramps { axis, step }
            }
        };
        Self { reach, ..self }
    }

    /// Moves the cursor by one row (`down`) or one column (`right`), clamped.
    ///
    /// One signature for both reaches because the keys mean the same thing in each: the vertical
    /// keys cross between colour groups and the horizontal ones run along the group the cursor is
    /// in. What a "group" is differs — a ramp there, a row of the grid here — which is exactly the
    /// difference this method absorbs so the key handler does not have to.
    fn moved(self, down: isize, right: isize) -> Self {
        let reach = match self.reach {
            // Steps clamp against the *new* ramp: grey has 24 positions to a hue's 5, so leaving
            // grey(20) for a hue must land on a real step.
            Reach::Ramps { axis, step } => {
                let axes = ramp::axes();
                let axis = shift(axis, down, axes.len());
                Reach::Ramps {
                    axis,
                    step: shift(step, right, axes[axis].len()),
                }
            }
            Reach::Full { cell } => {
                let cells = full_grid_cells();
                Reach::Full {
                    cell: moved_grid_cell(&cells, cell, down, right),
                }
            }
        };
        Self { reach, ..self }
    }
}

/// Moves a full-grid cursor by one row and/or one column, skipping pads and clamping at the edges.
///
/// A pad is a hole in the grid rather than a choice, so the cursor may never rest on one: a move
/// that lands in the padding continues in the same direction until it finds a colour, and if the
/// grid runs out first the move is refused rather than wrapping to the far side. Refusing keeps the
/// two axes independent — `j` off the bottom of a column must not jump to a different hue family.
fn moved_grid_cell(cells: &[Option<u8>], cell: usize, down: isize, right: isize) -> usize {
    let width = FULL_GRID_WIDTH as isize;
    // A row move is a whole row's worth of cells; a column move is one. Combining them this way
    // keeps a diagonal impossible, which no key produces anyway.
    let step = down * width + right;
    if step == 0 {
        return cell;
    }

    let mut candidate = cell as isize;
    loop {
        let next = candidate + step;
        // Off either end of the grid: keep the cursor where it was.
        if next < 0 || next as usize >= cells.len() {
            return cell;
        }
        // A horizontal move must stay on its own row, or `l` at a row's end would silently continue
        // onto the next one — a jump the eye cannot follow.
        if down == 0 && next / width != candidate / width {
            return cell;
        }
        candidate = next;
        if cells[candidate as usize].is_some() {
            return candidate as usize;
        }
    }
}

/// Adds a signed delta to an index, clamped to `0..len`.
fn shift(index: usize, delta: isize, len: usize) -> usize {
    index.saturating_add_signed(delta).min(len.saturating_sub(1))
}

/// A view's title.
fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(Text::PRIMARY_FG())))
}

/// The keys a view answers to, under its title.
fn hint(text: String) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(Text::DIMMED_FG())))
}

/// The outcome a pane key is bound to, for looking its chord up in the keymap.
fn palette(command: PaneCommand) -> CommandOutcome {
    ShellCommand::Palette(command).into()
}

fn key_group(keymap: &Keymap, scope: Scope, commands: impl IntoIterator<Item = PaneCommand>, label: &str) -> String {
    let keys = commands
        .into_iter()
        .filter_map(|command| keymap.primary_key(scope, &palette(command)))
        .collect::<Vec<_>>()
        .join("/");
    if keys.is_empty() || label.is_empty() {
        keys
    } else {
        format!("{keys} {label}")
    }
}

fn join_hints(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

/// One selectable row in the menu or built-in chooser.
fn chosen_row(label: &str, selected: bool) -> Line<'static> {
    let mut style = Style::default().fg(Text::PRIMARY_FG());
    if selected {
        style = style.bg(Selection::FOCUS_BG()).add_modifier(Modifier::BOLD);
    }
    Line::from(Span::styled(format!(" {label}"), style))
}

/// Renders one row's swatch strip, optionally marking the active swatch at `selected_col`.
///
/// Shared between the 2D sheet and the colour picker's sibling preview so the two cannot disagree
/// on swatch widths, cluster spacing, or live colour values.
fn swatch_strip_line(row: &SwatchRow, selected_col: Option<usize>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(STRIP_INDENT))];
    let mut col = 0;
    for (cluster_idx, cluster) in row.clusters.iter().enumerate() {
        if cluster_idx > 0 {
            spans.push(Span::raw(" ".repeat(CLUSTER_GAP)));
        }
        for swatch in *cluster {
            let color = palette::entry_color(swatch.entry);
            let is_selected = selected_col == Some(col);
            let text = if is_selected { SWATCH_SELECTED } else { SWATCH_BLANK };
            let mut style = Style::default().bg(color);
            if is_selected {
                style = style.fg(swatch_fg(color)).add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(text, style));
            col += 1;
        }
    }
    Line::from(spans)
}

/// The palette pane's state. Absent from the shell entirely when closed.
#[derive(Debug, Default)]
pub struct PalettePane {
    /// The focused swatch on the 2D sheet.
    sheet: SheetCursor,
    /// Cursor in [`MENU`]. Kept across a visit to a submenu, so Esc returns to the row that was
    /// opened rather than to the top.
    menu: ListCursor,
    mode: Mode,
}

/// How App should respond to a command the pane applied.
#[derive(Debug, PartialEq, Eq)]
pub enum PaneAction {
    /// Handled internally; nothing for the app to do.
    None,
    /// Close the pane.
    Close,
    /// Write the palette to the active palette file.
    ///
    /// Carries nothing: the palette is global state the writer reads for itself, and handing over a
    /// copy would let the two disagree about what "the current palette" is.
    Save,
    /// Select the named built-in palette.
    ///
    /// The shell routes the same outcome as `:set colorscheme <name>`.
    SelectPalette(&'static str),
}

impl PalettePane {
    pub(crate) fn scope(&self) -> Scope {
        match self.mode {
            Mode::Menu => Scope::PaletteMenu,
            Mode::Select(_) => Scope::PaletteSelect,
            Mode::List => Scope::PaletteEntries,
            Mode::Picker(_) => Scope::PalettePicker,
        }
    }

    pub(crate) fn apply(&mut self, command: PaneCommand) -> PaneAction {
        match &mut self.mode {
            Mode::Picker(picker) => {
                let picker = *picker;
                self.apply_picker(picker, command);
                PaneAction::None
            }
            Mode::Menu => self.apply_menu(command),
            Mode::Select(cursor) => {
                match command {
                    PaneCommand::List(ListMove::Close) => self.mode = Mode::Menu,
                    PaneCommand::Close => return PaneAction::Close,
                    PaneCommand::List(ListMove::Down) => cursor.down(BUILT_INS.len()),
                    PaneCommand::List(ListMove::Up) => cursor.up(),
                    // Keep the chooser open so several palettes can be compared.
                    PaneCommand::List(ListMove::Select) => {
                        return PaneAction::SelectPalette(BUILT_INS[cursor.selected()].name);
                    }
                    _ => {}
                }
                PaneAction::None
            }
            Mode::List => self.apply_entries(command),
        }
    }

    fn apply_menu(&mut self, command: PaneCommand) -> PaneAction {
        match command {
            PaneCommand::Close => return PaneAction::Close,
            PaneCommand::List(ListMove::Down) => self.menu.down(MENU.len()),
            PaneCommand::List(ListMove::Up) => self.menu.up(),
            PaneCommand::List(ListMove::Select) => self.mode = MENU[self.menu.selected()].1,
            _ => {}
        }
        PaneAction::None
    }

    fn apply_entries(&mut self, command: PaneCommand) -> PaneAction {
        match command {
            PaneCommand::Close => return PaneAction::Close,
            PaneCommand::List(ListMove::Close) => self.mode = Mode::Menu,
            PaneCommand::Move(direction) => self.sheet.move_dir(direction),
            PaneCommand::List(ListMove::Select) => {
                self.mode = Mode::Picker(Picker::opening_on(palette::entry_color(self.sheet.entry())));
            }
            PaneCommand::Save => return PaneAction::Save,
            _ => {}
        }
        PaneAction::None
    }

    fn apply_picker(&mut self, picker: Picker, command: PaneCommand) {
        let moved = match command {
            PaneCommand::List(ListMove::Close) => {
                palette::set_entry(self.sheet.entry(), picker.original);
                self.mode = Mode::List;
                return;
            }
            PaneCommand::List(ListMove::Select) => {
                self.mode = Mode::List;
                return;
            }
            PaneCommand::ToggleReach => {
                self.mode = Mode::Picker(picker.toggled_reach());
                return;
            }
            PaneCommand::Move(Direction::Up) => picker.moved(-1, 0),
            PaneCommand::Move(Direction::Down) => picker.moved(1, 0),
            PaneCommand::Move(Direction::Left) => picker.moved(0, -1),
            PaneCommand::Move(Direction::Right) => picker.moved(0, 1),
            _ => return,
        };
        palette::set_entry(self.sheet.entry(), moved.color());
        self.mode = Mode::Picker(moved);
    }

    /// Draws the pane into its own area.
    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect, keymap: &Keymap) {
        // The pane sits over whatever the layout gave it, so it clears rather than trusting the
        // area to be empty.
        frame.render_widget(Clear, area);
        frame
            .buffer_mut()
            .set_style(area, Style::default().bg(Surface::PANEL_BG()));

        let scope = self.scope();
        let lines = match &mut self.mode {
            Mode::Menu => Self::menu_lines(
                area.height,
                [heading("Palette"), Self::list_hint(keymap, scope, PaneCommand::Close)],
                &mut self.menu,
                MENU.len(),
                |index| MENU[index].0,
            ),
            // The app beside the pane is the whole-palette preview.
            Mode::Select(cursor) => Self::menu_lines(
                area.height,
                [
                    heading("Select palette"),
                    Self::list_hint(keymap, scope, PaneCommand::List(ListMove::Close)),
                ],
                cursor,
                BUILT_INS.len(),
                |index| BUILT_INS[index].name,
            ),
            Mode::List => self.sheet_lines(area, keymap),
            Mode::Picker(picker) => {
                let picker = *picker;
                self.picker_lines(picker, area, keymap)
            }
        };
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn menu_lines(
        height: u16,
        header: [Line<'static>; 2],
        cursor: &mut ListCursor,
        total: usize,
        label: impl Fn(usize) -> &'static str,
    ) -> Vec<Line<'static>> {
        let height = usize::from(height);
        // Leave room for the selected item, even when the title and hint cannot both fit.
        let mut lines: Vec<_> = header.into_iter().take(height.saturating_sub(1)).collect();
        lines.extend(
            cursor
                .window(height - lines.len(), total)
                .map(|index| chosen_row(label(index), index == cursor.selected())),
        );
        lines
    }

    fn list_hint(keymap: &Keymap, scope: Scope, close: PaneCommand) -> Line<'static> {
        let movement = key_group(
            keymap,
            scope,
            [PaneCommand::List(ListMove::Down), PaneCommand::List(ListMove::Up)],
            "move",
        );
        let actions = keymap.hints(
            scope,
            [palette(PaneCommand::List(ListMove::Select)), palette(close)],
            "  ",
        );
        hint(join_hints([movement, actions]))
    }

    /// Pinned header (hints + active entry readout), then the scrollable 2D swatch sheet.
    fn sheet_lines(&mut self, area: Rect, keymap: &Keymap) -> Vec<Line<'static>> {
        use Direction::{Down, Left, Right, Up};

        let height = usize::from(area.height);
        if height == 0 {
            return Vec::new();
        }

        let scope = self.scope();
        let move_keys = key_group(keymap, scope, [Left, Down, Up, Right].map(PaneCommand::Move), "");
        let actions = keymap.hints(
            scope,
            [
                palette(PaneCommand::List(ListMove::Select)),
                palette(PaneCommand::Save),
                palette(PaneCommand::List(ListMove::Close)),
            ],
            "  ",
        );
        let swatch = self.sheet.swatch();
        let (ns, name) = REGISTRY[swatch.entry];
        let colour = format_color(palette::entry_color(swatch.entry));

        let header = [
            hint(join_hints([move_keys, actions])),
            Line::from(Span::styled(
                format!("{ns}::{name}"),
                Style::default().fg(Text::PRIMARY_FG()).add_modifier(Modifier::BOLD),
            )),
            hint(format!("{} · {colour}", swatch.label)),
            Line::from(Span::styled(
                "─".repeat(usize::from(area.width).saturating_sub(1)),
                Style::default().fg(Chrome::DIVIDER_FG()),
            )),
        ];
        debug_assert_eq!(header.len(), SHEET_HEADER_ROWS);

        // Keep at least two rows for the active swatch row when the pane is very short; at height 1
        // drop the entire header so the swatch strip itself stays visible.
        let header_budget = height.saturating_sub(2).min(header.len());
        let mut lines: Vec<Line<'static>> = header.into_iter().take(header_budget).collect();
        let body_rows = height - lines.len();
        let window = self.sheet.window(body_rows);

        let body = self.render_sheet_body();
        lines.extend(body.into_iter().skip(window.start).take(window.len()));
        lines
    }

    /// Builds every line of the scrollable sheet body: section headings and 2-line swatch rows.
    fn render_sheet_body(&self) -> Vec<Line<'static>> {
        let mut body = Vec::with_capacity(layout::total_body_lines());
        let active_col = self.sheet.effective_col();
        let mut flat_row = 0;

        for section in SECTIONS {
            body.push(Line::from(Span::styled(
                section.title,
                Style::default().fg(Text::PRIMARY_FG()).add_modifier(Modifier::BOLD),
            )));
            for row in section.rows {
                let is_active = flat_row == self.sheet.row;
                let label_style = if is_active {
                    Style::default()
                        .fg(Text::PRIMARY_FG())
                        .bg(Selection::FOCUS_BG())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Text::DIMMED_FG())
                };
                body.push(Line::from(Span::styled(format!(" {}", row.label), label_style)));
                body.push(swatch_strip_line(row, is_active.then_some(active_col)));
                flat_row += 1;
            }
        }
        body
    }

    /// The picker: entry heading, live sibling swatch strip, key hints, and colour grid.
    ///
    /// The whole grid is drawn rather than just the current position, so a colour is chosen by
    /// seeing the neighbours it is being picked over.
    fn picker_lines(&self, picker: Picker, area: Rect, keymap: &Keymap) -> Vec<Line<'static>> {
        use Direction::{Down, Left, Right, Up};

        let (ns, name) = REGISTRY[self.sheet.entry()];
        let scope = self.scope();
        let moves = |directions: &[Direction], label| {
            key_group(keymap, scope, directions.iter().copied().map(PaneCommand::Move), label)
        };
        let toggle = |label| key_group(keymap, scope, [PaneCommand::ToggleReach], label);
        let keep = keymap.hints(scope, [palette(PaneCommand::List(ListMove::Select))], "  ");
        let mut lines = vec![
            heading(&format!("{ns}::{name}")),
            swatch_strip_line(self.sheet.row_spec(), Some(self.sheet.effective_col())),
        ];
        match picker.reach {
            Reach::Ramps { axis, step } => {
                lines.push(hint(join_hints([
                    moves(&[Down, Up], "hue"),
                    moves(&[Left, Right], "step"),
                    toggle("all"),
                    keep,
                ])));
                debug_assert_eq!(lines.len(), PICKER_HEADER_ROWS);
                lines.extend(Self::ramp_grid(axis, step));
            }
            Reach::Full { cell } => {
                lines.push(hint(join_hints([
                    moves(&[Left, Down, Up, Right], "move"),
                    toggle("ramps"),
                    keep,
                ])));
                debug_assert_eq!(lines.len(), PICKER_HEADER_ROWS);
                let rows = usize::from(area.height).saturating_sub(lines.len());
                lines.extend(Self::full_grid(cell, rows));
            }
        }
        lines
    }

    /// The ramp grid: one row per axis, each showing that axis' steps as swatches.
    fn ramp_grid(picker_axis: usize, picker_step: usize) -> Vec<Line<'static>> {
        ramp::axes()
            .into_iter()
            .enumerate()
            .map(|(index, axis)| {
                let mut spans = vec![Span::styled(
                    format!("{:>7} ", axis.name),
                    Style::default().fg(if index == picker_axis {
                        Text::PRIMARY_FG()
                    } else {
                        Text::DIMMED_FG()
                    }),
                )];
                spans.extend((0..axis.len()).map(|step| {
                    // One cell per step, unlike the sheet's two: grey's 24 steps and a label have
                    // to fit the same PANE_WIDTH, and a truncated ramp would hide real choices.
                    // The cursor is a glyph over the swatch rather than a background, so the cell
                    // keeps showing the colour being chosen.
                    let color = axis.at(step);
                    let marker = if index == picker_axis && step == picker_step {
                        "+"
                    } else {
                        " "
                    };
                    Span::styled(marker, Style::default().bg(color).fg(swatch_fg(color)))
                }));
                Line::from(spans)
            })
            .collect()
    }

    /// The full-space grid: every index the palette can hold, [`FULL_GRID_WIDTH`] to a row.
    ///
    /// Cells are three columns wide here where the ramp grid uses one. The ramp grid is packed
    /// because grey's 24 steps must fit `PANE_WIDTH` on one row; this grid is eight cells across,
    /// so the width is free and a wider swatch is easier to judge a colour from.
    ///
    /// Scrolled to keep the cursor's row on screen: the whole grid is forty-odd rows and a terminal
    /// is routinely shorter. `rows` is what the caller has left after its own headings — the grid
    /// cannot know that for itself.
    fn full_grid(cursor: usize, rows: usize) -> Vec<Line<'static>> {
        let cursor_row = cursor / FULL_GRID_WIDTH;
        // Show the cursor's row, keeping as much above it as fits — the same rule as the entry
        // list's `ListCursor::window`, computed here because the grid's rows are not registry rows.
        let first_row = cursor_row.saturating_sub(rows.saturating_sub(1));

        full_grid_cells()
            .chunks(FULL_GRID_WIDTH)
            .enumerate()
            .skip(first_row)
            .take(rows)
            .map(|(row, cells)| {
                Line::from(
                    cells
                        .iter()
                        .enumerate()
                        .map(|(column, cell)| match cell {
                            // The cursor is a glyph over the swatch, as in the ramp grid, so the
                            // cell keeps showing the colour being chosen.
                            Some(value) => {
                                let color = Color::Indexed(*value);
                                Span::styled(
                                    if row * FULL_GRID_WIDTH + column == cursor {
                                        " + "
                                    } else {
                                        "   "
                                    },
                                    Style::default().bg(color).fg(swatch_fg(color)),
                                )
                            }
                            None => Span::raw("   "),
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    const SELECT: PaneCommand = PaneCommand::List(ListMove::Select);
    const BACK: PaneCommand = PaneCommand::List(ListMove::Close);
    const LIST_DOWN: PaneCommand = PaneCommand::List(ListMove::Down);
    const MOVE_UP: PaneCommand = PaneCommand::Move(Direction::Up);
    const MOVE_DOWN: PaneCommand = PaneCommand::Move(Direction::Down);
    const RIGHT: PaneCommand = PaneCommand::Move(Direction::Right);
    const LEFT: PaneCommand = PaneCommand::Move(Direction::Left);

    fn entries() -> PalettePane {
        PalettePane {
            mode: Mode::List,
            ..PalettePane::default()
        }
    }

    fn draw(pane: &mut PalettePane, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("backend");
        terminal
            .draw(|frame| pane.draw(frame, Rect::new(0, 0, width, height), &Keymap::defaults()))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn layout_covers_every_registry_entry_once() {
        let mut seen = vec![0usize; REGISTRY.len()];
        for section in SECTIONS {
            assert!(!section.rows.is_empty(), "section {:?} must have rows", section.title);
            for row in section.rows {
                assert!(row.len() > 0, "row {:?} must have at least one swatch", row.label);
                for cluster in row.clusters {
                    assert!(
                        !cluster.is_empty(),
                        "row {:?} must not have an empty cluster",
                        row.label
                    );
                    for swatch in *cluster {
                        assert!(
                            swatch.entry < REGISTRY.len(),
                            "entry index {} out of bounds",
                            swatch.entry
                        );
                        seen[swatch.entry] += 1;
                    }
                }
            }
        }
        for (index, &count) in seen.iter().enumerate() {
            let (ns, name) = REGISTRY[index];
            assert_eq!(
                count, 1,
                "registry entry {ns}::{name} (index {index}) must appear in the sheet exactly once"
            );
        }
    }

    #[test]
    fn every_swatch_row_fits_within_pane_width() {
        for section in SECTIONS {
            for row in section.rows {
                assert!(
                    row.display_width() <= usize::from(PANE_WIDTH),
                    "row {:?} swatch strip ({} cols) exceeds PANE_WIDTH ({PANE_WIDTH})",
                    row.label,
                    row.display_width()
                );
            }
        }
    }

    #[test]
    fn sheet_navigation_moves_in_two_dimensions_and_preserves_column_across_short_rows() {
        let mut pane = entries();
        // Find a wide row followed by a narrower row so horizontal memory is exercised meaningfully.
        let wide_row = (0..layout::row_count().saturating_sub(1))
            .find(|&row| layout::row_at(row).len() > layout::row_at(row + 1).len())
            .expect("layout must have a wide row above a narrower row");
        for _ in 0..wide_row {
            pane.apply(MOVE_DOWN);
        }
        let target_col = layout::row_at(wide_row).len() - 1;
        for _ in 0..target_col {
            pane.apply(RIGHT);
        }
        assert_eq!(pane.sheet.entry(), layout::row_at(wide_row).swatch(target_col).entry);

        // Moving down clamps to the shorter row's last swatch; moving back up restores target_col.
        pane.apply(MOVE_DOWN);
        let short_last = layout::row_at(wide_row + 1).len() - 1;
        assert_eq!(
            pane.sheet.entry(),
            layout::row_at(wide_row + 1).swatch(short_last).entry
        );

        pane.apply(MOVE_UP);
        assert_eq!(pane.sheet.entry(), layout::row_at(wide_row).swatch(target_col).entry);

        // Moving left on the shorter row syncs the desired column to the real clamped position.
        pane.apply(MOVE_DOWN);
        pane.apply(LEFT);
        let expected_after_left = short_last.saturating_sub(1);
        assert_eq!(
            pane.sheet.entry(),
            layout::row_at(wide_row + 1).swatch(expected_after_left).entry
        );
        pane.apply(MOVE_UP);
        assert_eq!(
            pane.sheet.entry(),
            layout::row_at(wide_row).swatch(expected_after_left).entry
        );
    }

    #[test]
    fn picker_header_and_sheet_render_each_swatch_at_its_layout_column() {
        let _palette = palette::lock_for_test();
        let mut pane = entries();
        // Pick the widest row (Bases, multi-cluster) and seed every swatch with a distinct colour.
        let widest = (0..layout::row_count())
            .max_by_key(|&row| layout::row_at(row).len())
            .expect("layout has rows");
        let row = layout::row_at(widest);
        assert!(row.clusters.len() > 1, "widest row must span multiple clusters");
        for col in 0..row.len() {
            let color = Color::Indexed(palette::FIRST_ADDRESSABLE_INDEX + u8::try_from(col * 7 + 3).expect("fits"));
            palette::set_entry(row.swatch(col).entry, color);
        }

        for _ in 0..widest {
            pane.apply(MOVE_DOWN);
        }
        pane.apply(RIGHT);

        // Check sheet rendering on a tall pane where no scrolling clips the row.
        let height = u16::try_from(layout::total_body_lines() + SHEET_HEADER_ROWS + 1).expect("fits");
        let sheet_buf = draw(&mut pane, PANE_WIDTH, height);
        let (_, strip_body_line) = layout::sheet_line_span(widest);
        let sheet_y = u16::try_from(SHEET_HEADER_ROWS + strip_body_line).expect("fits");
        for col in 0..row.len() {
            let x = u16::try_from(row.swatch_x(col)).expect("fits");
            let expected = palette::entry_color(row.swatch(col).entry);
            for dx in 0..u16::try_from(SWATCH_WIDTH).expect("fits") {
                assert_eq!(
                    sheet_buf.cell((x + dx, sheet_y)).expect("in bounds").bg,
                    expected,
                    "sheet swatch col {col} cell +{dx} must paint its live entry colour"
                );
            }
        }

        // Open the picker and verify its sibling preview row paints the same columns live.
        pane.apply(SELECT);
        let picker_buf = draw(&mut pane, PANE_WIDTH, height);
        const SIBLING_ROW_Y: u16 = 1;
        for col in 0..row.len() {
            let x = u16::try_from(row.swatch_x(col)).expect("fits");
            let expected = palette::entry_color(row.swatch(col).entry);
            for dx in 0..u16::try_from(SWATCH_WIDTH).expect("fits") {
                assert_eq!(
                    picker_buf.cell((x + dx, SIBLING_ROW_Y)).expect("in bounds").bg,
                    expected,
                    "picker sibling swatch col {col} cell +{dx} must paint its live entry colour"
                );
            }
        }
    }

    #[test]
    fn a_short_sheet_keeps_the_focused_swatch_visible() {
        let _palette = palette::lock_for_test();
        let mut pane = entries();
        let last_row = layout::row_count() - 1;
        let last_swatch = layout::row_at(last_row).swatch(0);
        let sentinel = ramp::purple(4);
        // Set every other entry to a different colour so only the target swatch paints `sentinel`.
        for idx in 0..REGISTRY.len() {
            palette::set_entry(idx, ramp::grey(1));
        }
        palette::set_entry(last_swatch.entry, sentinel);

        for _ in 0..last_row {
            pane.apply(MOVE_DOWN);
        }
        for height in [10, 4, 2, 1] {
            let buffer = draw(&mut pane, PANE_WIDTH, height);
            let visible = (0..height)
                .flat_map(|y| (0..PANE_WIDTH).map(move |x| (x, y)))
                .any(|(x, y)| buffer.cell((x, y)).expect("in bounds").bg == sentinel);
            assert!(visible, "focused swatch must stay visible at height {height}");
        }
    }

    #[test]
    fn menu_selection_enters_a_view_that_can_return_or_close() {
        let mut pane = PalettePane::default();
        assert_eq!(pane.apply(PaneCommand::Close), PaneAction::Close);
        for (_, mode) in MENU {
            pane.apply(SELECT);
            assert_eq!(pane.mode, *mode);
            assert_eq!(pane.apply(PaneCommand::Close), PaneAction::Close);
            assert_eq!(pane.apply(BACK), PaneAction::None);
            assert_eq!(pane.scope(), Scope::PaletteMenu);
            pane.apply(LIST_DOWN);
        }
    }

    #[test]
    fn a_short_pane_shows_the_option_that_select_will_apply() {
        let _palette = palette::lock_for_test();
        let mut pane = PalettePane::default();
        let check_visible = |pane: &mut PalettePane, label: &str| {
            for height in [3, 2, 1] {
                let buffer = draw(pane, PANE_WIDTH, height);
                let visible = (0..height).any(|y| {
                    let row: String = (0..PANE_WIDTH).map(|x| buffer[(x, y)].symbol()).collect();
                    row.trim() == label
                });
                assert!(visible, "{label:?} must be visible at height {height}");
            }
        };
        let (chooser, (label, _)) = MENU
            .iter()
            .enumerate()
            .find(|(_, (_, mode))| matches!(mode, Mode::Select(_)))
            .expect("the menu opens the built-in chooser");

        for _ in 0..chooser {
            pane.apply(LIST_DOWN);
        }
        check_visible(&mut pane, label);
        pane.apply(SELECT);

        for built_in in BUILT_INS {
            check_visible(&mut pane, built_in.name);
            assert_eq!(pane.apply(SELECT), PaneAction::SelectPalette(built_in.name));
            pane.apply(LIST_DOWN);
        }
        pane.apply(BACK);
        check_visible(&mut pane, label);
        pane.apply(SELECT);
        check_visible(&mut pane, BUILT_INS[0].name);
    }

    #[test]
    fn entry_save_requests_save_and_stays_in_entries() {
        let mut pane = entries();

        assert_eq!(pane.apply(PaneCommand::Save), PaneAction::Save);
        assert_eq!(pane.scope(), Scope::PaletteEntries);
    }

    #[test]
    fn built_in_selection_leaves_palette_application_to_app() {
        let _palette = palette::lock_for_test();
        let before: Vec<_> = (0..REGISTRY.len()).map(palette::entry_color).collect();
        let mut pane = PalettePane {
            mode: Mode::Select(ListCursor::default()),
            ..PalettePane::default()
        };

        for built_in in BUILT_INS {
            assert_eq!(pane.apply(SELECT), PaneAction::SelectPalette(built_in.name));
            assert_eq!(pane.scope(), Scope::PaletteSelect);
            assert_eq!(
                (0..REGISTRY.len()).map(palette::entry_color).collect::<Vec<_>>(),
                before
            );
            pane.apply(LIST_DOWN);
        }
    }

    #[test]
    fn live_preview_can_be_reverted_or_kept_after_toggling_reach() {
        let _palette = palette::lock_for_test();
        let axis = ramp::axes()
            .into_iter()
            .find(|axis| axis.len() > 1)
            .expect("an axis has adjacent colours");
        let original = axis.at(0);
        let preview = axis.at(1);
        let mut pane = entries();
        // Move off origin so the test also proves edits target the 2D sheet cursor.
        pane.apply(MOVE_DOWN);
        pane.apply(RIGHT);
        let target = pane.sheet.entry();
        palette::set_entry(target, original);

        for (finish, expected) in [(BACK, original), (SELECT, preview)] {
            pane.apply(SELECT);
            pane.apply(RIGHT);
            assert_eq!(palette::entry_color(target), preview);
            pane.apply(PaneCommand::ToggleReach);
            assert_eq!(palette::entry_color(target), preview);
            assert_eq!(pane.apply(PaneCommand::Close), PaneAction::None);
            assert_eq!(pane.scope(), Scope::PalettePicker);
            pane.apply(finish);
            assert_eq!(palette::entry_color(target), expected);
            assert_eq!(pane.scope(), Scope::PaletteEntries);
        }
    }

    #[test]
    fn leaving_a_deep_grey_step_clamps_to_the_destination_ramp() {
        let _palette = palette::lock_for_test();
        const MID_GREY: usize = (ramp::RAMP_STEPS + ramp::GREY_SCALE.len()) / 2;
        // Starting at grey's last step would let a clamp against either ramp produce the same result.
        const _: () = assert!(MID_GREY >= ramp::RAMP_STEPS && MID_GREY < ramp::GREY_SCALE.len() - 1);
        let mut pane = entries();
        let target = pane.sheet.entry();
        palette::set_entry(target, ramp::grey(MID_GREY));
        let axes = ramp::axes();
        let hue = &axes[axes.len() - 2];

        pane.apply(SELECT);
        pane.apply(MOVE_UP);
        assert_eq!(palette::entry_color(target), hue.at(hue.len() - 1));

        // Axis::at clamps too, so only the next move can show whether the stored step clamped.
        pane.apply(LEFT);
        assert_eq!(palette::entry_color(target), hue.at(hue.len() - 2));
    }

    #[test]
    fn full_grid_navigation_applies_off_ramp_indices_and_stays_within_rows() {
        let _palette = palette::lock_for_test();
        let mut pane = entries();
        let target = pane.sheet.entry();
        let cells = full_grid_cells();
        let colour_at = |cell: usize| Color::Indexed(cells[cell].expect("a colour cell"));
        let start = (0..FULL_GRID_WIDTH - 1)
            .find(|&cell| ramp::locate_color(colour_at(cell)).is_none())
            .expect("a first-row off-ramp colour with a neighbour");
        palette::set_entry(target, colour_at(start));

        pane.apply(SELECT);
        pane.apply(RIGHT);
        assert_eq!(palette::entry_color(target), colour_at(start + 1));
        pane.apply(MOVE_DOWN);
        assert_eq!(palette::entry_color(target), colour_at(start + 1 + FULL_GRID_WIDTH));
        pane.apply(MOVE_UP);
        pane.apply(LEFT);
        assert_eq!(palette::entry_color(target), colour_at(start));

        for _ in 0..FULL_GRID_WIDTH {
            pane.apply(RIGHT);
        }
        assert_eq!(palette::entry_color(target), colour_at(FULL_GRID_WIDTH - 1));
    }

    /// The full grid reaches exactly what the palette parser accepts.
    #[test]
    fn the_full_grid_holds_every_index_once() {
        let mut cells: Vec<u8> = full_grid_cells().iter().flatten().copied().collect();
        cells.sort_unstable();
        let expected: Vec<u8> = (0..=u8::MAX).collect();
        assert_eq!(cells, expected, "the grid must be exactly every index");
    }

    /// Check every starting cell because moves do not necessarily encounter a pad in a walk.
    #[test]
    fn no_move_ever_lands_on_a_pad() {
        let cells = full_grid_cells();
        assert!(
            cells.iter().any(Option::is_none),
            "the grid must have padding for this to test"
        );

        for (cell, occupant) in cells.iter().enumerate() {
            // Only real starting positions: the cursor can never be on a pad to begin with.
            if occupant.is_none() {
                continue;
            }
            for (down, right) in [(0, 1), (0, -1), (1, 0), (-1, 0)] {
                let landed = moved_grid_cell(&cells, cell, down, right);
                assert!(
                    cells[landed].is_some(),
                    "moving ({down}, {right}) from cell {cell} landed on pad {landed}"
                );
            }
        }
    }

    /// The frame and the picker must attach the same index to each layout cell.
    #[test]
    fn the_full_grid_places_each_index_at_its_layout_position() {
        let _palette = palette::lock_for_test();
        let mut pane = entries();
        palette::set_entry(pane.sheet.entry(), ramp::grey(0));
        pane.apply(SELECT);
        pane.apply(PaneCommand::ToggleReach);

        const HEIGHT: u16 = 60;
        let buffer = draw(&mut pane, PANE_WIDTH, HEIGHT);

        let grid_top = u16::try_from(PICKER_HEADER_ROWS).expect("header rows fit");
        // Three columns per swatch: the width `full_grid` draws each cell at.
        const CELL_WIDTH: u16 = 3;

        for (position, cell) in full_grid_cells().iter().copied().enumerate() {
            let Some(index) = cell else { continue };
            let y = grid_top + u16::try_from(position / FULL_GRID_WIDTH).expect("a row fits");
            if y >= HEIGHT {
                break;
            }
            let x = u16::try_from(position % FULL_GRID_WIDTH).expect("a column fits") * CELL_WIDTH;
            assert_eq!(
                buffer.cell((x, y)).expect("the grid must draw this cell").bg,
                Color::Indexed(index),
                "index {index} must draw at cell {position}"
            );
        }
    }

    /// A deep selection cannot pass this check if the grid stays scrolled to the top.
    #[test]
    fn opening_on_a_deep_colour_keeps_it_on_screen() {
        let _palette = palette::lock_for_test();
        // Taken from the end of the grid rather than named, so this follows the layout rather than
        // assuming which colours sit last.
        let deep = full_grid_cells()
            .iter()
            .flatten()
            .next_back()
            .copied()
            .expect("the grid is not empty");
        let mut pane = entries();
        palette::set_entry(pane.sheet.entry(), Color::Indexed(deep));
        pane.apply(SELECT);

        // Ten rows: header and seven of grid, against a grid over forty rows tall.
        const HEIGHT: u16 = 10;
        let buffer = draw(&mut pane, PANE_WIDTH, HEIGHT);
        let on_screen = (u16::try_from(PICKER_HEADER_ROWS).expect("fits")..HEIGHT)
            .flat_map(|y| (0..PANE_WIDTH).map(move |x| (x, y)))
            .any(|(x, y)| buffer.cell((x, y)).expect("in bounds").bg == Color::Indexed(deep));

        assert!(
            on_screen,
            "the cursor's colour must be visible in a pane too short for the whole grid"
        );
    }
}
