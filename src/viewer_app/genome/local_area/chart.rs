//! The local-area chart projects the summary cache's genomic bins into the track's current
//! [`AreaSpan`], then rasterises depth into vertical half-cells. The coverage chart uses two
//! genomic halves per column; the 5mC chart paints one mean across the full column.
//!
//! Spanning and deletion depths project as bp-overlap-weighted means, preserving a boundary bin's
//! influence in proportion to its overlap. Insertions are zero-reference-span point events, so each
//! bin's insertion count and length are attributed once at its clamped midpoint and then projected
//! across resident neighbours as a symmetric inverted wedge whose top width matches the mean
//! inserted length at the track's bp scale. The resulting spanning, deletion, and insertion depths
//! form a bottom-up stack; breakpoint highlighting changes only the plain layer.
//!
//! Complete summary coverage is the contract: missing halves stay fog, and must never be treated
//! as zero depth or breakpoint evidence. This full-width projection is the rendering expression
//! of invariant 9: each terminal column keeps the same genomic meaning across tracks. The main
//! raster trap is that one terminal cell can carry only two colours, so the quadrant fallback
//! deliberately sacrifices horizontal detail rather than inventing a colour or painting a layer
//! on the wrong genomic side.

use std::ops::Range;

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Style},
};

use crate::basepair::BasePair;
use crate::style::{Alignment, Coverage, Indel, LocalArea, Surface, Text};
use crate::viewer_app::genome::geometry::AreaSpan;

use super::summary::SummaryCache;
use super::{FOG_GLYPH, column_span};

const HALF_LEVELS_PER_ROW: usize = 2;

/// Least-squares window for the breakpoint gradient, in half-columns.
///
/// Denominated in half-columns rather than bp so it scales with the span for free: the
/// gradient it produces is depth-per-half-column, which is the unit the angle threshold
/// compares against. A bp-denominated window would have to be re-derived per frame and
/// would mean a different sensitivity at the same setting on a wide versus narrow pane.
///
/// Odd, so a centred window has an equal number of samples either side of its subject.
///
/// The width is also what sets the reachable angle range, which is why it is 5 rather than
/// something wider. A least-squares fit spreads a step's whole change across the window, so
/// the fitted slope carries a factor of roughly one over the width: at 9 half-columns even a
/// total dropout only reaches ~21 degrees, squeezing every real event into the bottom of the
/// scale. At 5 the steepest possible profile reaches exactly [`MAX_BREAKPOINT_ANGLE`], and a
/// 2:1 step — the copy-number event this exists to catch — sits near 12, so the useful
/// thresholds are spread across the range instead of bunched at its floor.
const BREAKPOINT_WINDOW: usize = 5;

/// The steepest angle [`BREAKPOINT_WINDOW`] can produce.
///
/// A normalised slope cannot exceed this: the window mean it divides by is itself raised by
/// the depth on the high side of a step. `localareabreakangle` declares this as its upper
/// bound, and `breakpoint_setting_range_matches_the_reachable_angle_ceiling` proves the two
/// agree — widening the window without lowering that bound would leave the top of the
/// setting's range unable to flag anything, which no other test here would notice.
#[cfg(test)]
const MAX_BREAKPOINT_ANGLE: u64 = 45;
const QUADRANT_GLYPHS: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▙', '▗', '▟', '▐', '▜', '▄', '▛', '▚', '█',
];

#[derive(Clone, Copy, Default)]
struct ProjectedStack {
    spanning_depth: f64,
    deletion_depth: f64,
    insertion_depth: f64,
    has_point_insertion: bool,
}

impl ProjectedStack {
    fn plain_depth(&self) -> f64 {
        (self.spanning_depth - self.deletion_depth - self.insertion_depth).max(0.0)
    }
}

#[derive(Clone, Copy, Default)]
struct ProjectedHalf {
    spanning_depth: f64,
    deletion_depth: f64,
    insertion_depth: f64,
    mean_insertion_len: f64,
}

#[derive(Clone, Copy)]
struct Layer {
    depth: f64,
    color: Color,
    floor: bool,
}

/// Breakpoint highlighting as the settings ask for it, resolved to one per-column decision.
#[derive(Clone, Copy)]
pub(in crate::viewer_app::genome) struct Breakpoints {
    pub(in crate::viewer_app::genome) enabled: bool,
    pub(in crate::viewer_app::genome) angle: f64,
}

impl Breakpoints {
    /// One flag per terminal column; all-false when disabled, so the caller has one path.
    fn flags(self, projected: &[Option<(ProjectedStack, ProjectedStack)>]) -> Vec<bool> {
        match self.enabled {
            true => flagged_columns(projected, self.angle),
            false => vec![false; projected.len()],
        }
    }
}

