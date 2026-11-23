//! Shared geometry for genome-track rendering.
//!
//! [`AreaSpan`] is the local-area track's deliberately different viewport: it scales the shell
//! viewport, clamps the result to the active contig, and slides the fixed-width span at either
//! edge. Unlike [`ViewPosition`], it never carries out-of-bounds width. Feature projections are
//! half-open and preserve visibility by assigning every non-empty intersection at least one
//! terminal column.
//!
//! The remaining helpers preserve the shared-column contract. The centre column is absolute
//! within the pane, and left-edge underflow becomes leading fallback columns rather than sliding
//! genomic content into column zero. Column arithmetic stays wide until saturation so large bp
//! spans cannot wrap when converted to ratatui coordinates.

use std::ops::Range;

use ratatui::layout::Rect;

use crate::basepair::BasePair;
use crate::viewer_app::viewport::ViewPosition;

/// A half-open genomic span fully contained by one contig.
///
/// Unlike [`ViewPosition`], this span slides at contig edges instead of
/// carrying out-of-bounds width. The local-area track derives one every draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AreaSpan {
    start: BasePair,
    end: BasePair,
}

impl AreaSpan {
    /// Derives a scaled viewport span and slides it inside the contig.
    pub(super) fn for_viewport(position: ViewPosition, scale: u64, contig_length: BasePair) -> Self {
        if contig_length == 0 {
            return Self {
                start: BasePair(0),
                end: BasePair(0),
            };
        }

        let requested_width = position.width.as_u64().saturating_mul(scale).max(1);
        let width = BasePair(requested_width.min(contig_length.as_u64()));
        let start = position
            .centre
            .checked_sub(width / 2)
            .unwrap_or(BasePair(0))
            .min(contig_length - width);

        Self {
            start,
            end: start + width,
        }
    }

    pub(super) fn start(self) -> BasePair {
        self.start
    }

    pub(super) fn end(self) -> BasePair {
        self.end
    }

    pub(super) fn width(self) -> BasePair {
        self.end - self.start
    }

    /// Maps a non-empty genomic projection to every intersecting terminal column.
    pub(super) fn projected_columns(self, start: BasePair, end: BasePair, columns: u16) -> Option<(u16, u16)> {
        let start = start.max(self.start);
        let end = end.min(self.end);
        if columns == 0 || self.width() == BasePair(0) || start >= end {
            return None;
        }

        let span_width = u128::from(self.width().as_u64());
        let columns = u128::from(columns);
        let start_offset = u128::from((start - self.start).as_u64());
        let end_offset = u128::from((end - self.start).as_u64());
        let first = start_offset * columns / span_width;
        let past_last = (end_offset * columns).div_ceil(span_width);
        let first = first.min(columns - 1) as u16;
        let past_last = past_last.max(u128::from(first) + 1).min(columns) as u16;
        Some((first, past_last))
    }
}

/// The absolute centre column, or `None` for an empty area.
pub(super) fn cent_column(area: Rect) -> Option<u16> {
    (area.width != 0).then(|| area.x + area.width / 2)
}

/// Clamps a content-relative column after adding leading screen columns.
///
/// The addition and clamp stay in `u64` so genomic spans wider than a terminal
/// column range cannot wrap before they are narrowed to the ratatui type.
pub(super) fn clamp_column(column_count: u64, leading: u16, width: u16) -> u16 {
    column_count.saturating_add(u64::from(leading)).min(u64::from(width)) as u16
}

/// Converts viewport underflow in bp to leading screen columns.
pub(super) fn leading_columns(underflow: BasePair, width: u16, bp_per_column: u64) -> u16 {
    clamp_column((underflow / bp_per_column).as_u64(), 0, width)
}

/// Returns the half-open screen-column range overlapping `[0, contig_length)`.
///
/// Columns before `start` are leading underflow before genomic position zero;
/// columns at or after `end` lie completely past `contig_length`.
pub(super) fn in_bounds_column_range(
    position: ViewPosition,
    contig_length: BasePair,
    width: u16,
    bp_per_column: u64,
) -> Range<u16> {
    let leading = leading_columns(position.underflow, width, bp_per_column);
    let first_past_end = match contig_length.checked_sub(position.left) {
        Some(remaining) => clamp_column(remaining.as_u64().div_ceil(bp_per_column), leading, width),
        None => leading,
    };
    leading..first_past_end
}

