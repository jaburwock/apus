//! Geometry and painting helpers for the genome coordinate ruler.
//!
//! The helpers keep ruler banding, ticks, labels, group prefixes, and the centreline in the same
//! shell-owned coordinate space. They account for leading underflow and the active contig boundary
//! before writing cells, so the ruler cannot drift from the reference or alignment tracks at
//! the pane edges.
//!
//! `centline_mark` is also the shared centreline resolver used by other tracks. It returns the
//! column and glyph together: at zoom levels that pack multiple bases into a cell, the half-cell is
//! derived from the centre's offset from the viewport's left edge, not from the centre coordinate's
//! absolute parity. Re-deriving either rule in a consumer is the main way these tracks can disagree.
//!
//! Tick selection converts displayed coordinates back to internal `BasePair` values. Tick marks use
//! the resulting column and half-cell; labels anchor from the tick column. Group prefixes use signed
//! placement so they can scroll and hand off at the pane edge rather than behaving as a permanently
//! fixed heading.
//!
//! Painting deliberately preserves each cell's existing background. This is required because marks
//! overlay ruler banding and out-of-bounds fills; replacing the background would punch holes in the
//! track beneath the annotation.
//!
//! Note: roughly 60% of this file is its test module; the production helpers are substantially
//! smaller than the file size suggests.

use ratatui::{Frame, layout::Rect, style::Modifier};

use crate::basepair::BasePair;
use crate::style::{Chrome, Coordinate, Text};
use crate::viewer_app::painter;
use crate::viewer_app::viewport::ViewPosition;

use super::super::geometry::{cent_column, clamp_column, in_bounds_column_range, leading_columns};

/// Tick spacing in bp; zero disables ticks.
const RULER_TICK_STEP: BasePair = BasePair(20);

/// `ViewPosition::left` saturates at zero; turn missing bp into leading columns
/// shared by every full-width ruler row.
fn leading_oob_columns(position: ViewPosition, area_width: u16, bp_per_column: u64) -> u16 {
    leading_columns(position.underflow, area_width, bp_per_column)
}

/// Draws alternating banding across row 0 within the contig's in-bounds columns.
pub(in crate::viewer_app::genome) fn draw_ruler_banding(
    frame: &mut Frame,
    area: Rect,
    position: ViewPosition,
    contig_length: BasePair,
    bp_per_column: u64,
) {
    if area.height == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    let in_bounds = in_bounds_column_range(position, contig_length, area.width, bp_per_column);
    let left_band = position.left.as_u64() / bp_per_column;
    for col in in_bounds.clone() {
        if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
            let bg = Chrome::ruler_band(left_band + u64::from(col - in_bounds.start));
            painter::paint_cell(cell, " ", Text::DIMMED_FG(), bg);
        }
    }
}

/// One selected ruler tick.
struct Tick {
    /// Content-relative screen column.
    column: u64,
    /// Which base within that column, i.e. which half-cell at `Mid`.
    half: u64,
    /// The tick's internal 0-based coordinate.
    value: BasePair,
}

/// Selects displayed 1-based ticks, converts them to internal 0-based values,
/// and returns screen-column offsets. Includes coordinate zero at contig origin.
///
/// `half` is the remainder of the same division that yields `column`: which
/// base of a packed cell a tick occupies follows from its distance to `offset`,
/// never from its own parity — see `centline_mark`.
fn visible_ticks(offset: BasePair, span: BasePair, step: BasePair, bp_per_column: u64) -> Vec<Tick> {
    let window_start_displayed = offset.to_display();
    let window_end_displayed = (offset + span).to_display();
    let first_displayed = BasePair(window_start_displayed).div_ceil(step) * step;
    let mut ticks: Vec<Tick> = first_displayed
        .step_to(BasePair(window_end_displayed), step)
        .map(|displayed| {
            let value = BasePair::from_display(displayed.as_u64());
            let delta = (value - offset).as_u64();
            Tick {
                column: delta / bp_per_column,
                half: delta % bp_per_column,
                value,
            }
        })
        .collect();
    if offset == BasePair(0) && !ticks.iter().any(|t| t.value == BasePair(0)) {
        ticks.insert(
            0,
            Tick {
                column: 0,
                half: 0,
                value: BasePair(0),
            },
        );
    }
    ticks
}

