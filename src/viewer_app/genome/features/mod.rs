//! The genome viewer's feature track projects the viewer-owned [`FeatureIndex`] into two rows:
//! labels above transcript bars, exon blocks, and strand arrows. It sits beside the other concrete
//! genome tracks, with its feature index and filter remaining explicit inputs because no other
//! track uses them.
//!
//! The two coordinate projections are a small local enum, `FeatureProjection`. Both mappings are
//! immediate-mode calculations from the current [`ViewPosition`], supporting invariant 9's shared
//! columns and invariant 10's rule that tracks do not own horizontal position.
//!
//! Filtering selects visible transcript groups, but local-area rendering then restores the selected
//! group's complete exon structure so one matching exon does not destroy its transcript context.
//! The main trap is querying or filtering only the exons inside the window: off-screen exons still
//! determine the connector and arrow span, including when the viewport lies between exons.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
};

use crate::basepair::BasePair;
use crate::locus::{FeatureFilter, FeatureIndex, FeatureStrand, Locus};
use crate::style::{Features, Surface, Text};
#[cfg(test)]
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::painter;
use crate::viewer_app::viewport::ViewPosition;

use super::geometry::{AreaSpan, clamp_column, leading_columns};
#[cfg(test)]
use super::track::Contig;
use super::track::TrackView;

/// Fixed two-row detail-track height.
pub const FEATURES_TRACK_HEIGHT: u16 = 2;

/// Fixed two-row feature context below the local-area chart.
pub const LOCAL_AREA_FEATURES_TRACK_HEIGHT: u16 = 2;

const FEATURE_LABEL_ROW: u16 = 0;
const FEATURE_BAR_ROW: u16 = 1;
const TRANSCRIPT_ARROW_SPACING_COLUMNS: u64 = 5;
const HORIZONTAL_BAR: &str = "─";

/// Draws loaded features from the current view, index, and filter.
#[derive(Default)]
pub struct FeaturesTrack;

/// An enum rather than a trait or a shared struct: two mappings answer the same questions, and the
/// match arms are cheaper to read. Revisit when a third projection makes them expensive.
enum FeatureProjection {
    Detail {
        position: ViewPosition,
        contig_length: BasePair,
        leading: u16,
        bp_per_column: u64,
        area_width: u16,
    },
    LocalArea {
        span: AreaSpan,
        area_width: u16,
    },
}

impl FeatureProjection {
    fn query_range(&self) -> (BasePair, BasePair) {
        match self {
            Self::Detail { position, .. } => (position.left, position.left + position.width),
            Self::LocalArea { span, .. } => (span.start(), span.end()),
        }
    }

    fn columns(&self, locus: &Locus) -> Option<(u16, u16)> {
        self.range_columns(locus.start, locus.end)
    }

    fn range_columns(&self, start: BasePair, end: BasePair) -> Option<(u16, u16)> {
        match self {
            Self::Detail {
                position,
                contig_length,
                leading,
                bp_per_column,
                area_width,
            } => detail_range_columns(
                start,
                end,
                *position,
                *contig_length,
                *area_width,
                *leading,
                *bp_per_column,
            ),
            Self::LocalArea { span, area_width } => span.projected_columns(start, end, *area_width),
        }
    }

    fn arrow_spacing(&self) -> BasePair {
        match self {
            Self::Detail { bp_per_column, .. } => {
                BasePair::length(TRANSCRIPT_ARROW_SPACING_COLUMNS.saturating_mul(*bp_per_column).max(1))
            }
            Self::LocalArea { span, area_width } => BasePair::length(
                span.width()
                    .as_u64()
                    .saturating_mul(TRANSCRIPT_ARROW_SPACING_COLUMNS)
                    .div_ceil(u64::from((*area_width).max(1)))
                    .max(1),
            ),
        }
    }

    fn arrow_column(&self, bp: BasePair) -> Option<u16> {
        match self {
            Self::Detail {
                position,
                contig_length,
                leading,
                bp_per_column,
                area_width,
            } => {
                if bp < position.left || bp >= position.left + position.width || bp >= *contig_length {
                    return None;
                }
                Some(clamp_column(
                    (bp - position.left).as_u64() / bp_per_column,
                    *leading,
                    *area_width,
                ))
            }
            Self::LocalArea { span, area_width } => {
                if *area_width == 0 || span.width() == BasePair::new(0) || bp < span.start() || bp >= span.end() {
                    return None;
                }
                let column = u128::from((bp - span.start()).as_u64()) * u128::from(*area_width)
                    / u128::from(span.width().as_u64());
                Some(column.min(u128::from(*area_width - 1)) as u16)
            }
        }
    }
}

