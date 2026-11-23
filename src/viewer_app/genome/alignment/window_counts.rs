//! Counts aligned alleles, insertion support and CpG calls before the reference is available.
//! The pileup classifier test `window_counts_match_pileup_alleles_at_every_covered_base` checks the walk.
//! `window_cpg_counts_match_pileup_calls` checks the same walk for 5mC.

use std::ops::Range;

use crate::basepair::BasePair;
use crate::cigar::CigarKind;
use crate::viewer_app::genome::mod_5mc::{Call, CpGCallCursor};

use super::insertions::{Insertion, cluster_support, with_insertion_context};
use super::read::Read;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in crate::viewer_app::genome) struct BaseCounts {
    pub a: u32,
    pub c: u32,
    pub g: u32,
    pub t: u32,
    /// Missing or ambiguous sequence, including BAM's reference-relative `=` symbol.
    pub n: u32,
    pub del: u32,
}

impl BaseCounts {
    pub(in crate::viewer_app::genome) fn depth(self) -> u32 {
        [self.a, self.c, self.g, self.t, self.n, self.del]
            .into_iter()
            .fold(0, u32::saturating_add)
    }

    fn add_base(&mut self, base: Option<u8>) {
        let count = match base {
            Some(b'A') => &mut self.a,
            Some(b'C') => &mut self.c,
            Some(b'G') => &mut self.g,
            Some(b'T') => &mut self.t,
            _ => &mut self.n,
        };
        *count = count.saturating_add(1);
    }
}

pub(in crate::viewer_app::genome) type CallCounts = [u32; 3];

pub(in crate::viewer_app::genome) struct WindowCounts {
    pub bases: Vec<BaseCounts>,
    /// Indexed like `bases`, allocated on the first aligned CpG call.
    pub calls: Vec<CallCounts>,
    /// Sparse, sorted cluster anchors and their supporting read counts.
    pub insertions: Vec<(BasePair, u32)>,
}

impl WindowCounts {
    fn add_calls(&mut self, slots: Range<usize>, first_query: usize, cursor: &mut CpGCallCursor<'_>) {
        let end_query = first_query + slots.len();
        for (query, prob) in cursor.in_span(first_query..end_query) {
            if self.calls.is_empty() {
                self.calls.resize(self.bases.len(), CallCounts::default());
            }
            let slot = slots.start + (query - first_query);
            let count = &mut self.calls[slot][Call::from_prob(prob) as usize];
            *count = count.saturating_add(1);
        }
    }
}

/// Borrowed per-base counts from one resident window.
#[derive(Clone, Copy)]
pub(in crate::viewer_app::genome) struct CoverageWindow<'a> {
    pub counts: &'a [BaseCounts],
    pub calls: &'a [CallCounts],
    pub insertions: &'a [(BasePair, u32)],
    pub window_start: BasePair,
}

impl CoverageWindow<'_> {
    fn index_of(&self, pos: BasePair) -> Option<usize> {
        let index = pos.checked_sub(self.window_start)?.as_u64() as usize;
        (index < self.counts.len()).then_some(index)
    }
}

/// The viewport's centre window and its resident neighbours, with centre precedence.
#[derive(Clone, Copy)]
pub(in crate::viewer_app::genome) struct WindowCoverage<'a> {
    pub centre: CoverageWindow<'a>,
    pub left: Option<CoverageWindow<'a>>,
    pub right: Option<CoverageWindow<'a>>,
}

impl WindowCoverage<'_> {
    pub(in crate::viewer_app::genome) fn at(&self, pos: BasePair) -> Option<BaseCounts> {
        let (window, index) = self.resolve(pos)?;
        Some(window.counts[index])
    }

    pub(in crate::viewer_app::genome) fn at_with_calls(&self, pos: BasePair) -> Option<(BaseCounts, CallCounts)> {
        let (window, index) = self.resolve(pos)?;
        Some((
            window.counts[index],
            window.calls.get(index).copied().unwrap_or_default(),
        ))
    }

    pub(in crate::viewer_app::genome) fn at_with_insertions(&self, pos: BasePair) -> Option<(BaseCounts, u32)> {
        let (window, index) = self.resolve(pos)?;
        let support = window
            .insertions
            .binary_search_by_key(&pos, |&(anchor, _)| anchor)
            .map_or(0, |hit| window.insertions[hit].1);
        Some((window.counts[index], support))
    }

    fn resolve(&self, pos: BasePair) -> Option<(&CoverageWindow<'_>, usize)> {
        [Some(&self.centre), self.left.as_ref(), self.right.as_ref()]
            .into_iter()
            .flatten()
            .find_map(|window| Some((window, window.index_of(pos)?)))
    }
}

