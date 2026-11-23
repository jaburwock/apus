//! Tests for `VCFViewer` in `vcf/mod.rs`, split out as a child module so they keep private access.
//!
//! The siblings with their own test modules — `ingest`, `line_index`, `chunks`,
//! `column_picker` — are tested there; what lands here is the viewer itself: table geometry,
//! column filtering, the locus jump, and the trait impls.

use super::*;
use crate::{basepair::BasePair, command::protocol::ShellCommand};

const FIXTURE: &str = "testdata/vcf/sample.vcf";
const FIXTURE_GZ: &str = "testdata/vcf/sample.vcf.gz";
const MULTISAMPLE_FIXTURE: &str = "testdata/hg002/multisample/hg002-multisample.vcf.gz";
const RAW_INFO_FIXTURE: &str = "testdata/vcf/raw_info.vcf";
const RAW_INFO_BLOB: &str = "DP=10;AF=0.5";

/// Chunk size for viewer tests. The fixture holds 5 records, so this splits
/// it across three chunks and makes every boundary crossing a real one.
const TEST_CHUNK: usize = 2;

/// A viewport tall enough to show the whole fixture at once.
const TALL: usize = 32;

fn viewer() -> VCFViewer {
    VCFViewer::new(IoWorkerHandle::spawn())
}

#[test]
fn an_outcome_for_another_viewer_is_not_applicable() {
    assert_eq!(
        viewer().apply_outcome(GenomeCommand::ActiveFasta(PathBuf::from("ignored.fa")).into()),
        Applied::NotApplicable
    );
}

/// A bp-only goto is the table's to refuse, not to pass on: `Err` rather than `NotApplicable`, so
/// the user is told why instead of the outcome looking like it belongs to some other tab.
#[test]
fn a_goto_the_table_owns_but_cannot_read_keeps_its_own_error() {
    let mut viewer = indexed(FIXTURE);
    let applied = viewer.apply_outcome(CommandOutcome::Goto {
        line: None,
        horizontal: Some(BasePair(100)),
        vertical: None,
    });

    assert!(matches!(applied, Applied::Err(_)), "got {applied:?}");
}

fn load(path: &str) -> VCFViewer {
    let mut viewer = viewer();
    viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from(path)).into())
        .into_target_result()
        .expect("the fixture must load");
    viewer
}

/// Ticks until nothing is in flight, as `App` would each frame.
fn settle(viewer: &mut VCFViewer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while viewer.index_build.is_some() || viewer.chunks.is_fetching() {
        assert!(std::time::Instant::now() < deadline, "background work did not finish");
        viewer.tick();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Ticks the nearest-locus job until the command has been consumed.
fn settle_locus(viewer: &mut VCFViewer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while viewer.locus_jump.is_some() {
        assert!(std::time::Instant::now() < deadline, "the locus lookup did not finish");
        viewer.tick();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// A loaded, fully indexed viewer whose chunks are small enough to page.
fn indexed(path: &str) -> VCFViewer {
    let mut viewer = viewer();
    viewer.chunks = ChunkBuffer::with_chunk_rows(TEST_CHUNK);
    viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from(path)).into())
        .into_target_result()
        .expect("the fixture must load");
    settle(&mut viewer);
    assert!(viewer.line_index.is_some(), "{path} must end up indexed");
    viewer
}

/// Every column the table has, for tests about rows rather than columns.
fn all_columns(viewer: &VCFViewer) -> usize {
    viewer.total_columns()
}

/// Every declared column index of a table read outside the viewer — the
/// oracle side of a comparison, built from the table itself rather than
/// borrowed from the viewer under test.
fn every_column(table: &VcfTable) -> Vec<usize> {
    (0..table.total_columns()).collect()
}

/// The lines currently displayed, across every column.
fn display_rows(viewer: &VCFViewer, height: usize) -> Vec<DisplayRow> {
    viewer.display_rows(height, all_columns(viewer))
}

/// The text of the one record the viewer has queued, or `None` if it queued nothing.
/// Panics on more than one, so a test asserting silence cannot pass by looking at the
/// wrong record.
fn drained_text(viewer: &mut VCFViewer) -> Option<String> {
    let mut drained = viewer.drain_notifications();
    assert!(drained.len() <= 1, "expected at most one record, got {drained:?}");
    drained.pop().map(|record| record.text)
}

/// The variant cells currently displayed, ignoring loading and marker lines.
fn variants(viewer: &VCFViewer, height: usize) -> Vec<Vec<String>> {
    display_rows(viewer, height)
        .into_iter()
        .filter_map(|row| match row {
            DisplayRow::Variant { cells, .. } => Some(cells),
            _ => None,
        })
        .collect()
}

#[test]
fn an_unreadable_vcf_reports_an_error_rather_than_replacing_the_table() {
    let mut viewer = load(FIXTURE);
    let loaded = display_rows(&viewer, TALL).len();

    let err = viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from("testdata/vcf/nope.vcf")).into())
        .into_target_result()
        .unwrap_err();

    assert!(err.starts_with("Failed to read VCF"), "got {err:?}");
    assert_eq!(
        display_rows(&viewer, TALL).len(),
        loaded,
        "a failed load must leave the previous table in place"
    );
}

#[test]
fn the_preview_marker_terminates_a_truncated_table_until_the_index_replaces_it() {
    let mut viewer = viewer();
    viewer.table = Some(ingest::read_preview(Path::new(FIXTURE), 2).unwrap());
    let rows = display_rows(&viewer, TALL);
    assert_eq!(rows.len(), 3, "two records plus the marker");
    assert_eq!(rows.last(), Some(&DisplayRow::Marker(PREVIEW_END_MARKER)));

    // Once rows continue past the preview the marker would be a lie.
    let indexed = indexed(FIXTURE);
    assert!(
        !display_rows(&indexed, TALL).contains(&DisplayRow::Marker(PREVIEW_END_MARKER)),
        "an indexed table scrolls past the preview, so nothing marks its end"
    );
}