impl FeaturesTrack {
    pub fn new() -> Self {
        Self
    }

    /// Draws overlapping features and returns the number rejected by `filter`.
    pub(super) fn draw_features(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        feature_index: Option<&FeatureIndex>,
        filter: &FeatureFilter,
    ) -> usize {
        let TrackView {
            position,
            settings,
            contig,
        } = view;
        frame
            .buffer_mut()
            .set_style(area, Style::default().bg(Surface::TRACK_BG()));

        if area.height <= FEATURE_BAR_ROW {
            return 0;
        }
        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let leading = leading_columns(position.underflow, area.width, bp_per_column);
        let projection = FeatureProjection::Detail {
            position,
            contig_length: contig.length,
            leading,
            bp_per_column,
            area_width: area.width,
        };
        draw_feature_groups(frame, area, &projection, &contig.name, feature_index, filter)
    }

    /// Draws the two-row feature context aligned to the local-area span.
    pub(super) fn draw_local_area_features(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        feature_index: Option<&FeatureIndex>,
        filter: &FeatureFilter,
    ) {
        frame
            .buffer_mut()
            .set_style(area, Style::default().bg(Surface::TRACK_BG()));
        if area.height <= FEATURE_BAR_ROW {
            return;
        }

        let span = AreaSpan::for_viewport(view.position, view.settings.localareascale, view.contig.length);
        let projection = FeatureProjection::LocalArea {
            span,
            area_width: area.width,
        };
        let Some(feature_index) = feature_index else { return };
        let (query_start, query_end) = projection.query_range();
        let overlapping = feature_index.overlapping(&view.contig.name, query_start, query_end);
        let visible_candidates = feature_groups(overlapping.iter(), filter, false);
        let candidates = if visible_candidates.is_empty() {
            groups_covering(feature_index, &view.contig.name, query_start, query_end, filter, false)
        } else {
            visible_candidates
        };
        let Some(selected) = select_feature_group(candidates) else {
            return;
        };
        let selected_stem = selected.stem;
        let selected_group = feature_groups(
            feature_index.loci_for_stem(&view.contig.name, selected_stem),
            filter,
            true,
        )
        .into_iter()
        .find(|group| group.stem == selected_stem);
        let Some(selected_group) = selected_group else { return };
        draw_group(
            frame,
            area,
            &projection,
            &selected_group,
            FeatureLabelMode::Mini { stem: selected_stem },
        );
    }
}

#[derive(Clone, Copy)]
enum FeatureLabelMode<'a> {
    Full,
    Mini { stem: &'a str },
}

struct FeatureGroup<'a> {
    stem: &'a str,
    loci: Vec<&'a Locus>,
    matching_exons: usize,
    first_start: BasePair,
    strand: FeatureStrand,
}

fn feature_groups<'a>(
    loci: impl IntoIterator<Item = &'a Locus>,
    filter: &FeatureFilter,
    keep_unmatched: bool,
) -> Vec<FeatureGroup<'a>> {
    let mut groups: Vec<FeatureGroup<'a>> = Vec::new();
    for locus in loci {
        let matches = filter.matches(locus);
        if !keep_unmatched && !matches {
            continue;
        }
        let Some(group) = groups.iter_mut().find(|group| group.stem == locus.feature_stem()) else {
            groups.push(FeatureGroup {
                stem: locus.feature_stem(),
                loci: vec![locus],
                matching_exons: usize::from(matches),
                first_start: locus.start,
                strand: locus.strand(),
            });
            continue;
        };
        group.loci.push(locus);
        group.matching_exons += usize::from(matches);
        group.strand = merge_strand(group.strand, locus.strand());
    }
    groups.sort_by(|left, right| left.first_start.cmp(&right.first_start).then(left.stem.cmp(right.stem)));
    groups
}