/// Draws complete summary columns and returns the maximum depth used to scale them.
pub(in crate::viewer_app::genome) fn draw_area_chart(
    buffer: &mut Buffer,
    area: Rect,
    span: AreaSpan,
    summary: &SummaryCache,
    breakpoints: Breakpoints,
    centreline_read_spans: &[Range<BasePair>],
) -> Option<f64> {
    let rows = area.height.min(super::REFERENCE_BAND_ROW);
    if rows == 0 || area.width == 0 {
        return None;
    }

    let mut raw_halves = Vec::with_capacity(usize::from(area.width) * 2);
    for column in 0..area.width {
        let (left_start, left_end) = half_column_span(span, column, false, area.width);
        let (right_start, right_end) = half_column_span(span, column, true, area.width);
        match project(summary, span, left_start, left_end).zip(project(summary, span, right_start, right_end)) {
            Some((left, right)) => {
                raw_halves.push(Some(left));
                raw_halves.push(Some(right));
            }
            None => {
                raw_halves.push(None);
                raw_halves.push(None);
            }
        }
    }

    let wedged = apply_insertion_wedges(&raw_halves, span);
    let mut projected = Vec::with_capacity(usize::from(area.width));
    let mut max_depth = 0.0f64;
    let mut has_resident = false;
    for &[left, right] in wedged.as_chunks::<2>().0 {
        let pair = left.zip(right);
        if let Some((left, right)) = &pair {
            has_resident = true;
            max_depth = max_depth.max(left.spanning_depth).max(right.spanning_depth);
        }
        projected.push(pair);
    }

    let flagged = breakpoints.flags(&projected);
    for (column, pair) in projected.iter().enumerate() {
        let Some((left, right)) = pair else {
            draw_fog_column(buffer, area, rows, column as u16, Surface::TRACK_BG());
            continue;
        };
        let plain_color = match flagged[column] {
            true => LocalArea::BREAKPOINT_COVERAGE_FG(),
            false => Coverage::FG(),
        };
        let rasterize_half = |stack, right| {
            let centreline_depth = if centreline_read_spans.is_empty() {
                0.0
            } else {
                let (start, end) = half_column_span(span, column as u16, right, area.width);
                mean_span_depth(centreline_read_spans, start, end)
            };
            rasterize(stack, max_depth, rows, plain_color, centreline_depth)
        };
        let left = rasterize_half(left, false);
        let right = rasterize_half(right, true);
        for row in 0..rows {
            let top = (usize::from(rows - row) * HALF_LEVELS_PER_ROW) - 1;
            let bottom = top - 1;
            let colours = [left[top], right[top], left[bottom], right[bottom]];
            if let Some(cell) = buffer.cell_mut((area.x + column as u16, area.y + row)) {
                paint_quadrants(cell, colours);
            }
        }
    }

    has_resident.then_some(max_depth)
}

struct Projected5mC {
    spanning_depth: f64,
    mean_ml: Option<f64>,
}

pub(in crate::viewer_app::genome) fn draw_area_chart_5mc(
    buffer: &mut Buffer,
    area: Rect,
    span: AreaSpan,
    summary: &SummaryCache,
) -> Option<f64> {
    let rows = area.height.min(super::REFERENCE_BAND_ROW);
    if rows == 0 || area.width == 0 {
        return None;
    }

    let projected: Vec<_> = (0..area.width)
        .map(|column| {
            let (start, end) = column_span(span, column, area.width);
            project_5mc(summary, start, end)
        })
        .collect();
    let max_depth = projected
        .iter()
        .flatten()
        .map(|column| column.spanning_depth)
        .reduce(f64::max);
    let background = Coverage::BG();
    let levels = usize::from(rows) * HALF_LEVELS_PER_ROW;
    for (column, projected) in (0..area.width).zip(projected) {
        let Some(projected) = projected else {
            draw_fog_column(buffer, area, rows, column, background);
            continue;
        };
        let filled = if projected.spanning_depth > 0.0 {
            let fraction = projected.spanning_depth / max_depth.unwrap_or_default();
            ((fraction * levels as f64).round() as usize).clamp(1, levels)
        } else {
            0
        };
        let empty = Style::default().bg(background);
        let foreground = projected
            .mean_ml
            .map_or_else(LocalArea::MOD_5MC_NO_DATA, LocalArea::mod_5mc_grey);
        let bar = Style::default().fg(foreground).bg(background);
        for row in 0..rows {
            let below = usize::from(rows - row - 1) * HALF_LEVELS_PER_ROW;
            let glyph = [' ', '▄', '█'][filled.saturating_sub(below).min(HALF_LEVELS_PER_ROW)];
            let style = if glyph == ' ' { empty } else { bar };
            if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
                cell.set_char(glyph).set_style(style);
            }
        }
    }
    max_depth
}

fn project_5mc(summary: &SummaryCache, start: BasePair, end: BasePair) -> Option<Projected5mC> {
    let mut total_overlap = 0.0;
    let mut spanning = 0.0;
    let mut prob_sum = 0.0;
    let mut call_count = 0.0;
    let complete = summary.visit_resident_bins(start, end, |bin, _, _, overlap| {
        let weight = overlap as f64;
        total_overlap += weight;
        spanning += bin.spanning_depth as f64 * weight;
        if bin.mod_5mc_call_count > 0 {
            prob_sum += bin.mod_5mc_prob_sum as f64 * weight;
            call_count += bin.mod_5mc_call_count as f64 * weight;
        }
    });
    complete.then(|| Projected5mC {
        spanning_depth: spanning / total_overlap,
        mean_ml: (call_count > 0.0).then(|| prob_sum / call_count),
    })
}

