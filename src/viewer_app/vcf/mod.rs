//! The isolated viewer for an active VCF's variant table.
//!
//! `VCFViewer` owns table geometry, visible-column filtering, cursor-following, the modal
//! picker, and the VCF work that drives them. Loading shows a bounded preview synchronously,
//! verifies that preview through `LineIndex`, then hands whole-file scrolling to `ChunkBuffer`.
//! The verified rows are seeded into the buffer before the handover, so the first indexed frame
//! does not go blank and an index failure can leave the preview in place.
//!
//! `visible_column_indices` maps declared columns to the filtered visible list; headings, widths,
//! and cells must all use that window once. `draw` records the terminal viewport for movement
//! commands, while cursor movement and paging feed the same offsets to the chunk
//! buffer. Row indexing, locus lookup, and chunk reads share the I/O worker;
//! `tick` polls them and `background_work` exposes their progress to the shell.
//!
//! Commands enter through `CommandTarget`. A cross-viewer row jump emits typed follow-ups for
//! the router. Keep `cursor_row`'s preview/chunk split identical
//! to `display_rows`, or jumps and headings can silently disagree with what was drawn.

pub(crate) mod chunks;
mod column_picker;
pub(crate) mod ingest;
pub(crate) mod line_index;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::{
    Frame,
    layout::{Constraint, Position, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Row, Table},
};

use crate::basepair::{BasePair, format_with_commas};
use crate::command::protocol::{
    Applied, CommandOutcome, CommandTarget, Direction, GenomeCommand, ListMove, ScrollStep, TableCommand,
};
use crate::input::{MouseEvent, MouseKind};
use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{JobHandle, JobPoll};
use crate::keys::{Keymap, Scope};
use crate::list_cursor::{ListCursor, follow_row};
use crate::notification::{Notification, NotificationQueue};
use crate::style::{Chrome, Selection, Surface, Text};
use crate::vcf_file::AlternateSampleLookup;
use crate::viewer::{BackgroundWork, StatusProvider, ViewerApp};

use super::bam_switcher::BamSwitcher;
use chunks::ChunkBuffer;
use ingest::{FIXED_COLUMNS, VcfTable};
use line_index::LineIndex;

/// How many records the synchronous fast path reads.
///
/// The preview **is** the width-measurement sample — one concept at one size,
/// not two constants to keep in sync. Nothing may assume this value: everything
/// derives from the rows the read returns, so retuning it changes how much is
/// sampled and nothing else.
const PREVIEW_RECORDS: usize = 1_000;

/// Marks the end of the preview so it does not read as the end of the file.
///
/// The word leads: the marker lives in the first cell, which is only as wide as
/// the first column, so anything in front of it is what gets truncated away.
const PREVIEW_END_MARKER: &str = "Preview --";

/// Display widths for the fixed fields, in [`FIXED_COLUMNS`] order.
const FIXED_WIDTHS: [u16; FIXED_COLUMNS.len()] = [10, 12, 14, 10, 10, 8, 10];

/// Blank columns between two cells, matching ratatui's default column spacing.
/// Counted when deciding how many columns fit, or the window overshoots.
const COLUMN_SPACING: u16 = 1;

/// Rows or columns moved per cursor step.
const SCROLL_STEP: usize = 1;

/// Fixed row count used for paging.
const PAGE_STEP: usize = 20;

/// Columns moved per shifted horizontal key press, counted in visible columns.
/// Fixed for the same reason [`PAGE_STEP`] is.
const COLUMN_PAGE_STEP: usize = 10;

/// Per-row heap budget, checked against the preview's own rows.
///
/// Two chunks of `CHUNK_ROWS` rows are resident, so the buffer costs roughly this
/// times `2 * CHUNK_ROWS`. Measuring rather than capping is the point: INFO is
/// stored whole, so row width follows how densely the file is annotated and the
/// check must reflect what was actually read. Exceeding it warns and loads
/// anyway; refusing a file the user asked for is not this check's job.
const ROW_BYTES_BUDGET: usize = 2_048;

/// Row and column offsets for the table's top-left corner. These are indices,
/// not base pairs. Keys usually move the cursor first. Paging and dragging move the viewport first.
#[derive(Debug, Clone, Copy, Default)]
struct TableScrollState {
    horizontal: usize,
    vertical: usize,
}

/// The selected row and column — what a per-row or per-column operation acts on.
///
/// `column` indexes the **visible** column list, exactly as
/// [`TableScrollState::horizontal`] does, so `visible_column_indices` stays the
/// one place filtering is resolved and the cursor can never land on a hidden
/// column.
#[derive(Debug, Clone, Copy, Default)]
struct TableCursor {
    row: usize,
    column: usize,
}

impl TableCursor {
    /// Moves the cursor, clamping at both ends rather than wrapping.
    fn step(&mut self, direction: Direction, step: usize, max_column: usize, max_row: usize) {
        match direction {
            Direction::Left => self.column = self.column.saturating_sub(step),
            Direction::Right => self.column = self.column.saturating_add(step).min(max_column),
            Direction::Up => self.row = self.row.saturating_sub(step),
            Direction::Down => self.row = self.row.saturating_add(step).min(max_row),
        }
    }
}

struct TableGesture {
    last: Position,
    record: Option<usize>,
    horizontal_cells: i32,
}

/// Which of the table's columns are drawn.
///
/// Indexed by declared-column index — fixed fields first, then INFO in header
/// order — so it lines up with [`VcfTable::heading`] and needs no notion of the
/// two being different kinds of thing. Fixed fields are toggleable like any
/// other: CHROM and POS are often the least interesting columns when comparing
/// annotations, and one uniform rule beats a special case at [`FIXED_COLUMNS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ColumnFilter {
    /// One flag per declared column. Rebuilt per file rather than resized: a
    /// second VCF's keys are different keys, so carrying the flags over would
    /// apply one file's choices to another's columns.
    shown: Vec<bool>,
}

