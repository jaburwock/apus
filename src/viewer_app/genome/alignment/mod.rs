//! The alignment track orchestrates BAM-backed pileup rendering.
//!
//! Horizontal position comes from the genome shell, while this track owns the pileup-only
//! vertical offset and lane packing. A draw resolves visible windows, paints bp-by-lane data into
//! a [`Framebuffer`], reduces it to terminal cells, and then applies the overlays that must draw
//! over the finished track. Keeping those stages here makes the dependency order visible:
//! alignment populates coverage before the depth readouts consume it.
//!
//! The source and cache are separate owners: [`ReadSource`] supplies non-blocking windows, and
//! [`ReadWindowCache`] supplies bounded residency, deduplication, and stable lanes. Do not move
//! horizontal scroll into either one; the shell-owned position is what keeps genome tracks aligned
//! (invariant 9), while vertical zoom must preserve the top lane when its packing changes.

use std::ops::Range;

mod centline;
mod depth_indicator;
mod indel_length_label;
mod insertions;
mod pileup;
mod read;
mod read_cache;
mod read_length_label;
mod read_source;
#[cfg(feature = "resident-demo")]
pub(super) use read_source::ResidentBamSource;
mod window_counts;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use ratatui::{Frame, layout::Rect};

use super::mod_5mc::Call;
use super::track::TrackView;
use super::zoom::{HorizontalZoom, VerticalZoom};
use crate::basepair::BasePair;
use crate::style::{Alignment, Chrome, Text};
use crate::viewer_app::framebuffer::{Framebuffer, ReducedCell};
use depth_indicator::draw_depth_indicator;
use indel_length_label::draw_indel_length_labels;
#[cfg(feature = "profiling")]
pub(super) use pileup::ProfileLaneCoverage as LaneCoverage;
use pileup::{draw_reads_against, pileup_dimensions};
#[cfg(feature = "profiling")]
pub(super) use read::Read;
#[cfg(test)]
use read_cache::WindowState;
#[cfg(feature = "profiling")]
pub(super) use read_cache::windows_touching;
use read_cache::{ReadSource, ReadWindowCache};
use read_length_label::draw_read_length_labels;
pub(crate) use read_source::{EmptySource, OnDiskBamSource};
#[cfg(feature = "profiling")]
pub(super) use read_source::{fetch_window, sample_window_sizing};
#[cfg(test)]
pub(super) use window_counts::CoverageWindow;
pub(super) use window_counts::{BaseCounts, CallCounts, WindowCoverage};

pub(super) fn mod_5mc_color(call: Call) -> Color {
    mod_5mc_pigment(call)()
}

pub(super) fn mod_5mc_pigment(call: Call) -> fn() -> Color {
    match call {
        Call::Hypo => Alignment::MOD_5MC_HYPO,
        Call::Uncertain => Alignment::MOD_5MC_UNCERTAIN,
        Call::Hyper => Alignment::MOD_5MC_HYPER,
    }
}

/// Pileup vertical position and lane packing.
#[derive(Debug, Clone, Copy)]
pub(super) struct VerticalView {
    /// Vertical scroll, in terminal rows.
    offset: BasePair,
    zoom: VerticalZoom,
}

impl VerticalView {
    pub(super) fn lanes_per_row(self) -> usize {
        self.zoom.lanes_per_row()
    }

    /// Returns the vertical row offset.
    pub(super) fn row_offset(self) -> BasePair {
        self.offset
    }

    /// Builds a test view.
    #[cfg(test)]
    pub(super) fn for_test(offset: BasePair, zoom: VerticalZoom) -> Self {
        Self { offset, zoom }
    }
}