#[test]
fn every_row_in_the_file_is_reachable_and_matches_a_sequential_read() {
    for path in [FIXTURE, FIXTURE_GZ, MULTISAMPLE_FIXTURE] {
        // The oracle carries its own columns, measured by the sequential read
        // rather than borrowed from the viewer under test.
        let sequential = ingest::read_preview(Path::new(path), usize::MAX).unwrap();
        let mut viewer = indexed(path);
        assert_eq!(
            viewer.total_rows(),
            sequential.rows.len(),
            "{path} must span the whole file"
        );

        for (row, want) in sequential.rows.iter().enumerate() {
            viewer.scroll.vertical = row;
            viewer.ensure_visible();
            settle(&mut viewer);
            assert_eq!(
                variants(&viewer, 1).first(),
                Some(&want.cells(&every_column(&sequential), &sequential)),
                "{path} row {row} must read back as the sequential read has it"
            );
        }
    }
}

#[test]
fn scrolling_spans_the_whole_file_once_indexed_and_only_the_preview_before() {
    // A preview deliberately shorter than the file, so the two possible clamp
    // sources give different answers and the test can tell them apart.
    const PREVIEW: usize = 2;
    let full = ingest::read_preview(Path::new(FIXTURE), usize::MAX).unwrap().rows.len();
    assert!(
        full > PREVIEW,
        "the fixture must outrun the preview for this to mean anything"
    );

    let mut viewer = viewer();
    viewer.table = Some(ingest::read_preview(Path::new(FIXTURE), PREVIEW).unwrap());
    for _ in 0..50 {
        viewer.move_cursor(Direction::Down, SCROLL_STEP);
    }
    assert_eq!(
        viewer.cursor.row,
        PREVIEW - 1,
        "unindexed, the preview's last row is the floor"
    );

    // Same short preview, now indexed: the clamp must follow the file.
    viewer.chunks = ChunkBuffer::with_chunk_rows(TEST_CHUNK);
    viewer.path = Some(PathBuf::from(FIXTURE));
    viewer.line_index = Some(Arc::new(line_index::build(Path::new(FIXTURE)).unwrap()));
    for _ in 0..50 {
        viewer.move_cursor(Direction::Down, SCROLL_STEP);
    }
    assert_eq!(
        viewer.cursor.row,
        full - 1,
        "indexed, the clamp follows the record count, not the preview's length"
    );
}

/// A missing chunk draws nothing rather than a placeholder. The shell reports
/// the fetch through [`BackgroundWork`]; the table's job is to show the rows
/// it has, never to invent a line for one it does not.
#[test]
fn display_rows_include_only_resident_rows() {
    let mut viewer = indexed(FIXTURE);
    viewer.chunks.clear();
    assert!(
        display_rows(&viewer, TALL).is_empty(),
        "a wholly missing span contributes no rows"
    );

    // Fetch only the first chunk, leaving the rows past it missing.
    viewer.ensure_visible();
    settle(&mut viewer);

    let rows = display_rows(&viewer, TALL);
    assert_eq!(
        rows.len(),
        TEST_CHUNK,
        "the resident chunk's rows must still be drawn, and the gap past them adds nothing"
    );
    assert!(
        rows.iter().all(|row| matches!(row, DisplayRow::Variant { .. })),
        "every line drawn stands for a row that actually arrived"
    );
}

#[test]
fn the_cursor_clamps_at_both_ends_rather_than_wrapping() {
    for (forward, back) in [(Direction::Right, Direction::Left), (Direction::Down, Direction::Up)] {
        let mut viewer = indexed(FIXTURE);
        let horizontal = forward == Direction::Right;
        // The horizontal limit spans the INFO columns too, so it is asked for
        // rather than assumed to be the fixed fields' width.
        let limit = if horizontal {
            viewer.total_columns() - 1
        } else {
            viewer.total_rows() - 1
        };
        let axis = |viewer: &VCFViewer| {
            if horizontal {
                viewer.cursor.column
            } else {
                viewer.cursor.row
            }
        };

        for _ in 0..50 {
            viewer.move_cursor(forward, SCROLL_STEP);
        }
        assert_eq!(axis(&viewer), limit, "{forward:?} must stop at the last index");

        for _ in 0..50 {
            viewer.move_cursor(back, SCROLL_STEP);
        }
        assert_eq!(axis(&viewer), 0, "{back:?} must stop at the first index");
    }
}

#[test]
fn paging_clamps_at_both_ends_rather_than_wrapping() {
    let mut viewer = tall_table(PAGE_STEP * 3);
    let last = viewer.total_rows() - 1;

    for _ in 0..50 {
        assert_eq!(
            viewer.apply_outcome(TableCommand::Page { up: false }.into()),
            Applied::Ok
        );
    }
    assert_eq!(viewer.cursor.row, last, "paging down must stop at the last record");

    for _ in 0..50 {
        assert_eq!(
            viewer.apply_outcome(TableCommand::Page { up: true }.into()),
            Applied::Ok
        );
    }
    assert_eq!(viewer.cursor.row, 0, "and paging up at the first");
    assert_eq!(viewer.scroll.vertical, 0, "with the viewport back at the top");
}

#[test]
fn dragging_from_an_indexed_gap_moves_the_table_and_fetches_the_new_rows() {
    use ratatui::{Terminal, backend::TestBackend};

    let _palette = crate::style::palette::lock_for_test();
    let mut viewer = indexed(FIXTURE);
    viewer.columns.toggle(1).unwrap();
    viewer.chunks.clear();
    let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
    let keys = Keymap::defaults();
    let mut draw = |viewer: &mut VCFViewer| {
        terminal.draw(|frame| viewer.draw(frame, frame.area(), &keys)).unwrap();
    };
    let mouse = |viewer: &mut VCFViewer, kind, column, row| viewer.on_mouse(MouseEvent { kind, column, row });
    draw(&mut viewer);
    assert!(viewer.drawn_rows.is_empty());

    assert_eq!(mouse(&mut viewer, MouseKind::LeftDown, 24, 6), None);
    assert_eq!(mouse(&mut viewer, MouseKind::LeftUp, 24, 6), None);
    mouse(&mut viewer, MouseKind::LeftDown, 24, 6);
    mouse(&mut viewer, MouseKind::LeftDrag, 18, 5);
    assert_eq!((viewer.scroll.horizontal, viewer.scroll.vertical), (0, 1));
    mouse(&mut viewer, MouseKind::LeftDrag, 13, 4);
    assert_eq!((viewer.scroll.horizontal, viewer.scroll.vertical), (1, 2));
    assert_eq!(viewer.visible_window(1), [2], "panning skips the hidden column");
    mouse(&mut viewer, MouseKind::LeftDrag, 0, 0);
    let last = viewer.total_rows() - 1;
    assert_eq!((viewer.scroll.vertical, viewer.cursor.row), (last, last));
    assert_eq!(mouse(&mut viewer, MouseKind::LeftUp, 0, 0), None);

    settle(&mut viewer);
    draw(&mut viewer);
    mouse(&mut viewer, MouseKind::LeftDown, 2, 2);
    assert_eq!(
        mouse(&mut viewer, MouseKind::LeftUp, 2, 2),
        Some(CommandOutcome::JumpToPeer)
    );
    assert_eq!(viewer.cursor.row, last);
}