fn half_column_span(span: AreaSpan, column: u16, right: bool, columns: u16) -> (BasePair, BasePair) {
    let width = u128::from(span.width().as_u64());
    let halves = u128::from(columns) * 2;
    let half = u128::from(column) * 2 + u128::from(right);
    let start_offset = half * width / halves;
    let end_offset = (half + 1) * width / halves;
    let start = span.start() + BasePair(start_offset as u64);
    let end = (span.start() + BasePair(end_offset.max(start_offset + 1) as u64)).min(span.end());
    (start, end)
}

fn mean_span_depth(spans: &[Range<BasePair>], start: BasePair, end: BasePair) -> f64 {
    let covered = spans
        .iter()
        .map(|span| (span.end.min(end) - span.start.max(start)).as_f64())
        .sum::<f64>();
    covered / (end - start).as_f64()
}

fn project(summary: &SummaryCache, span: AreaSpan, start: BasePair, end: BasePair) -> Option<ProjectedHalf> {
    let mut total_overlap = 0u64;
    let mut spanning = 0.0;
    let mut deletion = 0.0;
    let mut raw_insertion_depth = 0.0;
    let mut raw_insertion_bp = 0.0;
    let last_in_span = span.end() - BasePair(1);
    let complete = summary.visit_resident_bins(start, end, |bin, bin_start, bin_end, overlap| {
        total_overlap += overlap;
        let weight = overlap as f64;
        spanning += bin.spanning_depth as f64 * weight;
        deletion += bin.deletion_depth as f64 * weight;

        // Point events belong to the single half-column covering the bin's clamped midpoint so
        // insertions are neither diluted across multi-bin halves nor split across boundaries.
        let mid = (bin_start + (bin_end - bin_start) / 2).clamp(span.start(), last_in_span);
        if mid >= start && mid < end {
            raw_insertion_depth += bin.insertion_depth as f64;
            raw_insertion_bp += bin.insertion_bp as f64;
        }
    });
    if !complete || total_overlap == 0 {
        return None;
    }

    let denominator = total_overlap as f64;
    let spanning_depth = spanning / denominator;
    let deletion_depth = deletion / denominator;
    let insertion_depth = raw_insertion_depth.min(spanning_depth);
    let mean_insertion_len = if raw_insertion_depth > 0.0 {
        raw_insertion_bp / raw_insertion_depth
    } else {
        0.0
    };
    Some(ProjectedHalf {
        spanning_depth,
        deletion_depth,
        insertion_depth,
        mean_insertion_len,
    })
}

/// Projects each anchor's insertion across resident neighbours as a symmetric inverted wedge
/// whose top width matches its mean inserted length at the track's bp scale.
fn apply_insertion_wedges(halves: &[Option<ProjectedHalf>], span: AreaSpan) -> Vec<Option<ProjectedStack>> {
    if halves.is_empty() {
        return Vec::new();
    }
    let bp_per_half_column = span.width().as_f64() / halves.len() as f64;
    let mut wings = vec![0.0f64; halves.len()];

    if bp_per_half_column > 0.0 {
        for (anchor, maybe_half) in halves.iter().enumerate() {
            let Some(half) = maybe_half else {
                continue;
            };
            if half.insertion_depth <= 0.0 || half.mean_insertion_len <= 0.0 {
                continue;
            }
            let half_width = (half.mean_insertion_len / bp_per_half_column) / 2.0;
            if half_width <= 1.0 {
                continue;
            }
            for direction in [-1isize, 1isize] {
                let mut dx = 1usize;
                while (dx as f64) < half_width {
                    let Some(target) = anchor.checked_add_signed(direction * dx as isize) else {
                        break;
                    };
                    if target >= halves.len() || halves[target].is_none() {
                        break;
                    }
                    let depth = half.insertion_depth * (1.0 - dx as f64 / half_width);
                    wings[target] = wings[target].max(depth);
                    dx += 1;
                }
            }
        }
    }

    halves
        .iter()
        .zip(wings)
        .map(|(maybe_half, wing)| {
            let half = (*maybe_half)?;
            let has_point_insertion = half.insertion_depth > 0.0;
            let capped_wing = wing.min((half.spanning_depth - half.deletion_depth).max(0.0));
            let insertion_depth = if has_point_insertion {
                half.insertion_depth.max(capped_wing).min(half.spanning_depth)
            } else {
                capped_wing
            };
            Some(ProjectedStack {
                spanning_depth: half.spanning_depth,
                deletion_depth: half.deletion_depth,
                insertion_depth,
                has_point_insertion,
            })
        })
        .collect()
}

