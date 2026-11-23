//! The genome viewer's viewport-scale context pane, between whole-contig and detail tracks.
//! `GenomeViewer` owns this concrete track and supplies the shell's `TrackView`; the track derives
//! its independent `AreaSpan` and keeps only summary/cache state, never a horizontal scroll offset.
//! Its lifecycle bridges the BAM-backed summary source to rendering: source replacement and contig
//! changes clear derived data, while `tick` progressively fills the current span without blocking
//! the detail view. `draw` composes the summary chart, viewport projection, and reference
//! band; `draw_divider` paints the span readout in its separate full-width pane.
//! The projection deliberately overpaints the chart's bottom row, making draw order the collision
//! rule rather than adding another row. This rests on invariants 6, 9, and 10: rows stay fixed,
//! every track pane preserves the shared genomic columns, and position is re-derived per draw.
//! The main trap is reading an incomplete summary as zero depth: missing coverage is fog, not data.
//! Note: roughly 80% of this file is its test module; the production track is comparatively small.

mod chart;
mod summary;

use std::ops::Range;

use ratatui::{Frame, buffer::Buffer, layout::Rect, style::Style};

use crate::basepair::BasePair;
use crate::residue::BaseResidue;
use crate::setting::ReadColor;
use crate::style::{Chrome, Coverage, LocalArea, Reference, Status, Surface, Text};
use crate::viewer_app::painter;

use super::depth_axis::draw_depth_axis;
use super::divider::{DividerGlyphs, draw_divider, layout_divider};
use super::geometry::AreaSpan;
use super::reference_data::{BaseHistogram, ColumnBases, ReferenceCounts, ReferenceData, ReferenceLookup};
use super::settings::GenomeViewerSettings;
use super::track::TrackView;
#[cfg(feature = "profiling")]
pub(super) use chart::{Breakpoints, draw_area_chart, draw_area_chart_5mc};
pub(super) use summary::OnDiskSummarySource;
#[cfg(any(test, feature = "profiling"))]
pub(super) use summary::SummaryBin;
use summary::{GridKey, SummaryCache, SummaryMode};
#[cfg(feature = "profiling")]
pub(super) use summary::{GridKey as ProfileGridKey, SummaryCache as ProfileSummaryCache, scan_summary};

/// Fixed development height: nine chart rows and one reference row. The viewport
/// marker is drawn over the chart's bottom row, so it costs no height of its own.
pub const LOCAL_AREA_TRACK_HEIGHT: u16 = 10;

/// One row above the track, naming the area span.
pub const LOCAL_AREA_DIVIDER_HEIGHT: u16 = 1;

const REFERENCE_BAND_ROW: u16 = 9;

/// The chart's bottom row, one above the reference band, where the viewport marker lands.
const VIEWPORT_MARKER_ROW: u16 = REFERENCE_BAND_ROW - 1;
const FULL_BLOCK: char = '█';
const VIEWPORT_LABEL: &str = "< Pileup view location";
const PENDING_GLYPH: char = '·';
const FAILED_GLYPH: char = '×';
const OUT_OF_CONTIG_GLYPH: char = '-';
const NO_REFERENCE_LABEL: &str = "no reference loaded";
const TOO_WIDE_LABEL: &str = "reference span too wide";
const FOG_GLYPH: char = '\u{28ff}';

/// Local-area context with a progressively resident alignment summary.
pub struct LocalAreaTrack {
    summary: SummaryCache,
}

impl LocalAreaTrack {
    pub fn new() -> Self {
        Self {
            summary: SummaryCache::new(),
        }
    }