/// Writes reduced pileup cells at the content origin.
///
/// `cells` is content-relative: its row stride is `content_width` and its first column is index 0.
/// The underflow columns exist only in the destination (`origin_x`). Striding by `area.width` and
/// offsetting the read by `leading` once shifted the pileup right near a contig start.
fn write_cells(
    buf: &mut Buffer,
    cells: &[ReducedCell],
    area: Rect,
    origin_x: u16,
    content_width: u16,
    out_rows: usize,
    reduced_row_width: usize,
) {
    let oob_bg = Chrome::OUT_OF_BOUNDS_BG();
    buf.set_style(area, Style::default().fg(Text::DIMMED_FG()).bg(oob_bg));

    let out_cols = content_width as usize;
    for out_y in 0..out_rows.min(usize::from(area.height)) {
        for out_x in 0..out_cols {
            let cell = &cells[out_y * reduced_row_width + out_x];
            let buf_x = origin_x + out_x as u16;
            let buf_y = area.y + out_y as u16;
            if let Some(target) = buf.cell_mut((buf_x, buf_y)) {
                let mut utf8_buf = [0u8; 4];
                target.set_symbol(cell.symbol.encode_utf8(&mut utf8_buf));
                target.set_style(Style::default().fg(cell.fg).bg(cell.bg).add_modifier(cell.modifier));
            }
        }
    }
}

/// Hard cap on vertical scroll. A cap tracking the last occupied lane was rejected as not worth
/// its complexity; below the cap, over-scrolling just draws blank.
const MAX_VERTICAL_OFFSET: BasePair = BasePair(20_000);

/// Read pileup track with a track-owned source and vertical axis.
pub struct AlignmentTrack {
    vertical_offset: BasePair,
    vzoom: VerticalZoom,
    /// Rows available at the last draw.
    last_rows: Option<u16>,
    /// Columns available at the last draw.
    last_columns: u16,
    read_cache: ReadWindowCache,
    source: Box<dyn ReadSource>,
    framebuffer: Framebuffer,
    reduced: Vec<ReducedCell>,
}

impl AlignmentTrack {
    pub fn new() -> Self {
        Self {
            vertical_offset: BasePair(0),
            vzoom: VerticalZoom::default(),
            last_rows: None,
            last_columns: 0,
            read_cache: ReadWindowCache::with_defaults(),
            source: Box::new(EmptySource),
            framebuffer: Framebuffer::new(0, 0, 1, 1),
            reduced: Vec::new(),
        }
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn resident(source: ResidentBamSource) -> Self {
        let mut track = Self::new();
        track.source = Box::new(source);
        track
    }

    /// Installs `source` and clears windows resolved against the old source.
    pub(crate) fn set_source(&mut self, source: OnDiskBamSource) {
        let (capacity, window_len) = source.window_sizing();
        self.read_cache = ReadWindowCache::new(capacity, window_len);
        self.source = Box::new(source);
    }

    /// Returns whether a nonblocking query has produced reads.
    #[cfg(test)]
    pub(crate) fn has_reads_in(&mut self, contig: &str, start: BasePair, end: BasePair) -> bool {
        match self.source.take_window(contig, start, end) {
            WindowState::Ready(data) => !data.reads.is_empty(),
            WindowState::Pending => {
                self.source.request_window(contig, start, end);
                false
            }
            WindowState::Failed(_) => false,
        }
    }

    /// Drains asynchronous BAM-window errors discovered while drawing.
    pub(super) fn take_errors(&mut self) -> Vec<String> {
        self.read_cache.take_errors()
    }

    /// Returns resident coverage around `pos` without fetching.
    pub(super) fn coverage_at(&self, contig: &str, pos: BasePair) -> Option<WindowCoverage<'_>> {
        let centre = self.read_cache.window_coverage(contig, pos)?;
        let (left, right) = self.read_cache.neighbour_windows(contig, pos);
        Some(WindowCoverage { centre, left, right })
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let mut track = Self::new();
        track.set_source(
            OnDiskBamSource::from_path(
                std::path::Path::new("testdata/bam/tiny.bam"),
                crate::io::IoWorkerHandle::spawn(),
            )
            .expect("tiny.bam fixture must be readable"),
        );
        track
    }