/// A viewer whose table carries at least `rows` records, for tests that need
/// more than the fixture's handful. Built by repeating the fixture's own rows
/// rather than inventing records.
fn tall_table(rows: usize) -> VCFViewer {
    let mut viewer = viewer();
    let mut table = ingest::read_preview(Path::new(FIXTURE), usize::MAX).unwrap();
    while table.rows.len() < rows {
        table.rows.extend_from_within(..);
    }
    // Truncated is what the preview marker keys off; these rows stand for the
    // whole table, so nothing follows them.
    table.truncated = false;
    viewer.table = Some(table);
    viewer
}

/// The property `u`/`d` must keep: the viewport moves and the selection rides
/// along, so the highlight stays on the screen line it was already on.
#[test]
fn paging_keeps_the_cursor_on_the_same_screen_line() {
    const HEIGHT: usize = 10;
    let mut viewer = tall_table(PAGE_STEP * 4);
    viewer.viewport = (HEIGHT, 200);
    // Somewhere other than the top edge, or a cursor pinned there would pass
    // without riding along at all.
    for _ in 0..4 {
        viewer.move_cursor(Direction::Down, SCROLL_STEP);
    }
    let line = viewer.cursor.row - viewer.scroll.vertical;
    assert!(line > 0, "the cursor must start off the top edge for this to bite");

    for _ in 0..3 {
        assert_eq!(
            viewer.apply_outcome(TableCommand::Page { up: false }.into()),
            Applied::Ok
        );
        assert_eq!(
            viewer.cursor.row - viewer.scroll.vertical,
            line,
            "paging down must carry the cursor, not slide it down the frame"
        );
    }
    for _ in 0..3 {
        assert_eq!(
            viewer.apply_outcome(TableCommand::Page { up: true }.into()),
            Applied::Ok
        );
        assert_eq!(
            viewer.cursor.row - viewer.scroll.vertical,
            line,
            "and paging back up must too"
        );
    }
}

/// The end of the file is where a page moves less than it was asked for. The
/// cursor must travel the distance the viewport actually did, or it slides
/// down the frame exactly when the rows stop arriving to explain it.
#[test]
fn a_page_clamped_at_the_end_never_moves_the_cursor_down_in_frame() {
    const HEIGHT: usize = 10;
    // Deliberately not a whole number of pages, so the last one is short.
    let mut viewer = tall_table(PAGE_STEP * 2 + PAGE_STEP / 2);
    viewer.viewport = (HEIGHT, 200);
    for _ in 0..4 {
        viewer.move_cursor(Direction::Down, SCROLL_STEP);
    }
    let line = viewer.cursor.row - viewer.scroll.vertical;

    for _ in 0..5 {
        assert_eq!(
            viewer.apply_outcome(TableCommand::Page { up: false }.into()),
            Applied::Ok
        );
    }

    assert_eq!(
        viewer.cursor.row,
        viewer.total_rows() - 1,
        "paging to the end must reach the last record"
    );
    assert!(viewer.cursor.row - viewer.scroll.vertical <= line);
}

#[test]
fn loading_a_second_vcf_resets_the_cursor_and_drops_the_old_files_rows() {
    let mut viewer = indexed(FIXTURE);
    viewer.move_cursor(Direction::Down, SCROLL_STEP);
    viewer.move_cursor(Direction::Right, SCROLL_STEP);
    settle(&mut viewer);

    viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from(FIXTURE)).into())
        .into_target_result()
        .unwrap();

    assert_eq!(viewer.scroll.vertical, 0);
    assert_eq!(viewer.scroll.horizontal, 0);
    assert_eq!(viewer.cursor.row, 0, "a second file's rows are different rows");
    assert_eq!(viewer.cursor.column, 0, "and its columns different columns");
    assert!(viewer.line_index.is_none(), "the old file's index must not outlive it");
    assert!(
        viewer.chunks.row(0).is_none(),
        "nor may its rows, which would be another file's data"
    );
}

#[test]
fn the_indexed_rows_that_replace_the_preview_are_the_rows_it_showed() {
    for path in [FIXTURE, FIXTURE_GZ] {
        let mut viewer = load(path);
        let previewed = display_rows(&viewer, TALL);

        settle(&mut viewer);

        assert!(viewer.line_index.is_some(), "{path} must end up indexed");
        assert_eq!(display_rows(&viewer, TALL), previewed, "{path} swap must be invisible");
        assert_eq!(
            drained_text(&mut viewer),
            None,
            "an invisible swap has nothing to report"
        );
    }
}