fn merge_strand(left: FeatureStrand, right: FeatureStrand) -> FeatureStrand {
    if left == right { left } else { FeatureStrand::Unknown }
}

fn select_feature_group<'a>(mut groups: Vec<FeatureGroup<'a>>) -> Option<FeatureGroup<'a>> {
    groups.sort_by(|left, right| {
        right
            .matching_exons
            .cmp(&left.matching_exons)
            .then(left.first_start.cmp(&right.first_start))
            .then(left.stem.cmp(right.stem))
    });
    groups.into_iter().next()
}

fn groups_covering<'a>(
    feature_index: &'a FeatureIndex,
    contig_name: &str,
    query_start: BasePair,
    query_end: BasePair,
    filter: &FeatureFilter,
    keep_unmatched: bool,
) -> Vec<FeatureGroup<'a>> {
    feature_index
        .stems_covering(contig_name, query_start, query_end)
        .into_iter()
        .filter_map(|stem| {
            feature_groups(feature_index.loci_for_stem(contig_name, stem), filter, keep_unmatched)
                .into_iter()
                .find(|group| group.stem == stem)
        })
        .collect()
}

fn draw_feature_groups(
    frame: &mut Frame,
    area: Rect,
    projection: &FeatureProjection,
    contig_name: &str,
    feature_index: Option<&FeatureIndex>,
    filter: &FeatureFilter,
) -> usize {
    let Some(feature_index) = feature_index else { return 0 };
    let (query_start, query_end) = projection.query_range();
    let overlapping = feature_index.overlapping(contig_name, query_start, query_end);
    let filtered_out = overlapping.iter().filter(|locus| !filter.matches(locus)).count();
    for group in groups_covering(feature_index, contig_name, query_start, query_end, filter, false) {
        draw_group(frame, area, projection, &group, FeatureLabelMode::Full);
    }
    filtered_out
}

fn draw_group(
    frame: &mut Frame,
    area: Rect,
    projection: &FeatureProjection,
    group: &FeatureGroup<'_>,
    label_mode: FeatureLabelMode<'_>,
) {
    let Some(group_start) = group.loci.iter().map(|locus| locus.start).min() else {
        return;
    };
    let Some(group_end) = group.loci.iter().map(|locus| locus.end).max() else {
        return;
    };
    let Some((first_col, last_col)) = projection.range_columns(group_start, group_end) else {
        return;
    };
    let projected = group
        .loci
        .iter()
        .filter_map(|locus| projection.columns(locus).map(|columns| (*locus, columns)))
        .collect::<Vec<_>>();
    draw_connector(frame, area, first_col, last_col);
    for (locus, (start_col, end_col)) in &projected {
        draw_exon_block(frame, area, *start_col, *end_col);
        let label = match label_mode {
            FeatureLabelMode::Full => locus.key.as_ref(),
            FeatureLabelMode::Mini { .. } => locus.exon_number().unwrap_or(locus.key.as_ref()),
        };
        draw_label(
            frame,
            area,
            FEATURE_LABEL_ROW,
            *start_col,
            label,
            matches!(label_mode, FeatureLabelMode::Full),
        );
    }
    draw_arrows(frame, area, projection, group_start, group_end, group.strand);
    if let FeatureLabelMode::Mini { stem } = label_mode {
        draw_label(frame, area, FEATURE_LABEL_ROW, 0, stem, true);
    }
}

fn draw_connector(frame: &mut Frame, area: Rect, start_col: u16, end_col: u16) {
    for col in start_col..end_col {
        if let Some(cell) = frame.buffer_mut().cell_mut((area.x + col, area.y + FEATURE_BAR_ROW)) {
            painter::paint_cell(cell, HORIZONTAL_BAR, Features::INTRON_FG(), Surface::TRACK_BG());
        }
    }
}

fn draw_exon_block(frame: &mut Frame, area: Rect, start_col: u16, end_col: u16) {
    for col in start_col..end_col {
        if let Some(cell) = frame.buffer_mut().cell_mut((area.x + col, area.y + FEATURE_BAR_ROW)) {
            painter::paint_cell(cell, " ", Text::PRIMARY_FG(), Features::EXON_BG());
        }
    }
}