    /// Sets vertical zoom, applying horizontal constraints and recentering.
    pub(crate) fn set_vzoom(&mut self, horizontal_zoom: HorizontalZoom, vzoom: VerticalZoom) {
        let old_lanes = self.vzoom.lanes_per_row();
        self.vzoom = super::zoom::constrain_vzoom(horizontal_zoom, vzoom);
        let new_lanes = self.vzoom.lanes_per_row();
        if old_lanes != new_lanes {
            self.recentre_vertical(old_lanes, new_lanes);
        }
    }

    /// Steps vertical zoom toward `Mid` or `Close`.
    pub(crate) fn step_vertical_zoom(&mut self, to_mid: bool, horizontal_zoom: HorizontalZoom) {
        let next = match (self.vzoom, to_mid) {
            (VerticalZoom::Close, true) => VerticalZoom::Mid,
            (VerticalZoom::Mid, false) => VerticalZoom::Close,
            (current, _) => current,
        };
        self.set_vzoom(horizontal_zoom, next);
    }

    /// Re-applies the horizontal/vertical zoom constraint.
    pub(crate) fn resync_vertical_zoom(&mut self, horizontal_zoom: HorizontalZoom) {
        self.set_vzoom(horizontal_zoom, self.vzoom);
    }

    /// Returns the current vertical zoom.
    pub(crate) fn vertical_zoom(&self) -> VerticalZoom {
        self.vzoom
    }

    /// Preserves the top lane when lane packing changes.
    ///
    /// Not the centre, unlike horizontal zoom: lane 0 is a hard edge, and keeping the centre fixed
    /// once scrolled an unscrolled pileup off-screen.
    fn recentre_vertical(&mut self, old_lanes: usize, new_lanes: usize) {
        if self.last_rows.is_none() {
            return;
        }
        let top_lane = self.vertical_offset * BasePair(old_lanes as u64);
        self.vertical_offset = top_lane / new_lanes as u64;
    }

    /// Scrolls vertically and reports whether the upper cap was reached.
    pub(crate) fn scroll_vertical(&mut self, step: BasePair, down: bool) -> bool {
        if down {
            let target = self.vertical_offset + step;
            self.vertical_offset = target.min(MAX_VERTICAL_OFFSET);
            target > MAX_VERTICAL_OFFSET
        } else {
            self.vertical_offset = self.vertical_offset.checked_sub(step).unwrap_or(BasePair(0));
            false
        }
    }

    /// Jumps to an absolute vertical offset and reports clamping.
    pub(crate) fn jump_vertical(&mut self, offset: BasePair) -> bool {
        self.vertical_offset = offset.min(MAX_VERTICAL_OFFSET);
        offset > MAX_VERTICAL_OFFSET
    }

    /// Returns the draw-time vertical view.
    pub(super) fn vertical_view(&self) -> VerticalView {
        VerticalView {
            offset: self.vertical_offset,
            zoom: self.vzoom,
        }
    }

    /// Polls in-flight source fetches outside `draw`, returning whether any window settled.
    pub(super) fn tick(&mut self) -> bool {
        self.source.poll()
    }

    /// Returns whether a source fetch is pending.
    pub(crate) fn is_fetching(&self) -> bool {
        self.source.is_fetching()
    }
}

impl Default for AlignmentTrack {
    fn default() -> Self {
        Self::new()
    }
}

