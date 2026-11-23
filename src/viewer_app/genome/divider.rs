//! The divider rows that head the minimap, local-area, coverage, and pileup tracks.
//!
//! Layout and painting live together here because the underline needs the centred
//! label's bounds — computing them once and carrying them to the buffer is what keeps the painter
//! from re-deriving a parse of its own input.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::settings::GenomeViewerSettings;
use crate::basepair::{BasePair, format_with_commas};
use crate::style::palette::{Chrome, Surface};
use crate::viewer_app::viewport::ViewPosition;

/// Rule cells between a cap and the edge label beside it, so neither coordinate touches the
/// opening arrow or the box's curve.
const DIVIDER_EDGE_PAD: usize = 2;

/// Rule cells kept between an edge label and the centred one, so a wide centre label reads as
/// separate from the coordinates rather than running into them.
const DIVIDER_LABEL_GAP: usize = 2;

/// The dot a fully degraded divider is filled with.
const DIVIDER_DOT: char = '·';

/// The glyphs one divider row is drawn from.
///
/// Both spellings are the same three parts in the same three places, so a divider's width,
/// label column, and degradation are properties of `divider_line` rather than of either
/// vocabulary — swapping them cannot move anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DividerGlyphs {
    open: char,
    rule: char,
    close: char,
}

impl DividerGlyphs {
    /// `<-label------->`, for a font without box drawing.
    const ASCII: Self = Self {
        open: '<',
        rule: '-',
        close: '>',
    };

    /// `╭─label───────╮`. Every divider heads the track it names, so the corners turn *down*
    /// into the content below and bracket their subject rather than pointing away from it.
    const BOX: Self = Self {
        open: '╭',
        rule: '─',
        close: '╮',
    };

    /// The vocabulary the `dividerbox` setting selects.
    pub(super) fn for_settings(settings: &GenomeViewerSettings) -> Self {
        if settings.dividerbox { Self::BOX } else { Self::ASCII }
    }
}

/// A laid-out divider row: its cells, and where the centred label sits among them.
///
/// The range is carried rather than recovered, because only the layout knows it. Scanning the
/// finished text back for a non-rule run would need a second copy of the glyph vocabulary in the
/// painter, which is exactly the kind of duplicate that goes stale when a vocabulary is added.
/// A degraded row has no centred label, which [`DividerRow::dots`] expresses as `None`.
pub(super) struct DividerRow {
    text: String,
    centre: Option<Range<usize>>,
}

impl DividerRow {
    /// A full row of dots, the shared degradation for a pane too narrow to name anything.
    pub(super) fn dots(width: u16) -> Self {
        Self {
            text: std::iter::repeat_n(DIVIDER_DOT, usize::from(width)).collect(),
            centre: None,
        }
    }
}

/// Tries the expanded label first, then the base label with coordinates, then the base label
/// alone, then dots. The expanded label can stand alone only for a span with no coordinates.
pub(super) fn layout_divider(
    width: u16,
    centre: &str,
    expanded_centre: Option<&str>,
    span: Range<BasePair>,
    glyphs: DividerGlyphs,
) -> DividerRow {
    let edges = divider_edges(span);
    let edges = edges.as_ref().map(|(left, right)| (left.as_str(), right.as_str()));
    expanded_centre
        .and_then(|expanded| divider_line(width, expanded, edges, glyphs))
        .or_else(|| edges.and_then(|pair| divider_line(width, centre, Some(pair), glyphs)))
        .or_else(|| divider_line(width, centre, None, glyphs))
        .unwrap_or_else(|| DividerRow::dots(width))
}

pub(super) fn draw_pileup_divider(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    position: ViewPosition,
    mean: Option<f64>,
    glyphs: DividerGlyphs,
) {
    if area.is_empty() {
        return;
    }

    let label = format!("{title}: {}bp", position.width.format_with_commas());
    let with_mean = mean.map(|mean| format!("{label} [Average depth over span = {mean:.2}X]"));
    // Underflow occupies screen columns before the contig starts.
    let visible_end = position.left + position.width - position.underflow;
    let row = layout_divider(
        area.width,
        &label,
        with_mean.as_deref(),
        position.left..visible_end,
        glyphs,
    );
    draw_divider(buffer, area, &row);
}

/// The pair names the first and last covered base. A single coordinate would describe only
/// one end of the span.
fn divider_edges(span: Range<BasePair>) -> Option<(String, String)> {
    if span.is_empty() {
        return None;
    }
    Some((divider_edge(span.start), divider_edge(span.end - BasePair(1))))
}

