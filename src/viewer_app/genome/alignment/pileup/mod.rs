//! Base-space pileup orchestration, classification, and reduction.
//!
//! This module is the mediator between the alignment cache, the reference service, the framebuffer,
//! and the terminal track. It resolves reads in the visible lane range, keeps geometry in genomic
//! bp and lane coordinates, and returns centreline observations for the caller's post-reduction
//! overlays. The caller computes dimensions once so the leading underflow columns, framebuffer
//! size, and terminal write origin cannot drift apart.
//!
//! `classify` supplies reusable per-base meaning and `paint` applies the layer order; this module
//! owns neither a concrete BAM source nor terminal-cell styling. Horizontal and vertical zoom are
//! passed as scale, so letters, arrows, and half-cell reduction obey the same reachable LOD states
//! described in [`crate::viewer_app::genome::zoom`].
//!
//! Note: roughly 90% of this file is its test module; the production renderer itself is small.

mod camera;
mod classify;
mod paint;

use std::ops::Range;

use paint::{LaneGlyphs, paint_expansion_lane, paint_one_lane_against};

use ratatui::layout::Rect;

use crate::basepair::BasePair;
use crate::setting::ReadColor;
use crate::style::Chrome;
use crate::viewer_app::framebuffer::Framebuffer;
use crate::viewer_app::genome::geometry::leading_columns;
use crate::viewer_app::genome::reference_data::ReferenceLookup;
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::viewport::ViewPosition;

use super::super::track::Contig;
use super::centline::centline_px;
use super::read::Read;
#[cfg(test)]
use super::read_cache::WindowState;
use super::{
    VerticalView,
    read_cache::{LaneRead, ReadSource, ReadWindowCache},
};
#[cfg(feature = "profiling")]
pub(in crate::viewer_app::genome) use classify::LaneCoverage as ProfileLaneCoverage;
use classify::{BaseKind, ExpandedInsertion, LaneCoverage, visible_expanded_insertions};

/// Minimum insertion length expanded into a synthetic row beneath its parent read when
/// `:set expandinsertions` is active.
pub(super) const EXPAND_INSERTION_MIN_LEN: BasePair = BasePair(50);

/// One visual framebuffer lane: either a real read-packing lane or a synthetic expansion sub-lane.
enum DisplayLane<'a> {
    Reads {
        logical_lane: usize,
        reads: Vec<&'a Read>,
    },
    Expansion {
        logical_lane: usize,
        spans: Vec<ExpandedInsertion>,
    },
}

/// Packs qualifying visible insertion spans under one logical lane into the fewest non-overlapping
/// sub-lanes, reusing the first row whose previous span ends at or before `span.start`.
fn pack_expansions(mut spans: Vec<ExpandedInsertion>) -> Vec<Vec<ExpandedInsertion>> {
    spans.sort_by_key(|span| (span.start, span.length));
    let mut sub_lanes: Vec<Vec<ExpandedInsertion>> = Vec::new();
    for span in spans {
        if let Some(lane) = sub_lanes
            .iter_mut()
            .find(|lane| lane.last().is_some_and(|prev| prev.start + prev.length <= span.start))
        {
            lane.push(span);
        } else {
            sub_lanes.push(vec![span]);
        }
    }
    sub_lanes
}

/// Builds the `visible_height` display lanes starting at `top_visual_lane`.
///
/// When `expand_insertions` is false, display lanes map 1:1 to logical lanes without scanning
/// CIGARs. When true, logical lanes `0..=max_lane` are flattened with their viewport-visible
/// expansion sub-lanes, and any visual lanes past the last occupied logical lane remain plain
/// empty logical lanes so scrolling past the pileup stays consistent.
fn build_visible_lanes<'a>(
    lane_reads: &[LaneRead<'a>],
    top_visual_lane: usize,
    visible_height: usize,
    view_start: BasePair,
    view_end: BasePair,
    expand_insertions: bool,
) -> Vec<DisplayLane<'a>> {
    if !expand_insertions {
        let mut visible: Vec<DisplayLane<'a>> = (0..visible_height)
            .map(|y| DisplayLane::Reads {
                logical_lane: top_visual_lane + y,
                reads: Vec::new(),
            })
            .collect();
        for lr in lane_reads {
            if lr.lane >= top_visual_lane {
                let y = lr.lane - top_visual_lane;
                if y < visible_height
                    && let DisplayLane::Reads { reads, .. } = &mut visible[y]
                {
                    reads.push(lr.read);
                }
            }
        }
        return visible;
    }

    let needed_end = top_visual_lane.saturating_add(visible_height);
    let mut flattened: Vec<DisplayLane<'a>> = Vec::new();
    let next_logical_lane = if let Some(max_lane) = lane_reads.iter().map(|lr| lr.lane).max() {
        let mut by_logical_lane: Vec<Vec<&'a Read>> = vec![Vec::new(); max_lane + 1];
        for lr in lane_reads {
            by_logical_lane[lr.lane].push(lr.read);
        }

        let mut lane_spans = Vec::new();
        for (logical_lane, reads) in by_logical_lane.into_iter().enumerate() {
            if flattened.len() >= needed_end {
                break;
            }
            lane_spans.clear();
            for read in &reads {
                visible_expanded_insertions(read, EXPAND_INSERTION_MIN_LEN, view_start, view_end, &mut lane_spans);
            }
            flattened.push(DisplayLane::Reads { logical_lane, reads });
            if !lane_spans.is_empty() {
                for spans in pack_expansions(std::mem::take(&mut lane_spans)) {
                    flattened.push(DisplayLane::Expansion { logical_lane, spans });
                }
            }
        }
        max_lane + 1
    } else {
        0
    };

    let flattened_len = flattened.len();
    let mut visible: Vec<DisplayLane<'a>> = Vec::with_capacity(visible_height);
    if top_visual_lane < flattened_len {
        let end = needed_end.min(flattened_len);
        visible.extend(flattened.drain(top_visual_lane..end));
    }
    while visible.len() < visible_height {
        let visual_lane = top_visual_lane + visible.len();
        let logical_lane = next_logical_lane + (visual_lane - flattened_len);
        visible.push(DisplayLane::Reads {
            logical_lane,
            reads: Vec::new(),
        });
    }
    visible
}

/// An indel or reference skip intersected by the centreline and its full operation length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum IndelLabel {
    Insertion(BasePair),
    Deletion(BasePair),
    Skip(BasePair),
}

/// Framebuffer dimensions for one pileup frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PileupDimensions {
    pub origin_x: u16,
    pub content_width: u16,
    pub bp_width: usize,
    pub fb_height: usize,
}

pub(super) fn pileup_dimensions(
    area: Rect,
    underflow: BasePair,
    bp_per_column: u64,
    lanes_per_row: usize,
) -> PileupDimensions {
    let leading = leading_columns(underflow, area.width, bp_per_column);
    let content_width = area.width - leading;
    PileupDimensions {
        origin_x: area.x + leading,
        content_width,
        bp_width: usize::from(content_width) * bp_per_column as usize,
        fb_height: usize::from(area.height) * lanes_per_row,
    }
}

/// Observations produced while painting for later overlays.
pub(super) struct PileupRenderResult {
    /// Centreline terminal column, if visible.
    pub centline_col: Option<u16>,
    /// Centreline read lengths by terminal row.
    pub centreline_read_lengths: Vec<Option<BasePair>>,
    /// Centreline indels by terminal row.
    pub centreline_indels: Vec<Option<IndelLabel>>,
    /// Reference spans of all reads crossing the centreline, including offscreen lanes.
    pub centreline_read_spans: Vec<Range<BasePair>>,
    /// Logical lane at the top of the pileup, including the parent of an expansion lane.
    pub top_logical_lane: Option<usize>,
    /// Logical lane index for each framebuffer lane (`area.height * lanes_per_row`), or `None` for
    /// synthetic expansion sub-lanes.
    pub lane_logical_indices: Vec<Option<usize>>,
}