impl AlignmentTrack {
    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        reference: &mut super::reference_data::ReferenceData,
    ) -> Vec<Range<BasePair>> {
        let TrackView {
            position,
            settings,
            contig,
        } = view;
        self.last_rows = Some(area.height);
        self.last_columns = area.width;
        let vertical = self.vertical_view();

        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let lanes_per_row = vertical.lanes_per_row();
        let dimensions = pileup_dimensions(area, position.underflow, bp_per_column, lanes_per_row);

        self.framebuffer.resize_if_needed(
            dimensions.bp_width,
            dimensions.fb_height,
            bp_per_column as usize,
            lanes_per_row,
        );
        self.framebuffer.clear();

        let result = draw_reads_against(
            &mut self.framebuffer,
            dimensions,
            area,
            position,
            contig,
            vertical,
            &mut self.read_cache,
            &mut self.source,
            settings,
            reference,
        );

        // Nothing paints between `draw_reads_against` and the reduction: a mark inside the pileup
        // takes its place in `paint_one_lane_against`'s order. Only the overlays below draw after,
        // over the finished cells.
        self.framebuffer.reduce_into(
            &mut self.reduced,
            bp_per_column as usize,
            lanes_per_row,
            crate::style::Surface::TRACK_BG(),
        );

        let buf = frame.buffer_mut();
        write_cells(
            buf,
            &self.reduced,
            area,
            dimensions.origin_x,
            dimensions.content_width,
            area.height as usize,
            dimensions.content_width as usize,
        );

        if settings.readlength
            && let Some(centline_col) = result.centline_col
        {
            draw_read_length_labels(frame, area, centline_col, &result.centreline_read_lengths);
        }

        if settings.indellength
            && let Some(centline_col) = result.centline_col
        {
            draw_indel_length_labels(frame, area, centline_col, &result.centreline_indels);
        }

        if settings.depthindicator {
            draw_depth_indicator(
                frame,
                area,
                result.top_logical_lane,
                &result.lane_logical_indices,
                lanes_per_row,
            );
        }

        result.centreline_read_spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_area_projects_centreline_depth_independently_of_pileup_scroll() {
        use super::super::geometry::AreaSpan;
        use super::super::local_area::SummaryBin;
        use super::super::{GenomeViewer, GenomeViewerSettings};
        use crate::io::IoWorkerHandle;
        use crate::keys::Keymap;
        use crate::refgenome::IndexedFasta;
        use crate::setting::ReadColor;
        use crate::style::{Coverage, palette};
        use crate::viewer::ViewerApp;
        use crate::viewer_app::viewport::ViewPosition;
        use ratatui::{Terminal, backend::TestBackend};
        use read::{Read, ReadId, Strand};

        let _palette = palette::lock_for_test();
        let contig_length = BasePair(510);
        let centre = BasePair(253);
        let genome = IndexedFasta::new("projection test".to_string(), "chr1\t510\t0\t510\t511\n").unwrap();
        let mut settings = GenomeViewerSettings::new();
        settings.localareatrack = true;
        settings.localareascale = 20;
        settings.localareareadspans = true;
        settings.localareabreakpoints = false;
        settings.colorlegend = false;
        settings.centline = true;
        settings.readcolor = ReadColor::Lane;
        let mut viewer = GenomeViewer::new(settings, genome, AlignmentTrack::new(), None, IoWorkerHandle::spawn());
        let reads = [(21, 254), (130, 363), (0, 280), (253, 480), (65, 253), (254, 450)]
            .into_iter()
            .enumerate()
            .map(|(index, (start, end))| Read {
                start: BasePair(start),
                length: BasePair::length(end - start),
                id: ReadId::from_name(&[index as u8]),
                cigar: crate::cigar::parse_ops(&format!("{}M", end - start)),
                seq: None,
                strand: Strand::Forward,
                mod_5mc: None,
            })
            .collect();
        viewer
            .track_align
            .read_cache
            .insert_reads("chr1".into(), BasePair(0), reads);
        viewer.centre.jump_to(centre, contig_length);
        let area = Rect::new(3, 2, 51, 50);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).unwrap();
        let keymap = Keymap::defaults();
        let span = AreaSpan::for_viewport(ViewPosition::new(centre, BasePair(51)), 20, contig_length);
        let mut bin = SummaryBin::default();
        bin.spanning_depth = 8;
        viewer.track_local_area.seed_summary_bins("chr1", span, vec![bin; 510]);
        let highlight = Alignment::HIGHLIGHT_A();

        for (horizontal, vertical) in [
            (HorizontalZoom::Close, VerticalZoom::Close),
            (HorizontalZoom::Close, VerticalZoom::Mid),
            (HorizontalZoom::Mid, VerticalZoom::Close),
        ] {
            viewer.settings.horizontal_zoom = horizontal;
            viewer.track_align.set_vzoom(horizontal, vertical);
            let panes = GenomeViewer::layout(area, &viewer.settings);
            let local = panes.local_area;
            // 10 bp per column. The last two reads touch the centreline but do not cover it.
            // A share of 2/8 fills 5 half-rows, and 3/8 fills 7. The viewport marker can cover
            // the baseline, so compare the rows above it.
            for offset in [0, 128] {
                viewer.track_align.jump_vertical(BasePair(offset));
                terminal.draw(|frame| viewer.draw(frame, area, &keymap)).unwrap();
                let buffer = terminal.backend().buffer();
                for (row, depth) in [(0, 8), (4, 4), (8, 0)] {
                    let label = buffer[(local.x + 1, local.y + row)].symbol();
                    assert_eq!(label.parse::<u32>(), Ok(depth), "{horizontal:?}/{vertical:?}");
                }
                for (column, expected) in [(8, &[6, 7][..]), (18, &[5, 6, 7]), (31, &[6, 7]), (36, &[7]), (49, &[])] {
                    let rows = (1..8)
                        .filter(|&row| {
                            let cell = &buffer[(local.x + column, local.y + row)];
                            cell.fg == highlight || cell.bg == highlight
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        rows, expected,
                        "{horizontal:?}/{vertical:?}, offset {offset}, column {column}"
                    );
                }
                // At column 36 a read ends 3 bp into the left half. Its partial depth is
                // enough to reach row 7 on the left while the right half still shows plain depth.
                let partial = &buffer[(local.x + 36, local.y + 7)];
                assert_eq!(
                    (partial.symbol(), partial.fg, partial.bg),
                    ("▌", highlight, Coverage::FG())
                );
            }
        }
    }

    #[test]
    fn coverage_and_pileup_agree_on_signal_columns_at_both_contig_edges() {
        use super::super::mod_5mc::BaseMod5mC;
        use super::super::packed_seq::PackedSeq;
        use super::super::{GenomeViewer, GenomeViewerSettings};
        use crate::io::IoWorkerHandle;
        use crate::keys::Keymap;
        use crate::refgenome::IndexedFasta;
        use crate::residue::BaseResidue;
        use crate::setting::ReadColor;
        use crate::style::{Coverage, Reference, palette};
        use crate::viewer::ViewerApp;
        use rand::{RngExt, SeedableRng, rngs::StdRng};
        use ratatui::{Terminal, backend::TestBackend};
        use read::{Read, ReadId, Strand};

        let _palette = palette::lock_for_test();
        let mut rng = StdRng::seed_from_u64(0xA11E_1E05);
        let contig_len = 2 * rng.random_range(43..56) + 1;
        let contig_length = BasePair::length(contig_len);
        let mut reference: Vec<_> = (0..contig_len).map(|_| b"ACGT"[rng.random_range(0..4)]).collect();
        let loci = [(4, 2 * rng.random_range(4..8) + 1), (contig_len - 2, contig_len - 1)];
        for (_, bp) in loci {
            reference[bp as usize - 1..=bp as usize].copy_from_slice(b"CG");
        }
        let genome = IndexedFasta::new(
            "coverage test".to_owned(),
            &format!("chr1\t{contig_len}\t0\t{contig_len}\t{}\n", contig_len + 1),
        )
        .unwrap();
        let mut settings = GenomeViewerSettings::new();
        settings.coveragetrack = true;
        settings.allelefraction = 50;
        settings.centline = false;
        settings.depthindicator = false;
        settings.localareatrack = false;
        let mut viewer = GenomeViewer::new(settings, genome, AlignmentTrack::new(), None, IoWorkerHandle::spawn());
        viewer.reference_data.seed_chunk("chr1", BasePair(0), &reference);
        let area = Rect::new(3, 2, 31, 30);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).unwrap();
        let keymap = Keymap::defaults();
        let call_reads = [3, 2, 1, 2];

        for (centre, bp) in loci {
            let read = Read {
                start: BasePair(0),
                length: contig_length,
                id: ReadId::from_name(b"read"),
                cigar: crate::cigar::parse_ops(&format!("{contig_len}M")),
                seq: Some(PackedSeq::from_ascii(&reference)),
                strand: Strand::Reverse,
                mod_5mc: None,
            };
            let mut seq = reference.clone();
            seq[bp as usize] = b'T';
            let mismatch = Read {
                seq: Some(PackedSeq::from_ascii(&seq)),
                ..read.clone()
            };
            let methylation_reads = [Some(240), Some(120), Some(20), None]
                .into_iter()
                .zip(call_reads)
                .flat_map(|(prob, count)| std::iter::repeat_n(prob, count))
                .enumerate()
                .map(|(i, prob)| Read {
                    id: ReadId::from_name(&[i as u8]),
                    mod_5mc: prob.map(|prob| BaseMod5mC {
                        implicit_unmodified: false,
                        calls: vec![(bp as u32, prob)],
                    }),
                    ..read.clone()
                })
                .collect::<Vec<_>>();
            let cases = [
                (
                    ReadColor::Lane,
                    vec![mismatch.clone(), mismatch, read],
                    vec![Reference::base_color(BaseResidue::T)],
                ),
                (
                    ReadColor::FiveMc,
                    methylation_reads,
                    [Call::Hyper, Call::Uncertain, Call::Hypo].map(mod_5mc_color).to_vec(),
                ),
            ];

            for (mode, reads, pigments) in cases {
                viewer.settings.readcolor = mode;
                viewer.track_align.read_cache = ReadWindowCache::with_defaults();
                let depth = reads.len().to_string();
                viewer
                    .track_align
                    .read_cache
                    .insert_reads("chr1".into(), BasePair(0), reads);
                viewer.centre.jump_to(BasePair(centre), contig_length);

                for (horizontal, vertical) in [
                    (HorizontalZoom::Close, VerticalZoom::Close),
                    (HorizontalZoom::Close, VerticalZoom::Mid),
                    (HorizontalZoom::Mid, VerticalZoom::Close),
                ] {
                    viewer.settings.horizontal_zoom = horizontal;
                    viewer.track_align.set_vzoom(horizontal, vertical);
                    let panes = GenomeViewer::layout(area, &viewer.settings);
                    terminal.draw(|frame| viewer.draw(frame, area, &keymap)).unwrap();

                    let scale = horizontal.bp_per_column();
                    let half_span = u64::from(area.width) * scale / 2;
                    let left = centre.saturating_sub(half_span);
                    let leading = half_span.saturating_sub(centre) / scale;
                    let expected = area.x + (leading + (bp - left) / scale) as u16;
                    let buffer = terminal.backend().buffer();
                    let axis_label: String = (panes.coverage.x + 1..panes.coverage.right())
                        .map(|x| buffer[(x, panes.coverage.y)].symbol())
                        .take_while(|symbol| symbol.as_bytes().iter().all(u8::is_ascii_digit))
                        .collect();
                    assert_eq!(axis_label, depth, "coverage scale at {horizontal:?}/{vertical:?}");
                    let has_pigment = |x, y, pigment| {
                        let cell = &buffer[(x, y)];
                        cell.fg == pigment || cell.bg == pigment
                    };
                    for &pigment in &pigments {
                        for pane in [panes.coverage, panes.alignment] {
                            let columns: Vec<_> = (pane.x..pane.right())
                                .filter(|&x| (pane.y..pane.bottom()).any(|y| has_pigment(x, y, pigment)))
                                .collect();
                            assert_eq!(
                                columns,
                                [expected],
                                "{mode:?}, {horizontal:?}/{vertical:?}, bp {bp}, pane {pane:?}"
                            );
                        }
                    }
                    // Every read spans the contig, so the bar fills the pane; only the order is pinned
                    // here, since segment sizes are the `segments` property test's.
                    let mut stack: Vec<_> = (panes.coverage.y..panes.coverage.bottom())
                        .rev()
                        .flat_map(|y| {
                            let cell = &buffer[(expected, y)];
                            match cell.symbol() {
                                "█" => [cell.fg, cell.fg],
                                "▀" => [cell.bg, cell.fg],
                                symbol => panic!("unexpected bar cell {symbol}"),
                            }
                        })
                        .collect();
                    stack.dedup();
                    let order: Vec<_> = pigments.iter().copied().chain([Coverage::FG()]).collect();
                    assert_eq!(stack, order, "{mode:?} stack from the baseline at bp {bp}");
                    if horizontal == HorizontalZoom::Mid && bp == contig_len - 1 {
                        assert_eq!(buffer[(expected, panes.alignment.y)].bg, Chrome::OUT_OF_BOUNDS_BG());
                    }
                }
            }
        }
    }

    #[test]
    fn insertion_coverage_marks_cluster_support_at_its_median_across_windows() {
        use super::super::coverage::{COVERAGE_TRACK_HEIGHT, CoverageTrack};
        use super::super::reference_data::{ReferenceData, ReferenceLookup};
        use super::super::settings::GenomeViewerSettings;
        use super::super::track::Contig;
        use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
        use crate::setting::ReadColor;
        use crate::style::{Indel, palette};
        use crate::viewer_app::bam_index::SharedIndex;
        use crate::viewer_app::viewport::ViewPosition;
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = palette::lock_for_test();
        let contig = Contig {
            name: "chr1".to_owned(),
            length: BasePair::length(4_000),
        };
        let mut sam = String::new();
        for (name, cigar) in [
            ("halo", "18M90I"),
            ("double", "19M95I3M96I28M"),
            ("median", "21M100I29M"),
            ("right", "23M98I27M"),
            ("short", "20M75I30M"),
            ("far", "24M96I26M"),
        ]
        .into_iter()
        .chain(["a", "b", "c", "d", "e", "f", "g"].map(|name| (name, "50M")))
        {
            use std::fmt::Write;
            writeln!(sam, "{name}\t0\tchr1\t1981\t60\t{cigar}\t*\t0\t0\t*\t*").unwrap();
        }
        let (path, header) = crate::testgen::write_indexed_test_bam("insertion_coverage", &sam);
        let index = SharedIndex::read(&path).unwrap();
        let seam = BasePair(2_000);
        let fetch =
            |start, end| read_source::fetch_window(&path, &index, &header, &contig.name, start, end, || false).unwrap();
        let left = fetch(BasePair(0), seam);
        let right = fetch(seam, contig.length);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

        // The cluster includes a left-window-only read and two observations from one read.
        assert_eq!(left.reads.len(), 13);
        assert_eq!(right.reads.len(), 12);
        assert_eq!(left.counts.insertions, [(BasePair(1_999), 1)]);
        assert_eq!(right.counts.insertions, [(seam, 4), (BasePair(2_003), 1)]);
        let coverage = WindowCoverage {
            centre: CoverageWindow {
                counts: &left.counts.bases,
                calls: &[],
                insertions: &left.counts.insertions,
                window_start: BasePair(0),
            },
            left: None,
            right: Some(CoverageWindow {
                counts: &right.counts.bases,
                calls: &[],
                insertions: &right.counts.insertions,
                window_start: seam,
            }),
        };
        let mut settings = GenomeViewerSettings::new();
        settings.centline = false;
        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let area = Rect::new(2, 1, 14, COVERAGE_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).unwrap();
        let mut track = CoverageTrack::new();

        for zoom in [HorizontalZoom::Close, HorizontalZoom::Mid] {
            settings.horizontal_zoom = zoom;
            let scale = zoom.bp_per_column();
            let position = ViewPosition::new(BasePair(1_997), BasePair(u64::from(area.width) * scale));
            let expected = area.x + ((seam - position.left).as_u64() / scale) as u16;
            for (mode, threshold, visible) in [
                (ReadColor::Lane, 33, true),
                (ReadColor::Lane, 34, false),
                (ReadColor::FiveMc, 33, false),
            ] {
                settings.readcolor = mode;
                settings.allelefraction = threshold;
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
                            Some(coverage),
                            ReferenceLookup::new(&mut reference, &contig),
                            13,
                        )
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let highlighted = (area.x..area.right())
                    .flat_map(|x| (area.y..area.bottom()).map(move |y| (x, y)))
                    .filter(|&point| {
                        let cell = &buffer[point];
                        cell.fg == Indel::INSERTION() || cell.bg == Indel::INSERTION()
                    })
                    .collect::<Vec<_>>();
                let expected = if visible {
                    vec![(expected, area.bottom() - 2), (expected, area.bottom() - 1)]
                } else {
                    Vec::new()
                };
                assert_eq!(highlighted, expected, "{zoom:?}, {mode:?}, {threshold}%");
            }
        }
    }

    #[test]
    fn vertical_zoom_constraint_covers_each_state_and_resync() {
        for (horizontal, requested, expected) in [
            (HorizontalZoom::Close, VerticalZoom::Close, VerticalZoom::Close),
            (HorizontalZoom::Close, VerticalZoom::Mid, VerticalZoom::Mid),
            (HorizontalZoom::Mid, VerticalZoom::Close, VerticalZoom::Close),
            (HorizontalZoom::Mid, VerticalZoom::Mid, VerticalZoom::Close),
        ] {
            let mut track = AlignmentTrack::for_test();
            track.set_vzoom(horizontal, requested);
            assert_eq!(track.vertical_zoom(), expected, "{horizontal:?}/{requested:?}");
        }

        let mut track = AlignmentTrack::for_test();
        track.set_vzoom(HorizontalZoom::Close, VerticalZoom::Mid);
        track.resync_vertical_zoom(HorizontalZoom::Mid);
        assert_eq!(track.vertical_zoom(), VerticalZoom::Close);
    }

    fn top_lane(track: &AlignmentTrack) -> u64 {
        track.vertical_offset.as_u64() * track.vzoom.lanes_per_row() as u64
    }

    #[test]
    fn vertical_zoom_preserves_the_top_lane_after_draw_and_before_first_draw() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        for (initial, target, offset) in [
            (VerticalZoom::Close, VerticalZoom::Mid, BasePair(0)),
            (VerticalZoom::Close, VerticalZoom::Mid, BasePair(100)),
            (VerticalZoom::Mid, VerticalZoom::Close, BasePair(1)),
        ] {
            let mut track = AlignmentTrack {
                last_rows: Some(area.height),
                last_columns: area.width,
                ..AlignmentTrack::for_test()
            };
            track.set_vzoom(HorizontalZoom::Close, initial);
            track.jump_vertical(offset);
            let before = top_lane(&track);

            track.set_vzoom(HorizontalZoom::Close, target);

            assert!(
                before.abs_diff(top_lane(&track)) <= 1,
                "{initial:?} to {target:?} at {offset:?}"
            );
        }

        let mut before_draw = AlignmentTrack::for_test();
        let original_offset = before_draw.vertical_offset;
        before_draw.set_vzoom(HorizontalZoom::Close, VerticalZoom::Mid);
        before_draw.set_vzoom(HorizontalZoom::Close, VerticalZoom::Close);
        assert_eq!(before_draw.vertical_offset, original_offset);
    }
}
