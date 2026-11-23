//! Per-base depth bars, grey except where alternate alleles reach `allelefraction` or 5mC calls
//! exist. Coloured segments stack from the baseline, next to the pileup, beneath the grey of the
//! remaining depth. In allele mode a qualifying insertion takes precedence in its column,
//! since its supporting reads may also contribute to the SNP or deletion counts.
//! The composed-frame seam test lives in `alignment::tests` to seed the read cache directly.

use std::cmp::Reverse;

use ratatui::{Frame, layout::Rect, style::Color};

use crate::basepair::BasePair;
use crate::residue::BaseResidue;
use crate::setting::ReadColor;
use crate::style::{Coverage, Indel, Reference, Surface, Text};
use crate::viewer_app::framebuffer::{Framebuffer, ReducedCell};
use crate::viewer_app::painter::paint_cell;

use super::alignment::{BaseCounts, CallCounts, WindowCoverage, mod_5mc_pigment};
use super::coordinate::marks::centline_mark;
use super::depth_axis::draw_depth_axis;
use super::geometry::{in_bounds_base_range, in_bounds_column_range};
use super::mod_5mc::Call;
use super::reference_data::ReferenceLookup;
use super::track::{TrackView, paint_track_background};

pub(super) const COVERAGE_TRACK_HEIGHT: u16 = 5;
const PIXELS_PER_ROW: usize = 2;
type Pigment = fn() -> Color;

pub(super) struct CoverageTrack {
    framebuffer: Framebuffer,
    reduced: Vec<ReducedCell>,
}

impl CoverageTrack {
    pub(super) fn new() -> Self {
        Self {
            framebuffer: Framebuffer::new(0, 0, 1, PIXELS_PER_ROW),
            reduced: Vec::new(),
        }
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        coverage: Option<WindowCoverage<'_>>,
        mut reference: ReferenceLookup<'_>,
        max: u32,
    ) {
        if area.is_empty() {
            return;
        }
        let bp_per_column = view.settings.horizontal_zoom.bp_per_column();
        let in_bounds = in_bounds_column_range(view.position, view.contig.length, area.width, bp_per_column);
        let visible = in_bounds_base_range(view.position, view.contig.length, bp_per_column);
        let width = usize::from(area.width);
        let height = usize::from(area.height) * PIXELS_PER_ROW;
        self.framebuffer.resize_if_needed(width, height, 1, PIXELS_PER_ROW);
        self.framebuffer.clear();
        let show_5mc = view.settings.readcolor == ReadColor::FiveMc;

        if let Some(coverage) = coverage.filter(|_| max != 0) {
            let threshold = view.settings.allelefraction;
            let column_span = BasePair(bp_per_column);
            for (column, start) in in_bounds.clone().zip(visible.start.step_to(visible.end, column_span)) {
                let end = (start + column_span).min(visible.end);
                let selected = start
                    .step_to(end, BasePair(1))
                    .filter_map(|pos| {
                        if show_5mc {
                            let (counts, calls) = coverage.at_with_calls(pos)?;
                            let depth = counts.depth();
                            (depth != 0).then(|| Bar::five_mc(depth, calls))
                        } else {
                            let (counts, insertions) = coverage.at_with_insertions(pos)?;
                            let depth = counts.depth();
                            // A shifted insertion anchor can bring in a read that ends before the median.
                            let support = insertions.min(depth);
                            (depth != 0).then(|| {
                                if reaches_threshold(support, depth, threshold) {
                                    Bar {
                                        kind: BarKind::Insertion,
                                        depth,
                                        signal: support,
                                    }
                                } else {
                                    Bar::alleles(counts, depth, reference.base_at(pos))
                                }
                            })
                        }
                    })
                    .reduce(|left, right| if right.more_salient_than(left) { right } else { left });
                if let Some(bar) = selected {
                    let pixels = bar_height(bar.depth, max, height);
                    let x = usize::from(column);
                    match bar.kind {
                        BarKind::Insertion => {
                            let weights = [
                                (Indel::INSERTION as Pigment, bar.signal),
                                (Coverage::FG, bar.depth - bar.signal),
                            ];
                            self.paint_stack(x, height, segments(weights, bar.depth, Some(threshold), pixels, height));
                        }
                        BarKind::Alternates(alternates) if reaches_threshold(bar.signal, bar.depth, threshold) => {
                            let weighted = allele_weights(alternates, bar.depth);
                            let stack = segments(weighted, bar.depth, Some(threshold), pixels, height);
                            self.paint_stack(x, height, stack);
                        }
                        BarKind::FiveMc(calls) if bar.signal != 0 => {
                            let weighted = five_mc_weights(calls, bar.depth);
                            self.paint_stack(x, height, segments(weighted, bar.depth, None, pixels, height));
                        }
                        _ => self
                            .framebuffer
                            .fill_rect(x, height - pixels, 1, pixels, Coverage::FG()),
                    }
                }
            }
        }

        let background = if show_5mc { Coverage::BG() } else { Surface::TRACK_BG() };
        self.framebuffer
            .reduce_into(&mut self.reduced, 1, PIXELS_PER_ROW, background);
        let buffer = frame.buffer_mut();
        paint_track_background(buffer, area, in_bounds.clone());
        for (row, cells) in self.reduced.chunks_exact(width).enumerate() {
            let y = area.y + row as u16;
            for column in in_bounds.clone() {
                let pixel = cells[usize::from(column)];
                if let Some(cell) = buffer.cell_mut((area.x + column, y)) {
                    let mut utf8 = [0; 4];
                    paint_cell(cell, pixel.symbol.encode_utf8(&mut utf8), pixel.fg, pixel.bg);
                }
            }
        }
        if view.settings.centline
            && let Some((x, glyph)) = centline_mark(area, bp_per_column, view.position)
            && (0..PIXELS_PER_ROW).all(|y| self.framebuffer.pixel(usize::from(x - area.x), y).is_none())
            && let Some(cell) = buffer.cell_mut((x, area.y))
        {
            paint_cell(cell, glyph, Text::PRIMARY_FG(), cell.bg);
        }
        if coverage.is_some() {
            draw_depth_axis(buffer, area, COVERAGE_TRACK_HEIGHT, max, max.div_ceil(2));
        }
    }