/// At `Mid` a tick marks whichever half of its cell holds that base.
pub(in crate::viewer_app::genome) fn draw_tickmarks(
    frame: &mut Frame,
    area: Rect,
    position: ViewPosition,
    bp_per_column: u64,
    contig_length: BasePair,
) {
    if RULER_TICK_STEP == BasePair(0) {
        return;
    }
    let Some(mark_row) = area.y.checked_add(1).filter(|&y| y < area.y + area.height) else {
        return;
    };
    let leading = leading_oob_columns(position, area.width, bp_per_column);
    let buf = frame.buffer_mut();
    let content_width = area.width - leading;
    let width = BasePair::from(content_width) * BasePair(bp_per_column);
    for tick in visible_ticks(position.left, width, RULER_TICK_STEP, bp_per_column) {
        if tick.value >= contig_length {
            continue;
        }
        let glyph = match (bp_per_column, tick.half) {
            (1, _) => "|",
            (_, 0) => painter::LEFT_HALF_BLOCK,
            _ => painter::RIGHT_HALF_BLOCK,
        };
        let col = clamp_column(tick.column, leading, area.width);
        if let Some(cell) = buf.cell_mut((area.x + col, mark_row)) {
            let bg = cell.bg;
            painter::paint_cell(cell, glyph, Text::PRIMARY_FG(), bg);
        }
    }
}

/// Formats the low three digits of a tick within its thousands group.
fn tick_label_text(displayed_value: u64) -> String {
    if displayed_value < 1000 {
        displayed_value.to_string()
    } else {
        format!(",{:03}", displayed_value % 1000)
    }
}

/// Draws tick labels over the tick row.
pub(in crate::viewer_app::genome) fn draw_ticklabels(
    frame: &mut Frame,
    area: Rect,
    position: ViewPosition,
    bp_per_column: u64,
    contig_length: BasePair,
) {
    if RULER_TICK_STEP == BasePair(0) {
        return;
    }

    let buf = frame.buffer_mut();
    let track_right = area.x + area.width;
    let track_bottom_y = area.y + area.height;

    if let Some(mark_row) = area.y.checked_add(1).filter(|&y| y < track_bottom_y) {
        let leading = leading_oob_columns(position, area.width, bp_per_column);
        let content_width = area.width - leading;
        let width = BasePair::from(content_width) * BasePair(bp_per_column);
        for tick in visible_ticks(position.left, width, RULER_TICK_STEP, bp_per_column) {
            if tick.value >= contig_length {
                continue;
            }
            let label = tick_label_text(tick.value.to_display());
            let start = area.x + clamp_column(tick.column + 1, leading, area.width);
            for (i, ch) in label.chars().enumerate() {
                let x = start + i as u16;
                if x >= track_right {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, mark_row)) {
                    let bg = cell.bg;
                    painter::paint_cell(cell, &ch.to_string(), Coordinate::NUMBERING_FG(), bg);
                }
            }
        }
    }
}

/// Returns the displayed value's thousands group.
fn group_number(displayed_value: u64) -> u64 {
    displayed_value / 1000
}

/// Formats a group's higher-order digits as a complete round number.
fn group_prefix_text(group: u64) -> String {
    format!("{},000", BasePair::length(group).format_with_commas())
}

/// Converts an internal coordinate to a possibly off-screen column.
fn signed_column(internal: BasePair, position_left: BasePair, bp_per_column: u64, leading: u16) -> i64 {
    let delta = internal.as_u64() as i64 - position_left.as_u64() as i64;
    // Floor division keeps off-screen group starts on the left side.
    let cols = delta.div_euclid(bp_per_column as i64);
    leading as i64 + cols
}

/// Returns a group's natural, unclamped column.
fn group_natural_column(group: u64, position_left: BasePair, bp_per_column: u64, leading: u16) -> i64 {
    let group_start_internal = BasePair::from_display(group * 1000);
    signed_column(group_start_internal, position_left, bp_per_column, leading)
}

/// Draws one visible group prefix over the existing banding.
fn draw_one_group(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    track_right: u16,
    contig_length: BasePair,
    group: u64,
    col: u16,
) {
    if group == 0 {
        return;
    }
    let group_start_internal = BasePair::from_display(group * 1000);
    if group_start_internal >= contig_length {
        return;
    }
    let text = group_prefix_text(group);
    for (i, ch) in text.chars().enumerate() {
        let x = area.x + col + i as u16;
        if x >= track_right {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            let bg = cell.bg;
            painter::paint_cell(cell, &ch.to_string(), Coordinate::NUMBERING_FG(), bg);
            cell.modifier |= Modifier::BOLD;
        }
    }
}