fn draw_arrows(
    frame: &mut Frame,
    area: Rect,
    projection: &FeatureProjection,
    start: BasePair,
    end: BasePair,
    strand: FeatureStrand,
) {
    let Some(arrow) = strand.arrow() else { return };
    let spacing = projection.arrow_spacing().as_u64();
    let phase = spacing / 2;
    let bucket = start.as_u64() / spacing;
    let mut bp = bucket.saturating_mul(spacing).saturating_add(phase);
    if bp < start.as_u64() {
        bp = bp.saturating_add(spacing);
    }
    while bp < end.as_u64() {
        if let Some(col) = projection.arrow_column(BasePair::new(bp))
            && let Some(cell) = frame.buffer_mut().cell_mut((area.x + col, area.y + FEATURE_BAR_ROW))
        {
            let background = cell.bg;
            painter::paint_cell(cell, arrow, Text::PRIMARY_FG(), background);
        }
        let Some(next) = bp.checked_add(spacing) else { break };
        bp = next;
    }
}

fn detail_range_columns(
    start: BasePair,
    end: BasePair,
    position: ViewPosition,
    contig_length: BasePair,
    area_width: u16,
    leading: u16,
    bp_per_column: u64,
) -> Option<(u16, u16)> {
    let clamped_start = start.max(position.left);
    let clamped_end = end.min(contig_length);
    if clamped_end <= clamped_start {
        return None;
    }

    let start_col = clamp_column(
        (clamped_start - position.left).as_u64() / bp_per_column,
        leading,
        area_width,
    );
    let end_col = clamp_column(
        (clamped_end - position.left).as_u64() / bp_per_column,
        leading,
        area_width,
    );
    if end_col <= start_col {
        None
    } else {
        Some((start_col, end_col))
    }
}