impl ColumnFilter {
    /// All `total` columns shown — the state a freshly loaded file starts in.
    fn all_shown(total: usize) -> Self {
        Self {
            shown: vec![true; total],
        }
    }

    /// Whether a declared column is drawn. Unknown columns count as shown, so a
    /// filter that has not caught up with a wider table cannot hide anything.
    fn is_shown(&self, column: usize) -> bool {
        self.shown.get(column).copied().unwrap_or(true)
    }

    fn shown_count(&self) -> usize {
        self.shown.iter().filter(|shown| **shown).count()
    }

    /// Toggles one column, refusing to hide the last one standing.
    ///
    /// A table with no columns is not a state worth being able to reach, and
    /// `visible_columns` already promises at least one column draws. Refused
    /// rather than ignored so the key press is not a silent no-op.
    fn toggle(&mut self, column: usize) -> Result<(), String> {
        let last_standing = self.shown_count() == 1;
        let Some(shown) = self.shown.get_mut(column) else {
            return Ok(());
        };
        if *shown && last_standing {
            return Err("The last visible column cannot be hidden".to_string());
        }
        *shown = !*shown;
        Ok(())
    }

    fn show_all(&mut self) {
        self.shown.fill(true);
    }

    /// Hides everything but `column`. The last-column guard makes an empty table
    /// unreachable, so this is what "none" can usefully mean — and on a file
    /// declaring hundreds of keys it is the operation actually wanted.
    fn show_only(&mut self, column: usize) {
        self.shown.fill(false);
        if let Some(shown) = self.shown.get_mut(column) {
            *shown = true;
        }
    }
}

/// One line of the drawn table. A variant carries only the cells for the
/// currently visible columns, not one per declared column.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayRow {
    Variant {
        index: usize,
        cells: Vec<String>,
    },
    /// The preview's end, shown only until the index makes rows continue past it.
    Marker(&'static str),
}

enum VcfPopover {
    Columns(ListCursor),
    VariantBams {
        switcher: BamSwitcher,
        active: Option<PathBuf>,
        contig: String,
        position: BasePair,
    },
}

/// Viewer hosting the variant table.
pub struct VCFViewer {
    io_worker: Option<IoWorkerHandle>,
    /// `None` until a VCF is set. Holds the preview until the index lands, and
    /// stays as the header/contig record afterwards.
    table: Option<VcfTable>,
    path: Option<PathBuf>,
    scroll: TableScrollState,
    /// The selected row and column. Movement keys move this; `scroll` follows it
    /// only when it would otherwise leave the frame.
    cursor: TableCursor,
    /// The last drawn table viewport, in rows and columns of terminal space.
    ///
    /// Cursor-following uses these dimensions between frames. Zero before the
    /// first frame means there is nothing yet to scroll.
    viewport: (usize, u16),
    drawn_body: Rect,
    drawn_rows: Vec<usize>,
    mouse_gesture: Option<TableGesture>,
    /// Which declared columns are drawn. `scroll.horizontal` counts the columns
    /// this leaves visible, not the columns the file declares.
    columns: ColumnFilter,
    popover: Option<VcfPopover>,
    popover_bounds: Option<Rect>,
    /// The record-offset index, in flight until the worker's scan reports back,
    /// with when it started — one of the three phases [`Self::work_started_at`]
    /// spans.
    index_build: Option<(JobHandle<std::io::Result<LineIndex>>, crate::Instant)>,
    /// `None` until the scan lands. Shared with each chunk fetch rather than
    /// cloned — the checkpoint table reaches megabytes on a large VCF.
    line_index: Option<Arc<LineIndex>>,
    /// The resident rows once scrolling is index-driven.
    chunks: ChunkBuffer,
    /// A nearest-locus lookup, kept separate from viewport work so a newer
    /// scroll cannot make a deliberate command stale.
    locus_jump: Option<(JobHandle<std::io::Result<Option<usize>>>, crate::Instant)>,
    notify: NotificationQueue,
}

impl VCFViewer {
    pub fn new(io_worker: IoWorkerHandle) -> Self {
        Self::with_worker(Some(io_worker))
    }

    fn with_worker(io_worker: Option<IoWorkerHandle>) -> Self {
        Self {
            io_worker,
            table: None,
            path: None,
            scroll: TableScrollState::default(),
            cursor: TableCursor::default(),
            viewport: (0, 0),
            drawn_body: Rect::ZERO,
            drawn_rows: Vec::new(),
            mouse_gesture: None,
            columns: ColumnFilter::default(),
            popover: None,
            popover_bounds: None,
            index_build: None,
            line_index: None,
            chunks: ChunkBuffer::default(),
            locus_jump: None,
            notify: NotificationQueue::default(),
        }
    }

    #[cfg(feature = "resident-demo")]
    pub(crate) fn resident(vcf: &[u8]) -> std::io::Result<Self> {
        let table = ingest::read_bgzf(vcf)?;
        let mut viewer = Self::with_worker(None);
        viewer.columns = ColumnFilter::all_shown(table.total_columns());
        viewer.table = Some(table);
        Ok(viewer)
    }

    /// Loads a VCF: the synchronous fast path first, then a background scan for
    /// the record-offset index. The table is drawn from the fast path either way,
    /// so the scan's cost never delays the first frame.
    fn set_active_vcf(&mut self, path: &Path) -> Result<(), String> {
        let Some(worker) = self.io_worker.clone() else {
            return Err("Loading VCF files is unavailable in this demo".to_string());
        };
        let table = ingest::read_preview(path, PREVIEW_RECORDS)
            .map_err(|err| format!("Failed to read VCF {}: {err}", path.display()))?;

        self.warn_if_rows_are_oversized(table.mean_row_bytes());
        // Rebuilt from this file's own column count, never resized from the last
        // one's: a second VCF declares different keys at the same indices.
        self.columns = ColumnFilter::all_shown(table.total_columns());
        self.popover = None;
        self.table = Some(table);
        self.path = Some(path.to_path_buf());
        self.scroll = TableScrollState::default();
        // A second file's rows are different rows, so the selection cannot carry
        // over any more than the offset can.
        self.cursor = TableCursor::default();
        self.drawn_rows.clear();
        self.cancel_mouse_gesture();
        self.line_index = None;
        self.chunks.clear();
        self.locus_jump = None;
        self.index_build = Some((
            worker.submit(IoSchedule::Fifo, {
                let path = path.to_path_buf();
                move || line_index::build(&path)
            }),
            crate::Instant::now(),
        ));
        Ok(())
    }