/// Bases actually drawn, after reserving whole columns for left underflow.
pub(super) fn in_bounds_base_range(
    position: ViewPosition,
    contig_length: BasePair,
    bp_per_column: u64,
) -> Range<BasePair> {
    let leading_span = (position.underflow / bp_per_column) * BasePair(bp_per_column);
    position.left..(position.left + (position.width - leading_span)).min(contig_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(centre: u64, viewport_width: u64, scale: u64, contig_length: u64) -> AreaSpan {
        AreaSpan::for_viewport(
            ViewPosition::new(BasePair(centre), BasePair(viewport_width)),
            scale,
            BasePair(contig_length),
        )
    }

    #[test]
    fn area_span_clamps_scaled_viewports_inside_each_contig() {
        for (centre, viewport_width, scale, contig_length) in [
            (500, 100, 2, 1_000),
            (0, 100, 2, 1_000),
            (999, 100, 2, 1_000),
            (5, 100, 10_000, 80),
            (0, 0, 100, 10),
            (u64::MAX, u64::MAX, 10_000, u64::MAX),
        ] {
            let span = area(centre, viewport_width, scale, contig_length);
            let expected_width = if contig_length == 0 {
                0
            } else {
                viewport_width.saturating_mul(scale).max(1).min(contig_length)
            };
            assert_eq!(span.width(), BasePair(expected_width));
            assert!(span.start() <= span.end());
            assert!(span.end() <= BasePair(contig_length));

            if expected_width < contig_length {
                let half_width = expected_width / 2;
                assert!(span.start() <= BasePair(centre.saturating_sub(half_width)));
                assert!(span.end() >= BasePair(centre.saturating_add(expected_width - half_width).min(contig_length)));
            }
        }

        let mut state = 0x8b8b_8b8b_3c6e_f372u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..1_024 {
            let contig_length = next() % 1_000_000;
            let viewport_width = next() % 10_000;
            let centre = next() % contig_length.saturating_add(1);
            let scale = 1 + next() % 9_999;
            let span = area(centre, viewport_width, scale, contig_length);
            let expected_width = if contig_length == 0 {
                0
            } else {
                viewport_width.saturating_mul(scale).max(1).min(contig_length)
            };
            let expected_start = centre
                .saturating_sub(expected_width / 2)
                .min(contig_length - expected_width);
            assert_eq!(
                span,
                AreaSpan {
                    start: BasePair(expected_start),
                    end: BasePair(expected_start + expected_width)
                }
            );
        }
    }

    #[test]
    fn area_span_projection_partitions_intersections_and_keeps_thin_features_visible() {
        let span = AreaSpan {
            start: BasePair(50),
            end: BasePair(150),
        };
        for (start, end, expected) in [
            (40, 60, Some((0, 1))),
            (60, 61, Some((1, 2))),
            (100, 101, Some((5, 6))),
            (149, 200, Some((9, 10))),
            (0, 50, None),
        ] {
            let projected = span.projected_columns(BasePair(start), BasePair(end), 10);
            assert_eq!(projected, expected);
            if let Some((first, past_last)) = projected {
                assert!(first < past_last);
                assert!(past_last <= 10);
            }
        }

        for (start, end, columns) in [(50u64, 150u64, 10u16), (101, 108, 13), (99, 103, 2)] {
            let span = AreaSpan {
                start: BasePair(start),
                end: BasePair(end),
            };
            for position in start.saturating_sub(1)..=end {
                let feature_end = position.saturating_add(1);
                let clipped_start = position.max(start);
                let clipped_end = feature_end.min(end);
                let expected = if clipped_start >= clipped_end {
                    None
                } else {
                    let width = u128::from(end - start);
                    let first = u128::from(clipped_start - start) * u128::from(columns) / width;
                    let past = (u128::from(clipped_end - start) * u128::from(columns)).div_ceil(width);
                    Some((first as u16, past.max(first + 1).min(u128::from(columns)) as u16))
                };
                assert_eq!(
                    span.projected_columns(BasePair(position), BasePair(feature_end), columns),
                    expected
                );
            }
        }
    }

    #[test]
    fn column_helpers_saturate_wide_values_and_preserve_absolute_columns() {
        for width in [0u16, 1, 2, 3, 41, 80, 81, 120] {
            for x in [0u16, 7] {
                let area = Rect::new(x, 0, width, 9);
                assert_eq!(cent_column(area), (width != 0).then(|| x + width / 2));
            }
        }
        assert_eq!(clamp_column(70_000, 0, u16::MAX), u16::MAX);
        assert_eq!(clamp_column(u64::MAX, u16::MAX, u16::MAX), u16::MAX);
        assert_eq!(clamp_column(20, 7, 30), 27);
        assert_eq!(clamp_column(20, 17, 30), 30);
        for (underflow, width, scale, expected) in [(0, 100, 1, 0), (10, 100, 1, 10), (10, 5, 1, 5), (10, 100, 2, 5)] {
            assert_eq!(leading_columns(BasePair(underflow), width, scale), expected);
        }
        assert_eq!(
            in_bounds_column_range(ViewPosition::new(BasePair(0), BasePair(80)), BasePair(25), 80, 1),
            40..65
        );
        assert_eq!(
            in_bounds_column_range(ViewPosition::new(BasePair(24), BasePair(160)), BasePair(25), 80, 2),
            28..41
        );
        assert_eq!(
            in_bounds_column_range(ViewPosition::new(BasePair(0), BasePair(80)), BasePair(0), 80, 1),
            40..40
        );
    }
}