/// Spells one edge coordinate in the same display notation as the coordinate track.
fn divider_edge(bp: BasePair) -> String {
    format!("g.{}", format_with_commas(bp.to_display()))
}

/// Fits a single choice of labels. Failing the fit lets the caller drop a whole label.
fn divider_line(width: u16, centre: &str, edges: Option<(&str, &str)>, glyphs: DividerGlyphs) -> Option<DividerRow> {
    let width = usize::from(width);
    let centre_len = centre.chars().count();
    // A cap at each end, and rule enough that the centre label is never flush against one.
    let required = centre_len + 2 + 2 * DIVIDER_LABEL_GAP;
    if width < required {
        return None;
    }

    // Centre on the pane so differing coordinate widths cannot pull the label off its track's middle.
    let centre_start = (width - centre_len) / 2;
    let mut cells: Vec<char> = std::iter::repeat_n(glyphs.rule, width).collect();
    cells[0] = glyphs.open;
    cells[width - 1] = glyphs.close;
    for (offset, ch) in centre.chars().enumerate() {
        cells[centre_start + offset] = ch;
    }

    if let Some((left, right)) = edges {
        let left_len = left.chars().count();
        let right_len = right.chars().count();
        // Both edge labels must clear the centred one, or a coordinate would overwrite it.
        if 1 + DIVIDER_EDGE_PAD + left_len + DIVIDER_LABEL_GAP > centre_start
            || centre_start + centre_len + DIVIDER_LABEL_GAP + right_len + DIVIDER_EDGE_PAD + 1 > width
        {
            return None;
        }
        for (offset, ch) in left.chars().enumerate() {
            cells[1 + DIVIDER_EDGE_PAD + offset] = ch;
        }
        let right_start = width - 1 - DIVIDER_EDGE_PAD - right_len;
        for (offset, ch) in right.chars().enumerate() {
            cells[right_start + offset] = ch;
        }
    }

    Some(DividerRow {
        text: cells.into_iter().collect(),
        centre: (centre_len != 0).then_some(centre_start..centre_start + centre_len),
    })
}

/// Paints one laid-out divider row across its pane in the shared divider colours.
///
/// The one place a divider reaches the buffer. Its ground is [`Surface::APP_BG`] rather
/// than the headed track's, which keeps the dividers reading as one repeated rule.
pub(super) fn draw_divider(buffer: &mut Buffer, area: Rect, row: &DividerRow) {
    let style = Style::default()
        .fg(Chrome::DIVIDER_FG())
        .bg(Surface::APP_BG())
        .add_modifier(Modifier::BOLD);
    for (column, ch) in row.text.chars().take(usize::from(area.width)).enumerate() {
        if let Some(cell) = buffer.cell_mut((area.x + column as u16, area.y)) {
            cell.set_char(ch);
            cell.set_style(match &row.centre {
                Some(range) if range.contains(&column) => style.add_modifier(Modifier::UNDERLINED),
                _ => style,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn divider_drops_both_coordinates_when_only_one_fits() {
        let _palette = crate::style::palette::lock_for_test();
        let first = BasePair(16);
        let last = BasePair(10_008);
        let span = first..last + BasePair(1);
        let centre = "span";
        let expected = [first, last].map(|bp| format!("g.{}", format_with_commas(bp.to_display())));

        // The right coordinate is longer. It needs this many cells after the centre, and
        // owns the extra cell when the available space divides unevenly.
        let side_overhead = DIVIDER_LABEL_GAP + DIVIDER_EDGE_PAD + 1;
        let right_space = side_overhead + expected[1].len();
        let fits_both = u16::try_from(centre.len() + 2 * right_space - 1).unwrap();
        let too_narrow = fits_both - 1;
        assert!((usize::from(too_narrow) - centre.len()) / 2 >= side_overhead + expected[0].len());

        for glyphs in [DividerGlyphs::ASCII, DividerGlyphs::BOX] {
            for (width, coordinates) in [(fits_both, expected.as_slice()), (too_narrow, &[])] {
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        let row = layout_divider(area.width, centre, None, span.clone(), glyphs);
                        draw_divider(frame.buffer_mut(), area, &row);
                    })
                    .unwrap();
                let row: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                let drawn: Vec<_> = row.split(glyphs.rule).filter(|label| label.starts_with("g.")).collect();
                assert_eq!(drawn, coordinates, "width {width}");
                assert!(row.contains(centre), "width {width}");
            }
        }
    }
}