    pub(super) fn set_source(&mut self, source: OnDiskSummarySource) {
        self.summary.set_source(source);
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn set_resident_source(&mut self, source: super::alignment::ResidentBamSource) {
        self.summary.set_resident_source(source);
    }

    pub(super) fn clear_contig(&mut self) {
        self.summary.clear();
    }

    pub(super) fn tick(&mut self, enabled: bool) -> bool {
        self.summary.tick(enabled)
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.summary.is_fetching()
    }

    pub(super) fn is_waiting_for_reference(&self) -> bool {
        self.summary.is_waiting_for_reference()
    }

    pub(super) fn resume_after_reference(&mut self, reference: ReferenceLookup<'_>) {
        self.summary.prepare_scan(Some(reference));
    }

    pub(super) fn take_errors(&mut self) -> Vec<String> {
        self.summary.take_errors()
    }

    #[cfg(test)]
    pub(super) fn has_source(&self) -> bool {
        self.summary.has_source()
    }

    #[cfg(test)]
    pub(super) fn seed_summary_bins(&mut self, contig: &str, span: AreaSpan, bins: Vec<SummaryBin>) {
        self.summary.seed_bins(contig, span, bins);
    }

    /// Draws the local area and returns the span shared by its divider and mouse navigation.
    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        reference: &mut ReferenceData,
        centreline_read_spans: &[Range<BasePair>],
    ) -> AreaSpan {
        let span = AreaSpan::for_viewport(view.position, view.settings.localareascale, view.contig.length);
        let buffer = frame.buffer_mut();
        buffer.set_style(area, Style::default().bg(Surface::TRACK_BG()));
        let is_5mc = view.settings.readcolor == ReadColor::FiveMc && self.summary.supports_5mc();
        if is_5mc {
            buffer.set_style(chart_area(area), Style::default().bg(Coverage::BG()));
        }
        if area.width == 0 || area.height == 0 || view.contig.length == BasePair(0) {
            return span;
        }

        let mut key = GridKey::for_span(&view.contig.name, span);
        let mut reference = ReferenceLookup::new(reference, view.contig);
        let (counts, reference_fits) = if is_5mc {
            (ReferenceCounts::BasesAndCpg, key.reference_fits(&reference))
        } else {
            let counts = ReferenceCounts::Bases;
            (counts, reference.reference_band_fits(span.start(), span.end(), counts))
        };
        if is_5mc && reference.has_real_reference() && reference_fits {
            key.mode = SummaryMode::FiveMc {
                min_cpgs: view.settings.localareamin5mc,
            };
        }
        self.summary.ensure(key, Some(reference.reborrow()));
        let max_depth = if is_5mc {
            chart::draw_area_chart_5mc(buffer, area, span, &self.summary)
        } else {
            let breakpoints = chart::Breakpoints {
                enabled: view.settings.localareabreakpoints,
                angle: view.settings.localareabreakangle as f64,
            };
            chart::draw_area_chart(buffer, area, span, &self.summary, breakpoints, centreline_read_spans)
        };
        if area.height > VIEWPORT_MARKER_ROW {
            draw_viewport_marker(buffer, area, span, view);
        }
        if let Some(max) = max_depth {
            draw_depth_axis(
                buffer,
                chart_area(area),
                REFERENCE_BAND_ROW,
                max.round(),
                (max / 2.0).round(),
            );
        }
        if area.height > REFERENCE_BAND_ROW {
            draw_area_reference(buffer, area, span, reference, counts, reference_fits);
        }
        span
    }

    /// Draws the span readout above the track.
    pub(super) fn draw_divider(&self, frame: &mut Frame, area: Rect, span: AreaSpan, settings: &GenomeViewerSettings) {
        if area.width == 0 || area.height == 0 || span.width() == BasePair(0) {
            return;
        }

        let label = format!("Region span: {}bp", span.width().format_with_commas());
        let row = layout_divider(
            area.width,
            &label,
            None,
            span.start()..span.end(),
            DividerGlyphs::for_settings(settings),
        );
        draw_divider(frame.buffer_mut(), area, &row);
    }
}

/// The chart rows above the reference band.
pub(super) fn chart_area(area: Rect) -> Rect {
    Rect {
        height: area.height.min(REFERENCE_BAND_ROW),
        ..area
    }
}