    /// When the viewer's oldest outstanding work started, for the phase of the
    /// shell's progress spinner. One clock across every phase, so the glyph keeps
    /// turning through a handover rather than resetting to its first frame each time.
    fn work_started_at(&self) -> Option<crate::Instant> {
        [
            self.index_build.as_ref().map(|(_, started)| *started),
            self.locus_jump.as_ref().map(|(_, started)| *started),
            self.chunks.started_at(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Flags rows wide enough that a full buffer would cost real memory.
    ///
    /// The preview is a free sample of the file's own rows, so this measures
    /// rather than assumes. A warning only: the load continues either way.
    fn warn_if_rows_are_oversized(&mut self, mean_row_bytes: usize) {
        if mean_row_bytes <= ROW_BYTES_BUDGET {
            return;
        }
        let projected = mean_row_bytes * 2 * chunks::CHUNK_ROWS / 1_048_576;
        self.notify.warn(format!(
            "Wide rows: {mean_row_bytes} bytes each, so a full row buffer is about {projected} MB"
        ));
    }

    /// Replaces the preview's rows with the same rows read back through the
    /// index. Both are a from-the-top read of one file, so they must agree
    /// exactly; a mismatch means the offsets are wrong and is reported rather
    /// than drawn, since the whole point of the swap is that it is invisible.
    ///
    /// The one inline read left, acceptable only because this is the preview's
    /// span: at most `PREVIEW_RECORDS` records from a known offset. Scroll-driven
    /// reads go through `chunks`, on the worker.
    fn swap_in_indexed_rows(&mut self, index: LineIndex) {
        let (Some(path), Some(table)) = (&self.path, &mut self.table) else {
            return;
        };

        match index.read_rows(path, 0, table.rows.len()) {
            // Equal by construction when the offsets are right, so the swap
            // itself changes nothing visible — keeping the index is the point.
            Ok(rows) if rows == table.rows => {
                // Seeded *before* `line_index` flips `display_rows` to the buffer,
                // so the rows on screen are already being served by it and the
                // handover draws nothing empty. These are the verified rows, so
                // this costs no read of its own; `ensure_visible` tops the partial
                // chunk up afterwards.
                self.chunks.seed(0, rows.clone());
                // Only `rows` is replaced. `info_columns` was measured against
                // the rows going out, and since these are the same rows it would
                // re-measure identically — but assigning a whole `VcfTable` here
                // would drop the columns and relayout the table the instant the
                // index landed, which is exactly the invisible-swap property this
                // path exists to preserve.
                table.rows = rows;
                // Scrolling is unaffected by disorder, so this warns rather than
                // refusing the index: only locus lookups can miss on such a file.
                if let Some(disorder) = index.disorder() {
                    let disorder = disorder.to_string();
                    self.notify
                        .warn(format!("VCF records are not sorted ({disorder}); locus jumps may miss"));
                }
                self.line_index = Some(Arc::new(index));
            }
            Ok(_) => {
                self.notify
                    .error("Indexed rows disagree with the sequential read; keeping the preview");
            }
            Err(err) => self
                .notify
                .error(format!("VCF indexed, but reading it back failed: {err}")),
        }
    }

    /// Total rows the table can scroll over: the whole file once indexed, and
    /// only the preview before that.
    fn total_rows(&self) -> usize {
        match (&self.line_index, &self.table) {
            (Some(index), _) => index.total_records(),
            (None, Some(table)) => table.rows.len(),
            (None, None) => 0,
        }
    }

    /// The declared-column indices currently shown, in table order.
    ///
    /// **The one place filtering is resolved.** Everything downstream indexes
    /// this list rather than the declared set, which is what lets
    /// `scroll.horizontal` count visible columns: no consumer walks past a hidden
    /// column, and the offset can never land on one.
    fn visible_column_indices(&self) -> Vec<usize> {
        (0..self.declared_columns())
            .filter(|column| self.columns.is_shown(*column))
            .collect()
    }

    /// Every column the file declares, shown or not — the picker's list, and the
    /// space `visible_column_indices` filters.
    fn declared_columns(&self) -> usize {
        self.table.as_ref().map_or(FIXED_COLUMNS.len(), VcfTable::total_columns)
    }

    fn declared_width(&self, column: usize) -> u16 {
        match FIXED_WIDTHS.get(column) {
            Some(fixed) => *fixed,
            None => self
                .table
                .as_ref()
                .and_then(|table| table.extra_column(column))
                .map_or(0, |(_, width)| width),
        }
    }

    /// The visible columns' widths, in draw order — what the visible window is
    /// cut from.
    fn column_widths(&self) -> Vec<u16> {
        self.visible_column_indices()
            .into_iter()
            .map(|column| self.declared_width(column))
            .collect()
    }

    /// How many columns fit in `width`, starting at the horizontal scroll offset.
    ///
    /// **This is what makes horizontal scrolling virtual.** It walks widths only,
    /// touching no row data, so its cost is the number of columns that fit rather
    /// than the number declared — and it is what stops a file with hundreds of
    /// INFO keys handing hundreds of constraints to ratatui's layout solver, or
    /// building hundreds of cells per row, to draw the ten that are on screen.
    ///
    /// At least one column, so a pane too narrow for the first still draws it
    /// clipped rather than showing an empty table.
    fn visible_columns(&self, width: u16) -> usize {
        let mut used = 0u16;
        let mut fitted = 0usize;
        for column in self.column_widths().into_iter().skip(self.scroll.horizontal) {
            used = used.saturating_add(column);
            if used > width {
                break;
            }
            used = used.saturating_add(COLUMN_SPACING);
            fitted += 1;
        }
        fitted.max(1)
    }

    /// The declared-column indices for the drawn window: `columns` visible
    /// columns from the horizontal offset.
    ///
    /// Headings, widths, and row cells are all built from this one list, so the
    /// three cannot disagree about which column is leftmost — the row-wise
    /// reading of invariant 9.
    fn visible_window(&self, columns: usize) -> Vec<usize> {
        self.visible_column_indices()
            .into_iter()
            .skip(self.scroll.horizontal)
            .take(columns)
            .collect()
    }

    /// The record under the cursor, or `None` if nothing is there to read yet.
    ///
    /// Split by the same rule [`Self::display_rows`] uses, and for the same reason: before the
    /// index lands the preview in `table.rows` *is* the table, and after it the rows come from the
    /// chunk buffer, where only a window around the viewport is resident. Reading `table.rows`
    /// unconditionally silently returns a preview record for any row past `PREVIEW_RECORDS` — a
    /// wrong record rather than no record, which is worse.
    ///
    /// `None` for a row the buffer has not faulted in. The caller must not substitute anything:
    /// there is no correct answer for a row nobody has read.
    fn cursor_row(&self) -> Option<&ingest::VcfRow> {
        let table = self.table.as_ref()?;
        match self.line_index {
            None => table.rows.get(self.cursor.row),
            Some(_) => self.chunks.row(self.cursor.row),
        }
    }

    fn sample_lookup(&self) -> Option<AlternateSampleLookup> {
        let table = self.table.as_ref()?;
        if table.sample_names.len() <= 1 {
            return None;
        }
        let row = self.cursor.row;
        let (offset, skip) = self
            .line_index
            .as_ref()
            .and_then(|index| index.seek_start(row))
            .map_or((None, row), |(offset, skip)| (Some(offset), skip));
        Some(AlternateSampleLookup {
            path: self.path.clone()?,
            sample_names: Arc::clone(&table.sample_names),
            offset,
            skip,
        })
    }

    /// The lines to draw for `height` rows starting at the scroll offset, each
    /// carrying `columns` cells from the horizontal offset.
    ///
    /// Before the index lands this is the preview, terminated by a marker saying
    /// it stops short of the file's end. Afterwards rows come from the chunk
    /// buffer and continue past the preview, so nothing terminates them.
    ///
    /// A span whose chunk has not arrived draws nothing at all. In-flight work is
    /// the shell's to report, through [`BackgroundWork`] — a viewer announcing its
    /// own progress inside the table is what the notification system replaced.
    fn display_rows(&self, height: usize, columns: usize) -> Vec<DisplayRow> {
        let Some(table) = &self.table else { return Vec::new() };
        let first = self.scroll.vertical;
        // The window is resolved once, here, and every row is cut to exactly it.
        // `draw` must not skip these again: a second cut shifts the values out
        // from under their headings.
        let window = self.visible_window(columns);
        let cells = |offset, row: &ingest::VcfRow| DisplayRow::Variant {
            index: first + offset,
            cells: row.cells(&window, table),
        };

        let Some(_) = &self.line_index else {
            let mut rows: Vec<DisplayRow> = table.rows[first.min(table.rows.len())..]
                .iter()
                .take(height)
                .enumerate()
                .map(|(offset, row)| cells(offset, row))
                .collect();
            if table.truncated && rows.len() < height {
                rows.push(DisplayRow::Marker(PREVIEW_END_MARKER));
            }
            return rows;
        };

        let visible = height.min(self.total_rows().saturating_sub(first));
        self.chunks
            .slice(first, visible)
            .into_iter()
            .enumerate()
            .filter_map(|(offset, row)| row.map(|row| cells(offset, row)))
            .collect()
    }

    /// Brings the row buffer in line with the scroll position.
    ///
    /// Only the top edge is passed: the pair keeps the edge's chunk and, near a
    /// boundary, its neighbour — which is what covers a viewport straddling the
    /// two. The viewport's height does not enter into it, so movement and drawing
    /// can both call this without one of them knowing less than the other.
    fn ensure_visible(&mut self) {
        let (Some(index), Some(path), Some(worker)) = (&self.line_index, &self.path, &self.io_worker) else {
            return;
        };
        let total = self.total_rows();
        self.chunks.advance(self.scroll.vertical, total, index, path, worker);
    }

    /// Names the loaded file and its size, for the table's header and the status row.
    ///
    /// Deliberately not `title`: that is [`ViewerApp::title`], the identity `:focustab` resolves
    /// and the keybind namespace is scoped by, and it must never change. This one changes with
    /// every load, so the two names are kept apart rather than resolved by inherent-before-trait
    /// lookup at the call site.
    fn file_summary(&self) -> String {
        match (&self.path, &self.table) {
            (Some(path), Some(_)) => {
                let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                format!("{name} ({} variants)", self.total_rows())
            }
            (None, Some(_)) => format!("HG002 ({} variants)", self.total_rows()),
            _ => "no VCF loaded — :set activevcf <path>".to_string(),
        }
    }

    /// The cursor's record number, for the title bar's left half.
    ///
    /// The cursor's row rather than the top one: with a selection on screen, the
    /// number wanted is the one under it. Only this one line is numbered — a
    /// per-row gutter would cost seven columns on every row of a multimillion-
    /// record file, and invariant 9 forbids carving that out of the table's width
    /// in any case. Empty with no rows to number, rather than claiming a line the
    /// file does not have.
    fn line_indicator(&self) -> String {
        match self.total_rows() {
            0 => String::new(),
            // `cursor.row` is a 0-based index; displayed 1-based like every other
            // coordinate in the app (invariant 1).
            _ => format!(" Current line: {} ", format_with_commas(self.cursor.row as u64 + 1)),
        }
    }

    /// Every column the table can scroll across — the *visible* ones, since
    /// `scroll.horizontal` counts those. Hiding columns therefore tightens the
    /// clamp for free.
    fn total_columns(&self) -> usize {
        self.visible_column_indices().len()
    }

    /// Every declared column as `(heading, shown)`, in table order — the
    /// picker's list. Built per frame from the table and the filter, so the
    /// picker holds no copy that could go stale.
    fn column_entries(&self) -> Vec<(String, bool)> {
        (0..self.declared_columns())
            .map(|column| {
                let name = match &self.table {
                    Some(table) => table.heading(column).to_string(),
                    None => FIXED_COLUMNS.get(column).copied().unwrap_or_default().to_string(),
                };
                (name, self.columns.is_shown(column))
            })
            .collect()
    }

    /// Pulls the horizontal offset and the cursor's column back inside the
    /// visible columns.
    ///
    /// Hiding a column shortens that list, so a value that was in range can fall
    /// past the end. Clamping here matches the cursor's own movement, which
    /// already saturates rather than wrapping.
    fn clamp_horizontal(&mut self) {
        let last = self.total_columns().saturating_sub(1);
        self.scroll.horizontal = self.scroll.horizontal.min(last);
        self.cursor.column = self.cursor.column.min(last);
        self.follow_cursor();
    }

    /// Moves the cursor, bringing the viewport after it.
    fn move_cursor(&mut self, direction: Direction, step: usize) {
        let max_column = self.total_columns().saturating_sub(1);
        let max_row = self.total_rows().saturating_sub(1);
        self.cursor.step(direction, step, max_column, max_row);
        self.follow_cursor();
        self.ensure_visible();
    }

    /// Pages the viewport, carrying the cursor with it.
    ///
    /// The cursor moves by the rows the *viewport* actually travelled, not by the
    /// step asked for: at the file's end the viewport clamps, and moving the
    /// cursor the full step there would slide it down the frame rather than
    /// leaving it on the line it was on.
    fn page(&mut self, up: bool) {
        let rows = PAGE_STEP as isize;
        self.shift_rows(if up { -rows } else { rows });
        self.follow_cursor();
        self.ensure_visible();
    }

    fn shift_rows(&mut self, rows: isize) {
        let max_row = self.total_rows().saturating_sub(1);
        let before = self.scroll.vertical;
        self.scroll.vertical = before.saturating_add_signed(rows).min(max_row);
        let moved = before.abs_diff(self.scroll.vertical);
        let direction = if rows < 0 { Direction::Up } else { Direction::Down };
        self.cursor.step(direction, moved, 0, max_row);
    }

    /// Moves only across whole visible columns, saving any unspent part of the drag.
    fn pan_columns(&mut self, cells: &mut i32) {
        let widths = self.column_widths();
        let before = self.scroll.horizontal;
        let mut offset = before;
        while *cells != 0 {
            let step = cells.signum();
            let Some(next) = offset
                .checked_add_signed(step as isize)
                .filter(|&next| next < widths.len())
            else {
                *cells = 0;
                break;
            };
            let width = i32::from(widths[offset.min(next)]) + i32::from(COLUMN_SPACING);
            if cells.abs() < width {
                break;
            }
            offset = next;
            *cells -= step * width;
        }
        if offset != before {
            self.scroll.horizontal = offset;
            let relative = self.cursor.column.saturating_sub(before);
            let visible = self.visible_columns(self.viewport.1);
            self.cursor.column = offset + relative.min(visible - 1);
        }
    }

    fn drag_table(&mut self, cell: Position) {
        let Some(mut gesture) = self.mouse_gesture.take() else {
            return;
        };
        if cell != gesture.last {
            gesture.record = None;
            let rows = gesture.last.y as isize - cell.y as isize;
            if rows != 0 {
                self.shift_rows(rows);
                let top = self.scroll.vertical;
                let bottom = top
                    .saturating_add(self.viewport.0.saturating_sub(1))
                    .min(self.total_rows().saturating_sub(1));
                self.cursor.row = self.cursor.row.clamp(top, bottom);
                self.ensure_visible();
            }
            gesture.horizontal_cells += i32::from(gesture.last.x) - i32::from(cell.x);
            if gesture.horizontal_cells != 0 {
                self.pan_columns(&mut gesture.horizontal_cells);
            }
            gesture.last = cell;
        }
        self.mouse_gesture = Some(gesture);
    }

    fn open_popover(&mut self, popover: VcfPopover) {
        self.cancel_mouse_gesture();
        self.drawn_body = Rect::ZERO;
        self.popover = Some(popover);
        self.popover_bounds = None;
    }

    fn drawn_record_at(&self, cell: Position) -> Option<usize> {
        if !self.drawn_body.contains(cell) {
            return None;
        }
        self.drawn_rows.get(usize::from(cell.y - self.drawn_body.y)).copied()
    }

    /// Brings the viewport to the cursor, moving by the least that does so.
    ///
    /// Called after every cursor move rather than from `draw`, so the offset the
    /// chunk buffer is fed is already the one that will be drawn.
    fn follow_cursor(&mut self) {
        let (height, width) = self.viewport;
        self.scroll.vertical = follow_row(self.scroll.vertical, self.cursor.row, height);
        // How many columns fit depends on their widths, so the right edge is
        // found by walking rather than by arithmetic: advance the offset until
        // the window it produces reaches the cursor. Bounded by the column count,
        // and each step walks widths only — no row data is touched.
        self.scroll.horizontal = self.scroll.horizontal.min(self.cursor.column);
        while self.cursor.column >= self.scroll.horizontal + self.visible_columns(width) {
            self.scroll.horizontal += 1;
        }
    }

    /// Jumps to a zero-based record row and leads the viewport to it.
    fn goto_row(&mut self, row: usize) -> Result<(), String> {
        let total = self.total_rows();
        if total == 0 {
            return Err("no VCF loaded — :set activevcf <path>".to_string());
        }
        self.cursor.row = row.min(total - 1);
        self.scroll.vertical = self.cursor.row;
        self.ensure_visible();
        Ok(())
    }

    /// Jumps to a record, 1-based like the number the title bar shows. The
    /// argument is a bare integer, with no separators or unit suffix.
    ///
    /// `None` is the parser declining to read the argument as a row number: it
    /// held a suffix or a second axis, both meaningful on a bp axis and neither
    /// here. Report the shape wanted rather than guess at a conversion.
    fn goto_line(&mut self, line: Option<u64>) -> Result<(), String> {
        let Some(line) = line else {
            return Err(
                "goto takes a single record number here, optionally k/M-suffixed (e.g. /5000, /10k)".to_string(),
            );
        };
        self.goto_row(usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX))
    }

    /// Starts a nearest-variant lookup against the ready line index.
    ///
    /// The index is a prerequisite rather than a best-effort optimization: a
    /// command typed while it is building is rejected synchronously, so the user
    /// never gets a success acknowledgement for work that cannot yet start.
    fn start_locus_jump(&mut self, contig: String, position: BasePair) -> Result<(), String> {
        let Some(worker) = &self.io_worker else {
            let row = self
                .table
                .as_ref()
                .and_then(|table| {
                    table
                        .rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| row.contig == contig)
                        .min_by_key(|(_, row)| (row.position.as_u64().abs_diff(position.as_u64()), row.position))
                        .map(|(index, _)| index)
                })
                .ok_or_else(|| "That contig is not in the loaded VCF".to_string())?;
            return self.goto_row(row);
        };
        let Some(index) = self.line_index.clone() else {
            return Err(if self.index_build.is_some() {
                "VCF row index is still building".to_string()
            } else {
                "VCF row index is not ready".to_string()
            });
        };
        let Some(path) = self.path.clone() else {
            return Err("no VCF loaded — :set activevcf <path>".to_string());
        };

        // `Fifo`, not `Viewport`: scrolling doesn't make a deliberate locus request stale.
        self.locus_jump = Some((
            worker.submit(IoSchedule::Fifo, move || index.nearest_row(&path, &contig, position)),
            crate::Instant::now(),
        ));
        Ok(())
    }

    /// Polls the deliberate locus lookup. Unlike viewport work, a completed
    /// row remains the user's requested target even if scrolling happened while
    /// the worker was reading it.
    fn poll_locus_jump(&mut self) -> bool {
        let Some((handle, _)) = &mut self.locus_jump else {
            return false;
        };
        match handle.try_recv() {
            JobPoll::Empty => true,
            JobPoll::Stale => {
                self.locus_jump = None;
                false
            }
            JobPoll::Ready(Ok(Some(row))) => {
                self.locus_jump = None;
                if let Err(message) = self.goto_row(row) {
                    self.notify.error(message);
                }
                true
            }
            JobPoll::Ready(Ok(None)) => {
                self.locus_jump = None;
                // The lookup has no distance limit, so this never means "nothing
                // nearby": the contig itself is absent from the file.
                self.notify.warn("That contig is not in the loaded VCF");
                true
            }
            JobPoll::Ready(Err(error)) => {
                self.locus_jump = None;
                self.notify.error(format!("VCF locus lookup failed: {error}"));
                true
            }
            JobPoll::Disconnected => {
                self.locus_jump = None;
                self.notify.error("VCF locus lookup failed: the I/O worker stopped");
                true
            }
        }
    }
}

/// The background for a cell the cursor's row and column pass through, or `None`
/// for a cell they miss.
///
/// Three tiers, so one glance gives both axes and the exact cell: the crossing is
/// the selection, the two bands are the context that says where it sits.
fn cursor_background(on_row: bool, on_column: bool) -> Option<Color> {
    match (on_row, on_column) {
        (true, true) => Some(Selection::FOCUS_BG()),
        (true, false) | (false, true) => Some(Selection::CONTEXT_BG()),
        (false, false) => None,
    }
}

impl CommandTarget for VCFViewer {
    fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
        match outcome {
            CommandOutcome::Table(command) => self.apply_table(command),
            CommandOutcome::Goto { line, .. } => match self.goto_line(line) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            CommandOutcome::List(movement) => self.apply_list(movement),
            CommandOutcome::Scroll {
                direction,
                step: ScrollStep::Near,
            } => {
                self.move_cursor(direction, SCROLL_STEP);
                Applied::Ok
            }
            CommandOutcome::JumpToPeer => match self.cursor_row() {
                Some(row) if let Some(samples) = self.sample_lookup() => Applied::FollowUps(vec![
                    GenomeCommand::ChooseVariantBam {
                        contig: row.contig.clone(),
                        position: row.position,
                        samples,
                    }
                    .into(),
                ]),
                Some(row) => Applied::in_tab(
                    "GenomeViewer",
                    [GenomeCommand::JumpToVariant {
                        contig: row.contig.clone(),
                        position: row.position,
                        bam: None,
                    }
                    .into()],
                ),
                None => Applied::Ok,
            },
            // The table moves one cell per scroll and binds no coarser step.
            CommandOutcome::Scroll {
                step: ScrollStep::NearX10 | ScrollStep::Mid | ScrollStep::Far,
                ..
            }
            | CommandOutcome::Setting(_)
            | CommandOutcome::Shell(_)
            | CommandOutcome::Genome(_) => Applied::NotApplicable,
            // The router consumes this before dispatch; direct callers still get a silent no-op.
            CommandOutcome::None => Applied::Ok,
        }
    }
}