    fn paint_stack(&mut self, x: usize, mut bottom: usize, stack: impl IntoIterator<Item = (Pigment, usize)>) {
        for (pigment, size) in stack {
            bottom -= size;
            self.framebuffer.fill_rect(x, bottom, 1, size, pigment());
        }
    }
}

fn bar_height(depth: u32, max: u32, pane_pixels: usize) -> usize {
    let depth = u64::from(depth.min(max));
    let max = u64::from(max);
    ((depth * pane_pixels as u64 + max / 2) / max).max(1) as usize
}

#[derive(Clone, Copy)]
struct Bar {
    kind: BarKind,
    depth: u32,
    signal: u32,
}

#[derive(Clone, Copy)]
enum BarKind {
    /// A, C, G, T and deletion counts, with the reference base's zeroed.
    Alternates([u32; 5]),
    Insertion,
    FiveMc(CallCounts),
}

impl Bar {
    /// Without a known reference base nothing counts as alternate, so the bar stays grey.
    fn alleles(counts: BaseCounts, depth: u32, reference: Option<u8>) -> Self {
        let BaseCounts { a, c, g, t, n: _, del } = counts;
        let mut alternates = [a, c, g, t, del];
        match reference.and_then(|base| BaseResidue::try_from(base).ok()) {
            Some(BaseResidue::A) => alternates[0] = 0,
            Some(BaseResidue::C) => alternates[1] = 0,
            Some(BaseResidue::G) => alternates[2] = 0,
            Some(BaseResidue::T) => alternates[3] = 0,
            _ => alternates = [0; 5],
        }
        Self {
            kind: BarKind::Alternates(alternates),
            depth,
            signal: alternates.into_iter().sum(),
        }
    }

    fn five_mc(depth: u32, calls: CallCounts) -> Self {
        Self {
            kind: BarKind::FiveMc(calls),
            depth,
            signal: calls.into_iter().fold(0, u32::saturating_add),
        }
    }

    fn more_salient_than(self, other: Self) -> bool {
        match (self.kind, other.kind) {
            (BarKind::Insertion, BarKind::Insertion) => {}
            (BarKind::Insertion, _) => return true,
            (_, BarKind::Insertion) => return false,
            _ => {}
        }
        u64::from(self.signal) * u64::from(other.depth) > u64::from(other.signal) * u64::from(self.depth)
    }
}