/// Marks the detail viewport's projection over the chart's bottom row.
///
/// Drawn after the chart and before the reference band, so it takes the coverage cells it covers
/// outright: at this scale the box is what the reader is looking for, and the depth under it is
/// still readable in the eight rows above. Its label stays immediately to the right and is skipped
/// whole when that position cannot hold it.
fn draw_viewport_marker(buffer: &mut Buffer, area: Rect, span: AreaSpan, view: TrackView<'_>) {
    let row = area.y + VIEWPORT_MARKER_ROW;
    let projected_start = view.position.left;
    let projected_width = view.position.width - view.position.underflow;
    let projected_end = (projected_start + projected_width).min(view.contig.length);
    let Some((start, end)) = span.projected_columns(projected_start, projected_end, area.width) else {
        return;
    };
    // The full block supplies the marker foreground; keep its background on the track surface so
    // the two channels retain their separate roles.
    let style = Style::default()
        .fg(LocalArea::VIEWPORT_MARKER_FG())
        .bg(Surface::TRACK_BG());
    for column in start..end {
        if let Some(cell) = buffer.cell_mut((area.x + column, row)) {
            cell.set_char(FULL_BLOCK);
            cell.set_style(style);
        }
    }

    if VIEWPORT_LABEL.chars().count() > usize::from(area.width - end) {
        return;
    }
    for (offset, ch) in VIEWPORT_LABEL.chars().enumerate() {
        if let Some(cell) = buffer.cell_mut((area.x + end + offset as u16, row)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

fn draw_area_reference(
    buffer: &mut Buffer,
    area: Rect,
    span: AreaSpan,
    mut reference: ReferenceLookup<'_>,
    counts: ReferenceCounts,
    reference_fits: bool,
) {
    let row = area.y + REFERENCE_BAND_ROW;
    if counts == ReferenceCounts::BasesAndCpg && !reference.has_real_reference() {
        draw_band_label(buffer, area, row, NO_REFERENCE_LABEL);
        return;
    }
    if !reference_fits {
        draw_band_label(buffer, area, row, TOO_WIDE_LABEL);
        return;
    }

    for column in 0..area.width {
        let (start, end) = column_span(span, column, area.width);
        let bases = reference.column_bases(start, end, counts);
        if let Some(cell) = buffer.cell_mut((area.x + column, row)) {
            draw_reference_cell(cell, bases, counts);
        }
    }
}

fn draw_band_label(buffer: &mut Buffer, area: Rect, row: u16, label: &str) {
    for (column, ch) in label.chars().take(usize::from(area.width)).enumerate() {
        if let Some(cell) = buffer.cell_mut((area.x + column as u16, row)) {
            cell.set_char(ch);
            cell.set_style(Style::default().fg(Text::DIMMED_FG()).bg(Surface::TRACK_BG()));
        }
    }
}

pub(super) fn column_span(span: AreaSpan, column: u16, columns: u16) -> (BasePair, BasePair) {
    let width = u128::from(span.width().as_u64());
    let columns = u128::from(columns);
    let start_offset = u128::from(column) * width / columns;
    let end_offset = (u128::from(column) + 1) * width / columns;
    let start = span.start() + BasePair(start_offset as u64);
    let end = (span.start() + BasePair(end_offset.max(start_offset + 1) as u64)).min(span.end());
    (start, end)
}

fn draw_reference_cell(cell: &mut ratatui::buffer::Cell, bases: ColumnBases, counts: ReferenceCounts) {
    match bases {
        ColumnBases::Complete(histogram) => match counts {
            ReferenceCounts::Bases => {
                let pair = Reference::base_color_pair(modal_base(histogram));
                painter::paint_cell(cell, painter::LEFT_HALF_BLOCK, pair.0, pair.1);
            }
            ReferenceCounts::BasesAndCpg => {
                let valid: u32 = histogram.acgt.into_iter().sum();
                // Match `modal_base`: a mostly-N gap column with a stray CG must read as N rather
                // than spiking to maximum CpG density.
                let color = if valid == 0 || histogram.invalid > valid {
                    Reference::BASE_N()
                } else {
                    LocalArea::cpg_density_color(f64::from(histogram.cpg) / f64::from(valid))
                };
                paint_state(cell, FULL_BLOCK, color, Surface::TRACK_BG());
            }
        },
        ColumnBases::Partial => paint_state(cell, PENDING_GLYPH, Text::DIMMED_FG(), Surface::TRACK_BG()),
        ColumnBases::Failed => paint_state(cell, FAILED_GLYPH, Text::ON_STATUS_FG(), Status::ERROR_BG()),
        ColumnBases::NoReference => paint_state(cell, ' ', Text::DIMMED_FG(), Surface::TRACK_BG()),
        ColumnBases::OutOfContig => {
            paint_state(cell, OUT_OF_CONTIG_GLYPH, Text::DIMMED_FG(), Chrome::OUT_OF_BOUNDS_BG())
        }
    }
}

/// The most common residue in a column, counting ambiguity codes as `N`.
///
/// `invalid` competes rather than being ignored: assembly gaps and telomeric
/// N-runs are megabase-scale, so a column that is mostly `N` must read as `N`
/// instead of promoting whichever stray base happened to appear. ACGT win a
/// tie against `N`, and among themselves the earliest index wins.
fn modal_base(histogram: BaseHistogram) -> BaseResidue {
    let [a, c, g, t] = histogram.acgt;
    let base_max = [a, c, g, t].into_iter().max().unwrap_or(0);
    if base_max == 0 || histogram.invalid > base_max {
        return BaseResidue::N;
    }
    match [a, c, g, t].iter().position(|&count| count == base_max).unwrap_or(0) {
        0 => BaseResidue::A,
        1 => BaseResidue::C,
        2 => BaseResidue::G,
        _ => BaseResidue::T,
    }
}

fn paint_state(
    cell: &mut ratatui::buffer::Cell,
    glyph: char,
    foreground: ratatui::style::Color,
    background: ratatui::style::Color,
) {
    cell.set_char(glyph);
    cell.set_style(Style::default().fg(foreground).bg(background));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
    use crate::style::palette::{lock_for_test, ramp};
    use crate::viewer_app::genome::settings::GenomeViewerSettings;
    use crate::viewer_app::genome::track::Contig;
    use crate::viewer_app::genome::zoom::HorizontalZoom;
    use crate::viewer_app::viewport::ViewPosition;
    use ratatui::{Terminal, backend::TestBackend};

    fn contig(length: u64) -> Contig {
        Contig {
            name: "chr1".to_string(),
            length: BasePair(length),
        }
    }

    fn render_marker(centre: u64, viewport_width: u64, scale: u64, contig_length: u64, pane_width: u16) -> Vec<u16> {
        let contig = contig(contig_length);
        let position = ViewPosition::new(BasePair(centre), BasePair(viewport_width));
        let span = AreaSpan::for_viewport(position, scale, contig.length);
        let end = (position.left + position.width - position.underflow).min(contig.length);
        span.projected_columns(position.left, end, pane_width)
            .map_or_else(Vec::new, |(start, end)| (start..end).collect())
    }

    #[test]
    fn summary_residency_and_rendered_columns_follow_distinct_seeded_bins() {
        let _palette = lock_for_test();
        let position = ViewPosition::new(BasePair(18), BasePair(18));
        let contig = contig(36);
        let settings = {
            let mut value = GenomeViewerSettings::new();
            value.localareascale = 2;
            value
        };
        let span = AreaSpan::for_viewport(position, settings.localareascale, contig.length);
        // Three bases per column. The right half of the last resident column averages 6 and 7.
        let mut bins = vec![SummaryBin::default(); 15];
        bins.extend([1, 1, 3, 3, 3, 4, 4, 4, 6, 6, 6, 7].map(|depth| SummaryBin {
            spanning_depth: depth,
            ..SummaryBin::default()
        }));
        let mut track = LocalAreaTrack::new();
        track.seed_summary_bins(&contig.name, span, bins);
        assert!(track.summary.covers(span.start(), span.start() + BasePair(27)));
        assert!(!track.summary.covers(span.start() + BasePair(27), span.end()));

        let mut terminal = Terminal::new(TestBackend::new(12, LOCAL_AREA_TRACK_HEIGHT)).expect("backend");
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    frame.area(),
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig,
                    },
                    &mut reference,
                    &[],
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 0)].symbol().parse::<u32>(), Ok(7));
        assert_eq!(buffer[(1, 4)].symbol().parse::<u32>(), Ok(3));
        let chart_pixels = |column: u16| {
            (1..VIEWPORT_MARKER_ROW)
                .filter(|&row| buffer.cell((column, row)).expect("chart cell").symbol() != " ")
                .count()
        };
        let observed: Vec<_> = (5..9).map(chart_pixels).collect();
        assert_eq!(observed, [2, 4, 6, 7], "columns 5..9 project bins 15..27 in order");
    }

    #[test]
    fn viewport_marker_projection_covers_expected_edges_scales_and_pane_widths() {
        let cases: [(u64, u64, u64, u64, u16, Vec<u16>); 8] = [
            (0, 100, 10, 1_000, 40, vec![0, 1]),
            (500, 100, 10, 1_000, 40, vec![18, 19, 20, 21]),
            (1_000, 100, 10, 1_000, 40, vec![38, 39]),
            (500, 100, 2, 1_000, 40, (10..30).collect()),
            (500, 200, 100, 1_000, 40, (16..24).collect()),
            (40, 100, 10_000, 80, 40, (0..40).collect()),
            (500, 1, 10_000, 1_000, 1, vec![0]),
            (500, 1, 10_000, 1_000, 200, vec![100]),
        ];
        for (centre, viewport, scale, contig_length, width, expected) in cases {
            assert_eq!(render_marker(centre, viewport, scale, contig_length, width), expected);
        }
    }

    #[test]
    fn reference_bucket_intervals_partition_wide_spans_and_repeat_short_spans() {
        for (span_width, columns) in [(13u64, 4u16), (17, 8), (3, 8), (1, 4)] {
            let span = AreaSpan::for_viewport(
                ViewPosition::new(BasePair(span_width / 2), BasePair(span_width)),
                1,
                BasePair(span_width),
            );
            let buckets: Vec<_> = (0..columns).map(|column| column_span(span, column, columns)).collect();
            for (column, &(start, end)) in buckets.iter().enumerate() {
                let offset = span.start().as_u64();
                let expected_start = offset + (column as u64 * span_width / u64::from(columns));
                let expected_end =
                    offset + (((column as u64 + 1) * span_width / u64::from(columns)).max(expected_start - offset + 1));
                assert_eq!(
                    (start.as_u64(), end.as_u64()),
                    (expected_start, expected_end.min(span.end().as_u64()))
                );
                assert!(start < end);
                if column > 0 && span_width >= u64::from(columns) {
                    assert_eq!(buckets[column - 1].1, start, "wide buckets must meet without gaps");
                }
            }
        }
    }

    #[test]
    fn modal_reference_counts_invalid_bases_and_uses_acgt_tie_order() {
        for (histogram, expected) in [
            (
                BaseHistogram {
                    acgt: [0, 0, 0, 0],
                    invalid: 3,
                    cpg: 0,
                },
                BaseResidue::N,
            ),
            (
                BaseHistogram {
                    acgt: [1, 0, 0, 0],
                    invalid: 2,
                    cpg: 0,
                },
                BaseResidue::N,
            ),
            (
                BaseHistogram {
                    acgt: [2, 0, 0, 0],
                    invalid: 2,
                    cpg: 0,
                },
                BaseResidue::A,
            ),
            (
                BaseHistogram {
                    acgt: [1, 1, 1, 1],
                    invalid: 1,
                    cpg: 0,
                },
                BaseResidue::A,
            ),
            (
                BaseHistogram {
                    acgt: [0, 3, 3, 0],
                    invalid: 0,
                    cpg: 0,
                },
                BaseResidue::C,
            ),
        ] {
            assert_eq!(modal_base(histogram), expected);
        }

        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        reference.seed_chunk("chr1", BasePair(0), b"ACGTNACG");
        for (start, end, expected) in [
            (
                0,
                4,
                BaseHistogram {
                    acgt: [1, 1, 1, 1],
                    invalid: 0,
                    cpg: 0,
                },
            ),
            (
                1,
                6,
                BaseHistogram {
                    acgt: [1, 1, 1, 1],
                    invalid: 1,
                    cpg: 0,
                },
            ),
            (
                4,
                8,
                BaseHistogram {
                    acgt: [1, 1, 1, 0],
                    invalid: 1,
                    cpg: 0,
                },
            ),
        ] {
            let ColumnBases::Complete(histogram) = reference.column_bases(
                "chr1",
                BasePair(start),
                BasePair(end),
                BasePair(8),
                ReferenceCounts::Bases,
            ) else {
                panic!("seeded reference span should be complete");
            };
            assert_eq!(histogram, expected);
            assert_eq!(modal_base(histogram), BaseResidue::A);
        }
    }

    #[test]
    fn reference_density_band_projects_cpgs_to_the_cytosines_column() {
        let _palette = lock_for_test();
        let mut sequence = [b'A'; 160];
        sequence[..40].fill(b'N');
        sequence[40..42].copy_from_slice(b"CG");
        sequence[59..61].copy_from_slice(b"cG");
        sequence[73..75].copy_from_slice(b"CG");
        sequence[100..118].fill(b'N');
        sequence[118..120].copy_from_slice(b"CG");
        let contig = contig(sequence.len() as u64);
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        reference.seed_chunk(&contig.name, BasePair(0), &sequence);
        let area = Rect::new(1, 1, 4, LOCAL_AREA_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("backend");
        let mut track = LocalAreaTrack::new();
        let mut settings = GenomeViewerSettings::new();
        settings.localareascale = 20;
        settings.readcolor = ReadColor::FiveMc;

        // Close covers [40, 120) in 20 bp columns (col 3 is 18 N + 1 CG -> BASE_N);
        // Mid widens to [0, 160) in 40 bp columns (col 0 all-N, col 2 has 22 valid vs 18 N).
        for (zoom, expected) in [
            (
                HorizontalZoom::Close,
                [ramp::cyan(0), ramp::cyan(2), ramp::cyan(4), Reference::BASE_N()],
            ),
            (
                HorizontalZoom::Mid,
                [Reference::BASE_N(), ramp::cyan(1), ramp::cyan(2), ramp::cyan(4)],
            ),
        ] {
            settings.horizontal_zoom = zoom;
            let viewport_width = BasePair(u64::from(area.width) * zoom.bp_per_column());
            let position = ViewPosition::new(contig.length / 2, viewport_width);
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position,
                            settings: &settings,
                            contig: &contig,
                        },
                        &mut reference,
                        &[],
                    );
                })
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let observed: Vec<_> = (area.left()..area.right())
                .map(|column| buffer.cell((column, area.y + REFERENCE_BAND_ROW)).unwrap().fg)
                .collect();
            assert_eq!(observed, expected);
        }
    }

    #[test]
    fn methylation_chart_projects_gated_bam_calls_onto_full_columns() {
        use std::fs;
        use std::time::{Duration, Instant};

        use noodles::{bam, fasta};

        let _palette = lock_for_test();
        let dir = std::env::temp_dir().join(format!("apus_local_area_5mc_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fasta_path = dir.join("reference.fa");
        let mut sequence = [b'A'; 1_026];
        for start in [0, 168, 172, 516] {
            sequence[start..start + 4].copy_from_slice(b"CGCG");
        }
        sequence[400..402].copy_from_slice(b"CG");
        let sequence = std::str::from_utf8(&sequence).unwrap();
        fs::write(&fasta_path, format!(">chr1\n{sequence}\n")).unwrap();
        fasta::fai::fs::write(dir.join("reference.fa.fai"), &fasta::fs::index(&fasta_path).unwrap()).unwrap();
        let genome = IndexedFasta::from_file(&fasta_path).unwrap();
        let bam_path = dir.join("reads.bam");
        let sam = format!(
            concat!(
                "calls\t0\tchr1\t1\t60\t1026M\t*\t0\t0\t{}\t*\tMM:Z:C+m?,0,0,0,0,0,0,0;\tML:B:C,255,255,0,0,32,32,247\n",
                "indels\t0\tchr1\t1\t60\t350M100D200M140I376M\t*\t0\t0\t{}\t*\n",
                "right1\t0\tchr1\t514\t60\t513M\t*\t0\t0\t{}\t*\n",
                "right2\t0\tchr1\t514\t60\t513M\t*\t0\t0\t{}\t*\n",
            ),
            sequence,
            "A".repeat(1_066),
            &sequence[513..],
            &sequence[513..],
        );
        let header = crate::testgen::header_from_ref_index(&genome.index);
        crate::testgen::write_bam_from_sam(fs::File::create(&bam_path).unwrap(), &header, &sam).unwrap();
        bam::bai::fs::write(dir.join("reads.bam.bai"), &bam::fs::index(&bam_path).unwrap()).unwrap();

        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut track = LocalAreaTrack::new();
        track.set_source(OnDiskSummarySource::from_path(&bam_path).unwrap());
        let contig = contig(sequence.len() as u64);
        let position = ViewPosition::new(contig.length / 2, contig.length / 2);
        let mut settings = GenomeViewerSettings::new();
        settings.localareascale = 2;
        settings.readcolor = ReadColor::FiveMc;
        settings.localareamin5mc = 2;
        settings.localareabreakpoints = true;
        settings.localareabreakangle = 1;
        let area = Rect::new(1, 1, 6, LOCAL_AREA_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, track: &mut LocalAreaTrack, reference: &mut ReferenceData| {
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position,
                            settings: &settings,
                            contig: &contig,
                        },
                        reference,
                        &[],
                    );
                })
                .unwrap();
        };
        draw(&mut terminal, &mut track, &mut reference);
        assert!(
            track.is_fetching(),
            "the unresolved reference gates must keep the track pending"
        );
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((area.x, area.y + 4)).unwrap().symbol(),
            FOG_GLYPH.to_string()
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        while track.is_fetching() {
            assert!(Instant::now() < deadline, "local-area methylation did not settle");
            track.tick(true);
            reference.poll();
            if track.is_waiting_for_reference() && !reference.is_fetching() {
                track.resume_after_reference(ReferenceLookup::new(&mut reference, &contig));
            }
            std::thread::yield_now();
        }
        assert!(track.take_errors().is_empty());
        assert!(reference.take_error().is_none());
        draw(&mut terminal, &mut track, &mut reference);
        fs::remove_dir_all(dir).unwrap();

        let rendered = terminal.backend().buffer();
        for (row, depth) in [(0, 4), (4, 2), (8, 0)] {
            let label = rendered[(area.x + 1, area.y + row)].symbol();
            assert_eq!(label.parse::<u32>(), Ok(depth));
        }

        // At the 171 bp column boundary, the 4 bp bin at 168 contributes three bases
        // left and one right. The high call at 400 is below the two-site gate. Inspect the
        // raw chart since the depth axis overlays the first column in the composed track.
        let no_data = LocalArea::MOD_5MC_NO_DATA();
        let expected = [ramp::grey(10), ramp::grey(21), no_data, no_data, no_data, no_data];
        let mut buffer = Buffer::empty(area);
        let span = AreaSpan::for_viewport(position, settings.localareascale, contig.length);
        chart::draw_area_chart_5mc(&mut buffer, area, span, &track.summary);
        let foregrounds: Vec<_> = (area.left()..area.right())
            .map(|x| buffer.cell((x, area.y + 7)).unwrap().fg)
            .collect();
        assert_eq!(foregrounds, expected);
        for x in area.left()..area.right() {
            for y in area.y + 1..area.y + VIEWPORT_MARKER_ROW {
                assert!(matches!(buffer.cell((x, y)).unwrap().symbol(), " " | "▄" | "█"));
            }
        }
        assert_eq!(buffer.cell((area.x, area.y + 4)).unwrap().symbol(), "▄");
        assert_eq!(buffer.cell((area.x + 5, area.y + 4)).unwrap().symbol(), "█");
    }

    #[test]
    fn fallback_reference_in_5mc_mode_reports_missing_reference_instead_of_an_error() {
        let _palette = lock_for_test();
        let contig = contig(1_000);
        let position = ViewPosition::new(BasePair(500), BasePair(30));
        let area = Rect::new(0, 0, 30, LOCAL_AREA_TRACK_HEIGHT);
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        let mut track = LocalAreaTrack::new();
        let mut settings = GenomeViewerSettings::new();
        settings.localareascale = 2;
        settings.readcolor = ReadColor::FiveMc;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("backend");
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig,
                    },
                    &mut reference,
                    &[],
                );
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let band: String = (0..NO_REFERENCE_LABEL.len() as u16)
            .map(|column| buffer.cell((column, REFERENCE_BAND_ROW)).unwrap().symbol())
            .collect();
        assert_eq!(band, NO_REFERENCE_LABEL);
        assert!(
            (0..area.width).all(|column| buffer.cell((column, REFERENCE_BAND_ROW)).unwrap().bg != Status::ERROR_BG())
        );
    }

    #[test]
    fn over_budget_reference_projection_does_not_submit_chunk_fetches() {
        let _palette = lock_for_test();
        let chunk = BasePair(32 * 1024);
        let contig_length = chunk * BasePair(19);
        let mut settings = GenomeViewerSettings::new();
        settings.localareascale = 2;
        let contig = contig(contig_length.as_u64());
        let position = ViewPosition::new(contig_length / 2, chunk * BasePair(10));
        let area = Rect::new(0, 0, 30, LOCAL_AREA_TRACK_HEIGHT);
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").expect("reference fixture");
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut track = LocalAreaTrack::new();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("backend");
        for readcolor in [ReadColor::Lane, ReadColor::FiveMc] {
            settings.readcolor = readcolor;
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position,
                            settings: &settings,
                            contig: &contig,
                        },
                        &mut reference,
                        &[],
                    );
                })
                .expect("draw");
            assert_eq!(reference.pending_chunk_count(), 0);
            let buffer = terminal.backend().buffer();
            let band: String = (0..TOO_WIDE_LABEL.len() as u16)
                .map(|column| buffer.cell((column, REFERENCE_BAND_ROW)).unwrap().symbol())
                .collect();
            assert_eq!(band, TOO_WIDE_LABEL);
        }

        // The visible span fits by itself. Rounding its end up to the grid boundary
        // puts the CpG lookahead in a nineteenth chunk.
        let width = chunk * BasePair(18) - BasePair(2);
        let position = ViewPosition::new(width / 2, width / 2);
        let span = AreaSpan::for_viewport(position, settings.localareascale, contig.length);
        assert!(reference.reference_band_fits(span.start(), span.end(), contig.length, ReferenceCounts::BasesAndCpg));
        track.seed_summary_bins(
            &contig.name,
            span,
            vec![
                summary::SummaryBin {
                    spanning_depth: 2,
                    ..summary::SummaryBin::default()
                };
                512
            ],
        );
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig,
                    },
                    &mut reference,
                    &[],
                );
            })
            .unwrap();
        assert_eq!(reference.pending_chunk_count(), 0);
        let buffer = terminal.backend().buffer();
        let band: String = (0..TOO_WIDE_LABEL.len() as u16)
            .map(|column| buffer.cell((column, REFERENCE_BAND_ROW)).unwrap().symbol())
            .collect();
        assert_eq!(band, TOO_WIDE_LABEL);
        assert_eq!(buffer.cell((1, 7)).unwrap().fg, LocalArea::MOD_5MC_NO_DATA());
    }
}