impl VCFViewer {
    fn apply_list(&mut self, movement: ListMove) -> Applied {
        let total = self.declared_columns();
        match (&mut self.popover, movement) {
            (None, _) => return Applied::NotApplicable,
            (Some(_), ListMove::Close) => self.popover = None,
            (Some(VcfPopover::Columns(picker)), ListMove::Up) => picker.up(),
            (Some(VcfPopover::Columns(picker)), ListMove::Down) => picker.down(total),
            (Some(VcfPopover::Columns(_)), ListMove::Select) => {}
            (Some(VcfPopover::VariantBams { switcher, .. }), ListMove::Up) => switcher.cursor.up(),
            (Some(VcfPopover::VariantBams { switcher, .. }), ListMove::Down) => switcher.down(),
            (
                Some(VcfPopover::VariantBams {
                    switcher,
                    contig,
                    position,
                    ..
                }),
                ListMove::Select,
            ) => {
                let Some(path) = switcher.selected() else {
                    return Applied::Ok;
                };
                let jump = GenomeCommand::JumpToVariant {
                    contig: std::mem::take(contig),
                    position: *position,
                    bam: Some(path.to_path_buf()),
                };
                self.popover = None;
                return Applied::in_tab("GenomeViewer", [jump.into()]);
            }
        }
        Applied::Ok
    }