/// Draws the visible read pileup into `fb` and returns overlay observations.
#[allow(
    clippy::too_many_arguments,
    reason = "7-arg signature: all fields come from separate sources and grouping them would obscure ownership"
)]
pub(super) fn draw_reads_against(
    fb: &mut Framebuffer,
    dimensions: PileupDimensions,
    area: Rect,
    position: ViewPosition,
    contig: &Contig,
    vertical: VerticalView,
    cache: &mut ReadWindowCache,
    source: &mut impl ReadSource,
    settings: &GenomeViewerSettings,
    reference: &mut super::super::reference_data::ReferenceData,
) -> PileupRenderResult {
    let bp_per_column = settings.horizontal_zoom.bp_per_column();
    let lanes_per_row = vertical.lanes_per_row();
    let ViewPosition { left, .. } = position;
    let PileupDimensions {
        origin_x,
        content_width,
        bp_width,
        fb_height,
    } = dimensions;
    let row_base = vertical.row_offset().as_u64().min(u64::from(u16::MAX)) as u16;
    let top_visual_lane = usize::from(row_base) * lanes_per_row;
    let view_span = BasePair::from(content_width) * BasePair(bp_per_column);
    let view_end = left + view_span;

    let in_bounds_span = view_span.min(contig.length - left);
    let lane_reads = cache.resolve(source, &contig.name, left, in_bounds_span);

    let visible_height = fb_height;
    let visible_lanes = build_visible_lanes(
        &lane_reads,
        top_visual_lane,
        visible_height,
        left,
        view_end,
        settings.expandinsertions,
    );
    let top_logical_lane = visible_lanes.first().map(|lane| match lane {
        DisplayLane::Reads { logical_lane, .. } | DisplayLane::Expansion { logical_lane, .. } => *logical_lane,
    });

    let bounds_bp = in_bounds_span.as_u64() as usize;

    if bounds_bp < bp_width {
        fb.fill_rect(
            bounds_bp,
            0,
            bp_width - bounds_bp,
            visible_height,
            Chrome::OUT_OF_BOUNDS_BG(),
        );
    }

    let centline_x = if settings.centline {
        centline_px(area, origin_x, bp_per_column, position)
    } else {
        None
    };
    let centre_bp = centline_x.filter(|&x| x < bounds_bp).map(|x| left + BasePair(x as u64));

    let glyphs = LaneGlyphs::new(settings, lanes_per_row, bp_per_column);
    let show_centreline_labels = lanes_per_row == 1 && bp_per_column == 1;

    let mut lane_logical_indices: Vec<Option<usize>> = Vec::with_capacity(visible_height);
    let mut centreline_read_lengths: Vec<Option<BasePair>> = vec![None; usize::from(area.height)];
    let mut centreline_indels: Vec<Option<IndelLabel>> = vec![None; usize::from(area.height)];
    let mut coverage = LaneCoverage::for_paint(
        content_width,
        left,
        bp_per_column,
        settings.readcolor == ReadColor::FiveMc,
    );

    for (row, (row_length, row_indel)) in centreline_read_lengths
        .iter_mut()
        .zip(centreline_indels.iter_mut())
        .enumerate()
    {
        for lane_offset in 0..lanes_per_row {
            let lane_y = row * lanes_per_row + lane_offset;
            match &visible_lanes[lane_y] {
                DisplayLane::Reads { logical_lane, reads } => {
                    lane_logical_indices.push(Some(*logical_lane));
                    paint_one_lane_against(
                        fb,
                        reads,
                        *logical_lane,
                        lane_y,
                        left,
                        bp_width,
                        bounds_bp,
                        content_width,
                        bp_per_column,
                        glyphs,
                        settings.readcolor,
                        centline_x,
                        &mut coverage,
                        ReferenceLookup::new(reference, contig),
                    );

                    if show_centreline_labels && let Some(centre_bp) = centre_bp {
                        *row_length = reads
                            .iter()
                            .find(|r| r.start <= centre_bp && centre_bp < r.start + r.length)
                            .map(|r| r.length);

                        *row_indel = coverage.indel_size_at_bp(centre_bp).and_then(|size| {
                            match coverage.kind_at_bp(centre_bp) {
                                BaseKind::InsertionFlank => Some(IndelLabel::Insertion(size)),
                                BaseKind::Deletion => Some(IndelLabel::Deletion(size)),
                                BaseKind::Skip => Some(IndelLabel::Skip(size)),
                                _ => None,
                            }
                        });
                    }
                }
                DisplayLane::Expansion { spans, .. } => {
                    lane_logical_indices.push(None);
                    paint_expansion_lane(
                        fb,
                        spans,
                        lane_y,
                        left,
                        bp_width,
                        bounds_bp,
                        bp_per_column,
                        glyphs,
                        centline_x,
                    );

                    if show_centreline_labels && let Some(centre_bp) = centre_bp {
                        *row_indel = spans
                            .iter()
                            .find(|span| span.start <= centre_bp && centre_bp < span.start + span.length)
                            .map(|span| IndelLabel::Insertion(span.length));
                    }
                }
            }
        }
    }

    let centline_col = centline_x.map(|cx| origin_x + cx as u16);
    let mut centreline_read_spans = Vec::new();
    if settings.localareatrack
        && settings.localareareadspans
        && settings.readcolor != ReadColor::FiveMc
        && let Some(centre_bp) = centre_bp
    {
        // Only the resident window containing the centre has the full cohort. Neighbouring
        // windows can supply bridging reads while it is pending.
        centreline_read_spans.extend(
            cache
                .window_reads(&contig.name, centre_bp)
                .iter()
                .map(|read| read.start..read.start + read.length)
                .filter(|span| span.contains(&centre_bp)),
        );
    }

    PileupRenderResult {
        centline_col,
        centreline_read_lengths,
        centreline_indels,
        centreline_read_spans,
        top_logical_lane,
        lane_logical_indices,
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::{Buffer, Cell},
        style::Color,
        symbols::half_block::LOWER,
    };

    use crate::basepair::BasePair;
    use crate::cigar::Cigar;
    use crate::residue::BaseResidue;
    use crate::style::{Alignment, Indel, Reference, Surface, Text};
    use crate::viewer_app::framebuffer::Glyph;

    use super::super::super::reference_data::ReferenceData;
    use super::super::VerticalZoom;
    use super::super::read::{Read, ReadId, Strand};
    use super::super::read_cache::WindowData;
    use super::classify::LaneCoverage;
    use super::*;
    use crate::setting::ReadColor;
    use crate::style::palette::lock_for_test;
    use crate::viewer_app::genome::geometry::cent_column;
    use crate::viewer_app::genome::mod_5mc::BaseMod5mC;
    use crate::viewer_app::genome::zoom::HorizontalZoom;

    fn id(name: &str) -> ReadId {
        ReadId::from_name(name.as_bytes())
    }

    fn read(start: u64, length: u64, name: &str) -> Read {
        Read {
            start: BasePair(start),
            length: BasePair::length(length),
            id: id(name),
            cigar: Cigar::default(),
            seq: None,
            strand: Strand::Forward,
            mod_5mc: None,
        }
    }

    fn test_cache(capacity: usize, window_len: BasePair) -> ReadWindowCache {
        ReadWindowCache::new(capacity, window_len)
    }

    struct UnreachableSource;
    impl ReadSource for UnreachableSource {
        fn take_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) -> WindowState {
            panic!("ReadSource::take_window must not be called when every touched window is already resident");
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    struct DenseTestSource;
    impl DenseTestSource {
        const MIN_LEN: u64 = 50;
        const MAX_LEN: u64 = 500;

        fn slot_read(&self, contig: &str, slot: u64) -> Option<Read> {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            contig.hash(&mut hasher);
            slot.hash(&mut hasher);
            let seed = hasher.finish();

            if seed % 100 >= 40 {
                return None;
            }
            let span = Self::MAX_LEN - Self::MIN_LEN + 1;
            let length = Self::MIN_LEN + (seed / 100) % span;
            Some(Read {
                start: BasePair(slot),
                length: BasePair::length(length),
                id: id(&format!("{contig}:{slot}")),
                cigar: Cigar::default(),
                seq: None,
                strand: Strand::Forward,
                mod_5mc: None,
            })
        }
    }
    impl ReadSource for DenseTestSource {
        fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState {
            let scan_start = start.checked_sub(BasePair(Self::MAX_LEN)).unwrap_or(BasePair(0));
            let reads: Vec<Read> = (scan_start.as_u64()..end.as_u64())
                .filter_map(|slot| self.slot_read(contig, slot))
                .filter(|r| r.start + r.length > start && r.start < end)
                .collect();
            WindowState::Ready(WindowData::from_reads(reads, start, end - start))
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    const FAR: BasePair = BasePair(1_000_000);

    fn contig(length: BasePair) -> crate::viewer_app::genome::track::Contig {
        crate::viewer_app::genome::track::Contig {
            name: "chr1".to_string(),
            length,
        }
    }

    fn view(left: BasePair, underflow: BasePair, columns: u16, bp_per_column: u64) -> ViewPosition {
        let width = BasePair::from(columns) * BasePair(bp_per_column);
        let half = width / 2;
        let centre = if underflow == BasePair(0) {
            left + half
        } else {
            half.checked_sub(underflow)
                .expect("underflow cannot exceed half the viewport")
        };
        let position = ViewPosition::new(centre, width);
        assert_eq!(position.left, left, "fixture must produce the requested left edge");
        assert_eq!(
            position.underflow, underflow,
            "fixture must produce the requested underflow"
        );
        position
    }

    fn position_at(centre: BasePair, columns: u16, bp_per_column: u64) -> ViewPosition {
        ViewPosition::new(centre, BasePair::from(columns) * BasePair(bp_per_column))
    }

    fn fallback_reference() -> ReferenceData {
        ReferenceData::new(
            &crate::refgenome::IndexedFasta::hs1_fallback(),
            crate::refgenome::ReferenceWorkerHandle::spawn(),
        )
    }

    fn seeded_reference() -> ReferenceData {
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), &[b'A'; 20]);
        reference
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_reads(
        fb: &mut Framebuffer,
        dimensions: PileupDimensions,
        area: Rect,
        position: ViewPosition,
        contig: &Contig,
        vertical: VerticalView,
        cache: &mut ReadWindowCache,
        source: &mut impl ReadSource,
        settings: &GenomeViewerSettings,
    ) -> PileupRenderResult {
        draw_reads_against(
            fb,
            dimensions,
            area,
            position,
            contig,
            vertical,
            cache,
            source,
            settings,
            &mut fallback_reference(),
        )
    }

    fn settings(bp_per_column: u64) -> GenomeViewerSettings {
        let mut settings = GenomeViewerSettings::new();
        settings.horizontal_zoom = match bp_per_column {
            1 => HorizontalZoom::Close,
            2 => HorizontalZoom::Mid,
            other => panic!("no HorizontalZoom renders {other}bp per column"),
        };
        settings.centline = false;
        // Keep paint extent, geometry, and CIGAR assertions independent of the color-mode default.
        settings.readcolor = ReadColor::Lane;
        settings
    }

    fn vertical(lanes_per_row: usize) -> VerticalView {
        vertical_at(BasePair(0), lanes_per_row)
    }

    fn vertical_at(row_offset: BasePair, lanes_per_row: usize) -> VerticalView {
        let zoom = match lanes_per_row {
            1 => VerticalZoom::Close,
            2 => VerticalZoom::Mid,
            other => panic!("no VerticalZoom packs {other} lanes per row"),
        };
        VerticalView::for_test(row_offset, zoom)
    }

    fn is_inked(cell: &Cell) -> bool {
        !cell.symbol().trim().is_empty()
    }

    fn draw_to_buffer(
        cache: &mut ReadWindowCache,
        source: &mut impl ReadSource,
        area: Rect,
        position: ViewPosition,
        contig: &Contig,
        vertical: VerticalView,
        settings: &GenomeViewerSettings,
    ) -> Buffer {
        let mut reference = fallback_reference();
        draw_to_buffer_against(
            cache,
            source,
            area,
            position,
            contig,
            vertical,
            settings,
            &mut reference,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test helper forwards the render inputs plus an explicit reference cache"
    )]
    fn draw_to_buffer_against(
        cache: &mut ReadWindowCache,
        source: &mut impl ReadSource,
        area: Rect,
        position: ViewPosition,
        contig: &Contig,
        vertical: VerticalView,
        settings: &GenomeViewerSettings,
        reference: &mut ReferenceData,
    ) -> Buffer {
        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let lanes_per_row = vertical.lanes_per_row();
        let dimensions = pileup_dimensions(area, position.underflow, bp_per_column, lanes_per_row);

        let mut fb = Framebuffer::new(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column as usize,
            lanes_per_row,
        );

        draw_reads_against(
            &mut fb, dimensions, area, position, contig, vertical, cache, source, settings, reference,
        );

        let cells = fb.reduce(bp_per_column as usize, lanes_per_row, Surface::TRACK_BG());

        let out_rows = area.height as usize;

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                super::super::write_cells(
                    buf,
                    &cells,
                    area,
                    dimensions.origin_x,
                    dimensions.content_width,
                    out_rows,
                    dimensions.content_width as usize,
                );
            })
            .expect("draw");

        terminal.backend().buffer().clone()
    }

    fn render(
        cache: &mut ReadWindowCache,
        left: BasePair,
        width: u16,
        height: u16,
        lanes_per_row: usize,
        bp_per_column: u64,
    ) -> Vec<Vec<bool>> {
        let settings = settings(bp_per_column);
        let position = view(left, BasePair(0), width, bp_per_column);
        let area = Rect::new(0, 0, width, height);

        let buf = draw_to_buffer(
            cache,
            &mut UnreachableSource,
            area,
            position,
            &contig(FAR),
            vertical(lanes_per_row),
            &settings,
        );

        let alignment_bg = Surface::TRACK_BG();
        let mut lanes = Vec::new();
        for y in 0..height {
            let mut top = Vec::new();
            let mut bottom = Vec::new();
            for x in 0..width {
                let cell = buf.get(x, y);
                let (top_channel, bottom_channel) = if cell.symbol().starts_with(LOWER) {
                    (cell.bg, cell.fg)
                } else {
                    (cell.fg, cell.bg)
                };
                let is_inked = |c: &ratatui::buffer::Cell| -> bool { !c.symbol().trim().is_empty() };
                top.push(is_inked(cell) && top_channel != alignment_bg);
                bottom.push(is_inked(cell) && bottom_channel != alignment_bg);
            }
            lanes.push(top);
            if lanes_per_row == 2 {
                lanes.push(bottom);
            }
        }
        lanes
    }

    #[test]
    fn each_lane_retains_all_reads_and_the_gap_at_both_row_compressions() {
        let _palette = lock_for_test();
        for lanes_per_row in [1, 2] {
            let mut cache = test_cache(4, BasePair(1_000));
            cache.insert_reads(
                "chr1".to_string(),
                BasePair(0),
                vec![read(0, 10, "left"), read(20, 10, "right")],
            );
            let painted = render(&mut cache, BasePair(0), 60, 8, lanes_per_row, 1);
            let lane = &painted[0];
            assert!(lane[0..10].iter().all(|&p| p), "left read remains in lane 0");
            assert!(lane[20..30].iter().all(|&p| p), "right read remains in lane 0");
            assert!(lane[10..20].iter().all(|&p| !p), "uncovered gap remains empty");
        }
    }

    fn read_cigar(start: u64, cigar: &str, seq: Option<Vec<u8>>) -> Read {
        Read {
            start: BasePair(start),
            length: BasePair::length(crate::cigar::ref_len(&crate::cigar::parse_ops(cigar))),
            id: id(&format!("read_{start}")),
            cigar: crate::cigar::parse_ops(cigar),
            seq: seq
                .as_deref()
                .map(crate::viewer_app::genome::packed_seq::PackedSeq::from_ascii),
            strand: Strand::Forward,
            mod_5mc: None,
        }
    }

    #[test]
    fn five_mc_calls_keep_their_cells_and_indel_precedence_at_every_zoom() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        let read = Read {
            mod_5mc: Some(BaseMod5mC {
                implicit_unmodified: true,
                calls: vec![(4, 217), (7, 64), (11, 192)],
            }),
            ..read_cigar(0, "5M1I4M1D3M", Some(b"ACGACGACGTACG".to_vec()))
        };
        let lane_1_read = Read {
            id: id("lane_1"),
            ..read_cigar(1, "12M", None)
        };
        cache.insert_reads("chr1".into(), BasePair(0), vec![read, lane_1_read]);
        // Width 26 with 2bp underflow puts the centline directly on bp 11 (`MOD_5MC_HYPER`) at 1bp/col.
        let area = Rect::new(0, 0, 26, 1);
        let contig = contig(BasePair(14));
        let mut reference = seeded_reference();

        for (bp_per_column, lanes_per_row) in [(1, 1), (1, 2), (2, 1)] {
            let mut settings = settings(bp_per_column);
            settings.centline = true;
            settings.readcolor = ReadColor::FiveMc;
            settings.readbasedigits = true;
            settings.readarrows = true;
            let underflow = BasePair(2);
            let buffer = draw_to_buffer_against(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(0), underflow, area.width, bp_per_column),
                &contig,
                vertical(lanes_per_row),
                &settings,
                &mut reference,
            );
            let cell_at = |bp| buffer.get(((underflow.as_u64() + bp) / bp_per_column) as u16, 0);
            for (bp, pigment) in [
                (1, Alignment::MOD_5MC_HYPO()),
                (4, Indel::INSERTION()),
                (5, Indel::INSERTION()),
                (6, Alignment::MOD_5MC_UNCERTAIN()),
                (9, Indel::DELETION()),
                (11, Alignment::MOD_5MC_HYPER()),
                (2, Alignment::pileup_lane_color_pair(0).pick(false)),
            ] {
                let cell = cell_at(bp);
                let in_foreground = cell.symbol() == ratatui::symbols::block::FULL
                    || lanes_per_row == 2
                    || (bp_per_column == 2 && bp % 2 == 0);
                let actual = if in_foreground { cell.fg } else { cell.bg };
                assert_eq!(actual, pigment, "bp {bp} at zoom ({bp_per_column}, {lanes_per_row})");
                if (bp_per_column, lanes_per_row) == (1, 1) {
                    if [1, 6, 11].contains(&bp) {
                        assert_eq!(
                            cell.symbol(),
                            ratatui::symbols::block::FULL,
                            "bp {bp} has no glyph overlay"
                        );
                    } else if bp == 2 {
                        assert_eq!(cell.symbol(), "3", "suppressed mismatch keeps read-base digit");
                    }
                }
            }
            assert_eq!(cell_at(14).fg, Chrome::OUT_OF_BOUNDS_BG());
        }
    }

    #[test]
    fn draws_reads_at_two_bp_per_column() {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read(0, 10, "a"), read(20, 10, "b")],
        );

        let painted = render(&mut cache, BasePair(0), 60, 8, 1, 2);
        let lane0 = &painted[0];
        assert!(
            lane0[0..5].iter().all(|&p| p),
            "the left read must draw at columns [0,5)"
        );
        assert!(
            lane0[10..15].iter().all(|&p| p),
            "the right read must draw at columns [10,15)"
        );
        assert!(
            lane0[5..10].iter().all(|&p| !p),
            "the gap between the two reads must stay background"
        );
    }

    #[test]
    fn reference_backed_mismatches_keep_their_genomic_columns_at_both_horizontal_zooms() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        let mut sequence = b"ACGTACGT".to_vec();
        sequence[2] = b'T';
        sequence[5] = b'G';
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read_cigar(0, "8M", Some(sequence))],
        );

        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), b"ACGTACGT");
        let close = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 8, 2),
            view(BasePair(0), BasePair(0), 8, 1),
            &contig(FAR),
            vertical(1),
            &settings(1),
            &mut reference,
        );
        let carries = |cell: &ratatui::buffer::Cell, color| cell.fg == color || cell.bg == color;
        assert!(carries(close.get(2, 0), Reference::base_color(BaseResidue::T)));
        assert!(carries(close.get(5, 0), Reference::base_color(BaseResidue::G)));
        let assert_mid_channel = |buffer: &Buffer, bp: BasePair, left: BasePair, underflow: BasePair, residue| {
            let genomic_offset = (bp - left).as_u64();
            let screen_col = (underflow.as_u64() / 2 + genomic_offset / 2) as u16;
            let color = Reference::base_color(residue);
            let cell = buffer.get(screen_col, 0);
            if genomic_offset.is_multiple_of(2) {
                assert_eq!(cell.fg, color, "bp {bp:?} is in the left half of column {screen_col}");
            } else {
                assert_eq!(cell.bg, color, "bp {bp:?} is in the right half of column {screen_col}");
            }
        };

        let mid = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 4, 2),
            view(BasePair(0), BasePair(0), 4, 2),
            &contig(FAR),
            vertical(1),
            &settings(2),
            &mut reference,
        );
        assert_mid_channel(&mid, BasePair(2), BasePair(0), BasePair(0), BaseResidue::T);
        assert_mid_channel(&mid, BasePair(5), BasePair(0), BasePair(0), BaseResidue::G);

        let shifted_mid = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 4, 2),
            view(BasePair(2), BasePair(0), 4, 2),
            &contig(FAR),
            vertical(1),
            &settings(2),
            &mut reference,
        );
        assert_mid_channel(&shifted_mid, BasePair(5), BasePair(2), BasePair(0), BaseResidue::G);

        let underflow_mid = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 4, 2),
            view(BasePair(0), BasePair(3), 4, 2),
            &contig(FAR),
            vertical(1),
            &settings(2),
            &mut reference,
        );
        assert_mid_channel(&underflow_mid, BasePair(2), BasePair(0), BasePair(3), BaseResidue::T);
    }

    #[test]
    fn bounded_paint_range_matches_read_extent_at_both_horizontal_zooms() {
        let _palette = lock_for_test();
        for bp_per_column in [1, 2] {
            let width = 20u16;
            let area = Rect::new(0, 0, width, 2);
            let settings = settings(bp_per_column);
            let mut cache = test_cache(4, BasePair(1_000));
            cache.insert_reads("chr1".to_string(), BasePair(0), vec![read_cigar(10, "4M1D", None)]);
            let buffer = draw_to_buffer(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(0), BasePair(0), width, bp_per_column),
                &contig(FAR),
                vertical(1),
                &settings,
            );

            let lane = Alignment::pileup_lane_color_pair(0);
            let expected_read = |bp: u64| lane.pick(bp % 2 == 1);
            let expected_bg = Surface::TRACK_BG();
            let read_start = 10 / bp_per_column;
            let read_end = 15u64.div_ceil(bp_per_column);

            for col in 0..width {
                let cell = buffer.get(col, 0);
                if u64::from(col) >= read_start && u64::from(col) < read_end {
                    let deletion_col = if bp_per_column == 1 { 14 } else { 7 };
                    if u64::from(col) == deletion_col {
                        if bp_per_column == 1 {
                            assert_eq!(cell.bg, Indel::DELETION());
                            assert_eq!(cell.symbol(), "D");
                        } else {
                            assert_eq!(cell.fg, Indel::DELETION());
                            assert_eq!(cell.bg, expected_bg);
                        }
                    } else if bp_per_column == 1 {
                        assert_eq!(cell.fg, expected_read(u64::from(col)));
                        assert_eq!(cell.bg, expected_read(u64::from(col)));
                    } else {
                        let left = u64::from(col) * bp_per_column;
                        assert_eq!(cell.fg, expected_read(left));
                        assert_eq!(cell.bg, expected_read(left + 1));
                    }
                } else {
                    assert_eq!(cell.fg, expected_bg, "zoom {bp_per_column}, column {col}");
                    assert_eq!(cell.bg, expected_bg, "zoom {bp_per_column}, column {col}");
                }
            }
        }
    }

    fn highlighted_column(centre: BasePair, bp_per_column: u64, width: u16) -> Option<u16> {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 400, "a")]);
        let mut settings = settings(bp_per_column);
        settings.centline = true;
        // The scan below looks for a highlight colour in either channel, and an arrow glyph over
        // the highlighted cell replaces the fg it is looking for. Arrows have their own tests; this
        // helper is about where the highlight lands.
        settings.readarrows = false;
        let position = ViewPosition::new(centre, BasePair::from(width) * BasePair(bp_per_column));
        let area = Rect::new(0, 0, width, 3);

        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            position,
            &contig(FAR),
            vertical(1),
            &settings,
        );

        (0..width).find(|&col| {
            let cell = buf.get(col, 0);
            let is_highlight = Alignment::is_highlight;
            is_highlight(cell.fg) || is_highlight(cell.bg)
        })
    }

    /// Invariant 9's whole point: the centre coordinate sits under one fixed
    /// column. Across an even-width pane the highlight lands on `cent_column`
    /// at every zoom and every parity — at `Mid` it only picks a different
    /// *half* of that cell, never a different cell.
    #[test]
    fn the_highlight_lands_on_cent_column_at_every_zoom() {
        let width = 20u16;
        let area = Rect::new(0, 0, width, 3);
        let cent_col = cent_column(area).unwrap();
        for bp_per_column in [1u64, 2] {
            for centre_val in 30u64..35 {
                let centre = BasePair(centre_val);
                let found = highlighted_column(centre, bp_per_column, width);
                assert_eq!(
                    found,
                    Some(cent_col),
                    "at {bp_per_column}bp/col, centre {centre:?} must highlight the centre column"
                );
            }
        }
    }

    #[test]
    fn the_mid_zoom_highlight_does_not_lag_by_a_keypress_near_a_contig_start() {
        let width = 10u16;
        let area = Rect::new(0, 0, width, 3);
        let cent_col = cent_column(area);
        for centre in (1u64..10).map(BasePair) {
            let expected = if !centre.as_u64().is_multiple_of(2) {
                cent_col.and_then(|x| x.checked_sub(1))
            } else {
                cent_col
            };
            assert_eq!(
                highlighted_column(centre, 2, width),
                expected,
                "centre {centre:?} underflows at Mid: the highlight must stay on the marked half-cell"
            );
        }
    }

    /// The highlight marks the half-cell holding `centre`, and which half that
    /// is comes from the offset `centre - left` — not from `centre`'s own
    /// parity. At `Mid` a cell packs the base at `left + 2c` into its fg and
    /// `left + 2c + 1` into its bg, so the offset's remainder picks the channel.
    ///
    /// Sweeping consecutive centres *and* both pane-width parities is the point:
    /// an odd width moves every base to the other half with no coordinate
    /// changing, which is what the old absolute-parity rule got wrong.
    #[test]
    fn the_highlight_marks_the_half_cell_holding_the_centre_base() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 400, "a")]);

        for width in [10u16, 11] {
            for centre_val in 100u64..104 {
                let area = Rect::new(0, 0, width, 3);
                let mut settings = settings(2);
                settings.centline = true;
                let position = ViewPosition::new(BasePair(centre_val), BasePair::from(width) * BasePair(2));

                let offset = (position.centre - position.left).as_u64();
                let (col, half) = ((offset / 2) as u16, offset % 2);

                let buf = draw_to_buffer(
                    &mut cache,
                    &mut UnreachableSource,
                    area,
                    position,
                    &contig(FAR),
                    vertical(1),
                    &settings,
                );

                let is_highlight = Alignment::is_highlight;
                let cell = buf.get(col, 0);
                let (lit, dark, lit_name) = if half == 0 {
                    (cell.fg, cell.bg, "left/fg")
                } else {
                    (cell.bg, cell.fg, "right/bg")
                };
                assert!(
                    is_highlight(lit),
                    "width {width}, centre {centre_val}: {lit_name} half of column {col} must be highlighted, got {lit:?}"
                );
                assert!(
                    !is_highlight(dark),
                    "width {width}, centre {centre_val}: the other half of column {col} must NOT be highlighted, got {dark:?}"
                );
            }
        }
    }

    #[test]
    fn columns_past_contig_length_show_the_out_of_bounds_fallback_not_a_read() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 20, "a")]);

        let (width, height) = (20u16, 3u16);
        let contig_length = BasePair(10);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 1),
            &contig(contig_length),
            vertical(1),
            &settings(1),
        );

        let oob = Chrome::OUT_OF_BOUNDS_BG();
        for col in 0..contig_length.as_u64() as u16 {
            assert_ne!(
                buf.get(col, 0).fg,
                oob,
                "in-bounds column {col} must not show the out-of-bounds fallback"
            );
        }
        for col in contig_length.as_u64() as u16..width {
            assert_eq!(
                buf.get(col, 0).fg,
                oob,
                "out-of-bounds column {col} must show the fallback, not a read"
            );
            assert_eq!(
                buf.get(col, 0).bg,
                oob,
                "out-of-bounds column {col} must show the fallback, not a read"
            );
        }
    }

    #[test]
    fn columns_before_genomic_zero_show_the_out_of_bounds_fallback_and_content_shifts_right() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 1, "a")]);

        let (width, height) = (20u16, 3u16);
        let left = BasePair(0);
        let underflow = BasePair(5);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(left, underflow, width, 1),
            &contig(FAR),
            vertical(1),
            &settings(1),
        );

        let oob = Chrome::OUT_OF_BOUNDS_BG();
        for col in 0..5u16 {
            assert_eq!(
                buf.get(col, 0).fg,
                Text::DIMMED_FG(),
                "leading column {col} must retain the default text channel"
            );
            assert_eq!(
                buf.get(col, 0).bg,
                oob,
                "leading column {col} must show the out-of-bounds fallback"
            );
        }
        assert_eq!(
            buf.get(5, 0).fg,
            Alignment::pileup_lane_color_pair(0).pick(false),
            "genomic position 0 must land at screen column `leading` (5), not 0, with the expected lane colour"
        );
        assert_ne!(
            buf.get(5, 0).bg,
            oob,
            "content bg at leading column must not show the out-of-bounds fallback"
        );
    }

    #[test]
    fn gap_between_reads_in_a_lane_is_not_drawn_in_default_foreground() {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read(0, 10, "a"), read(20, 10, "b")],
        );

        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 1),
            &contig(FAR),
            vertical(1),
            &settings(1),
        );

        for col in 10..20u16 {
            let cell = buf.get(col, 0);
            assert!(
                !is_inked(cell),
                "gap column {col} must draw nothing visible, got symbol {:?} fg {:?} bg {:?}",
                cell.symbol(),
                cell.fg,
                cell.bg
            );
        }
    }

    #[test]
    fn gap_between_reads_in_a_lane_is_not_drawn_in_default_foreground_at_two_lanes_per_row() {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read(0, 10, "a"), read(20, 10, "b")],
        );

        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 1),
            &contig(FAR),
            vertical(2),
            &settings(1),
        );

        for col in 10..20u16 {
            let cell = buf.get(col, 0);
            assert!(
                !is_inked(cell),
                "gap column {col} must draw nothing visible, got symbol {:?} fg {:?} bg {:?}",
                cell.symbol(),
                cell.fg,
                cell.bg
            );
        }
    }

    #[test]
    fn a_covered_bottom_lane_alone_does_not_ink_the_uncovered_top_lane() {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read(0, 10, "a"), read(0, 30, "b")],
        );

        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 1),
            &contig(FAR),
            vertical(2),
            &settings(1),
        );

        for col in 10..30u16 {
            let cell = buf.get(col, 0);
            assert!(
                is_inked(cell),
                "column {col} has a covered bottom lane and must draw something"
            );
        }
    }

    #[test]
    fn left_edge_and_visible_lanes_survive_bidirectional_scrolls() {
        let _palette = lock_for_test();
        let (width, height) = (8u16, 4u16);
        let reads = [
            read_cigar(
                4,
                "12M",
                Some({
                    let mut seq = vec![b'A'; 12];
                    seq[10] = b'C';
                    seq
                }),
            ),
            read_cigar(
                18,
                "10M",
                Some({
                    let mut seq = vec![b'A'; 10];
                    seq[3] = b'G';
                    seq
                }),
            ),
            read_cigar(
                30,
                "8M",
                Some({
                    let mut seq = vec![b'A'; 8];
                    seq[3] = b'T';
                    seq
                }),
            ),
        ];
        let windows: [(u64, &[usize]); 6] = [
            (0, &[0]),
            (8, &[0, 1]),
            (16, &[1]),
            (24, &[1, 2]),
            (32, &[2]),
            (40, &[]),
        ];
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), &[b'A'; 48]);
        for lanes_per_row in [1, 2] {
            let mut cache = test_cache(8, BasePair(8));
            for (start, read_indices) in windows {
                cache.insert_reads(
                    "chr1".to_string(),
                    BasePair(start),
                    read_indices.iter().map(|&index| reads[index].clone()).collect(),
                );
            }
            let mut source = UnreachableSource;
            let trace = [8u64, 12, 16, 20, 24, 28, 24, 20, 16, 12];
            let lanes = usize::from(height) * lanes_per_row;
            let mut previous: Vec<(ReadId, usize, BasePair, BasePair)> = Vec::new();
            for offset in trace {
                let visible: Vec<_> = cache
                    .resolve(&mut source, "chr1", BasePair(offset), BasePair::from(width))
                    .into_iter()
                    .filter(|lr| lr.lane < lanes)
                    .map(|lr| (lr.read.clone(), lr.lane))
                    .collect();
                let view_end = BasePair(offset + u64::from(width));
                for (id, lane, read_start, read_end) in &previous {
                    if *read_start < view_end && *read_end > BasePair(offset) {
                        let current = visible
                            .iter()
                            .find(|(read, _)| read.id == *id)
                            .expect("a read that still overlaps the viewport stays resolved");
                        assert_eq!(current.1, *lane, "visible read {id:?} changed lanes at {offset}");
                    }
                }
                let buffer = draw_to_buffer_against(
                    &mut cache,
                    &mut source,
                    Rect::new(0, 0, width, height),
                    view(BasePair(offset), BasePair(0), width, 1),
                    &contig(FAR),
                    vertical(lanes_per_row),
                    &settings(1),
                    &mut reference,
                );
                for (bp, residue) in [(14u64, BaseResidue::C), (21, BaseResidue::G), (33, BaseResidue::T)] {
                    if bp >= offset && bp < offset + u64::from(width) {
                        let cell = buffer.get((bp - offset) as u16, 0);
                        let color = Reference::base_color(residue);
                        assert!(cell.fg == color || cell.bg == color, "bp {bp} at viewport {offset}");
                    }
                }
                if matches!(offset, 12 | 20) {
                    let edge = buffer.get(0, 0);
                    assert!(edge.fg != Surface::TRACK_BG() || edge.bg != Surface::TRACK_BG());
                }
                previous = visible
                    .iter()
                    .map(|(read, lane)| (read.id, *lane, read.start, read.start + read.length))
                    .collect();
            }
        }
    }

    fn draw_to_framebuffer(
        cache: &mut ReadWindowCache,
        source: &mut impl ReadSource,
        area: Rect,
        position: ViewPosition,
        contig: &Contig,
        vertical: VerticalView,
        settings: &GenomeViewerSettings,
    ) -> (Framebuffer, PileupDimensions, PileupRenderResult) {
        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let lanes_per_row = vertical.lanes_per_row();
        let dimensions = pileup_dimensions(area, position.underflow, bp_per_column, lanes_per_row);

        let mut fb = Framebuffer::new(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column as usize,
            lanes_per_row,
        );
        let result = draw_reads(
            &mut fb, dimensions, area, position, contig, vertical, cache, source, settings,
        );
        (fb, dimensions, result)
    }

    #[test]
    fn framebuffer_dimensions_are_scroll_invariant() {
        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let bp_per_column = 1u64;
        let settings = settings(bp_per_column);
        let expected_bp_width = usize::from(width) * bp_per_column as usize;
        let expected_fb_height = usize::from(height) * 2;

        let geom_at = |offset: BasePair| -> (usize, usize) {
            let mut cache = test_cache(4, BasePair(1_000));
            let position = view(BasePair(0), BasePair(0), width, bp_per_column);
            let (_fb, geom, _) = draw_to_framebuffer(
                &mut cache,
                &mut DenseTestSource,
                area,
                position,
                &contig(FAR),
                vertical_at(offset, 2),
                &settings,
            );
            (geom.bp_width, geom.fb_height)
        };

        assert_eq!(
            geom_at(BasePair(0)),
            (expected_bp_width, expected_fb_height),
            "framebuffer geometry at scroll offset 0"
        );
        assert_eq!(
            geom_at(BasePair(1_000)),
            (expected_bp_width, expected_fb_height),
            "framebuffer geometry at scroll offset 1,000 must be identical to offset 0"
        );
        assert_eq!(
            geom_at(BasePair(u16::MAX as u64)),
            (expected_bp_width, expected_fb_height),
            "framebuffer geometry at u16::MAX scroll must be identical to offset 0"
        );
    }

    #[test]
    fn framebuffer_dimensions_are_depth_invariant() {
        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let bp_per_column = 1u64;
        let settings = settings(bp_per_column);
        let position = view(BasePair(0), BasePair(0), width, bp_per_column);
        let expected_bp_width = usize::from(width) * bp_per_column as usize;
        let expected_fb_height = usize::from(height) * 2;

        let stacked = |n: usize| -> Vec<Read> { (0..n).map(|i| read(0, 100, &format!("r{i}"))).collect() };

        let geom_with = |reads: Vec<Read>| -> (usize, usize) {
            let mut cache = test_cache(8, BasePair(10_000));
            cache.insert_reads("chr1".to_string(), BasePair(0), reads);
            let (_fb, geom, _) = draw_to_framebuffer(
                &mut cache,
                &mut UnreachableSource,
                area,
                position,
                &contig(FAR),
                vertical(2),
                &settings,
            );
            (geom.bp_width, geom.fb_height)
        };

        assert_eq!(
            geom_with(stacked(1)),
            (expected_bp_width, expected_fb_height),
            "1-read pileup framebuffer geometry"
        );
        assert_eq!(
            geom_with(stacked(200)),
            (expected_bp_width, expected_fb_height),
            "200-read pileup framebuffer geometry must be identical to 1-read"
        );
    }

    #[test]
    fn offscreen_lanes_below_visible_window_do_not_reach_the_canvas() {
        let _palette = lock_for_test();
        let (width, height) = (200u16, 4u16);
        let area = Rect::new(0, 0, width, height);
        let bp_per_column = 1u64;
        let settings = settings(bp_per_column);
        let alignment_bg = Surface::TRACK_BG();

        let visible_only: Vec<Read> = (0..4u64).map(|i| read(0, 100, &format!("r{i}"))).collect();
        let with_offscreen: Vec<Read> = (0..20u64).map(|i| read(0, 100, &format!("r{i}"))).collect();

        let canvas_pixels = |reads: Vec<Read>| -> Vec<(usize, usize, Color)> {
            let mut cache = test_cache(32, BasePair(10_000));
            cache.insert_reads("chr1".to_string(), BasePair(0), reads);
            let position = view(BasePair(0), BasePair(0), width, bp_per_column);
            let (fb, _geom, _) = draw_to_framebuffer(
                &mut cache,
                &mut UnreachableSource,
                area,
                position,
                &contig(FAR),
                vertical_at(BasePair(0), 1),
                &settings,
            );
            (0..fb.width())
                .flat_map(|x| (0..fb.height()).map(move |y| (x, y)))
                .filter_map(|(x, y)| fb.pixel(x, y).map(|c| (x, y, c)))
                .filter(|&(_, _, c)| c != alignment_bg)
                .collect()
        };

        let visible_pixels = canvas_pixels(visible_only);
        let offscreen_pixels = canvas_pixels(with_offscreen);
        assert!(
            !visible_pixels.is_empty(),
            "the 4 visible reads must produce pixels; empty result means the harness broke"
        );
        assert_eq!(
            visible_pixels, offscreen_pixels,
            "reads past the visible canvas must not change the painted pixel set"
        );
    }

    #[test]
    fn offscreen_lanes_above_visible_window_do_not_reach_the_canvas() {
        let _palette = lock_for_test();
        let (width, height) = (200u16, 4u16);
        let area = Rect::new(0, 0, width, height);
        let bp_per_column = 1u64;
        let settings = settings(bp_per_column);
        let alignment_bg = Surface::TRACK_BG();

        let above_window: Vec<Read> = vec![read(0, 100, "only")];
        let empty: Vec<Read> = Vec::new();

        let non_blank_pixels = |reads: Vec<Read>| -> Vec<(usize, usize, Color)> {
            let mut cache = test_cache(32, BasePair(10_000));
            cache.insert_reads("chr1".to_string(), BasePair(0), reads);
            let position = view(BasePair(0), BasePair(0), width, bp_per_column);
            let (fb, _geom, _) = draw_to_framebuffer(
                &mut cache,
                &mut UnreachableSource,
                area,
                position,
                &contig(FAR),
                vertical_at(BasePair(16), 1),
                &settings,
            );
            (0..fb.width())
                .flat_map(|x| (0..fb.height()).map(move |y| (x, y)))
                .filter_map(|(x, y)| fb.pixel(x, y).map(|c| (x, y, c)))
                .filter(|&(_, _, c)| c != alignment_bg)
                .collect()
        };

        let empty_pixels = non_blank_pixels(empty);
        let above_pixels = non_blank_pixels(above_window);
        assert!(
            empty_pixels.is_empty(),
            "an empty pileup at any scroll position must produce no read-colour pixels; \
             got {empty_pixels:?} (the canvas contains something it shouldn't)"
        );
        assert!(
            above_pixels.is_empty(),
            "a read at lane 0 with the view scrolled to top_lane_idx=16 must not paint; \
             got {above_pixels:?} (a non-cull cull maps it onto lane_y=0 via saturating_sub)"
        );
    }

    #[test]
    fn draw_reads_after_clear_matches_drawing_into_a_fresh_framebuffer() {
        let (width, height) = (60u16, 8u16);
        let area = Rect::new(0, 0, width, height);
        let bp_per_column = 2u64;
        let settings = settings(bp_per_column);
        let position = view(BasePair(0), BasePair(0), width, bp_per_column);

        let bp_per_column_u = bp_per_column as usize;
        let dimensions = pileup_dimensions(area, position.underflow, bp_per_column, vertical(2).lanes_per_row());

        let mut fresh = Framebuffer::new(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column_u,
            vertical(2).lanes_per_row(),
        );
        let mut cache_fresh = test_cache(4, BasePair(1_000));
        cache_fresh.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 5, "a")]);
        draw_reads(
            &mut fresh,
            dimensions,
            area,
            position,
            &contig(FAR),
            vertical(2),
            &mut cache_fresh,
            &mut UnreachableSource,
            &settings,
        );

        let mut reused = Framebuffer::new(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column_u,
            vertical(2).lanes_per_row(),
        );
        for y in 0..dimensions.fb_height {
            for x in 0..dimensions.bp_width {
                reused.set_pixel(x, y, Color::Indexed(77));
            }
            reused.set_glyph(
                0,
                y,
                Glyph {
                    ch: 'Z',
                    fg: Color::Indexed(15),
                    bg: Some(Color::Indexed(0)),
                    bold: false,
                },
            );
        }
        reused.clear();
        let mut cache_reused = test_cache(4, BasePair(1_000));
        cache_reused.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 5, "a")]);
        draw_reads(
            &mut reused,
            dimensions,
            area,
            position,
            &contig(FAR),
            vertical(2),
            &mut cache_reused,
            &mut UnreachableSource,
            &settings,
        );

        let fresh_pixels: Vec<_> = (0..fresh.width())
            .flat_map(|x| (0..fresh.height()).map(move |y| (x, y)))
            .map(|(x, y)| (x, y, fresh.pixel(x, y)))
            .collect();
        let reused_pixels: Vec<_> = (0..reused.width())
            .flat_map(|x| (0..reused.height()).map(move |y| (x, y)))
            .map(|(x, y)| (x, y, reused.pixel(x, y)))
            .collect();
        assert_eq!(
            fresh_pixels, reused_pixels,
            "draw_reads after clear must produce the same pixels as drawing into a fresh framebuffer"
        );

        let glyph_grid = |fb: &Framebuffer| -> Vec<(usize, usize, Option<Glyph>)> {
            (0..fb.width())
                .flat_map(|x| (0..fb.height()).map(move |y| (x, y)))
                .map(|(x, y)| (x, y, fb.glyph(x, y)))
                .collect()
        };
        assert_eq!(
            glyph_grid(&fresh),
            glyph_grid(&reused),
            "glyphs must be identical: a stale glyph surviving clear would occupy a cell \
             this frame left blank"
        );
    }

    #[test]
    fn all_m_cigar_read_renders_identically_to_flat_span() {
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read_cigar(0, "20M", None)]);

        let (width, height) = (20u16, 3u16);
        let area = Rect::new(0, 0, width, height);
        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 1),
            &contig(FAR),
            vertical(1),
            &settings(1),
        );

        for col in 0..20u16 {
            let cell = buf.get(col, 0);
            assert!(is_inked(cell), "all-M read column {col} must show a block glyph");
        }
    }

    #[test]
    fn mismatch_in_one_half_of_a_mid_column_colours_only_that_half() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        let mut seq = vec![b'A'; 20];
        seq[0] = b'C';
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read_cigar(0, "20M", Some(seq))]);

        let (width, height) = (10u16, 3u16);
        let area = Rect::new(0, 0, width, height);
        let mut reference = seeded_reference();
        let buf = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 2),
            &contig(FAR),
            vertical(1),
            &settings(2),
            &mut reference,
        );

        let cell = buf.get(0, 0);
        assert_eq!(
            cell.fg,
            Reference::base_color(BaseResidue::C),
            "bp 0 is the C mismatch and must colour the left half"
        );
        assert_eq!(
            cell.bg,
            Alignment::pileup_lane_color_pair(0).pick(true),
            "bp 1 is the A match and must retain the tinted right-half lane colour"
        );
    }

    #[test]
    fn mismatch_in_right_half_of_a_mid_column_colours_the_right_half_not_the_left() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        let mut seq = vec![b'A'; 20];
        seq[1] = b'C';
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read_cigar(0, "20M", Some(seq))]);

        let (width, height) = (10u16, 3u16);
        let area = Rect::new(0, 0, width, height);
        let mut reference = seeded_reference();
        let buf = draw_to_buffer_against(
            &mut cache,
            &mut UnreachableSource,
            area,
            view(BasePair(0), BasePair(0), width, 2),
            &contig(FAR),
            vertical(1),
            &settings(2),
            &mut reference,
        );

        let cell = buf.get(0, 0);
        assert_eq!(
            cell.fg,
            Alignment::pileup_lane_color_pair(0).pick(false),
            "bp 0 is the A match and must retain the plain left-half lane colour"
        );
        assert_eq!(
            cell.bg,
            Reference::base_color(BaseResidue::C),
            "bp 1 is the C mismatch and must colour the right half"
        );
    }

    #[test]
    fn the_highlight_marks_only_bases_a_read_covers() {
        let _palette = lock_for_test();
        let (width, height) = (21u16, 3u16);
        let area = Rect::new(0, 0, width, height);
        let centre = BasePair(10);

        let mut settings = settings(1);
        settings.centline = true;
        settings.readbasedigits = false;

        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 400, "a")]);

        let buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            area,
            position_at(centre, width, 1),
            &contig(FAR),
            vertical(1),
            &settings,
        );

        let col = cent_column(area).unwrap();
        let is_highlight = Alignment::is_highlight;

        let covered = buf.get(col, 0);
        assert!(
            is_highlight(covered.bg) || is_highlight(covered.fg),
            "a base a read covers must be highlighted, got fg {:?} bg {:?}",
            covered.fg,
            covered.bg
        );

        let uncovered = buf.get(col, 1);
        assert!(
            !is_highlight(uncovered.bg) && !is_highlight(uncovered.fg),
            "a base no read covers must not be highlighted — the centline's own mark is \
             what shows there. Got fg {:?} bg {:?}",
            uncovered.fg,
            uncovered.bg
        );
    }

    fn centreline_data_at(
        reference: &mut ReferenceData,
        cigar: &str,
        sequence: Option<Vec<u8>>,
        read_start: u64,
        centre: BasePair,
        bp_per_column: u64,
        lanes_per_row: usize,
    ) -> (Option<BasePair>, Option<IndelLabel>) {
        let (width, height) = (40u16, 3u16);
        let area = Rect::new(0, 0, width, height);

        let mut settings = settings(bp_per_column);
        settings.centline = true;

        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read_cigar(read_start, cigar, sequence)],
        );

        let position = position_at(centre, width, bp_per_column);
        let dimensions = pileup_dimensions(area, position.underflow, bp_per_column, lanes_per_row);
        let mut fb = Framebuffer::new(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column as usize,
            lanes_per_row,
        );
        let mut source = UnreachableSource;
        let result = draw_reads_against(
            &mut fb,
            dimensions,
            area,
            position,
            &contig(FAR),
            vertical(lanes_per_row),
            &mut cache,
            &mut source,
            &settings,
            reference,
        );

        (result.centreline_read_lengths[0], result.centreline_indels[0])
    }

    #[test]
    fn centreline_data_reports_read_and_indel_spans_only_at_close_single_lane() {
        let _palette = lock_for_test();
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), &[b'A'; 64]);
        let cases = [
            (
                "10M2I5M",
                None,
                0,
                9,
                1,
                1,
                Some(15),
                Some(IndelLabel::Insertion(BasePair::length(2))),
            ),
            (
                "10M2I5M",
                None,
                0,
                10,
                1,
                1,
                Some(15),
                Some(IndelLabel::Insertion(BasePair::length(2))),
            ),
            (
                "10M4D5M",
                None,
                0,
                11,
                1,
                1,
                Some(19),
                Some(IndelLabel::Deletion(BasePair::length(4))),
            ),
            (
                "10M30N5M",
                None,
                0,
                20,
                1,
                1,
                Some(45),
                Some(IndelLabel::Skip(BasePair::length(30))),
            ),
            ("10M30N5M", None, 0, 20, 2, 1, None, None),
            ("10M30N5M", None, 0, 20, 1, 2, None, None),
            ("10M4D5M", None, 0, 4, 1, 1, Some(19), None),
            ("10M4D5M", None, 0, 30, 1, 1, None, None),
            (
                "20M",
                Some({
                    let mut sequence = vec![b'A'; 20];
                    sequence[10] = b'C';
                    sequence
                }),
                0,
                10,
                1,
                1,
                Some(20),
                None,
            ),
            ("10M4D5M", None, 0, 11, 2, 1, None, None),
            ("10M4D5M", None, 0, 11, 1, 2, None, None),
        ];

        for (cigar, sequence, start, centre, bp_per_column, lanes_per_row, length, indel) in cases {
            assert_eq!(
                centreline_data_at(
                    &mut reference,
                    cigar,
                    sequence,
                    start,
                    BasePair(centre),
                    bp_per_column,
                    lanes_per_row,
                ),
                (length.map(BasePair::length), indel),
                "CIGAR {cigar}, centre {centre}, zoom {bp_per_column}, lanes {lanes_per_row}"
            );
        }
    }

    #[test]
    fn split_reads_render_skip_connectors_on_track_bg_and_centreline_label_across_zooms() {
        let _palette = lock_for_test();
        let mut cache = test_cache(4, BasePair(1_000));
        // Odd exon boundaries (bp 5..15) exercise both half-block exon edges at Mid zoom.
        let split = read_cigar(0, "5M10N5M", Some(vec![b'A'; 10]));
        // Start `sibling` 1 bp right so lane packing deterministically places `split` in lane 0.
        let sibling = Read {
            id: id("sibling"),
            ..read_cigar(1, "19M", None)
        };
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![split, sibling]);

        let area = Rect::new(0, 0, 20, 2);
        let track_bg = Surface::TRACK_BG();
        let lane0 = Alignment::pileup_lane_color_pair(0);
        let lane1 = Alignment::pileup_lane_color_pair(1);

        // 1. (Close, Close): hyphens on plain track bg, centline rule preserved, and `_10bp` label.
        {
            let mut settings = settings(1);
            settings.centline = true;
            settings.readarrows = true;
            settings.readbasedigits = true;
            settings.indellength = true;
            let position = view(BasePair(0), BasePair(0), area.width, 1);
            let dimensions = pileup_dimensions(area, position.underflow, 1, 1);
            let mut fb = Framebuffer::new(dimensions.bp_width, dimensions.fb_height, 1, 1);
            let mut reference = seeded_reference();
            let result = draw_reads_against(
                &mut fb,
                dimensions,
                area,
                position,
                &contig(FAR),
                vertical(1),
                &mut cache,
                &mut UnreachableSource,
                &settings,
                &mut reference,
            );
            let cells = fb.reduce(1, 1, track_bg);
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            terminal
                .draw(|frame| {
                    super::super::write_cells(
                        frame.buffer_mut(),
                        &cells,
                        area,
                        dimensions.origin_x,
                        dimensions.content_width,
                        area.height as usize,
                        dimensions.content_width as usize,
                    );
                    super::super::draw_indel_length_labels(
                        frame,
                        area,
                        result.centline_col.unwrap(),
                        &result.centreline_indels,
                    );
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let cent_col = result.centline_col.unwrap();
            assert_eq!(cent_col, 10, "centreline lands inside the 5..15 skip");
            for col in 5..15u16 {
                let cell = buf.get(col, 0);
                assert_ne!(cell.fg, Indel::DELETION());
                assert_ne!(cell.bg, Indel::DELETION());
                if col == cent_col {
                    assert_eq!(cell.symbol(), crate::viewer_app::painter::VERTICAL_RULE.to_string());
                    assert_eq!(cell.bg, track_bg);
                } else if (cent_col + 1..cent_col + 6).contains(&col) {
                    continue;
                } else {
                    assert_eq!(cell.symbol(), "-", "col {col}");
                    assert_eq!(cell.bg, track_bg, "col {col}");
                }
            }
            let label: String = (cent_col + 1..cent_col + 6).map(|x| buf.get(x, 0).symbol()).collect();
            assert_eq!(label, "_10bp");
            // Col 15 holds the final `'p'` of `_10bp`, so check exon bases outside the label.
            for col in [0u16, 4, 16, 19] {
                assert_eq!(buf.get(col, 0).bg, lane0.pick(col % 2 == 1), "exon col {col}");
            }
        }

        // 2. (Mid, Close): half-block exon boundaries preserved and full-skip 2bp columns draw `-`.
        {
            let mut settings = settings(2);
            settings.centline = false;
            settings.readarrows = true;
            let mut reference = seeded_reference();
            let buf = draw_to_buffer_against(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(0), BasePair(0), area.width, 2),
                &contig(FAR),
                vertical(1),
                &settings,
                &mut reference,
            );
            // Col 2 covers bp 4 (exon) + bp 5 (skip); col 7 covers bp 14 (skip) + bp 15 (exon).
            assert_eq!((buf.get(2, 0).fg, buf.get(2, 0).bg), (lane0.pick(false), track_bg));
            assert_eq!((buf.get(7, 0).fg, buf.get(7, 0).bg), (track_bg, lane0.pick(true)));
            for col in 3..7u16 {
                let cell = buf.get(col, 0);
                assert_eq!(cell.symbol(), "-", "mid-zoom full-skip col {col}");
                assert_eq!(cell.bg, track_bg, "mid-zoom full-skip col {col}");
            }
        }

        // 3. (Close, Mid): top lane skip stays plain track bg without clobbering the sibling lane.
        {
            let mut settings = settings(1);
            settings.centline = false;
            let mut reference = seeded_reference();
            let buf = draw_to_buffer_against(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(0), BasePair(0), area.width, 1),
                &contig(FAR),
                vertical(2),
                &settings,
                &mut reference,
            );
            for col in 5..15u16 {
                let cell = buf.get(col, 0);
                assert_eq!(cell.fg, track_bg, "top lane skip at col {col}");
                assert_eq!(cell.bg, lane1.pick(col % 2 == 1), "bottom sibling intact at col {col}");
            }
        }
    }

    const ARROW_GLYPHS: [&str; 2] = [">", "<"];

    fn arrow_settings(bp_per_column: u64) -> GenomeViewerSettings {
        let mut settings = settings(bp_per_column);
        settings.readarrows = true;
        settings
    }

    /// The terminal columns holding an arrow glyph in row 0.
    fn arrow_columns(buf: &Buffer, width: u16) -> Vec<u16> {
        (0..width)
            .filter(|&x| ARROW_GLYPHS.contains(&buf.get(x, 0).symbol()))
            .collect()
    }

    /// A read spanning several arrow intervals, carrying mismatches and both indel kinds.
    ///
    /// The reference is seeded to all `A`, so every `C` in the sequence classifies as a mismatch.
    /// Obstacles cover *some* arrow positions but not all of them: blocking every one would leave
    /// the frame empty and the assertion loop vacuous, which is what the emptiness guard below
    /// catches. The deletion and insertion supply the non-mismatch kinds the dodge must also refuse.
    fn obstructed_read() -> (Read, ReferenceData) {
        let cigar = "12M3D10M4I20M";
        let ref_len = crate::cigar::ref_len(&crate::cigar::parse_ops(cigar));
        let mut seq = vec![b'A'; 64];
        for offset in [5, 25, 45] {
            seq[offset] = b'C';
        }
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), &vec![b'A'; ref_len as usize + 8]);
        (read_cigar(0, cigar, Some(seq)), reference)
    }

    #[test]
    fn an_arrow_never_covers_anything_but_a_plain_match() {
        for bp_per_column in [1u64, 2] {
            let (read, mut reference) = obstructed_read();
            let mut cache = test_cache(4, BasePair(1_000));
            cache.insert_reads("chr1".to_string(), BasePair(0), vec![read.clone()]);

            let (width, height) = (40u16, 3u16);
            let area = Rect::new(0, 0, width, height);
            let buf = draw_to_buffer_against(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(0), BasePair(0), width, bp_per_column),
                &contig(FAR),
                vertical(1),
                &arrow_settings(bp_per_column),
                &mut reference,
            );

            // The oracle is `LaneCoverage`'s own classification of the same read, built here
            // independently of the arrow pass rather than read back out of it.
            let mut coverage = LaneCoverage::for_paint(width, BasePair(0), bp_per_column, false);
            coverage.reset(width, BasePair(0), bp_per_column);
            coverage.classify_read_against(&read, width, ReferenceLookup::new(&mut reference, &contig(FAR)));

            let arrows = arrow_columns(&buf, width);
            assert!(
                !arrows.is_empty(),
                "{bp_per_column}bp columns: the fixture must draw some arrows, or this test is vacuous"
            );

            for x in arrows {
                for offset in 0..bp_per_column {
                    let bp = BasePair(u64::from(x) * bp_per_column + offset);
                    assert_eq!(
                        coverage.kind_at_bp(bp),
                        BaseKind::Match,
                        "{bp_per_column}bp columns: an arrow at column {x} covers bp {} which is not a plain match",
                        bp.as_u64()
                    );
                }
            }
        }
    }

    #[test]
    fn arrows_are_pinned_to_the_read_not_the_screen() {
        let shift = 4u64;
        let mut columns = Vec::new();

        for left in [0u64, shift] {
            let mut cache = test_cache(4, BasePair(1_000));
            // Started far enough right that the whole arrow run stays on screen at both offsets.
            cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(shift, 60, "pinned")]);

            let (width, height) = (40u16, 3u16);
            let area = Rect::new(0, 0, width, height);
            let buf = draw_to_buffer(
                &mut cache,
                &mut UnreachableSource,
                area,
                view(BasePair(left), BasePair(0), width, 1),
                &contig(FAR),
                vertical(1),
                &arrow_settings(1),
            );
            columns.push(arrow_columns(&buf, width));
        }

        assert!(!columns[0].is_empty(), "the unscrolled frame must draw arrows");
        let shifted: Vec<u16> = columns[0].iter().map(|x| x - shift as u16).collect();
        assert_eq!(
            columns[1], shifted,
            "scrolling by {shift}bp must move every arrow left by exactly {shift} columns — \
             the marks are pinned to the read, not to a screen grid"
        );
    }

    #[test]
    fn insertion_expansion_filters_by_threshold_packs_non_overlapping_and_preserves_cache_lanes() {
        // Below threshold (49I) must not expand; at/above threshold (50I, 60I) must expand.
        let mut cache = test_cache(4, BasePair(1_000));
        let below = read_cigar(10, "10M49I10M", None);
        let leading = read_cigar(10, "50I20M", None);
        // Two non-overlapping >=50bp insertions on one read reuse a single expansion sub-lane:
        // first anchors at 10+5-1 = 14 ([14, 64)), second anchors at 10+5+60-1 = 74 ([74, 134)).
        let two_disjoint = Read {
            id: id("disjoint"),
            ..read_cigar(10, "5M50I60M60I10M", None)
        };
        // Two overlapping >=50bp insertions on one read open a second expansion sub-lane, and a
        // third non-overlapping insertion reuses the first sub-lane:
        // [14, 74), [24, 84), [74, 124).
        let overlapping = Read {
            id: id("overlapping"),
            ..read_cigar(10, "5M60I10M60I50M50I10M", None)
        };

        // Direct extraction and packing checks.
        let mut spans = Vec::new();
        visible_expanded_insertions(&below, EXPAND_INSERTION_MIN_LEN, BasePair(0), BasePair(200), &mut spans);
        assert!(spans.is_empty(), "49bp insertion is below the 50bp threshold");

        visible_expanded_insertions(
            &leading,
            EXPAND_INSERTION_MIN_LEN,
            BasePair(0),
            BasePair(200),
            &mut spans,
        );
        assert_eq!(
            spans,
            vec![ExpandedInsertion {
                start: BasePair(10),
                length: BasePair::length(50),
            }],
            "leading insertion must fall back to read.start as anchor"
        );

        spans.clear();
        visible_expanded_insertions(
            &two_disjoint,
            EXPAND_INSERTION_MIN_LEN,
            BasePair(0),
            BasePair(200),
            &mut spans,
        );
        let packed_disjoint = pack_expansions(spans.clone());
        assert_eq!(
            packed_disjoint.len(),
            1,
            "non-overlapping insertions under one lane must reuse a single expansion sub-lane"
        );
        assert_eq!(
            packed_disjoint[0],
            vec![
                ExpandedInsertion {
                    start: BasePair(14),
                    length: BasePair::length(50),
                },
                ExpandedInsertion {
                    start: BasePair(74),
                    length: BasePair::length(60),
                },
            ]
        );

        spans.clear();
        visible_expanded_insertions(
            &overlapping,
            EXPAND_INSERTION_MIN_LEN,
            BasePair(0),
            BasePair(200),
            &mut spans,
        );
        let packed_overlap = pack_expansions(spans);
        assert_eq!(
            packed_overlap.len(),
            2,
            "overlapping expanded spans must open a second sub-lane while reusing the first once free"
        );
        assert_eq!(
            packed_overlap[0],
            vec![
                ExpandedInsertion {
                    start: BasePair(14),
                    length: BasePair::length(60),
                },
                ExpandedInsertion {
                    start: BasePair(74),
                    length: BasePair::length(50),
                },
            ]
        );
        assert_eq!(
            packed_overlap[1],
            vec![ExpandedInsertion {
                start: BasePair(24),
                length: BasePair::length(60),
            }]
        );

        // Cache lane assignments must stay identical regardless of expansion mode.
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![below, two_disjoint, overlapping]);
        let before: Vec<(ReadId, usize)> = cache
            .resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(200))
            .into_iter()
            .map(|lr| (lr.read.id, lr.lane))
            .collect();

        let mut expanded_settings = settings(1);
        expanded_settings.expandinsertions = true;
        let _ = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 120, 10),
            view(BasePair(0), BasePair(0), 120, 1),
            &contig(FAR),
            vertical(1),
            &expanded_settings,
        );

        let after: Vec<(ReadId, usize)> = cache
            .resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(200))
            .into_iter()
            .map(|lr| (lr.read.id, lr.lane))
            .collect();
        assert_eq!(
            before, after,
            "viewport-scoped expansion must leave ReadWindowCache lane assignments untouched"
        );
    }

    #[test]
    fn expanded_insertions_render_accurately_across_zooms_edges_and_overlays() {
        let _palette = lock_for_test();
        let marker = paint::EXPANDED_INSERTION_MARKER.to_string();

        // Read at bp 5 with one short insertion (10I, flanks at 9..=10) and one qualifying
        // insertion (60I, anchor at 19, flanks at 19..=20, expanded span [19, 79)).
        let read_with_both = Read {
            id: id("both"),
            ..read_cigar(5, "5M10I10M60I90M", None)
        };
        let mut cache = test_cache(4, BasePair(1_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read_with_both]);

        // 1. (Close, Close): parent markers, expansion glyphs/span, and centreline indellength
        //    mid-bar (far from parent junction).
        let (width, height) = (100u16, 4u16);
        let area = Rect::new(0, 0, width, height);
        let mut close_settings = settings(1);
        close_settings.expandinsertions = true;
        close_settings.centline = true;
        let centre = BasePair(50); // inside [19, 79), away from flanks 19..=20
        let pos = position_at(centre, width, 1);
        let dims = pileup_dimensions(area, pos.underflow, 1, 1);
        let mut fb = Framebuffer::new(dims.bp_width, dims.fb_height, 1, 1);
        let result = draw_reads(
            &mut fb,
            dims,
            area,
            pos,
            &contig(FAR),
            vertical(1),
            &mut cache,
            &mut UnreachableSource,
            &close_settings,
        );
        let cells = fb.reduce(1, 1, Surface::TRACK_BG());
        let cell_at = |col: usize, row: usize| cells[row * usize::from(width) + col];

        // Short 10I flanks at bp 9 and 10 keep 'I'; expanded 60I flanks at bp 19 and 20 show marker.
        for bp in [9usize, 10] {
            let c = cell_at(bp, 0);
            assert_eq!(c.symbol, 'I', "unexpanded <50bp flank at bp {bp} keeps 'I'");
            assert_eq!(c.bg, Indel::INSERTION());
        }
        for bp in [19usize, 20] {
            let c = cell_at(bp, 0);
            assert_eq!(
                c.symbol.to_string(),
                marker,
                "expanded >=50bp flank at bp {bp} shows downward marker"
            );
            assert_eq!(c.bg, Indel::INSERTION());
        }
        // Row 1 holds 'I' across [19, 79) and background elsewhere (except centline rule).
        for bp in 0..usize::from(width) {
            let c = cell_at(bp, 1);
            if (19..79).contains(&bp) {
                assert_eq!(c.symbol, 'I', "expanded bar at bp {bp}");
                assert_eq!(c.fg, Text::ON_DATA_FG());
                assert_eq!(c.bg, Indel::INSERTION());
            } else {
                assert_ne!(c.bg, Indel::INSERTION(), "no insertion ink outside [19, 79) at bp {bp}");
            }
        }
        // Centreline at bp 50 reports the 60bp insertion on the expansion row and no read length.
        assert_eq!(result.centreline_indels[0], None);
        assert_eq!(
            result.centreline_indels[1],
            Some(IndelLabel::Insertion(BasePair::length(60)))
        );
        assert_eq!(result.centreline_read_lengths[1], None);

        // Scrolling right past the parent junction (left = 30) keeps [30, 79) expanded on row 1.
        let shifted = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 60, 3),
            view(BasePair(30), BasePair(0), 60, 1),
            &contig(FAR),
            vertical(1),
            &close_settings,
        );
        for col in 0..60u16 {
            let cell = shifted.get(col, 1);
            if col < 49 {
                assert_eq!(cell.symbol(), "I", "scrolled expansion bar covers col {col}");
                assert_eq!(cell.bg, Indel::INSERTION());
            } else {
                assert_ne!(cell.bg, Indel::INSERTION(), "scrolled expansion bar stops at col 49");
            }
        }

        // 2. (Close, Mid): 2 lanes per row packs parent read (top) and expansion bar (bottom) into row 0.
        let mut close_mid_settings = settings(1);
        close_mid_settings.expandinsertions = true;
        let close_mid = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 90, 2),
            view(BasePair(0), BasePair(0), 90, 1),
            &contig(FAR),
            vertical(2),
            &close_mid_settings,
        );
        // At bp 30 (inside [19, 79), plain match on parent): top half is read, bottom half is insertion.
        let mid_cell = close_mid.get(30, 0);
        assert_eq!(
            mid_cell.bg,
            Indel::INSERTION(),
            "expansion sub-lane sits in bottom half"
        );
        assert_ne!(mid_cell.fg, Indel::INSERTION(), "parent match sits in top half");
        // At bp 15 (before anchor 19): bottom half is empty track background.
        assert_eq!(close_mid.get(15, 0).bg, Surface::TRACK_BG());

        // 3. (Mid, Close): 2 bp per column preserves exact odd start (19) and odd end (79) half-cells.
        let mut mid_close_settings = settings(2);
        mid_close_settings.expandinsertions = true;
        let mid_close = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 50, 3),
            view(BasePair(0), BasePair(0), 50, 2),
            &contig(FAR),
            vertical(1),
            &mid_close_settings,
        );
        // Column 9 covers bp 18 (uncovered) and bp 19 (insertion start -> right half / bg).
        assert_eq!(mid_close.get(9, 1).fg, Surface::TRACK_BG());
        assert_eq!(mid_close.get(9, 1).bg, Indel::INSERTION());
        // Column 39 covers bp 78 (insertion end - 1 -> left half / fg) and bp 79 (uncovered -> bg).
        assert_eq!(mid_close.get(39, 1).fg, Indel::INSERTION());
        assert_eq!(mid_close.get(39, 1).bg, Surface::TRACK_BG());

        // 4. Contig-start underflow: expansion bar aligns with parent's shifted columns.
        let underflow_pos = view(BasePair(0), BasePair(4), 90, 1);
        let underflow_buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 90, 3),
            underflow_pos,
            &contig(FAR),
            vertical(1),
            &close_mid_settings,
        );
        assert_eq!(underflow_buf.get(3, 1).bg, Chrome::OUT_OF_BOUNDS_BG());
        // Anchor bp 19 shifts by leading=4 to column 23 on both parent and expansion rows.
        assert_eq!(underflow_buf.get(23, 0).bg, Indel::INSERTION());
        assert_eq!(underflow_buf.get(23, 1).symbol(), "I");
        assert_eq!(underflow_buf.get(23, 1).bg, Indel::INSERTION());
        assert_ne!(underflow_buf.get(22, 1).bg, Indel::INSERTION());

        // 5. Contig-end clamping: expanded bar [19, 79) clamps cleanly at contig length 50.
        let clamped_buf = draw_to_buffer(
            &mut cache,
            &mut UnreachableSource,
            Rect::new(0, 0, 90, 3),
            view(BasePair(0), BasePair(0), 90, 1),
            &contig(BasePair(50)),
            vertical(1),
            &close_mid_settings,
        );
        assert_eq!(clamped_buf.get(49, 1).symbol(), "I");
        assert_eq!(clamped_buf.get(49, 1).bg, Indel::INSERTION());
        assert_eq!(clamped_buf.get(50, 1).bg, Chrome::OUT_OF_BOUNDS_BG());
    }

    #[test]
    fn depth_indicator_labels_top_lane_and_periodic_lanes_across_scroll_and_zoom() {
        use super::super::AlignmentTrack;
        use crate::viewer_app::genome::track::TrackView;

        let _palette = lock_for_test();
        let mut track = AlignmentTrack {
            read_cache: test_cache(4, BasePair(1_000)),
            source: Box::new(UnreachableSource),
            ..AlignmentTrack::new()
        };
        // 12 overlapping reads occupy logical lanes 0..12; lane 0 carries a qualifying 60bp insertion.
        let mut reads = Vec::new();
        for i in 0..12 {
            let cigar = if i == 0 { "5M60I40M" } else { "45M" };
            reads.push(Read {
                id: id(&format!("r{i:02}")),
                ..read_cigar(10 + i, cigar, None)
            });
        }
        track.read_cache.insert_reads("chr1".to_string(), BasePair(0), reads);

        let area = Rect::new(3, 2, 80, 15);
        let contig = contig(FAR);
        let mut reference = fallback_reference();
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");

        for (bp_per_column, zoom, expand, offset, top, lane_11_row) in [
            (1, VerticalZoom::Close, false, 0, 1, Some(10)),
            (1, VerticalZoom::Close, false, 1, 2, Some(9)),
            (1, VerticalZoom::Close, false, 10, 11, None),
            (2, VerticalZoom::Close, false, 1, 2, Some(9)),
            (1, VerticalZoom::Close, true, 0, 1, Some(11)),
            (1, VerticalZoom::Close, true, 1, 1, Some(10)),
            (1, VerticalZoom::Mid, true, 1, 2, Some(4)),
            (1, VerticalZoom::Mid, true, 5, 10, None),
        ] {
            let mut settings = settings(bp_per_column);
            settings.depthindicator = true;
            settings.expandinsertions = expand;
            track.set_vzoom(settings.horizontal_zoom, zoom);
            track.jump_vertical(BasePair(offset));
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position: view(BasePair(0), BasePair(0), area.width, bp_per_column),
                            settings: &settings,
                            contig: &contig,
                        },
                        &mut reference,
                    );
                })
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let lane_number = |row| {
                (area.x..area.x + 5)
                    .flat_map(|x| buffer[(x, area.y + row)].symbol().chars())
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            };
            let case = (bp_per_column, zoom, expand, offset);
            assert_eq!(lane_number(0), Some(top), "{case:?}");
            assert_eq!(
                (1..area.height).find(|&row| lane_number(row) == Some(11)),
                lane_11_row,
                "{case:?}"
            );
        }
    }
}