pub(super) fn counts_from_reads(reads: &[Read], window_start: BasePair, window_len: BasePair) -> WindowCounts {
    let mut counts = WindowCounts {
        bases: vec![BaseCounts::default(); window_len.as_u64() as usize],
        calls: Vec::new(),
        insertions: Vec::new(),
    };
    let window_end = window_start + window_len;
    let insertion_range = with_insertion_context(window_start..window_end);
    let mut insertions = Vec::new();
    let offset = |pos: BasePair| (pos.min(window_end) - window_start).as_u64() as usize;

    for read in reads {
        if read.cigar.ops().is_empty() {
            // The pileup shows a span without CIGAR detail but has no mapped base letters for it.
            for slot in &mut counts.bases[offset(read.start)..offset(read.start + read.length)] {
                slot.add_base(None);
            }
            continue;
        }

        let seq = read.seq.as_ref().map(|seq| seq.as_view());
        let mut mods = read
            .mod_5mc
            .as_ref()
            .zip(seq)
            .map(|(mods, seq)| mods.cpg_cursor(seq, read.strand.is_reverse()));
        let mut reference = read.start;
        let mut query = 0;
        for op in &read.cigar {
            if reference > insertion_range.end {
                break;
            }
            let kind = op.kind();
            let query_start = query;
            if kind.consumes_read() {
                query += op.len();
            }
            if kind == CigarKind::Insertion && op.len() != 0 {
                let anchor = (reference - BasePair(1)).max(read.start);
                if insertion_range.contains(&anchor) {
                    insertions.push(Insertion {
                        anchor,
                        length: BasePair(op.len() as u64),
                        read: (read.id, read.start),
                    });
                }
            }
            if !kind.consumes_reference() {
                continue;
            }
            let start = reference;
            reference = reference + BasePair(op.len() as u64);
            if kind == CigarKind::Skip {
                continue;
            }

            let slots = offset(start)..offset(reference);
            if kind == CigarKind::Deletion {
                for slot in &mut counts.bases[slots] {
                    slot.del = slot.del.saturating_add(1);
                }
            } else if !slots.is_empty() {
                let first_query = query_start + (window_start - start).as_u64() as usize;
                for (i, slot) in counts.bases[slots.clone()].iter_mut().enumerate() {
                    slot.add_base(seq.and_then(|seq| seq.base_at(first_query + i)));
                }
                if let Some(cursor) = &mut mods {
                    counts.add_calls(slots, first_query, cursor);
                }
            }
        }
    }

    counts.insertions = cluster_support(insertions, window_start..window_end);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window<'a>(counts: &'a [BaseCounts], calls: &'a [CallCounts], start: u64) -> CoverageWindow<'a> {
        CoverageWindow {
            counts,
            calls,
            insertions: &[],
            window_start: BasePair(start),
        }
    }

    #[test]
    fn resident_windows_resolve_distinct_counts_with_centre_precedence() {
        let counts = |a| BaseCounts {
            a,
            ..BaseCounts::default()
        };
        let left = [13, 17, 23].map(counts);
        let centre = [31, 7, 19].map(counts);
        let right = [41, 43, 47].map(counts);
        let overlap = [101, 103, 107].map(counts);
        let calls = [[2, 3, 5], [7, 11, 13], [17, 19, 23]];
        let coverage = WindowCoverage {
            centre: window(&centre, &[], 88),
            left: Some(window(&left, &[], 85)),
            right: Some(window(&right, &calls, 91)),
        };
        for (position, expected) in [
            (86, Some(17)),
            (87, Some(23)),
            (88, Some(31)),
            (90, Some(19)),
            (91, Some(41)),
            (93, Some(47)),
            (94, None),
        ] {
            assert_eq!(coverage.at(BasePair(position)), expected.map(counts), "bp {position}");
        }
        assert_eq!(coverage.at_with_calls(BasePair(92)), Some((right[1], calls[1])));
        let overlapping = WindowCoverage {
            right: Some(window(&overlap, &calls, 88)),
            ..coverage
        };
        assert_eq!(overlapping.at(BasePair(88)), Some(centre[0]));
        assert_eq!(overlapping.at(BasePair(90)), Some(centre[2]));
        assert_eq!(overlapping.at_with_calls(BasePair(90)), Some((centre[2], [0; 3])));
    }
}