    fn apply_table(&mut self, command: TableCommand) -> Applied {
        match command {
            TableCommand::ActiveVcf(path) => match self.set_active_vcf(&path) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            TableCommand::GotoLocus { contig, position } => match self.start_locus_jump(contig, position) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            TableCommand::OpenVariantBams {
                contig,
                position,
                samples,
                paths,
                active,
            } => {
                let Some(worker) = &self.io_worker else {
                    return Applied::Err("Looking up BAM samples is unavailable in this demo".to_string());
                };
                let switcher = BamSwitcher::for_variant(paths, worker, samples);
                self.open_popover(VcfPopover::VariantBams {
                    switcher,
                    active,
                    contig,
                    position,
                });
                Applied::Ok
            }
            TableCommand::Page { up } => {
                self.page(up);
                Applied::Ok
            }
            TableCommand::ColumnPage { left } => {
                let direction = if left { Direction::Left } else { Direction::Right };
                self.move_cursor(direction, COLUMN_PAGE_STEP);
                Applied::Ok
            }
            TableCommand::OpenColumnPicker => {
                self.open_popover(VcfPopover::Columns(ListCursor::default()));
                Applied::Ok
            }
            TableCommand::ToggleColumn => {
                let Some(VcfPopover::Columns(picker)) = &self.popover else {
                    return Applied::NotApplicable;
                };
                if let Err(refusal) = self.columns.toggle(picker.selected()) {
                    self.notify.warn(refusal);
                }
                self.clamp_horizontal();
                Applied::Ok
            }
            TableCommand::ShowAllColumns => {
                if !matches!(self.popover, Some(VcfPopover::Columns(_))) {
                    return Applied::NotApplicable;
                }
                self.columns.show_all();
                self.clamp_horizontal();
                Applied::Ok
            }
            TableCommand::ShowOnlyColumn => {
                let Some(VcfPopover::Columns(picker)) = &self.popover else {
                    return Applied::NotApplicable;
                };
                self.columns.show_only(picker.selected());
                self.clamp_horizontal();
                Applied::Ok
            }
        }
    }
}