/// Flags each terminal column whose local depth gradient exceeds `angle` degrees.
///
/// A CNV boundary is a step in coverage, and a step's signature is a large *first*
/// derivative — hence a least-squares slope rather than a curvature measure, which would
/// peak at the step's corners and leave the step itself unflagged.
///
/// The slope is divided by the window's mean depth before `atan`, which is what makes the
/// threshold portable: a raw slope is depth-per-half-column, so the same proportional
/// dropoff reads an order of magnitude apart at 200X and at 20X. Normalised, the angle is
/// a claim about the drawn picture — the chart's vertical scale is the frame maximum, so a
/// normalised slope of 1.0 is roughly 45 degrees on screen. Normalising to the window mean
/// rather than that frame maximum keeps the measure locally referenced, at the cost of
/// making the on-screen correspondence approximate away from the deepest column.
///
/// A window containing a non-resident half-column yields nothing. Fog must not be read as
/// zero depth: doing so would turn every partially loaded frame's fog boundary into a
/// vertical cliff and light up the whole pane while it fills.
fn flagged_columns(projected: &[Option<(ProjectedStack, ProjectedStack)>], angle: f64) -> Vec<bool> {
    let halves: Vec<Option<f64>> = projected
        .iter()
        .flat_map(|pair| match pair {
            Some((left, right)) => [Some(left.spanning_depth), Some(right.spanning_depth)],
            None => [None, None],
        })
        .collect();

    let steep = |centre: usize| {
        let half = BREAKPOINT_WINDOW / 2;
        let start = centre.checked_sub(half)?;
        let end = centre + half + 1;
        let window = halves.get(start..end)?;
        let depths = window.iter().copied().collect::<Option<Vec<f64>>>()?;

        // Least-squares slope about the window's centre. x is symmetric around zero, so
        // the mean of x is zero and the usual covariance/variance form reduces to this.
        let mean_depth = depths.iter().sum::<f64>() / depths.len() as f64;
        if mean_depth <= 0.0 {
            return None;
        }
        let offset = |index: usize| index as f64 - half as f64;
        let covariance: f64 = depths
            .iter()
            .enumerate()
            .map(|(index, depth)| offset(index) * (depth - mean_depth))
            .sum();
        let variance: f64 = (0..depths.len()).map(|index| offset(index).powi(2)).sum();
        let slope = covariance / variance;
        Some((slope / mean_depth).abs().atan().to_degrees() >= angle)
    };

    (0..projected.len())
        .map(|column| {
            [column * 2, column * 2 + 1]
                .iter()
                .any(|&half| steep(half) == Some(true))
        })
        .collect()
}

/// Bottom-up: plain coverage, then deletions, then insertions. The summary counts each
/// read once per kind, so these three already partition spanning depth.
fn layers(stack: &ProjectedStack, plain_color: Color) -> Vec<Layer> {
    vec![
        Layer {
            depth: stack.plain_depth(),
            color: plain_color,
            floor: false,
        },
        Layer {
            depth: stack.deletion_depth,
            color: Indel::DELETION(),
            floor: true,
        },
        Layer {
            depth: stack.insertion_depth,
            color: Indel::INSERTION(),
            floor: stack.has_point_insertion,
        },
    ]
}

fn rasterize(
    stack: &ProjectedStack,
    max_depth: f64,
    rows: u16,
    plain_color: Color,
    centreline_depth: f64,
) -> Vec<Color> {
    let levels = usize::from(rows) * HALF_LEVELS_PER_ROW;
    let mut pixels = vec![Surface::TRACK_BG(); levels];
    if max_depth <= 0.0 || stack.spanning_depth <= 0.0 {
        return pixels;
    }

    let total_end = ((stack.spanning_depth / max_depth) * levels as f64)
        .round()
        .clamp(0.0, levels as f64) as usize;
    let layers = layers(stack, plain_color);
    let mut bounds = Vec::with_capacity(layers.len());
    let mut cumulative = 0.0;
    let mut previous = 0usize;
    for layer in &layers {
        cumulative += layer.depth;
        let end = ((cumulative / max_depth) * levels as f64)
            .round()
            .clamp(previous as f64, total_end as f64) as usize;
        bounds.push(end);
        previous = end;
    }

    // A present-but-rare indel rounds to nothing against a deep stack. Give it one
    // half-cell borrowed from the layer below rather than let it disappear, since
    // "something happened here" is the whole point of the colour at this scale. Borrowing
    // downward keeps the painted total equal to the quantised spanning depth. Projected
    // wedge wings opt out (`floor == false`) so tips taper smoothly to zero half-cells.
    for index in (1..layers.len()).rev() {
        if !layers[index].floor || layers[index].depth <= 0.0 || bounds[index] > bounds[index - 1] {
            continue;
        }
        let Some(donor) = (0..index).rev().find(|&below| {
            let start = if below == 0 { 0 } else { bounds[below - 1] };
            bounds[below] > start
        }) else {
            continue;
        };
        for boundary in &mut bounds[donor..index] {
            *boundary -= 1;
        }
    }

    let mut previous = 0usize;
    for (layer, &end) in layers.iter().zip(&bounds) {
        pixels[previous..end].fill(layer.color);
        previous = end;
    }
    if previous == 0 {
        pixels[0] = plain_color;
    }
    if centreline_depth > 0.0 {
        // Give the cohort its share of ordinary coverage and keep the indel layers separate.
        let fraction = (centreline_depth / stack.spanning_depth).min(1.0);
        let plain_levels = if previous == 0 { 1 } else { bounds[0] };
        let highlighted = (fraction * plain_levels as f64).round() as usize;
        pixels[..highlighted].fill(Alignment::HIGHLIGHT_A());
    }
    pixels
}

