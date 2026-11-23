//! Depth readouts over the pileup span: the mean the pileup divider names and the centre base's
//! depth the reference track labels.
//!
//! Both read the alignment track's resident coverage through `AlignmentTrack::coverage_at`, which
//! never fetches, so the alignment must draw first to request its windows. A base outside every
//! resident window is skipped, not counted as zero depth: a window still loading would otherwise
//! drag the mean down.

use crate::basepair::BasePair;
use crate::viewer_app::viewport::ViewPosition;

use super::alignment::{BaseCounts, WindowCoverage};
use super::geometry::in_bounds_base_range;

/// Depth statistics over the visible base span.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ViewportStats {
    /// Mean depth over visible resident bases.
    pub mean: f64,
    /// Greatest depth over visible resident bases.
    pub max: u32,
    /// Depth at the centre base, if visible and resident.
    pub point: Option<u32>,
}

impl ViewportStats {
    /// Scans the in-contig bases of `position`, rescanned every frame.
    ///
    /// Caching on movement was rejected: the centre moves through nudges, searches, feature jumps
    /// and contig clamps, and width changes with resize and zoom, so a correct key costs about what
    /// this scan does.
    pub(super) fn over(
        position: ViewPosition,
        contig_length: BasePair,
        bp_per_column: u64,
        coverage: WindowCoverage<'_>,
    ) -> Self {
        let visible = in_bounds_base_range(position, contig_length, bp_per_column);
        let (mut total, mut counted, mut max) = (0u64, 0u64, 0u32);
        for pos in visible.start.as_u64()..visible.end.as_u64() {
            if let Some(counts) = coverage.at(BasePair(pos)) {
                let depth = counts.depth();
                total += u64::from(depth);
                counted += 1;
                max = max.max(depth);
            }
        }
        let mean = if counted == 0 {
            0.0
        } else {
            total as f64 / counted as f64
        };
        let point = visible
            .contains(&position.centre)
            .then(|| coverage.at(position.centre))
            .flatten()
            .map(BaseCounts::depth);
        Self { mean, max, point }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer_app::genome::alignment::CoverageWindow;

    fn window(counts: &[BaseCounts], start: u64) -> CoverageWindow<'_> {
        CoverageWindow {
            counts,
            insertions: &[],
            calls: &[],
            window_start: BasePair(start),
        }
    }

    fn only(counts: &[BaseCounts], start: u64) -> WindowCoverage<'_> {
        WindowCoverage {
            centre: window(counts, start),
            left: None,
            right: None,
        }
    }

    fn adenine_counts(a: u32) -> BaseCounts {
        BaseCounts {
            a,
            ..BaseCounts::default()
        }
    }

    #[test]
    fn mean_and_max_count_resident_in_contig_bases_only() {
        let left = [1_000, 0, 23].map(adenine_counts);
        let centre = [31, 7].map(adenine_counts);
        let right = [
            BaseCounts {
                c: 30,
                del: 11,
                ..BaseCounts::default()
            },
            adenine_counts(43),
            adenine_counts(47),
            adenine_counts(999),
        ];
        let coverage = WindowCoverage {
            centre: window(&centre, 4),
            left: Some(window(&left, 0)),
            right: Some(window(&right, 6)),
        };
        // Bases 1..9 are visible: base 0 is resident but left of the view, 3 is in no window, and
        // 9 is past the contig end.
        let position = ViewPosition::new(BasePair(6), BasePair(10));
        let stats = ViewportStats::over(position, BasePair(9), 1, coverage);
        assert_eq!(
            coverage.at(BasePair(3)),
            None,
            "the visible genomic gap is not resident"
        );
        let expected = [0u32, 23, 31, 7, 41, 43, 47];
        assert_eq!(stats.mean, expected.iter().sum::<u32>() as f64 / expected.len() as f64);
        assert_eq!(stats.max, 47);
        assert_eq!(stats.point, Some(41));

        // At Mid, three bp of left underflow occupy one column, leaving bases 0..8 visible.
        let underflow = ViewPosition::new(BasePair(2), BasePair(10));
        let counts = [0, 0, 0, 0, 0, 0, 0, 29, 999].map(adenine_counts);
        let stats = ViewportStats::over(underflow, BasePair(9), 2, only(&counts, 0));
        assert_eq!(stats.max, 29);
        assert_eq!(stats.mean, 29.0 / 8.0);

        let past_end = ViewPosition::new(BasePair(20), BasePair(4));
        let empty = ViewportStats::over(past_end, BasePair(9), 1, coverage);
        assert_eq!(
            empty,
            ViewportStats {
                mean: 0.0,
                max: 0,
                point: None
            }
        );
    }

    #[test]
    fn point_depth_tracks_the_exact_centre_and_requires_visible_resident_bases() {
        let counts: Vec<_> = (0..20).map(|index| adenine_counts(10 + index * 3)).collect();
        let at = |centre: u64, width: u64, counts: &[BaseCounts]| {
            ViewportStats::over(
                ViewPosition::new(BasePair(centre), BasePair(width)),
                BasePair(20),
                1,
                only(counts, 0),
            )
            .point
        };
        assert_eq!(at(8, 10, &counts), Some(34));
        assert_eq!(at(9, 10, &counts), Some(37));
        assert_eq!(at(8, 10, &counts[..5]), None, "the centre is not resident");
        assert_eq!(at(20, 10, &counts), None, "the centre is past the contig end");
        assert_eq!(at(8, 0, &counts), None, "a zero-width view shows no base");
    }
}