impl StatusProvider for VCFViewer {
    fn status_override(&self) -> Option<String> {
        Some(self.file_summary())
    }

    fn drain_notifications(&mut self) -> Vec<Notification> {
        self.notify.drain()
    }

    /// Reports each outstanding VCF work category to the shell's status row, like any other job.
    fn background_work(&self) -> Option<BackgroundWork> {
        let mut tasks = Vec::new();
        if self.index_build.is_some() {
            tasks.push("Building VCF row index".to_string());
        }
        if self.locus_jump.is_some() {
            tasks.push("Looking up VCF locus".to_string());
        }
        if self.chunks.is_fetching() {
            tasks.push("Reading VCF rows".to_string());
        }
        (!tasks.is_empty()).then(|| BackgroundWork {
            tasks,
            started_at: self.work_started_at().unwrap_or_else(crate::Instant::now),
        })
    }
}

impl ViewerApp for VCFViewer {
    fn title(&self) -> &'static str {
        "VCFViewer"
    }

    fn display_name(&self) -> &'static str {
        "VCF"
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, keymap: &Keymap) {
        // One row goes to the border, one to the header.
        let height = usize::from(area.height.saturating_sub(2));
        // Recorded for `follow_cursor`, which runs between draws where no `Rect`
        // is in reach. A resize therefore takes effect on the next move rather
        // than immediately, which is what keeps the cursor from jumping under a
        // user who only dragged a window edge.
        self.viewport = (height, area.width);
        self.ensure_visible();

        // Two titles on the one border row. The line number goes left because
        // right-aligned titles render last: on a terminal too narrow for both,
        // the filename eats into the number rather than the changing value
        // being the one that disappears.
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Chrome::DIVIDER_FG()))
            .style(Style::default().fg(Text::PRIMARY_FG()).bg(Surface::PANEL_BG()))
            .title_top(Line::from(self.line_indicator()))
            .title_top(Line::from(self.file_summary()).right_aligned());
        // The visible window is computed first and everything below is cut to it,
        // so a file declaring hundreds of INFO columns costs no more to draw than
        // one declaring a handful.
        let visible = self.visible_columns(area.width);
        let rows = self.display_rows(height, visible);
        let body = if self.popover.is_none() {
            Rect::new(area.x, area.y.saturating_add(2), area.width, height as u16)
        } else {
            Rect::ZERO
        };
        if self.drawn_body != body {
            self.cancel_mouse_gesture();
            self.drawn_body = body;
        }
        self.drawn_rows.clear();
        if visible > 0 {
            self.drawn_rows.extend(rows.iter().map_while(|row| match row {
                DisplayRow::Variant { index, .. } => Some(*index),
                DisplayRow::Marker(_) => None,
            }));
        }

        // Headings, widths, and cells all come from the one window, so the
        // horizontal offset and the filter are each applied exactly once.
        let window = self.visible_window(visible);
        let headings = window.iter().map(|column| match &self.table {
            Some(table) => table.heading(*column).to_string(),
            None => FIXED_COLUMNS.get(*column).copied().unwrap_or_default().to_string(),
        });
        // Styled on the row rather than per heading cell so the ground also covers the padding
        // between columns — a per-cell background would leave the gaps showing the body's.
        let header = Row::new(headings.map(Cell::from)).style(Style::default().bg(Surface::LABEL_BG()));
        let widths: Vec<Constraint> = window
            .iter()
            .map(|column| Constraint::Length(self.declared_width(*column)))
            .collect();
        // The cursor's column as a position in the drawn window, so the two loops
        // below compare like with like. `None` once it has scrolled off.
        let cursor_column = self.cursor.column.checked_sub(self.scroll.horizontal);
        let body = rows.iter().map(|row| match row {
            // Already cut to the window by `display_rows`; skipping again here
            // would advance the data twice per column the header advances once.
            DisplayRow::Variant { index, cells } => {
                let on_cursor_row = *index == self.cursor.row;
                Row::new(cells.iter().enumerate().map(|(column, text)| {
                    let cell = Cell::from(text.clone());
                    match cursor_background(on_cursor_row, cursor_column == Some(column)) {
                        Some(bg) => cell.style(Style::default().bg(bg)),
                        None => cell,
                    }
                }))
            }
            // Spans the table by living in the first cell; the rest stay empty so
            // no column is reserved for it (invariant 9). Unstyled: it stands for
            // no record, so it cannot be selected.
            DisplayRow::Marker(text) => Row::new([Cell::from(*text)]),
        });

        // The base style carries the foreground and ground for every cell. Set on the widget rather
        // than per cell so the cursor styling above stays a background-only override and the
        // header's own ground a row-level one — none of the three compete for the same channel.
        frame.render_widget(
            Table::new(body, widths)
                .style(Style::default().fg(Text::PRIMARY_FG()).bg(Surface::PANEL_BG()))
                .header(header)
                .block(block),
            area,
        );

        // Drawn last, over the table, reserving none of its width (invariant 9).
        let entries = matches!(self.popover, Some(VcfPopover::Columns(_))).then(|| self.column_entries());
        self.popover_bounds = match &mut self.popover {
            Some(VcfPopover::Columns(cursor)) => {
                column_picker::draw(frame, area, cursor, entries.as_deref().unwrap_or_default(), keymap)
            }
            Some(VcfPopover::VariantBams { switcher, active, .. }) => {
                switcher.draw(frame, area, keymap, active.as_deref())
            }
            None => None,
        };
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Option<CommandOutcome> {
        let cell = mouse.position();
        if self.popover.is_some() {
            self.cancel_mouse_gesture();
            return (mouse.kind == MouseKind::LeftDown
                && self.popover_bounds.is_none_or(|bounds| !bounds.contains(cell)))
            .then_some(CommandOutcome::List(ListMove::Close));
        }
        match mouse.kind {
            MouseKind::LeftDown => {
                self.mouse_gesture = self.drawn_body.contains(cell).then(|| TableGesture {
                    last: cell,
                    record: self.drawn_record_at(cell),
                    horizontal_cells: 0,
                });
            }
            MouseKind::LeftDrag => self.drag_table(cell),
            MouseKind::LeftUp => {
                if let Some(gesture) = self.mouse_gesture.take()
                    && cell == gesture.last
                    && let Some(record) = gesture.record
                    && self.drawn_record_at(cell) == Some(record)
                {
                    self.cursor.row = record;
                    return Some(CommandOutcome::JumpToPeer);
                }
            }
            MouseKind::Moved => {}
        }
        None
    }

    fn cancel_mouse_gesture(&mut self) {
        self.mouse_gesture = None;
    }

    fn scopes(&self) -> Vec<Scope> {
        match self.popover {
            Some(VcfPopover::Columns(_)) => vec![Scope::ColumnPicker, Scope::Vcf],
            Some(VcfPopover::VariantBams { .. }) => vec![Scope::BamSwitcher, Scope::Vcf],
            None => vec![Scope::Vcf],
        }
    }

    /// Swaps the preview's rows for indexed ones once the scan lands, then keeps
    /// the chunk fetches moving. Stays truthy while a fetch is in flight so the
    /// shell keeps redrawing until the rows arrive.
    fn tick(&mut self) -> bool {
        if let Some(error) = self.chunks.poll() {
            self.notify.error(error);
        }
        let locating = self.poll_locus_jump();
        let mut fetching = self.chunks.is_fetching() || locating;
        if let Some(VcfPopover::VariantBams { switcher, active, .. }) = &mut self.popover {
            match switcher.tick(active.as_deref()) {
                Ok(checking) => fetching |= checking,
                Err(message) => {
                    self.notify.error(message);
                    self.popover = None;
                }
            }
        }
        if self.notify.has_pending() {
            fetching = true;
        }

        let Some((handle, _)) = &mut self.index_build else {
            return fetching;
        };
        match handle.try_recv() {
            JobPoll::Empty => true,
            JobPoll::Stale => {
                self.index_build = None;
                true
            }
            JobPoll::Ready(Ok(index)) => {
                self.index_build = None;
                self.swap_in_indexed_rows(index);
                // Tops the seeded partial chunk up to a full one; the seeded rows
                // keep the table populated while that lands.
                self.ensure_visible();
                fetching || self.chunks.is_fetching() || self.locus_jump.is_some()
            }
            JobPoll::Ready(Err(err)) => {
                self.index_build = None;
                self.notify.error(format!("VCF loaded, but indexing it failed: {err}"));
                true
            }
            JobPoll::Disconnected => {
                self.index_build = None;
                self.notify.error("VCF loaded, but the I/O worker stopped");
                true
            }
        }
    }

    fn has_pending_work(&self) -> bool {
        self.index_build.is_some()
            || self.locus_jump.is_some()
            || self.chunks.is_fetching()
            || matches!(&self.popover, Some(VcfPopover::VariantBams { switcher, .. }) if switcher.is_checking())
    }
}

#[cfg(test)]
mod tests;