fn paint_quadrants(cell: &mut Cell, mut colours: [Color; 4]) {
    let mut distinct = Vec::new();
    for colour in colours {
        if !distinct.contains(&colour) {
            distinct.push(colour);
        }
    }
    if distinct.len() > 2 {
        let left_area = usize::from(colours[0] != Surface::TRACK_BG()) + usize::from(colours[2] != Surface::TRACK_BG());
        let right_area =
            usize::from(colours[1] != Surface::TRACK_BG()) + usize::from(colours[3] != Surface::TRACK_BG());
        let (top, bottom) = if left_area >= right_area {
            (colours[0], colours[2])
        } else {
            (colours[1], colours[3])
        };
        colours = [top, top, bottom, bottom];
        distinct.clear();
        for colour in colours {
            if !distinct.contains(&colour) {
                distinct.push(colour);
            }
        }
    }

    if distinct.len() == 1 {
        let color = distinct[0];
        if color == Surface::TRACK_BG() {
            cell.set_symbol(" ");
            cell.set_style(Style::default().fg(color).bg(color));
        } else {
            cell.set_char('█');
            cell.set_style(Style::default().fg(color).bg(Surface::TRACK_BG()));
        }
        return;
    }

    let foreground = distinct
        .iter()
        .copied()
        .find(|&color| color != Surface::TRACK_BG())
        .unwrap_or(distinct[0]);
    let background = distinct
        .iter()
        .copied()
        .find(|&color| color != foreground)
        .unwrap_or(Surface::TRACK_BG());
    let mask = colours.iter().enumerate().fold(0u8, |mask, (index, &color)| {
        mask | (u8::from(color == foreground) << index)
    });
    cell.set_char(QUADRANT_GLYPHS[mask as usize]);
    cell.set_style(Style::default().fg(foreground).bg(background));
}