/// Draws the current group prefix and the next prefix as it enters the pane.
pub(in crate::viewer_app::genome) fn draw_group_prefix(
    frame: &mut Frame,
    area: Rect,
    position: ViewPosition,
    bp_per_column: u64,
    contig_length: BasePair,
) {
    let leading = leading_oob_columns(position, area.width, bp_per_column);
    let track_right = area.x + area.width;
    let g_left = group_number(position.left.to_display());

    let buf = frame.buffer_mut();

    let nc_left = group_natural_column(g_left, position.left, bp_per_column, leading);
    draw_one_group(buf, area, track_right, contig_length, g_left, nc_left.max(0) as u16);

    let g_next = g_left + 1;
    let nc_next = group_natural_column(g_next, position.left, bp_per_column, leading);
    if nc_next > 0 && nc_next < i64::from(area.width) {
        draw_one_group(buf, area, track_right, contig_length, g_next, nc_next as u16);
    }
}

/// Returns the shared centre mark's column and glyph.
///
/// At `Mid` a cell packs two bases: content column `c` holds `left + 2c` in its
/// left half and `left + 2c + 1` in its right. Which half `centre` occupies is
/// therefore fixed by the offset `centre - left`, **not** by `centre`'s own
/// parity — an odd pane width shifts every base to the other half without any
/// coordinate changing. Keying on absolute parity drew odd centres one subpixel
/// left of the base they mark; see history 2026-08-13.
///
/// Labels still anchor to the centre column, so they don't jitter with the mark.
///
/// The alignment track never stamps the `Mid` glyph but reads it as data: `centline_px` takes the
/// half-cell from it. Keep the glyph even where nothing draws it.
pub(in crate::viewer_app::genome) fn centline_mark(
    area: Rect,
    bp_per_column: u64,
    position: ViewPosition,
) -> Option<(u16, &'static str)> {
    let x = cent_column(area)?;
    if bp_per_column < 2 {
        return Some((x, "│"));
    }
    let leading = leading_oob_columns(position, area.width, bp_per_column);
    let offset = position.centre.checked_sub(position.left)?.as_u64();
    let (col, half) = (offset / bp_per_column, offset % bp_per_column);
    let mark_x = area.x + clamp_column(col, leading, area.width.saturating_sub(1));
    // With more than two bases per cell there is no glyph for an interior base;
    // the left half is the closest honest mark.
    if half == 0 || bp_per_column > 2 {
        Some((mark_x, painter::LEFT_HALF_BLOCK))
    } else {
        Some((mark_x, painter::RIGHT_HALF_BLOCK))
    }
}