/// A fixed order, so a given allele sits at the same place in every bar. The rest of `depth` is grey.
fn allele_weights(alternates: [u32; 5], depth: u32) -> [(Pigment, u32); 6] {
    let [a, c, g, t, del] = alternates;
    let grey = depth - alternates.into_iter().sum::<u32>();
    [
        (Reference::BASE_A, a),
        (Reference::BASE_C, c),
        (Reference::BASE_G, g),
        (Reference::BASE_T, t),
        (Indel::DELETION, del),
        (Coverage::FG, grey),
    ]
}

/// Reads without a call make up the grey rest of `depth`.
fn five_mc_weights(calls: CallCounts, depth: u32) -> [(Pigment, u32); 4] {
    let no_call_depth = depth - calls.into_iter().sum::<u32>();
    [
        (mod_5mc_pigment(Call::Hyper), calls[Call::Hyper as usize]),
        (mod_5mc_pigment(Call::Uncertain), calls[Call::Uncertain as usize]),
        (mod_5mc_pigment(Call::Hypo), calls[Call::Hypo as usize]),
        (Coverage::FG, no_call_depth),
    ]
}

fn reaches_threshold(count: u32, depth: u32, threshold: u64) -> bool {
    100 * u64::from(count) >= threshold * u64::from(depth)
}