fn draw_fog_column(buffer: &mut Buffer, area: Rect, rows: u16, column: u16, background: Color) {
    for row in 0..rows {
        if let Some(cell) = buffer.cell_mut((area.x + column, area.y + row)) {
            cell.set_char(FOG_GLYPH);
            cell.set_style(Style::default().fg(Text::DIMMED_FG()).bg(background));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer_app::genome::local_area::summary::SummaryBin;
    use crate::viewer_app::viewport::ViewPosition;

    fn span(width: u64) -> AreaSpan {
        AreaSpan::for_viewport(
            ViewPosition::new(BasePair(width / 2), BasePair(width / 2)),
            2,
            BasePair(width),
        )
    }

    fn cache_with_bins(bins: Vec<SummaryBin>) -> (SummaryCache, AreaSpan) {
        let area_span = span(bins.len() as u64);
        let mut cache = SummaryCache::new();
        cache.seed_bins("chr1", area_span, bins);
        (cache, area_span)
    }

    fn profile(depths: &[Option<f64>]) -> Vec<Option<(ProjectedStack, ProjectedStack)>> {
        let stack = |depth: f64| ProjectedStack {
            spanning_depth: depth,
            deletion_depth: 0.0,
            insertion_depth: 0.0,
            has_point_insertion: false,
        };
        depths
            .chunks(2)
            .map(|pair| match (pair[0], pair[1]) {
                (Some(left), Some(right)) => Some((stack(left), stack(right))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn chart_projection_partitions_intervals_and_preserves_depth_layers() {
        let mut bins = vec![SummaryBin::default(); 1_500];
        for (index, depth) in [3, 11, 29, 47].into_iter().enumerate() {
            bins[index].spanning_depth = depth;
            bins[index].deletion_depth = u64::from(index == 2);
            bins[index].insertion_depth = u64::from(index == 3);
        }
        let (cache, area_span) = cache_with_bins(bins);
        let projected = project(&cache, area_span, BasePair(0), BasePair(5)).expect("complete interval");
        assert_eq!(projected.spanning_depth, (3.0 * 4.0 + 11.0) / 5.0);
        assert_eq!(projected.deletion_depth, 0.0);
        assert!(project(&cache, area_span, BasePair(2_047), BasePair(2_050)).is_none());

        for (span_width, columns) in [(200, 40), (20_000, 80), (100_000, 201)] {
            let span = span(span_width);
            for column in 0..columns {
                let whole = super::super::column_span(span, column, columns);
                let left = half_column_span(span, column, false, columns);
                let right = half_column_span(span, column, true, columns);
                assert_eq!((whole.0, whole.1), (left.0, right.1));
                assert_eq!(left.1, right.0);
            }
        }

        let _palette = crate::style::palette::lock_for_test();
        let rows = 8;
        for (plain, deletion, insertion) in [(8.0, 0.0, 0.0), (7.0, 1.0, 0.0), (5.0, 2.0, 1.0), (1.0, 1.0, 1.0)] {
            let stack = ProjectedStack {
                spanning_depth: plain + deletion + insertion,
                deletion_depth: deletion,
                insertion_depth: insertion,
                has_point_insertion: insertion > 0.0,
            };
            let pixels = rasterize(&stack, stack.spanning_depth + 9.0, rows, Coverage::FG(), 0.0);
            let painted = pixels.iter().filter(|&&color| color != Surface::TRACK_BG()).count();
            let floors = usize::from(deletion > 0.0) + usize::from(insertion > 0.0);
            let budget = (((stack.spanning_depth / (stack.spanning_depth + 9.0)) * 16.0)
                .round()
                .max(1.0) as usize
                + floors)
                .min(16);
            assert!(painted <= budget);
            for (depth, color) in [
                (plain, Coverage::FG()),
                (deletion, Indel::DELETION()),
                (insertion, Indel::INSERTION()),
            ] {
                if depth > 0.0 {
                    assert!(pixels.contains(&color), "nonzero layer vanished");
                }
            }
        }

        // A sub-half-cell wedge wing must taper to zero rather than pick up a 1-half-cell floor,
        // while an anchor with the exact same depth keeps its 1-half-cell visibility floor.
        let tiny_wing = ProjectedStack {
            spanning_depth: 50.0,
            deletion_depth: 0.0,
            insertion_depth: 0.5,
            has_point_insertion: false,
        };
        let wing_pixels = rasterize(&tiny_wing, 50.0, rows, Coverage::FG(), 0.0);
        assert!(!wing_pixels.contains(&Indel::INSERTION()));

        let tiny_anchor = ProjectedStack {
            has_point_insertion: true,
            ..tiny_wing
        };
        let anchor_pixels = rasterize(&tiny_anchor, 50.0, rows, Coverage::FG(), 0.0);
        assert_eq!(
            anchor_pixels
                .iter()
                .filter(|&&color| color == Indel::INSERTION())
                .count(),
            1
        );
        assert!(area_span.width() > BasePair(0));
    }

    #[test]
    fn point_insertion_projection_avoids_dilution_and_boundary_splitting() {
        // 4_000 bp span -> bin_width = 8 (500 bins).
        let area_span = span(4_000);
        let mut bins = vec![
            SummaryBin {
                spanning_depth: 40,
                ..SummaryBin::default()
            };
            500
        ];
        // Bin 10 ([80, 88), midpoint 84) inside a 4-bin half-column [64, 96): no dilution.
        bins[10].insertion_depth = 15;
        bins[10].insertion_bp = 15 * 900;
        // Bins 20 ([160, 168), mid 164) and 21 ([168, 176), mid 172) in one half-column [160, 192):
        // depths sum and cap at spanning_depth without inflating mean_insertion_len.
        bins[20].insertion_depth = 30;
        bins[20].insertion_bp = 30 * 800;
        bins[21].insertion_depth = 30;
        bins[21].insertion_bp = 30 * 1_200;
        // Bin 30 ([240, 248), midpoint 244) straddles boundary 242 between [220, 242) and [242, 260).
        bins[30].insertion_depth = 12;
        bins[30].insertion_bp = 12 * 600;

        let mut cache = SummaryCache::new();
        cache.seed_bins("chr1", area_span, bins);

        let multi_bin = project(&cache, area_span, BasePair(64), BasePair(96)).expect("resident");
        assert_eq!(multi_bin.insertion_depth, 15.0);
        assert_eq!(multi_bin.mean_insertion_len, 900.0);

        let capped = project(&cache, area_span, BasePair(160), BasePair(192)).expect("resident");
        assert_eq!(capped.insertion_depth, 40.0);
        assert_eq!(capped.mean_insertion_len, 1_000.0);

        let left_of_mid = project(&cache, area_span, BasePair(220), BasePair(242)).expect("resident");
        let owner_of_mid = project(&cache, area_span, BasePair(242), BasePair(260)).expect("resident");
        assert_eq!(left_of_mid.insertion_depth, 0.0);
        assert_eq!(owner_of_mid.insertion_depth, 12.0);
        assert_eq!(owner_of_mid.mean_insertion_len, 600.0);

        // An edge bin straddling span.start() whose raw midpoint precedes span.start() clamps
        // into the leading half-column rather than dropping out: [102, 4_102) has bin_width = 8,
        // so bin 0 is [96, 104) with raw midpoint 100 < 102.
        let edge_span =
            AreaSpan::for_viewport(ViewPosition::new(BasePair(2_102), BasePair(2_000)), 2, BasePair(10_000));
        assert_eq!(edge_span.start(), BasePair(102));
        let mut edge_bins = vec![
            SummaryBin {
                spanning_depth: 20,
                ..SummaryBin::default()
            };
            502
        ];
        edge_bins[0].insertion_depth = 7;
        edge_bins[0].insertion_bp = 7 * 500;
        let mut edge_cache = SummaryCache::new();
        edge_cache.seed_bins("chr1", edge_span, edge_bins);
        let first_half = project(&edge_cache, edge_span, BasePair(102), BasePair(103)).expect("edge resident");
        let second_half = project(&edge_cache, edge_span, BasePair(103), BasePair(104)).expect("next resident");
        assert_eq!(first_half.insertion_depth, 7.0);
        assert_eq!(second_half.insertion_depth, 0.0);

        // Partial residency stays None (fog).
        let mut partial = SummaryCache::new();
        partial.seed_prefix("chr1", area_span, 2);
        assert!(project(&partial, area_span, BasePair(0), BasePair(32)).is_none());
    }

    #[test]
    fn insertion_wedges_taper_at_genomic_scale_and_respect_boundaries() {
        // 32 half-columns across 3_200 bp -> 100 bp per half-column.
        let area_span = span(3_200);
        let plain = |spanning: f64| {
            Some(ProjectedHalf {
                spanning_depth: spanning,
                deletion_depth: 0.0,
                insertion_depth: 0.0,
                mean_insertion_len: 0.0,
            })
        };

        // Single anchor at h = 10 with mean_len = 800 bp -> half_width = (800 / 100) / 2 = 4.0.
        let mut halves = vec![plain(40.0); 32];
        halves[10] = Some(ProjectedHalf {
            spanning_depth: 40.0,
            deletion_depth: 0.0,
            insertion_depth: 20.0,
            mean_insertion_len: 800.0,
        });
        let wedged = apply_insertion_wedges(&halves, area_span);
        for (h, expected_depth) in [
            (6, 0.0),
            (7, 5.0),
            (8, 10.0),
            (9, 15.0),
            (10, 20.0),
            (11, 15.0),
            (12, 10.0),
            (13, 5.0),
            (14, 0.0),
        ] {
            let stack = wedged[h].expect("resident");
            assert!(
                (stack.insertion_depth - expected_depth).abs() < 1e-9,
                "half {h}: got {}, expected {expected_depth}",
                stack.insertion_depth
            );
            assert_eq!(stack.has_point_insertion, h == 10);
        }

        // Overlapping wedges merge via max, not sum; short insertion (< 2 half-columns) stays at anchor only.
        halves[12] = Some(ProjectedHalf {
            spanning_depth: 40.0,
            deletion_depth: 0.0,
            insertion_depth: 16.0,
            mean_insertion_len: 800.0,
        });
        halves[25] = Some(ProjectedHalf {
            spanning_depth: 40.0,
            deletion_depth: 0.0,
            insertion_depth: 12.0,
            mean_insertion_len: 150.0, // half_width = 0.75 <= 1.0
        });
        // Fog wall at h = 8 stops anchor 10's left wing from crossing into h = 7.
        halves[8] = None;
        // Adjacent deletion at h = 11 leaves only 40 - 35 = 5 of headroom for wings.
        halves[11] = Some(ProjectedHalf {
            spanning_depth: 40.0,
            deletion_depth: 35.0,
            insertion_depth: 0.0,
            mean_insertion_len: 0.0,
        });
        let merged = apply_insertion_wedges(&halves, area_span);
        assert!(merged[8].is_none(), "fog must stay fog");
        assert_eq!(
            merged[7].expect("resident").insertion_depth,
            0.0,
            "wedge must stop at fog"
        );
        assert_eq!(
            merged[11].expect("resident").insertion_depth,
            5.0,
            "wing overhanging a deletion must cap at spanning - deletion"
        );
        // At h = 12, anchor 12 (16.0) beats wing from anchor 10 (10.0).
        assert_eq!(merged[12].expect("resident").insertion_depth, 16.0);
        // At h = 13, anchor 12 contributes 12.0 > anchor 10's 5.0 (max, not 17.0).
        assert!((merged[13].expect("resident").insertion_depth - 12.0).abs() < 1e-9);
        assert_eq!(merged[24].expect("resident").insertion_depth, 0.0);
        assert_eq!(merged[25].expect("resident").insertion_depth, 12.0);
        assert_eq!(merged[26].expect("resident").insertion_depth, 0.0);
    }

    #[test]
    fn draw_area_chart_paints_tapered_insertion_wedge_at_genomic_scale() {
        let _palette = crate::style::palette::lock_for_test();
        // 80 columns (160 half-columns) across 16_000 bp -> 100 bp/half-column.
        // GridKey::for_span(16_000) gives bin_width = 32 (500 bins).
        let area_span = span(16_000);
        let mut bins = vec![
            SummaryBin {
                spanning_depth: 40,
                ..SummaryBin::default()
            };
            500
        ];
        // Bin 250 covers [8_000, 8_032), midpoint 8_016 -> half-column 80 (column 40, left half).
        // 20 carriers with 1_600 bp insertions -> half_width = (1_600 / 100) / 2 = 8 half-columns (4 columns).
        bins[250].insertion_depth = 20;
        bins[250].insertion_bp = 20 * 1_600;
        let mut cache = SummaryCache::new();
        cache.seed_bins("chr1", area_span, bins);

        let area = Rect::new(0, 0, 80, super::super::REFERENCE_BAND_ROW);
        let mut buffer = Buffer::empty(area);
        draw_area_chart(
            &mut buffer,
            area,
            area_span,
            &cache,
            Breakpoints {
                enabled: false,
                angle: 12.0,
            },
            &[],
        );

        let insertion_rows = |column: u16| -> usize {
            (0..area.height)
                .filter(|&row| {
                    let cell = buffer.cell((column, row)).expect("chart cell");
                    cell.fg == Indel::INSERTION() || cell.bg == Indel::INSERTION()
                })
                .count()
        };

        let apex = insertion_rows(40);
        assert!(apex >= 4, "anchor column must paint deep insertion apex, got {apex}");
        // Both wings taper monotonically outward over ~4 columns and vanish outside the wedge.
        for offset in 1..=4u16 {
            assert!(
                insertion_rows(40 - offset) <= insertion_rows(40 - (offset - 1)),
                "left wing must taper monotonically at offset {offset}"
            );
            assert!(
                insertion_rows(40 + offset) <= insertion_rows(40 + (offset - 1)),
                "right wing must taper monotonically at offset {offset}"
            );
        }
        assert!(
            insertion_rows(38) > 0 && insertion_rows(42) > 0,
            "wedge must span neighbours"
        );
        assert!(
            insertion_rows(35) == 0 && insertion_rows(45) == 0,
            "outside the wedge must stay pure coverage with no 1-half-cell ears"
        );
    }

    #[test]
    fn breakpoint_flags_follow_transitions_thresholds_and_residency() {
        let step: Vec<Option<f64>> = (0..64).map(|half| Some(if half < 32 { 40.0 } else { 10.0 })).collect();
        let stepped_profile = profile(&step);
        let flags = flagged_columns(&stepped_profile, 8.0);
        assert!(flags[16]);
        assert!(!flags[2] && !flags[29]);
        let loose = flagged_columns(&stepped_profile, 5.0);
        let tight = flagged_columns(&stepped_profile, 40.0);
        assert!(tight.iter().zip(&loose).all(|(tight, loose)| !*tight || *loose));
        assert!(loose.iter().any(|&flag| flag));

        let mut fog = step;
        fog[32..].fill(None);
        assert!(flagged_columns(&profile(&fog), 8.0).iter().all(|&flag| !flag));
        for columns in 0..=BREAKPOINT_WINDOW {
            let narrow: Vec<Option<f64>> = (0..columns * 2).map(|half| Some(half as f64)).collect();
            assert_eq!(flagged_columns(&profile(&narrow), 8.0).len(), columns);
        }
        let disabled = Breakpoints {
            enabled: false,
            angle: 8.0,
        };
        assert_eq!(disabled.flags(&stepped_profile), vec![false; stepped_profile.len()]);
    }

    #[test]
    fn breakpoint_gradient_is_depth_scale_invariant() {
        for scale in [0.5, 2.0, 10.0, 100.0, 1_000.0, 5_000.0] {
            let flat = profile(&(0..64).map(|_| Some(scale)).collect::<Vec<_>>());
            assert!(flagged_columns(&flat, 8.0).iter().all(|&flag| !flag));
        }
        let stepped = |scale: f64, angle: f64| {
            let depths = (0..64)
                .map(|half| Some(if half < 32 { 4.0 * scale } else { scale }))
                .collect::<Vec<_>>();
            flagged_columns(&profile(&depths), angle)
        };
        for angle in 1..=MAX_BREAKPOINT_ANGLE {
            let expected = stepped(0.5, angle as f64);
            for scale in [2.0, 10.0, 100.0, 1_000.0, 5_000.0] {
                assert_eq!(stepped(scale, angle as f64), expected);
            }
        }
        assert!(stepped(0.5, 8.0).iter().any(|&flag| flag));
    }

    #[test]
    fn breakpoint_setting_range_matches_the_reachable_angle_ceiling() {
        use crate::setting::{Setting, SettingKind};
        let SettingKind::BoundedInteger { min, max } = Setting::Localareabreakangle.kind() else {
            panic!("localareabreakangle is a bounded integer");
        };
        assert!(min >= 1);
        assert_eq!(max, MAX_BREAKPOINT_ANGLE);

        let ceiling = MAX_BREAKPOINT_ANGLE as f64;
        let reaches_ceiling = (0..BREAKPOINT_WINDOW).any(|position| {
            let depths = (0..BREAKPOINT_WINDOW * 2)
                .map(|half| Some(if half < position * 2 { 100.0 } else { 0.0 }))
                .collect::<Vec<_>>();
            flagged_columns(&profile(&depths), ceiling).iter().any(|&flag| flag)
        });
        assert!(reaches_ceiling);
        for position in 0..=BREAKPOINT_WINDOW {
            for low in [66.0, 50.0, 25.0, 1.0, 0.0] {
                let depths = (0..BREAKPOINT_WINDOW * 2)
                    .map(|half| Some(if half < position * 2 { 100.0 } else { low }))
                    .collect::<Vec<_>>();
                assert!(
                    flagged_columns(&profile(&depths), ceiling + 0.001)
                        .iter()
                        .all(|&flag| !flag)
                );
            }
        }
    }
}