/// Draws the shared centre mark across both ruler rows.
pub(in crate::viewer_app::genome) fn draw_centreline(
    frame: &mut Frame,
    area: Rect,
    position: ViewPosition,
    bp_per_column: u64,
) {
    let Some((mark_x, glyph)) = centline_mark(area, bp_per_column, position) else {
        return;
    };
    let fg = Text::PRIMARY_FG();

    let buf = frame.buffer_mut();
    if let Some(cell) = buf.cell_mut((mark_x, area.y)) {
        let bg = cell.bg;
        painter::paint_cell(cell, glyph, fg, bg);
    }

    if let Some(row1_y) = area.y.checked_add(1).filter(|&y| y < area.y + area.height)
        && let Some(cell) = buf.cell_mut((mark_x, row1_y))
    {
        let bg = cell.bg;
        painter::paint_cell(cell, glyph, fg, bg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAR: BasePair = BasePair(1_000_000);

    #[test]
    fn visible_ticks_land_on_displayed_multiples_in_one_based_mode() {
        let ticks = visible_ticks(BasePair(0), BasePair(65), BasePair(20), 1);
        let values: Vec<BasePair> = ticks.iter().map(|t| t.value).collect();
        assert_eq!(values, vec![BasePair(0), BasePair(19), BasePair(39), BasePair(59)]);
        for tick in &ticks {
            assert_eq!(tick.column, tick.value.as_u64());
            assert_eq!(tick.half, 0, "one bp per column leaves no half to choose");
        }
    }

    #[test]
    fn visible_ticks_scales_screen_offset_by_bp_per_column() {
        let ticks = visible_ticks(BasePair(0), BasePair(130), BasePair(20), 2);
        let values: Vec<BasePair> = ticks.iter().map(|t| t.value).collect();
        assert_eq!(
            values,
            vec![
                BasePair(0),
                BasePair(19),
                BasePair(39),
                BasePair(59),
                BasePair(79),
                BasePair(99),
                BasePair(119)
            ]
        );
        for tick in &ticks {
            assert_eq!(tick.column, tick.value.as_u64() / 2);
        }
    }

    /// A tick's half-cell follows its distance from `offset`, so scrolling one
    /// bp moves every tick to the other half — the same rule `centline_mark`
    /// follows, and the one absolute parity got wrong.
    #[test]
    fn visible_ticks_half_follows_the_offset_not_the_tick_value() {
        let value = BasePair(19);
        for (offset, expected_half) in [(BasePair(0), 1), (BasePair(1), 0)] {
            let ticks = visible_ticks(offset, BasePair(130), BasePair(20), 2);
            let tick = ticks.iter().find(|t| t.value == value).expect("tick 19 is visible");
            assert_eq!(
                tick.half,
                expected_half,
                "at offset {offset:?}, tick {value:?} sits {} bp in",
                (value - offset).as_u64()
            );
        }
    }

    #[test]
    fn visible_ticks_only_special_cases_the_true_origin() {
        let at_origin = visible_ticks(BasePair(0), BasePair(10), BasePair(20), 1);
        assert!(at_origin.iter().any(|t| t.value == BasePair(0)));

        let mid_contig = visible_ticks(BasePair(100), BasePair(10), BasePair(20), 1);
        assert!(!mid_contig.iter().any(|t| t.value == BasePair(0)));
    }

    #[test]
    fn visible_ticks_does_not_duplicate_the_origin() {
        let ticks = visible_ticks(BasePair(0), BasePair(5), BasePair(20), 1);
        assert_eq!(ticks.iter().filter(|t| t.value == BasePair(0)).count(), 1);
    }

    #[test]
    fn group_natural_column_floor_divides_negative_deltas_at_two_bp_per_column() {
        let col = group_natural_column(1, BasePair(1004), 2, 0);
        assert_eq!(col, -3);
    }

    #[test]
    fn group_natural_column_matches_manual_math_at_one_bp_per_column() {
        let col = group_natural_column(1, BasePair(500), 1, 10);
        assert_eq!(col, 509);
    }

    #[test]
    fn group_prefix_sticks_slides_and_respects_genomic_bounds() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let cases = [
            (BasePair(1539), 600u16, FAR, 0u16, true),
            (BasePair(1539), 600, FAR, 500, true),
            (BasePair(1539), 600, BasePair(1999), 500, false),
            (BasePair(2038), 20, FAR, 0, true),
            (BasePair(2039), 20, FAR, 0, true),
            (BasePair(539), 20, FAR, 0, false),
            (BasePair(5), 80, FAR, 0, false),
        ];
        for (centre, width, contig_length, column, occupied) in cases {
            let area = Rect::new(0, 0, width, 1);
            let position = ViewPosition::new(centre, BasePair(80));
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
            terminal
                .draw(|frame| draw_group_prefix(frame, area, position, 1, contig_length))
                .expect("draw");
            assert_eq!(
                terminal.backend().buffer().cell((column, 0)).unwrap().symbol() != " ",
                occupied,
                "centre {centre:?}, contig length {contig_length:?}, column {column}"
            );
            if group_number(position.left.to_display()) == 0 {
                assert!((0..width).all(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol() == " "));
            }
            if centre == BasePair(1539) && contig_length == FAR {
                assert_ne!(terminal.backend().buffer().cell((0, 0)).unwrap().symbol(), " ");
            }
        }
    }

    #[test]
    fn draw_tickmark_lands_on_its_genomic_step() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 2,
        };
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        let position = ViewPosition::new(BasePair(20), BasePair(40));
        terminal
            .draw(|frame| {
                draw_tickmarks(frame, area, position, 1, FAR);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        // Displayed step 20 is internal position 19; left is zero, so it belongs in column 19.
        assert_ne!(
            buf.cell((19, area.y + 1)).unwrap().symbol(),
            " ",
            "displayed step 20 belongs in column 19"
        );
        assert_eq!(buf.cell((18, area.y + 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((20, area.y + 1)).unwrap().symbol(), " ");
    }

    #[test]
    fn draw_centreline_marks_both_ruler_rows() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 5,
        };
        let position = ViewPosition::new(BasePair(1_234), BasePair::from(area.width) * BasePair(2));
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        terminal
            .draw(|frame| draw_centreline(frame, area, position, 2))
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let (x, glyph) = centline_mark(area, 2, position).expect("centre mark");
        for row in area.y..area.y + 2 {
            assert_eq!(buf.cell((x, row)).unwrap().symbol(), glyph);
        }
    }

    #[test]
    fn draw_centreline_row1_is_column_aligned_with_cent_column() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let area = Rect {
            x: 5,
            y: 0,
            width: 20,
            height: 5,
        };
        let position = ViewPosition::new(BasePair(1_234), BasePair::from(area.width));
        let mut terminal = Terminal::new(TestBackend::new(area.x + area.width, area.height)).expect("test terminal");
        terminal
            .draw(|frame| draw_centreline(frame, area, position, 1))
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        let x = cent_column(area).unwrap();
        assert_ne!(buf.cell((x, area.y + 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((x - 1, area.y + 1)).unwrap().symbol(), " ");
    }
}