/// The seam the seed exists to close: not where the swap ends up, but every
/// frame in between. `settle` would hide this by running the top-up fetch to
/// completion before looking, which is why this ticks one frame at a time.
#[test]
fn the_table_never_blanks_while_the_index_lands() {
    let mut viewer = load(FIXTURE);
    let previewed = variants(&viewer, TALL);
    assert!(!previewed.is_empty(), "the preview must be showing rows to begin with");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while viewer.index_build.is_some() || viewer.chunks.is_fetching() {
        assert!(std::time::Instant::now() < deadline, "background work did not finish");
        viewer.tick();

        assert!(
            !display_rows(&viewer, TALL).is_empty(),
            "the handover must not blank rows that were already on screen"
        );
        assert_eq!(
            variants(&viewer, TALL),
            previewed,
            "nor may the rows themselves change mid-swap"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn an_unindexable_vcf_keeps_the_preview_and_says_so() {
    let mut viewer = load(FIXTURE);
    let previewed = display_rows(&viewer, TALL);
    // Point at a file the scan cannot open, leaving the loaded table alone.
    viewer.index_build = Some((
        viewer.io_worker.as_ref().unwrap().submit(IoSchedule::Fifo, || {
            line_index::build(Path::new("testdata/vcf/nope.vcf"))
        }),
        std::time::Instant::now(),
    ));

    settle(&mut viewer);

    assert_eq!(
        display_rows(&viewer, TALL),
        previewed,
        "a failed scan must not disturb the table"
    );
    assert!(viewer.line_index.is_none());
    assert!(
        drained_text(&mut viewer).is_some_and(|text| text.contains("indexing it failed")),
        "a failed scan must be reported"
    );
}

#[test]
fn an_in_flight_chunk_is_reported_as_one_task_until_it_lands() {
    let mut viewer = indexed(FIXTURE);
    viewer.chunks.clear();
    viewer.ensure_visible();

    assert!(viewer.chunks.is_fetching(), "clearing then ensuring must refetch");
    let fetching = viewer
        .background_work()
        .expect("an in-flight fetch belongs on the shell");
    assert_eq!(
        fetching.tasks.len(),
        1,
        "a lone fetch reports one line, not one per phase: {:?}",
        fetching.tasks
    );
    settle(&mut viewer);
    assert!(
        viewer.background_work().is_none(),
        "and nothing is reported once it lands"
    );
}

/// The shell redraws while a job is outstanding, and stops once it is not.
///
/// Driven by a handle whose sender is held here and never fired, rather than by a
/// real fetch: a job served by the I/O worker can land before the assertion runs,
/// which made the earlier version of this claim fail on roughly five runs in eight.
/// A job that cannot complete early states the same rule with no clock in it.
#[test]
fn an_outstanding_job_keeps_the_frame_loop_awake() {
    let mut viewer = indexed(FIXTURE);
    settle(&mut viewer);
    assert!(
        !viewer.tick(),
        "an idle viewer must let the frame loop rest, or the test below proves nothing"
    );

    let (sender, handle) = JobHandle::channel();
    viewer.index_build = Some((handle, std::time::Instant::now()));

    assert!(viewer.tick(), "an outstanding scan must keep the shell redrawing");
    assert!(viewer.background_work().is_some(), "and must name itself while it runs");

    // A vanished worker reports itself, and a pending notification is its own
    // reason to redraw — so drain that before asking whether the loop may rest.
    drop(sender);
    viewer.tick();
    assert!(
        drained_text(&mut viewer).is_some(),
        "a vanished worker must be reported"
    );
    assert!(
        !viewer.tick(),
        "a scan whose worker vanished must not leave the shell spinning forever"
    );
}

/// One clock across both phases. Restarting it at the handover would make the
/// shell's progress glyph jump back to its first frame just as the scan gives
/// way to a fetch.
#[test]
fn the_reported_clock_runs_from_the_scan_through_the_chunk_fetch() {
    let mut viewer = load(FIXTURE);
    let scanning = viewer.work_started_at().expect("the scan must start the clock");
    let scan_tasks = viewer
        .background_work()
        .expect("the scan belongs on the shell too")
        .tasks;
    assert!(!scan_tasks.is_empty(), "the index scan must name itself while it runs");

    settle(&mut viewer);
    assert_eq!(
        viewer.work_started_at(),
        None,
        "with nothing outstanding there is no phase"
    );

    // A fetch after the scan starts its own clock; what must not happen is the
    // scan's clock being read once the scan is over.
    viewer.chunks.clear();
    viewer.ensure_visible();
    assert!(
        viewer.work_started_at().is_some_and(|started| started > scanning),
        "a later fetch must time from itself, not from the finished scan"
    );
}

#[test]
fn an_ordinary_vcf_loads_without_a_size_warning() {
    let mut viewer = load(FIXTURE);
    assert_eq!(
        drained_text(&mut viewer),
        None,
        "the fixture's rows are well under budget, so nothing should be said"
    );
}

#[test]
fn oversized_rows_warn_but_still_load() {
    let mut viewer = load(FIXTURE);
    let rows = display_rows(&viewer, TALL).len();

    viewer.warn_if_rows_are_oversized(ROW_BYTES_BUDGET + 1);

    assert!(
        drained_text(&mut viewer).is_some_and(|text| text.contains("Wide rows")),
        "a row over budget must be reported"
    );
    assert_eq!(
        display_rows(&viewer, TALL).len(),
        rows,
        "the warning must not cost the user their table"
    );
}

#[test]
fn a_row_exactly_at_the_budget_is_not_flagged() {
    let mut viewer = load(FIXTURE);
    viewer.warn_if_rows_are_oversized(ROW_BYTES_BUDGET);
    assert_eq!(
        drained_text(&mut viewer),
        None,
        "the budget is a ceiling, not an exclusive bound"
    );
}

/// A viewer whose table declares `count` INFO columns, for the width tests.
/// Built directly because no fixture declares hundreds of keys.
fn with_info_columns(count: usize) -> VCFViewer {
    let mut viewer = load(FIXTURE);
    let table = viewer.table.as_mut().unwrap();
    table.info_columns = (0..count)
        .map(|n| ingest::InfoColumn {
            id: format!("K{n}"),
            width: 8,
        })
        .collect();
    // The filter is per-file and normally built in `set_active_vcf`; this
    // helper widens the table behind it, so it is rebuilt to match.
    viewer.columns = ColumnFilter::all_shown(viewer.declared_columns());
    viewer
}

/// **The invariant the virtual window exists for.** What is drawn must be a
/// function of the pane's width, not of how many columns the file declares —
/// otherwise a densely annotated VCF hands hundreds of constraints to
/// ratatui's layout solver, and builds hundreds of cells per row, to draw the
/// handful that fit.
/// Cells must line up with the headings drawn above them; a row carrying a
/// different count than the header would shift every value one column over.
#[test]
fn every_drawn_row_carries_exactly_the_visible_columns_cells() {
    let mut viewer = with_info_columns(40);
    for offset in [0, 1, 7, 20, 46] {
        viewer.scroll.horizontal = offset;
        let visible = viewer.visible_columns(80);
        // Deliberately not `variants`, which asks for every column: the point
        // here is what a real draw at this width produces.
        for row in viewer.display_rows(TALL, visible) {
            if let DisplayRow::Variant { cells, .. } = row {
                assert_eq!(cells.len(), visible, "at column offset {offset}");
            }
        }
    }
}

/// The heading and the value under it must name the same column, whatever the
/// horizontal offset — the row-wise reading of invariant 9.
#[test]
fn a_scrolled_column_keeps_its_heading_with_its_values() {
    let viewer = load(FIXTURE);
    let table = viewer.table.as_ref().unwrap();
    let first_info = FIXED_COLUMNS.len();

    // Read the fixture's first record directly: DP=30 is under heading DP.
    let dp = table.info_columns.iter().position(|c| c.id == "DP").unwrap() + first_info;
    assert_eq!(table.heading(dp), "DP");

    let cells = table.rows[0].cells(&[dp], table);
    assert_eq!(
        cells[0], "30",
        "the cell at a column offset must be that column's value"
    );
}

/// The drawn frame's lines, for assertions about what actually reaches the
/// screen rather than about what `display_rows` returned.
fn rendered(viewer: &mut VCFViewer, width: u16, height: u16) -> Vec<String> {
    use ratatui::{Terminal, backend::TestBackend};

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| viewer.draw(frame, frame.area(), &Keymap::defaults()))
        .expect("draw must succeed");
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn sample_counts_render_between_fixed_fields_and_annotations() {
    let mut viewer = load(MULTISAMPLE_FIXTURE);
    let frame = rendered(&mut viewer, 120, 5);
    let header = &frame[1];
    let filter = header.find("FILTER").unwrap();
    let samples = header.find("Samples").unwrap();
    let svtype = header.find("SVTYPE").unwrap();
    let svlen = header.find("SVLEN").unwrap();
    assert!(filter < samples && samples < svtype && svtype < svlen);
    for (row, (kind, len, count)) in
        frame[2..]
            .iter()
            .zip([("DEL", "2477", "1"), ("INS", "975", "1"), ("INS", "55", "2")])
    {
        assert_eq!(row[samples..svtype].trim(), count);
        assert_eq!(row[svtype..svlen].trim(), kind);
        assert_eq!(row[svlen..].trim(), len);
    }
}

/// The fallback heading and its value are assembled separately: `heading`
/// maps the synthetic column while `cells` extracts its raw blob. Assert on
/// the frame so a mismatch between those two paths cannot leave a blank
/// `INFO` column undetected.
#[test]
fn the_raw_info_fallback_reaches_the_rendered_cell_under_its_heading() {
    let mut viewer = load(RAW_INFO_FIXTURE);
    let frame = rendered(&mut viewer, 120, 4);

    assert!(
        frame[1].contains("INFO"),
        "the fallback heading must be drawn, got {:?}",
        frame[1]
    );
    assert!(
        frame[2].contains(RAW_INFO_BLOB),
        "the raw blob must be drawn under that heading, got {:?}",
        frame[2]
    );
}

/// The viewport is a follower: it must stay put while the cursor has room, and
/// move by exactly the overshoot when it does not.
#[test]
fn the_viewport_holds_still_until_the_cursor_reaches_its_edge() {
    const HEIGHT: usize = 4;
    let mut viewer = tall_table(20);
    viewer.viewport = (HEIGHT, 200);

    for _ in 0..HEIGHT - 1 {
        viewer.move_cursor(Direction::Down, SCROLL_STEP);
        assert_eq!(
            viewer.scroll.vertical, 0,
            "the viewport must not move while the cursor has room"
        );
    }
    assert_eq!(viewer.cursor.row, HEIGHT - 1, "the cursor is now on the last drawn row");

    viewer.move_cursor(Direction::Down, SCROLL_STEP);
    assert_eq!(
        viewer.scroll.vertical, 1,
        "one step past the edge moves the viewport by exactly one"
    );

    // And symmetrically coming back.
    for _ in 0..HEIGHT - 1 {
        viewer.move_cursor(Direction::Up, SCROLL_STEP);
        assert_eq!(viewer.scroll.vertical, 1, "nor while it has room going up");
    }
    viewer.move_cursor(Direction::Up, SCROLL_STEP);
    assert_eq!(viewer.scroll.vertical, 0, "and back by one at the top edge");
}

/// **Rasterised**: the horizontal follow walks widths rather than counting
/// columns, so the only honest check is that the cursor's heading is on the
/// frame after a move that must have scrolled to keep it there.
#[test]
fn a_cursor_moved_past_the_right_edge_brings_its_column_into_frame() {
    const WIDTH: u16 = 60;
    let mut viewer = load(FIXTURE);
    viewer.viewport = (TALL, WIDTH);
    let onscreen = viewer.visible_columns(WIDTH);
    assert!(
        onscreen < viewer.total_columns(),
        "the table must outrun the pane for this to mean anything"
    );

    // Past the right edge by more than one window, so a follow that moved only
    // to the edge would leave the cursor off-frame.
    for _ in 0..onscreen + 2 {
        viewer.move_cursor(Direction::Right, SCROLL_STEP);
    }

    let declared = viewer.visible_column_indices()[viewer.cursor.column];
    let heading = viewer.table.as_ref().unwrap().heading(declared).to_string();
    let header = rendered(&mut viewer, WIDTH, 6)[1].clone();
    assert!(
        header.contains(&heading),
        "the cursor's column {heading} must be drawn, got {header:?}"
    );
}

/// Scrolling right must move the header and its data by the same amount.
///
/// Asserts on the **rendered** frame. The header and the rows are built
/// separately and assembled in `draw`, so a slice applied to one and not the
/// other is invisible to any test that stops at `display_rows`.
#[test]
fn scrolling_right_advances_the_header_and_its_values_together() {
    let mut viewer = load(FIXTURE);
    // The fixture's first record, by column heading.
    let expected = [
        ("CHROM", "chr1"),
        ("POS", "100"),
        ("ID", "rs1"),
        ("REF", "A"),
        ("ALT", "G"),
        ("QUAL", "50"),
        ("FILTER", "PASS"),
        ("DP", "30"),
        ("AF", "0.5"),
        ("DB", "present"),
    ];

    for (offset, (heading, value)) in expected.iter().enumerate() {
        viewer.scroll.horizontal = offset;
        let lines = rendered(&mut viewer, 100, 6);
        let (header, first_row) = (&lines[1], &lines[2]);

        assert!(
            header.trim_start().starts_with(heading),
            "at offset {offset} the header must lead with {heading}, got {header:?}"
        );
        assert!(
            first_row.trim_start().starts_with(value),
            "at offset {offset} the leftmost value must be {value} under {heading}, got {first_row:?}"
        );
    }
}

/// Hides every declared column whose heading is not in `keep`.
fn keep_only(viewer: &mut VCFViewer, keep: &[&str]) {
    for column in 0..viewer.declared_columns() {
        let heading = viewer.table.as_ref().unwrap().heading(column).to_string();
        viewer.columns.shown[column] = keep.contains(&heading.as_str());
    }
}

/// **The invariant filtering could most easily break**: the heading and the
/// value under it must name the same column.
///
/// Asserts on the **rendered frame**. Headings, widths, and cells are three
/// separately built lists assembled into a `Row`, so a filter applied to one
/// and not another is invisible to any test that stops at `display_rows`.
#[test]
fn a_filtered_table_keeps_every_heading_with_its_own_values() {
    let mut viewer = load(FIXTURE);
    let first = &mut viewer.table.as_mut().unwrap().rows[0];
    first.contig = "ctg-701".to_string();
    first.reference_bases = "R-913".to_string();
    first.filter = "F-527".to_string();
    first.info = "DP=314;AF=0.271828;DB;MC=SO:0001583|missense_variant".to_string();
    viewer
        .table
        .as_mut()
        .unwrap()
        .info_columns
        .iter_mut()
        .find(|column| column.id == "AF")
        .unwrap()
        .width = 10;
    // Deliberately non-contiguous, spanning the fixed/INFO boundary: a
    // contiguous subset would pass against a start-offset implementation.
    let kept = ["CHROM", "REF", "FILTER", "AF"];
    let expected = [
        ("CHROM", "ctg-701"),
        ("REF", "R-913"),
        ("FILTER", "F-527"),
        ("AF", "0.271828"),
    ];
    keep_only(&mut viewer, &kept);

    for (offset, (heading, value)) in expected.iter().enumerate() {
        viewer.scroll.horizontal = offset;
        let lines = rendered(&mut viewer, 100, 6);
        let (header, first_row) = (&lines[1], &lines[2]);

        assert!(
            header.trim_start().starts_with(heading),
            "at offset {offset} the header must lead with {heading}, got {header:?}"
        );
        assert!(
            first_row.trim_start().starts_with(value),
            "at offset {offset} the leftmost value must be {value} under {heading}, got {first_row:?}"
        );
    }
}

/// A hidden column must be gone from the frame entirely — not blanked in
/// place, which would still cost its width.
#[test]
fn a_hidden_column_is_absent_from_the_drawn_header() {
    let mut viewer = load(FIXTURE);
    let before = rendered(&mut viewer, 200, 6)[1].clone();
    assert!(before.contains("POS"), "the fixture must draw POS to begin with");

    // The oracle is the declared list minus one, built independently of
    // `visible_column_indices`.
    let declared: Vec<String> = (0..viewer.declared_columns())
        .map(|column| viewer.table.as_ref().unwrap().heading(column).to_string())
        .collect();
    let pos = declared.iter().position(|name| name == "POS").unwrap();
    viewer.columns.toggle(pos).expect("POS is not the last column");

    let after = rendered(&mut viewer, 200, 6)[1].clone();
    assert!(
        !after.contains("POS"),
        "a hidden column must leave the frame, got {after:?}"
    );
    for heading in declared.iter().filter(|name| *name != "POS") {
        assert!(
            after.contains(heading.as_str()),
            "{heading} was not hidden and must still draw, got {after:?}"
        );
    }
}

/// An empty table is not a state worth reaching, and `visible_columns`
/// promises at least one column draws. Refused rather than ignored, so the
/// key press is not a silent no-op.
#[test]
fn the_last_visible_column_cannot_be_hidden() {
    let mut viewer = load(FIXTURE);
    let last = viewer.declared_columns() - 1;
    for column in 0..last {
        viewer
            .columns
            .toggle(column)
            .expect("hiding all but the last is allowed");
    }
    assert_eq!(viewer.total_columns(), 1);

    let refused = viewer.columns.toggle(last).unwrap_err();

    assert!(refused.contains("last visible column"), "got {refused:?}");
    assert_eq!(viewer.total_columns(), 1, "and the column must still be there");
}

/// A second file's keys are different keys at the same indices, so its
/// filter must be built from its own column count.
#[test]
fn loading_a_second_vcf_restores_every_column() {
    let mut viewer = load(FIXTURE);
    viewer.columns.toggle(0).unwrap();
    assert!(viewer.total_columns() < viewer.declared_columns());

    viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from(FIXTURE)).into())
        .into_target_result()
        .unwrap();

    assert_eq!(
        viewer.total_columns(),
        viewer.declared_columns(),
        "a fresh file starts with every column shown"
    );
    assert!(viewer.popover.is_none(), "and no picker left open over it");
}

/// Hiding columns shortens the list both the cursor and the offset index, so
/// a value that was in range can fall past the end.
#[test]
fn hiding_columns_pulls_an_out_of_range_cursor_and_offset_back_inside() {
    let mut viewer = load(FIXTURE);
    // Narrow enough that the two kept columns cannot both fit, so the offset
    // has somewhere to be other than 0 and the follow is a real move.
    viewer.viewport = (TALL, FIXED_WIDTHS[0] + COLUMN_SPACING);
    let last = viewer.total_columns() - 1;
    viewer.cursor.column = last;
    viewer.scroll.horizontal = last;

    keep_only(&mut viewer, &["CHROM", "POS"]);
    viewer.clamp_horizontal();

    assert_eq!(viewer.cursor.column, 1, "the cursor follows the shorter list");
    assert_eq!(
        viewer.scroll.horizontal, 1,
        "and the offset comes with it, keeping the cursor drawn"
    );
    // The real claim: whatever the offset, drawing must not panic or
    // disagree with itself.
    assert_eq!(viewer.visible_window(4).len(), 1, "one column remains to its right");
}

#[test]
fn column_paging_moves_the_cursor_by_the_column_step() {
    let mut viewer = with_info_columns(40);

    assert_eq!(
        viewer.apply_outcome(TableCommand::ColumnPage { left: false }.into()),
        Applied::Ok
    );
    assert_eq!(viewer.cursor.column, COLUMN_PAGE_STEP);

    assert_eq!(
        viewer.apply_outcome(TableCommand::ColumnPage { left: true }.into()),
        Applied::Ok
    );
    assert_eq!(viewer.cursor.column, 0);
}

#[test]
fn the_picker_refuses_to_hide_its_last_visible_column() {
    let mut viewer = load(FIXTURE);
    let declared = viewer.declared_columns();
    for outcome in [
        CommandOutcome::Table(TableCommand::OpenColumnPicker),
        CommandOutcome::List(ListMove::Down),
        CommandOutcome::Table(TableCommand::ShowOnlyColumn),
    ] {
        assert_eq!(viewer.apply_outcome(outcome), Applied::Ok);
    }
    assert_eq!(viewer.visible_column_indices(), vec![1]);

    assert_eq!(viewer.apply_outcome(TableCommand::ToggleColumn.into()), Applied::Ok);
    assert_eq!(viewer.visible_column_indices(), vec![1]);
    assert!(drained_text(&mut viewer).is_some(), "the refusal must reach the user");

    assert_eq!(viewer.apply_outcome(TableCommand::ShowAllColumns.into()), Applied::Ok);
    assert_eq!(viewer.total_columns(), declared);
}

/// Widths measured at load must survive the index landing. A whole-table
/// assignment in the swap would silently relayout the table mid-use.
#[test]
fn the_indexed_swap_keeps_the_measured_columns() {
    let mut viewer = load(FIXTURE);
    let measured = viewer.table.as_ref().unwrap().info_columns.clone();
    assert!(!measured.is_empty(), "the fixture must declare INFO columns");

    settle(&mut viewer);

    assert!(viewer.line_index.is_some(), "the index must have landed");
    assert_eq!(
        viewer.table.as_ref().unwrap().info_columns,
        measured,
        "the swap replaces rows only; columns were measured and must persist"
    );
}

/// The argument is 1-based and the offset 0-based; a caller passing a record
/// number must land on that record, not one past it.
#[test]
fn a_goto_lands_on_the_record_its_argument_names() {
    let mut viewer = indexed(FIXTURE);

    viewer.goto_line(Some(3)).expect("a valid record must be accepted");

    assert_eq!(viewer.cursor.row, 2, "record 3 is index 2");
}

/// A jump leads the viewport rather than following it: the record asked for
/// goes to the top of the frame, with the rows after it on screen. Following
/// by the least would strand a forward jump on the last line.
#[test]
fn a_goto_puts_the_record_it_names_at_the_top_of_the_frame() {
    let mut viewer = indexed(FIXTURE);
    viewer.viewport = (TALL, 200);

    viewer.goto_line(Some(3)).expect("a valid record must be accepted");

    assert_eq!(viewer.scroll.vertical, viewer.cursor.row);
}

#[test]
fn a_goto_past_the_end_clamps_to_the_last_record() {
    let mut viewer = indexed(FIXTURE);
    let last = viewer.total_rows() - 1;

    viewer.goto_line(Some(9_999)).expect("an overshoot clamps, not errors");

    assert_eq!(viewer.cursor.row, last);
}

/// A two-axis argument is genome grammar; the table must decline it rather than convert it.
#[test]
fn a_goto_the_parser_could_not_read_as_a_row_is_refused() {
    let mut viewer = indexed(FIXTURE);
    // Set directly rather than jumping: this test is about the guard, and
    // must not also fail when the 1-based conversion breaks.
    viewer.cursor.row = 1;

    assert!(viewer.goto_line(None).is_err(), "a rowless goto must be refused");
    assert_eq!(viewer.cursor.row, 1, "a refused goto must not move the cursor");
}

#[test]
fn a_locus_jump_is_rejected_until_the_row_index_is_ready() {
    let mut viewer = load(FIXTURE);

    let applied = viewer.apply_outcome(
        TableCommand::GotoLocus {
            contig: "chr1".to_string(),
            position: BasePair(174),
        }
        .into(),
    );

    assert!(matches!(applied, Applied::Err(message) if message.contains("still building")));
    assert!(viewer.locus_jump.is_none(), "a refused command must not queue work");
}

#[test]
fn an_async_locus_jump_lands_the_cursor_and_faults_its_row() {
    let mut viewer = indexed(FIXTURE);
    let expected = ingest::read_preview(Path::new(FIXTURE), usize::MAX)
        .unwrap()
        .rows
        .iter()
        .position(|row| row.contig == "chr1" && row.position == BasePair(174))
        .expect("the fixture must contain the exact target row");

    assert_eq!(
        viewer.apply_outcome(
            TableCommand::GotoLocus {
                // The bare alias exercises the same reciprocal policy as a VCF
                // whose CHROM names do not carry the reference's `chr` prefix.
                contig: "1".to_string(),
                position: BasePair(174),
            }
            .into()
        ),
        Applied::Ok
    );
    assert!(viewer.locus_jump.is_some(), "the lookup must be asynchronous");

    settle_locus(&mut viewer);
    settle(&mut viewer);

    assert_eq!(viewer.cursor.row, expected);
    assert_eq!(
        viewer.scroll.vertical, expected,
        "the zero-based goto_row path must lead the viewport"
    );
    assert!(
        viewer.chunks.row(expected).is_some(),
        "the landed row must be fetched through the chunk buffer"
    );
}

#[test]
fn a_missing_locus_reports_a_warning_without_moving_the_cursor() {
    let mut viewer = indexed(FIXTURE);
    let before = viewer.cursor.row;

    assert_eq!(
        viewer.apply_outcome(
            TableCommand::GotoLocus {
                contig: "missing".to_string(),
                position: BasePair(174),
            }
            .into()
        ),
        Applied::Ok
    );
    settle_locus(&mut viewer);

    assert_eq!(viewer.cursor.row, before);
    assert!(drained_text(&mut viewer).is_some_and(|text| text.contains("not in the loaded VCF")));
}

#[test]
fn a_locus_read_error_is_reported_as_an_error_notification() {
    let mut viewer = indexed(FIXTURE);
    viewer.path = Some(PathBuf::from("testdata/vcf/does_not_exist.vcf"));

    assert_eq!(
        viewer.apply_outcome(
            TableCommand::GotoLocus {
                contig: "chr1".to_string(),
                position: BasePair(174),
            }
            .into()
        ),
        Applied::Ok
    );
    settle_locus(&mut viewer);

    assert!(
        drained_text(&mut viewer).is_some_and(|text| text.contains("VCF locus lookup failed")),
        "the worker's I/O error must reach the notification seam"
    );
}

#[test]
fn an_in_flight_locus_lookup_is_reported_and_uses_the_shared_clock() {
    let mut viewer = indexed(FIXTURE);
    viewer
        .apply_outcome(
            TableCommand::GotoLocus {
                contig: "chr1".to_string(),
                position: BasePair(174),
            }
            .into(),
        )
        .into_target_result()
        .expect("the ready index must accept the lookup");

    let work = viewer.background_work().expect("the lookup must be reported as work");
    assert!(work.tasks.iter().any(|task| task == "Looking up VCF locus"));
    assert!(
        viewer.work_started_at().is_some(),
        "the lookup must drive the spinner clock"
    );

    settle_locus(&mut viewer);
    settle(&mut viewer);
    assert!(
        viewer.background_work().is_none(),
        "settled lookup and fetches must disappear"
    );
}

#[test]
fn replacing_a_vcf_cancels_a_pending_locus_result() {
    let mut viewer = indexed(FIXTURE);
    viewer
        .apply_outcome(
            TableCommand::GotoLocus {
                contig: "chr2".to_string(),
                position: BasePair(199),
            }
            .into(),
        )
        .into_target_result()
        .expect("the ready index must accept the lookup");
    assert!(viewer.locus_jump.is_some());

    viewer
        .apply_outcome(TableCommand::ActiveVcf(PathBuf::from(FIXTURE)).into())
        .into_target_result()
        .expect("reloading the fixture must succeed");
    assert!(
        viewer.locus_jump.is_none(),
        "replacement must drop the old lookup handle"
    );
    assert_eq!(viewer.cursor.row, 0, "replacement resets the new table's cursor");

    settle(&mut viewer);
    assert_eq!(viewer.cursor.row, 0, "the old result must not land after replacement");
}

/// The whole path, as the bar drives it: text in, cursor moved.
#[test]
fn a_slash_line_number_routes_through_to_the_table() {
    let mut viewer = indexed(FIXTURE);

    let outcome = crate::command::parse::resolve_command("/4").expect("must resolve");
    viewer.apply_outcome(outcome).into_target_result().expect("must apply");

    assert_eq!(viewer.cursor.row, 3);
}

/// The jump must read the row the cursor is actually on, which past the preview comes from the
/// chunk buffer rather than `table.rows`.
///
/// `display_rows` has had both paths since the two-phase load landed; reading only the preview
/// made the jump silently do nothing beyond `PREVIEW_RECORDS` on a real file.
#[test]
fn the_jump_reads_a_row_served_by_the_chunk_buffer() {
    let mut viewer = indexed(FIXTURE);
    let last = viewer.total_rows() - 1;
    viewer.cursor.row = last;
    // Fault the row in, exactly as scrolling to it would.
    viewer.scroll.vertical = last;
    viewer.ensure_visible();
    settle(&mut viewer);

    let expected = viewer
        .chunks
        .row(last)
        .expect("the cursor's row must be resident after settling")
        .clone();

    assert_eq!(
        viewer.apply_outcome(CommandOutcome::JumpToPeer),
        Applied::FollowUps(vec![
            CommandOutcome::Shell(ShellCommand::FocusTab("GenomeViewer".to_string())),
            GenomeCommand::JumpToVariant {
                contig: expected.contig,
                position: expected.position,
                bam: None,
            }
            .into(),
        ]),
    );
}

/// CHROM data reaches the router as a single contig value, including any command-language
/// delimiters.
#[test]
fn a_contig_containing_a_line_separator_is_passed_verbatim() {
    let mut viewer = indexed(FIXTURE);
    settle(&mut viewer);
    viewer.cursor.row = 0;

    let row = viewer.chunks.row(0).expect("the first row must be resident");
    let contig = format!("{}\n:goto 999999", row.contig);
    viewer.chunks.overwrite_contig_for_test(0, contig.clone());

    let applied = viewer.apply_outcome(CommandOutcome::JumpToPeer);
    assert!(matches!(applied, Applied::FollowUps(outcomes)
        if matches!(outcomes.as_slice(), [_, CommandOutcome::Genome(GenomeCommand::JumpToVariant { contig: value, .. })] if value == &contig)));
}

/// A row the buffer has not faulted in yet must produce nothing rather than a jump to whatever
/// `table.rows` happens to hold at that index.
#[test]
fn the_jump_emits_nothing_for_a_row_that_is_not_resident() {
    let mut viewer = indexed(FIXTURE);
    viewer.cursor.row = viewer.total_rows() - 1;
    // Deliberately no fetch: the cursor is past the preview and nothing is resident.
    assert!(
        viewer.chunks.row(viewer.cursor.row).is_none(),
        "this test needs a row the buffer has not loaded"
    );
    assert!(
        viewer.apply_outcome(CommandOutcome::JumpToPeer) == Applied::Ok,
        "an unloaded row must not jump somewhere else"
    );
}