/// Paints a label on the requested row, clipped only at the pane edge.
fn draw_label(frame: &mut Frame, area: Rect, row: u16, start_col: u16, label: &str, bold: bool) {
    for (offset, ch) in label.chars().enumerate() {
        let col = start_col.saturating_add(offset as u16);
        if col >= area.width {
            break;
        }
        if let Some(cell) = frame.buffer_mut().cell_mut((area.x + col, area.y + row)) {
            painter::paint_cell(cell, &ch.to_string(), Text::PRIMARY_FG(), Surface::TRACK_BG());
            if bold {
                cell.modifier |= Modifier::BOLD;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::HashMap;

    use crate::viewer_app::genome::zoom::HorizontalZoom;

    fn locus(start: u64, end: u64, key: &str) -> Locus {
        Locus {
            start: BasePair::new(start),
            end: BasePair::new(end),
            key: key.into(),
            strand: FeatureStrand::Unknown,
        }
    }

    fn stranded_locus(start: u64, end: u64, key: &str, strand: FeatureStrand) -> Locus {
        Locus {
            start: BasePair::new(start),
            end: BasePair::new(end),
            key: key.into(),
            strand,
        }
    }

    fn index(loci: Vec<Locus>) -> FeatureIndex {
        let by_contig = HashMap::from([("chr1".to_string(), 0..loci.len())]);
        FeatureIndex::new(loci, by_contig)
    }

    fn contig(length: u64) -> Contig {
        Contig {
            name: "chr1".to_string(),
            length: BasePair::new(length),
        }
    }

    fn settings() -> GenomeViewerSettings {
        GenomeViewerSettings::new()
    }

    fn draw(feature_index: Option<&FeatureIndex>, position: ViewPosition, width: u16) -> ratatui::buffer::Buffer {
        draw_filtered(feature_index, position, width, &FeatureFilter::none()).0
    }

    fn draw_filtered(
        feature_index: Option<&FeatureIndex>,
        position: ViewPosition,
        width: u16,
        filter: &FeatureFilter,
    ) -> (ratatui::buffer::Buffer, usize) {
        draw_zoomed_filtered(feature_index, position, width, HorizontalZoom::Close, filter)
    }

    fn draw_zoomed_filtered(
        feature_index: Option<&FeatureIndex>,
        position: ViewPosition,
        width: u16,
        horizontal_zoom: HorizontalZoom,
        filter: &FeatureFilter,
    ) -> (ratatui::buffer::Buffer, usize) {
        draw_zoomed_filtered_on_contig(feature_index, position, width, horizontal_zoom, filter, 1_000_000)
    }

    fn draw_zoomed_filtered_on_contig(
        feature_index: Option<&FeatureIndex>,
        position: ViewPosition,
        width: u16,
        horizontal_zoom: HorizontalZoom,
        filter: &FeatureFilter,
        contig_length: u64,
    ) -> (ratatui::buffer::Buffer, usize) {
        let mut settings = settings();
        settings.horizontal_zoom = horizontal_zoom;
        let area = Rect::new(0, 0, width, FEATURES_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(width, FEATURES_TRACK_HEIGHT)).expect("backend");
        let mut track = FeaturesTrack::new();
        let mut filtered_out = 0;
        terminal
            .draw(|frame| {
                filtered_out = track.draw_features(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig(contig_length),
                    },
                    feature_index,
                    filter,
                )
            })
            .expect("draw");
        (terminal.backend().buffer().clone(), filtered_out)
    }

    fn draw_local_area(
        feature_index: Option<&FeatureIndex>,
        position: ViewPosition,
        width: u16,
        filter: &FeatureFilter,
    ) -> ratatui::buffer::Buffer {
        draw_local_area_with_settings(feature_index, position, width, filter, HorizontalZoom::Close, 1)
    }

    fn draw_local_area_with_settings(
        feature_index: Option<&FeatureIndex>,
        position: ViewPosition,
        width: u16,
        filter: &FeatureFilter,
        horizontal_zoom: HorizontalZoom,
        local_area_scale: u64,
    ) -> ratatui::buffer::Buffer {
        let mut settings = settings();
        settings.horizontal_zoom = horizontal_zoom;
        settings.localareascale = local_area_scale;
        let area = Rect::new(0, 0, width, LOCAL_AREA_FEATURES_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(width, LOCAL_AREA_FEATURES_TRACK_HEIGHT)).expect("backend");
        let mut track = FeaturesTrack::new();
        terminal
            .draw(|frame| {
                track.draw_local_area_features(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig(1_000),
                    },
                    feature_index,
                    filter,
                )
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }
    #[test]
    fn offscreen_adjacent_exons_extend_connector_and_arrows_to_the_viewport_edges() {
        let fi = index(vec![
            stranded_locus(45, 50, "GENE:NM_000001.1:ex1", FeatureStrand::Forward),
            stranded_locus(120, 125, "GENE:NM_000001.1:ex2", FeatureStrand::Forward),
        ]);
        assert_eq!(fi.loci_for_stem("chr1", "GENE:NM_000001.1").len(), 2);

        let next_buffer = draw(Some(&fi), ViewPosition::new(BasePair::new(40), BasePair::new(80)), 80);
        assert_eq!(next_buffer.cell((60, 1)).unwrap().symbol(), HORIZONTAL_BAR);
        assert_eq!(next_buffer.cell((77, 1)).unwrap().symbol(), ">");

        let previous_buffer = draw(Some(&fi), ViewPosition::new(BasePair::new(100), BasePair::new(80)), 80);
        assert_eq!(previous_buffer.cell((20, 1)).unwrap().symbol(), HORIZONTAL_BAR);
        assert_eq!(previous_buffer.cell((2, 1)).unwrap().symbol(), ">");
    }

    #[test]
    fn transcript_connector_is_drawn_when_the_viewport_is_between_exons() {
        let fi = index(vec![
            stranded_locus(10, 15, "GENE:NM_000001.1:ex1", FeatureStrand::Forward),
            stranded_locus(90, 95, "GENE:NM_000001.1:ex2", FeatureStrand::Forward),
        ]);
        let buffer = draw(Some(&fi), ViewPosition::new(BasePair::new(50), BasePair::new(20)), 80);

        assert_eq!(buffer.cell((10, 1)).unwrap().symbol(), HORIZONTAL_BAR);
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), ">");
    }

    #[test]
    fn transcript_arrows_follow_global_coordinates_when_the_viewport_shifts() {
        let fi = index(vec![
            stranded_locus(10, 15, "GENE:NM_000001.1:ex1", FeatureStrand::Forward),
            stranded_locus(90, 95, "GENE:NM_000001.1:ex2", FeatureStrand::Forward),
        ]);
        let first = draw(Some(&fi), ViewPosition::new(BasePair::new(50), BasePair::new(20)), 80);
        let shifted = draw(Some(&fi), ViewPosition::new(BasePair::new(51), BasePair::new(20)), 80);

        assert_eq!(first.cell((2, 1)).unwrap().symbol(), ">");
        assert_eq!(shifted.cell((1, 1)).unwrap().symbol(), ">");
        assert_ne!(shifted.cell((2, 1)).unwrap().symbol(), ">");
    }

    #[test]
    fn local_area_arrows_render_at_mid_zoom_when_one_bp_is_narrower_than_a_cell() {
        let fi = index(vec![
            stranded_locus(100, 120, "GENE:NM_000001.1:ex1", FeatureStrand::Forward),
            stranded_locus(500, 520, "GENE:NM_000001.1:ex2", FeatureStrand::Forward),
        ]);
        let buffer = draw_local_area_with_settings(
            Some(&fi),
            ViewPosition::new(BasePair::new(160), BasePair::new(160)),
            80,
            &FeatureFilter::none(),
            HorizontalZoom::Mid,
            2,
        );

        assert_eq!(buffer.cell((27, 1)).unwrap().symbol(), ">");
    }
    #[test]
    fn feature_filter_include_restricts_drawn_loci_and_counts_the_rest_as_filtered() {
        let _palette = crate::style::palette::lock_for_test();
        let fi = index(vec![
            locus(10, 20, "GENE:NM_000001.1:ex1"),
            locus(30, 40, "GENE:NM_000002.1:ex1"),
        ]);
        let position = ViewPosition::new(BasePair::new(40), BasePair::new(80));

        let (unfiltered, unfiltered_count) = draw_filtered(Some(&fi), position, 80, &FeatureFilter::none());
        assert!(
            (0..80).any(|col| unfiltered.cell((col, 1)).unwrap().bg == Features::EXON_BG()),
            "unfiltered loci must paint feature boxes"
        );
        assert_eq!(unfiltered_count, 0);

        let filter = FeatureFilter {
            include: vec!["NM_000001.1".to_string()],
            exclude: vec![],
        };
        let (filtered, filtered_count) = draw_filtered(Some(&fi), position, 80, &filter);
        assert!(
            filtered
                .cell((10, 1))
                .is_some_and(|cell| cell.bg == Features::EXON_BG()),
            "the matching locus must still draw"
        );
        assert!(
            (0..80)
                .filter_map(|col| filtered.cell((col, 1)).map(|cell| cell.bg))
                .filter(|&bg| bg == Features::EXON_BG())
                .count()
                < (0..80)
                    .filter_map(|col| unfiltered.cell((col, 1)).map(|cell| cell.bg))
                    .filter(|&bg| bg == Features::EXON_BG())
                    .count(),
            "the non-matching locus must be filtered out"
        );
        assert_eq!(
            filtered_count, 1,
            "the one non-matching locus should be counted as filtered"
        );
    }

    #[test]
    fn local_area_features_choose_the_most_visible_group_and_draw_its_exons() {
        let _palette = crate::style::palette::lock_for_test();
        let fi = index(vec![
            locus(100, 105, "GENE1:TRANSCRIPT_1:ex1"),
            locus(120, 125, "GENE2:TRANSCRIPT_2:ex1"),
            locus(150, 155, "GENE1:TRANSCRIPT_1:ex2"),
        ]);
        let position = ViewPosition::new(BasePair::new(150), BasePair::new(100));
        let buffer = draw_local_area(Some(&fi), position, 80, &FeatureFilter::none());
        // The 100..200 span maps to 80 columns: starts 100, 150, and 120 land at 0, 40, and 16.
        assert_eq!(buffer.cell((0, 1)).unwrap().bg, Features::EXON_BG());
        assert_eq!(buffer.cell((40, 1)).unwrap().bg, Features::EXON_BG());
        assert_ne!(buffer.cell((16, 1)).unwrap().bg, Features::EXON_BG());
    }

    #[test]
    fn local_area_filter_selects_a_group_and_the_group_keeps_all_its_exons() {
        let _palette = crate::style::palette::lock_for_test();
        let fi = index(vec![
            locus(70, 75, "GENE1:TRANSCRIPT_1:ex1"),
            locus(100, 105, "GENE2:TRANSCRIPT_2:ex1"),
            locus(150, 155, "GENE2:TRANSCRIPT_2:ex2"),
            locus(170, 175, "GENE1:TRANSCRIPT_1:ex2"),
        ]);
        let position = ViewPosition::new(BasePair::new(150), BasePair::new(100));
        let filter = FeatureFilter {
            include: vec!["GENE2:TRANSCRIPT_2:ex2".to_string()],
            exclude: vec![],
        };
        let buffer = draw_local_area(Some(&fi), position, 80, &filter);
        // The selected transcript has exons at 100 and 150; the other visible exon starts at 170.
        assert_eq!(buffer.cell((0, 1)).unwrap().bg, Features::EXON_BG());
        assert_eq!(buffer.cell((40, 1)).unwrap().bg, Features::EXON_BG());
        assert_ne!(buffer.cell((56, 1)).unwrap().bg, Features::EXON_BG());
    }

    #[test]
    fn local_area_group_selection_breaks_equal_counts_by_stem() {
        let loci = [
            locus(100, 105, "GENE_B:TRANSCRIPT_2:ex1"),
            locus(100, 105, "GENE_A:TRANSCRIPT_1:ex1"),
        ];
        let selected = select_feature_group(feature_groups(loci.iter(), &FeatureFilter::none(), false));
        assert_eq!(selected.map(|group| group.stem), Some("GENE_A:TRANSCRIPT_1"));
    }
    #[test]
    fn features_outside_the_visible_span_are_not_drawn() {
        let _palette = crate::style::palette::lock_for_test();
        let fi = index(vec![locus(10_000, 10_100, "far_away")]);
        let position = ViewPosition::new(BasePair::new(40), BasePair::new(80));
        let buf = draw(Some(&fi), position, 80);
        assert!((0..80).all(|col| buf.cell((col, FEATURE_BAR_ROW)).unwrap().bg != Features::EXON_BG()));
    }

    #[test]
    fn a_feature_past_contig_length_clips_at_the_contig_length_column() {
        let _palette = crate::style::palette::lock_for_test();
        let position = ViewPosition::new(BasePair::new(40), BasePair::new(80));
        let fi = index(vec![locus(40, 200, "x")]);
        let (buffer, _) = draw_zoomed_filtered_on_contig(
            Some(&fi),
            position,
            80,
            HorizontalZoom::Close,
            &FeatureFilter::none(),
            50,
        );
        for column in 40..50 {
            assert_eq!(buffer.cell((column, FEATURE_BAR_ROW)).unwrap().bg, Features::EXON_BG());
        }
        assert_eq!(buffer.cell((50, FEATURE_BAR_ROW)).unwrap().bg, Surface::TRACK_BG());
    }

    #[test]
    fn a_feature_starting_before_the_window_clamps_to_the_underflow_derived_left_column() {
        let _palette = crate::style::palette::lock_for_test();
        let position = ViewPosition::new(BasePair::new(5), BasePair::new(80));
        let fi = index(vec![locus(0, 10, "x")]);

        for (zoom, expected_columns) in [(HorizontalZoom::Close, 35..45), (HorizontalZoom::Mid, 17..22)] {
            let (buffer, _) = draw_zoomed_filtered(Some(&fi), position, 80, zoom, &FeatureFilter::none());
            for column in 0..80 {
                assert_eq!(
                    buffer.cell((column, FEATURE_BAR_ROW)).unwrap().bg == Features::EXON_BG(),
                    expected_columns.contains(&column),
                    "unexpected feature placement at {zoom:?} column {column}"
                );
            }
        }
    }

    #[test]
    fn drawing_a_feature_wider_than_u16_columns_clamps_at_the_pane_edge() {
        let _palette = crate::style::palette::lock_for_test();
        let fi = index(vec![locus(0, u64::from(u16::MAX) + 1, "long")]);
        let width = 80;
        let position = ViewPosition::new(BasePair(40), BasePair(80));
        let (buf, _) = draw_zoomed_filtered_on_contig(
            Some(&fi),
            position,
            width,
            HorizontalZoom::Close,
            &FeatureFilter::none(),
            1_000_000,
        );

        assert_eq!(buf.cell((width - 1, FEATURE_BAR_ROW)).unwrap().bg, Features::EXON_BG());
    }
}