fn segments<const N: usize>(
    weighted: [(Pigment, u32); N],
    depth: u32,
    threshold: Option<u64>,
    pixels: usize,
    pane_pixels: usize,
) -> [(Pigment, usize); N] {
    let weights = weighted.map(|(_, weight)| weight);
    let required: [bool; N] = std::array::from_fn(|i| {
        i < N - 1 && threshold.is_some_and(|threshold| reaches_threshold(weights[i], depth, threshold))
    });
    let minimum = required.iter().filter(|&&required| required).count();
    let height = pixels.max(minimum).min(pane_pixels);
    let mut sizes = [0; N];
    // Give each qualifying allele a pixel, then assign pixels by proportional deficit.
    for _ in 0..height {
        let index = (0..N)
            .filter(|&i| weights[i] != 0)
            .max_by_key(|&i| {
                let needs_pixel = required[i] && sizes[i] == 0;
                let deficit = i64::from(weights[i]) * height as i64 - i64::from(depth) * sizes[i] as i64;
                (needs_pixel, deficit, Reverse(i))
            })
            .expect("stacked bars have at least one nonzero segment");
        sizes[index] += 1;
    }
    std::array::from_fn(|i| (weighted[i].0, sizes[i]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
    use crate::style::palette::lock_for_test;
    use crate::viewer_app::genome::{
        alignment::CoverageWindow, reference_data::ReferenceData, settings::GenomeViewerSettings, track::Contig,
        zoom::HorizontalZoom,
    };
    use crate::viewer_app::viewport::ViewPosition;
    use rand::{RngExt, SeedableRng, rngs::StdRng};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    #[test]
    fn centre_mark_preserves_a_partly_filled_top_cell() {
        let _palette = lock_for_test();
        let mut track = CoverageTrack::new();
        let contig = Contig {
            name: "chr1".to_owned(),
            length: BasePair::length(18),
        };
        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut settings = GenomeViewerSettings::new();
        settings.centline = true;
        settings.readcolor = ReadColor::Lane;
        let area = Rect::new(0, 0, 9, COVERAGE_TRACK_HEIGHT);
        let max = u32::from(area.height) * PIXELS_PER_ROW as u32;
        let mut counts = [BaseCounts {
            n: max - 1,
            ..BaseCounts::default()
        }; 18];
        counts[0].n = max;
        let coverage = WindowCoverage {
            centre: CoverageWindow {
                counts: &counts,
                calls: &[],
                insertions: &[],
                window_start: BasePair(0),
            },
            left: None,
            right: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();

        for zoom in [HorizontalZoom::Close, HorizontalZoom::Mid] {
            settings.horizontal_zoom = zoom;
            let span = BasePair(u64::from(area.width) * zoom.bp_per_column());
            let position = ViewPosition::new(span / 2, span);
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
                        max,
                    )
                })
                .unwrap();

            let buffer = terminal.backend().buffer();
            let centre = area.width / 2;
            let top = &buffer[(centre, 0)];
            assert_eq!(top.fg, Surface::TRACK_BG(), "the upper half stays empty at {zoom:?}");
            assert_eq!(top.bg, Coverage::FG(), "the lower half retains coverage at {zoom:?}");
            assert_eq!(
                top,
                &buffer[(centre + 1, 0)],
                "equal depths have equal bars at {zoom:?}"
            );
        }
    }

    #[test]
    fn axis_labels_show_the_scale_bounds_and_rounded_midpoint() {
        let _palette = lock_for_test();
        for rows in [1, 3, COVERAGE_TRACK_HEIGHT] {
            let area = Rect::new(3, 2, 12, rows);
            for (max, middle) in [(0, 0), (1, 1), (12, 6), (37, 19), (u32::MAX, 2_147_483_648)] {
                let expected = [Some(max), None, Some(middle), None, Some(0)];
                let mut buffer = Buffer::empty(area);
                draw_depth_axis(&mut buffer, area, COVERAGE_TRACK_HEIGHT, max, max.div_ceil(2));
                for (y, expected) in (area.top()..area.bottom()).zip(expected) {
                    let label: String = (area.left() + 1..area.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    assert_eq!(
                        label.trim().parse::<u32>().ok(),
                        expected,
                        "row {y} at scale {max} over {rows} rows"
                    );
                }
            }
        }
    }

    #[test]
    fn segments_partition_scaled_bars_and_preserve_qualifying_alleles() {
        let mut rng = StdRng::seed_from_u64(0xC0FE_12A1);
        let full_height = usize::from(COVERAGE_TRACK_HEIGHT) * PIXELS_PER_ROW;
        let inputs = [[4, 1, 0, 0, 0, 0], [1, 1, 0, 0, 0, 0], [1, 1, 1, 1, 1, 1]]
            .into_iter()
            .chain((0..200).map(|_| std::array::from_fn(|_| rng.random_range(0..100))));

        for weights @ [a, c, g, t, del, n] in inputs {
            let depth = weights.iter().sum::<u32>();
            if depth == 0 {
                continue;
            }
            let counts = BaseCounts { a, c, g, t, n, del };
            for (reference, reference_index) in [
                (Some(b'a'), Some(0)),
                (Some(b'C'), Some(1)),
                (Some(b'g'), Some(2)),
                (Some(b'T'), Some(3)),
                (Some(b'N'), None),
                (None, None),
            ] {
                let alternates: [u32; 5] = std::array::from_fn(|i| {
                    if reference_index.is_some_and(|r| r != i) {
                        weights[i]
                    } else {
                        0
                    }
                });
                let alternate = alternates.iter().sum::<u32>();
                let bar = Bar::alleles(counts, depth, reference);
                assert!(matches!(bar.kind, BarKind::Alternates(kind) if kind == alternates));
                let stacked: [u32; 6] =
                    std::array::from_fn(|i| alternates.get(i).copied().unwrap_or(depth - alternate));
                for &threshold in &[1, 20, 50, 100] {
                    let coloured = alternate * 100 >= threshold * depth;
                    assert_eq!(reaches_threshold(bar.signal, depth, u64::from(threshold)), coloured);
                    if !coloured {
                        continue;
                    }
                    let required = alternates.iter().map(|&weight| weight * 100 >= threshold * depth);
                    let minimum = required.clone().filter(|&required| required).count();
                    for capacity in [2, full_height] {
                        assert_eq!(bar_height(depth, depth, capacity), capacity);
                        for maximum in [depth, depth * 32] {
                            let pixels = bar_height(depth, maximum, capacity);
                            let segments = segments(
                                allele_weights(alternates, depth),
                                depth,
                                Some(u64::from(threshold)),
                                pixels,
                                capacity,
                            )
                            .map(|(_, size)| size);
                            let height = pixels.max(minimum).min(capacity);
                            assert_eq!(segments.iter().sum::<usize>(), height);
                            if minimum <= capacity {
                                for (i, required) in required.clone().enumerate() {
                                    assert!(!required || segments[i] > 0, "allele {i} vanished from {stacked:?}");
                                }
                            }
                            assert!(
                                segments
                                    .iter()
                                    .zip(stacked)
                                    .all(|(&size, weight)| size == 0 || weight > 0)
                            );
                            // A required allele with a whole pixel in its quota leaves rounding unchanged.
                            let height = height as u32;
                            if required
                                .clone()
                                .zip(stacked)
                                .all(|(needed, weight)| !needed || weight * height >= depth)
                            {
                                for (&size, weight) in segments.iter().zip(stacked) {
                                    assert!(
                                        (size as u32 * depth).abs_diff(weight * height) < depth,
                                        "segment for weight {weight} in {stacked:?} is {size}/{height}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
