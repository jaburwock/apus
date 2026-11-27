//! What each base *is*, walked from a read's CIGAR against the reference.
//!
//! The middle stage of the pileup's three: [`super::camera`] decides which columns a read occupies,
//! this module decides what every base in those columns means, and [`super::paint`] decides how
//! that looks. Splitting classification from painting is what makes the reference-backed mismatch
//! rendering testable — a [`BaseKind`] is a claim about the data with a real oracle, where a colour
//! is not.
//!
//! One [`LaneCoverage`] is filled per lane, then read back per base by the painter. It is indexed
//! in **base pairs, not columns**: at `Mid` zoom one column spans many bases, and a mismatch that
//! only exists at bp resolution would be lost if the buffers were column-sized. `covered_bp_range`
//! converts back to the painter's bounds, deliberately including the column holding a partially
//! covered final base — [`super::camera::visible_columns`] floors the read end, so the last base
//! would otherwise be dropped.
//!
//! The buffers are allocated once per pane and reused across lanes via `reset` rather than
//! reallocated per read; a pileup reclassifies every visible read on every frame, so the
//! parallel `Vec`s are a per-frame hot path.
//!
//! A read with an empty CIGAR is drawn as flat `Match` coverage for its whole length rather than
//! skipped, so a record with no alignment detail still reads as coverage instead of a hole. That
//! path does not consult the reference, so it can never show a mismatch.

use std::ops::Range;

use crate::basepair::BasePair;
use crate::cigar::CigarKind;
use crate::viewer_app::genome::reference_data::ReferenceLookup;

use super::camera::visible_columns;

/// Per-base CIGAR classification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BaseKind {
    Uncovered,
    Match,
    Mismatch,
    Deletion,
    Skip,
    InsertionFlank,
}

/// A qualifying insertion projected into a synthetic span beneath its parent read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ExpandedInsertion {
    /// Preceding-reference anchor (0-based), falling back to `read.start` for a leading insertion.
    pub(super) start: BasePair,
    /// Full inserted length (`BasePair::length`), used for both bar width and `indellength`.
    pub(super) length: BasePair,
}

/// Appends every insertion in `read` of at least `min_len` bp whose synthetic span intersects
/// `[view_start, view_end)`.
///
/// Walks the CIGAR reference cursor directly without consulting `ReferenceData`, so expansion
/// layout stays cheap and never triggers reference fetches.
pub(super) fn visible_expanded_insertions(
    read: &super::super::read::Read,
    min_len: BasePair,
    view_start: BasePair,
    view_end: BasePair,
    out: &mut Vec<ExpandedInsertion>,
) {
    let mut ref_cursor = read.start;
    for op in read.cigar.ops() {
        let len = op.len() as u64;
        match op.kind() {
            CigarKind::Match
            | CigarKind::SequenceMatch
            | CigarKind::SequenceMismatch
            | CigarKind::Deletion
            | CigarKind::Skip => {
                ref_cursor = ref_cursor + BasePair(len);
            }
            CigarKind::Insertion => {
                let ins_len = BasePair::length(len);
                if ins_len >= min_len {
                    let start = ref_cursor
                        .checked_sub(BasePair(1))
                        .filter(|&bp| bp >= read.start)
                        .unwrap_or(read.start);
                    let end = start + ins_len;
                    if end > view_start && start < view_end {
                        out.push(ExpandedInsertion { start, length: ins_len });
                    }
                }
            }
            CigarKind::SoftClip | CigarKind::HardClip | CigarKind::Pad => {}
        }
    }
}

/// Visible lane coverage and per-base CIGAR classification.
pub(in crate::viewer_app::genome) struct LaneCoverage {
    pub(super) kinds: Vec<BaseKind>,
    pub(super) base_letters: Vec<Option<u8>>,
    pub(super) digits: Vec<Option<u8>>,
    /// Full indel-operation length at each indel base, since a `BaseKind` per base cannot say how
    /// long its op was. Widening `BaseKind` to carry the length was rejected: it is `Copy` and
    /// compared with `==` throughout painting, all to serve the one `indellength` label.
    pub(super) indel_sizes: Vec<Option<BasePair>>,
    mod_5mc_probs: Vec<Option<u8>>,
    pub(super) first_col: u16,
    pub(in crate::viewer_app::genome) past_last_col: u16,
    pub(super) left: BasePair,
    pub(super) bp_per_column: u64,
}

impl LaneCoverage {
    pub(in crate::viewer_app::genome) fn for_paint(
        content_width: u16,
        left: BasePair,
        bp_per_column: u64,
        collect_5mc: bool,
    ) -> Self {
        let bp_width = content_width as u64 * bp_per_column;
        Self {
            kinds: vec![BaseKind::Uncovered; bp_width as usize],
            base_letters: vec![None; bp_width as usize],
            digits: vec![None; bp_width as usize],
            indel_sizes: vec![None; bp_width as usize],
            mod_5mc_probs: if collect_5mc {
                vec![None; bp_width as usize]
            } else {
                Vec::new()
            },
            first_col: content_width,
            past_last_col: 0,
            left,
            bp_per_column,
        }
    }

    /// Reuses the per-lane buffers without retaining the previous lane's coverage.
    pub(in crate::viewer_app::genome) fn reset(&mut self, content_width: u16, left: BasePair, bp_per_column: u64) {
        let bp_width = usize::from(content_width) * bp_per_column as usize;
        debug_assert_eq!(self.kinds.len(), bp_width);
        debug_assert_eq!(self.base_letters.len(), bp_width);
        debug_assert_eq!(self.digits.len(), bp_width);
        debug_assert_eq!(self.indel_sizes.len(), bp_width);
        debug_assert!(self.mod_5mc_probs.is_empty() || self.mod_5mc_probs.len() == bp_width);
        self.kinds.fill(BaseKind::Uncovered);
        self.base_letters.fill(None);
        self.digits.fill(None);
        self.indel_sizes.fill(None);
        self.mod_5mc_probs.fill(None);
        self.first_col = content_width;
        self.past_last_col = 0;
        self.left = left;
        self.bp_per_column = bp_per_column;
    }

    /// Returns the covered portion in the base-pair coordinate space used by the painter.
    pub(super) fn covered_bp_range(&self, bounds_bp: usize) -> Range<usize> {
        let scale = self.bp_per_column as usize;
        let first = usize::from(self.first_col).saturating_mul(scale).min(bounds_bp);
        // `visible_columns` floors the read end, so include the column containing a
        // partially covered final base before converting back to bp space.
        let past_last = usize::from(self.past_last_col)
            .saturating_add(1)
            .saturating_mul(scale)
            .min(bounds_bp);
        if first < past_last {
            first..past_last.min(self.kinds.len())
        } else {
            0..0
        }
    }

    pub(in crate::viewer_app::genome) fn classify_read_against(
        &mut self,
        read: &super::super::read::Read,
        content_width: u16,
        mut reference: ReferenceLookup<'_>,
    ) {
        let col_range = visible_columns(read, self.left, content_width, self.bp_per_column);
        self.first_col = self.first_col.min(col_range.start);
        self.past_last_col = self.past_last_col.max(col_range.end);

        let bp_width = content_width as u64 * self.bp_per_column;
        let view_start = self.left;
        let view_end = view_start + BasePair(bp_width);
        let to_idx = |bp: BasePair| -> Option<usize> {
            bp.checked_sub(view_start)
                .map(|o| o.as_u64() as usize)
                .filter(|&i| i < bp_width as usize)
        };

        let ops = read.cigar.ops();
        if ops.is_empty() {
            let vis_start = read.start.max(view_start);
            let vis_end = (read.start + read.length).min(view_end);
            for bp_val in vis_start.as_u64()..vis_end.as_u64() {
                let idx = (bp_val - view_start.as_u64()) as usize;
                let read_offset = bp_val - read.start.as_u64();
                self.kinds[idx] = BaseKind::Match;
                self.digits[idx] = Some(b'0' + ((read_offset + 1) % 10) as u8);
            }
            return;
        }

        let mut ref_cursor = read.start;
        let mut read_query_cursor = 0u64;
        let mut insertion_flanks: Vec<(BasePair, u64)> = Vec::new();
        let seq = read.seq.as_ref().map(|seq| seq.as_view());
        let mut mods = read
            .mod_5mc
            .as_ref()
            .zip(seq)
            .filter(|_| !self.mod_5mc_probs.is_empty())
            .map(|(mods, seq)| mods.cpg_cursor(seq, read.strand.is_reverse()));
        let read_end = read.start + read.length;

        // Query offsets index SEQ; reference offsets index genomic positions, so
        // indels advance the two cursors differently.
        for op in ops {
            // An insertion at `ref_cursor == view_end` can still mark `view_end - 1`, so stop
            // strictly past `view_end`.
            if ref_cursor > view_end {
                break;
            }
            let len = op.len() as u64;
            let kind = op.kind();
            match kind {
                CigarKind::Match | CigarKind::SequenceMatch | CigarKind::SequenceMismatch => {
                    let op_end = ref_cursor + BasePair(len);
                    let vis_start = ref_cursor.max(view_start);
                    let vis_end = op_end.min(view_end);
                    if vis_start < vis_end {
                        let start_i = (vis_start - ref_cursor).as_u64();
                        let end_i = (vis_end - ref_cursor).as_u64();
                        let query_start = read_query_cursor;
                        if let Some(calls) = &mut mods {
                            let first_query = (query_start + start_i) as usize;
                            let end_query = (query_start + end_i) as usize;
                            let first_slot = (vis_start - view_start).as_u64() as usize;
                            for (query, prob) in calls.in_span(first_query..end_query) {
                                self.mod_5mc_probs[first_slot + (query - first_query)] = Some(prob);
                            }
                        }
                        let exact_match = kind == CigarKind::SequenceMatch;
                        for i in start_i..end_i {
                            let bp = ref_cursor + BasePair(i);
                            let idx = (bp - view_start).as_u64() as usize;
                            let q_idx = (query_start + i) as usize;
                            let seq_base = seq.and_then(|seq| seq.base_at(q_idx));
                            let is_mismatch = if exact_match {
                                false
                            } else if let Some(seq_base) = seq_base
                                && !seq_base.eq_ignore_ascii_case(&b'N')
                            {
                                reference.base_at(bp).is_some_and(|ref_base| {
                                    !ref_base.eq_ignore_ascii_case(&seq_base) && !ref_base.eq_ignore_ascii_case(&b'N')
                                })
                            } else {
                                false
                            };
                            self.kinds[idx] = if is_mismatch {
                                BaseKind::Mismatch
                            } else {
                                BaseKind::Match
                            };
                            if let Some(base) = seq_base {
                                self.base_letters[idx] = Some(base);
                            }
                            let read_offset = read_query_cursor + i;
                            self.digits[idx] = Some(b'0' + ((read_offset + 1) % 10) as u8);
                        }
                    }
                    ref_cursor = op_end;
                }
                CigarKind::Deletion | CigarKind::Skip => {
                    let op_end = ref_cursor + BasePair(len);
                    let vis_start = ref_cursor.max(view_start);
                    let vis_end = op_end.min(view_end);
                    if vis_start < vis_end {
                        let s = (vis_start - view_start).as_u64() as usize;
                        let e = (vis_end - view_start).as_u64() as usize;
                        let base_kind = if kind == CigarKind::Skip {
                            BaseKind::Skip
                        } else {
                            BaseKind::Deletion
                        };
                        self.kinds[s..e].fill(base_kind);
                        self.indel_sizes[s..e].fill(Some(BasePair::length(len)));
                    }
                    ref_cursor = op_end;
                }
                CigarKind::Insertion => {
                    // A flank outside the read's own reference span is dropped, so a leading or
                    // trailing `I` never marks a base belonging to a neighbouring read.
                    let before = ref_cursor.checked_sub(BasePair(1));
                    let after = ref_cursor;
                    if let Some(bp) = before
                        && bp >= read.start
                        && bp < read_end
                        && bp >= view_start
                        && bp < view_end
                    {
                        insertion_flanks.push((bp, len));
                    }
                    if after >= read.start && after < read_end && after >= view_start && after < view_end {
                        insertion_flanks.push((after, len));
                    }
                }
                CigarKind::SoftClip | CigarKind::HardClip | CigarKind::Pad => {}
            }
            if kind.consumes_read() {
                read_query_cursor += len;
            }
        }

        for (bp, len) in insertion_flanks {
            if let Some(idx) = to_idx(bp) {
                self.kinds[idx] = BaseKind::InsertionFlank;
                self.indel_sizes[idx] = Some(BasePair::length(len));
            }
        }
    }

    pub(super) fn kind_at_bp(&self, bp: BasePair) -> BaseKind {
        let index = bp.checked_sub(self.left).map(|o| o.as_u64() as usize);
        match index {
            Some(i) if i < self.kinds.len() => self.kinds[i],
            _ => BaseKind::Uncovered,
        }
    }

    pub(super) fn mod_5mc_prob_at_bp(&self, bp: BasePair) -> Option<u8> {
        let index = bp.checked_sub(self.left)?.as_u64() as usize;
        self.mod_5mc_probs.get(index).copied().flatten()
    }

    /// Returns the indel-operation length covering `bp`, if any.
    pub(super) fn indel_size_at_bp(&self, bp: BasePair) -> Option<BasePair> {
        let index = bp.checked_sub(self.left).map(|o| o.as_u64() as usize);
        match index {
            Some(i) if i < self.indel_sizes.len() => self.indel_sizes[i],
            _ => None,
        }
    }

    pub(super) fn base_letter_at_bp(&self, bp: BasePair) -> Option<u8> {
        let index = bp.checked_sub(self.left).map(|o| o.as_u64() as usize);
        match index {
            Some(i) if i < self.base_letters.len() => self.base_letters[i],
            _ => None,
        }
    }

    pub(super) fn digit_at_bp(&self, bp: BasePair) -> Option<char> {
        let index = bp.checked_sub(self.left).map(|o| o.as_u64() as usize);
        match index {
            Some(i) if i < self.digits.len() => self.digits[i].map(|b| b as char),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::read::{Read, ReadId, Strand};
    use super::*;
    use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
    use crate::viewer_app::genome::mod_5mc::BaseMod5mC;
    use crate::viewer_app::genome::packed_seq::PackedSeq;
    use crate::viewer_app::genome::reference_data::ReferenceData;
    use crate::viewer_app::genome::track::Contig;

    fn contig(length: BasePair) -> Contig {
        Contig {
            name: "chr1".to_string(),
            length,
        }
    }

    fn read_cigar(start: u64, cigar: &str, seq: Option<Vec<u8>>) -> Read {
        Read {
            start: BasePair(start),
            length: BasePair::length(crate::cigar::ref_len(&crate::cigar::parse_ops(cigar))),
            id: ReadId::from_name(format!("read_{start}").as_bytes()),
            cigar: crate::cigar::parse_ops(cigar),
            seq: seq.as_deref().map(PackedSeq::from_ascii),
            strand: Strand::Forward,
            mod_5mc: None,
        }
    }

    #[test]
    fn window_counts_match_pileup_alleles_at_every_covered_base() {
        use super::super::super::window_counts::{BaseCounts, counts_from_reads};
        use rand::{RngExt, SeedableRng, rngs::StdRng};

        let mut rng = StdRng::seed_from_u64(0xBA5E_C017);
        let contig = contig(BasePair(96));
        let reference_bases: Vec<_> = (0..96).map(|_| b"ACGT"[rng.random_range(0..4)]).collect();
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        reference.seed_chunk(&contig.name, BasePair(0), &reference_bases);

        let (reads, skips): (Vec<_>, Vec<_>) = (0..24)
            .map(|i| {
                let start = rng.random_range(0..64);
                let clip = rng.random_range(1..4);
                let head = rng.random_range(2..7);
                let middle = rng.random_range(2..7);
                let tail = rng.random_range(2..7);
                let insertion = rng.random_range(1..4);
                let deletion = rng.random_range(1..4);
                let skip = rng.random_range(1..8);
                let cigar = format!("2H{clip}S{head}M{insertion}I{middle}={deletion}D{skip}N{tail}X{clip}S1P");
                let seq = (i % 4 != 0).then(|| {
                    (0..2 * clip + head + insertion + middle + tail)
                        .map(|_| b"ACGTNR="[rng.random_range(0..7)])
                        .collect()
                });
                let skip_start = BasePair(start + head + middle + deletion);
                (read_cigar(start, &cigar, seq), skip_start..skip_start + BasePair(skip))
            })
            .unzip();

        let mut saw_mismatch = false;
        for (start, width) in [(0, 96), (17, 23), (60, 25), (39, 0)] {
            let left = BasePair(start);
            let right = left + BasePair(u64::from(width));
            let mut lane = LaneCoverage::for_paint(width, left, 1, false);
            let mut expected = vec![[0u32; 6]; usize::from(width)];
            let mut span_depth = 0;
            for (read, skip) in reads.iter().zip(&skips) {
                lane.reset(width, left, 1);
                lane.classify_read_against(read, width, ReferenceLookup::new(&mut reference, &contig));
                // Reference skips classify as `BaseKind::Skip` and do not contribute any depth.
                span_depth += (right.min(read.start + read.length) - left.max(read.start)).as_u64()
                    - (right.min(skip.end) - left.max(skip.start)).as_u64();
                for (i, bp) in left.step_to(right, BasePair(1)).enumerate() {
                    let kind = lane.kind_at_bp(bp);
                    assert_eq!(kind == BaseKind::Skip, skip.contains(&bp), "bp {bp:?}");
                    saw_mismatch |= kind == BaseKind::Mismatch;
                    let allele = match kind {
                        BaseKind::Uncovered | BaseKind::Skip => continue,
                        BaseKind::Deletion => 5,
                        BaseKind::Match | BaseKind::Mismatch | BaseKind::InsertionFlank => b"ACGT"
                            .iter()
                            .position(|&base| Some(base) == lane.base_letter_at_bp(bp))
                            .unwrap_or(4),
                    };
                    expected[i][allele] += 1;
                }
            }

            let counts = counts_from_reads(&reads, left, right - left);
            assert!(counts.calls.is_empty());
            let counts = counts.bases;
            assert_eq!(counts.len(), expected.len());
            for (i, (&counts, [a, c, g, t, n, del])) in counts.iter().zip(expected).enumerate() {
                assert_eq!(counts, BaseCounts { a, c, g, t, n, del }, "bp {}", start + i as u64);
            }
            assert_eq!(
                counts.iter().map(|counts| u64::from(counts.depth())).sum::<u64>(),
                span_depth
            );
        }
        assert!(saw_mismatch, "the fixture must exercise reference mismatches");
    }

    #[test]
    fn window_cpg_counts_match_pileup_calls() {
        use super::super::super::window_counts::{CallCounts, counts_from_reads};
        use crate::viewer_app::genome::mod_5mc::Call;

        let read = read_cigar(10, "2H2S3M2I2=2X1D3N5M", Some(b"CGCGACGTCGACGCGC".to_vec()));
        let mut forward = read.clone();
        forward.mod_5mc = Some(BaseMod5mC {
            implicit_unmodified: true,
            calls: vec![(0, 35), (2, 228), (2, 102), (5, 49), (11, 179), (13, 246)],
        });
        let mut reverse = read.clone();
        reverse.strand = Strand::Reverse;
        reverse.mod_5mc = Some(BaseMod5mC {
            implicit_unmodified: false,
            calls: vec![(1, 135), (3, 111), (6, 43), (12, 206), (14, 111)],
        });
        let mut clipped_only = read.clone();
        clipped_only.mod_5mc = Some(BaseMod5mC {
            implicit_unmodified: false,
            calls: vec![(0, 217), (5, 201)],
        });
        let reads = [read, forward, reverse, clipped_only];
        let contig = contig(BasePair(100));
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());

        for (start, width) in [(0, 32), (11, 9), (14, 10), (17, 4), (23, 2), (11, 0)] {
            let left = BasePair(start);
            let right = left + BasePair(u64::from(width));
            let mut lane = LaneCoverage::for_paint(width, left, 1, true);
            let mut expected = vec![CallCounts::default(); usize::from(width)];
            for read in &reads {
                lane.reset(width, left, 1);
                lane.classify_read_against(read, width, ReferenceLookup::new(&mut reference, &contig));
                for (i, bp) in left.step_to(right, BasePair(1)).enumerate() {
                    if let Some(prob) = lane.mod_5mc_prob_at_bp(bp) {
                        expected[i][Call::from_prob(prob) as usize] += 1;
                    }
                }
            }

            let counts = counts_from_reads(&reads, left, right - left);
            if expected.iter().all(|n| *n == CallCounts::default()) {
                assert!(counts.calls.is_empty(), "window {start}..{right:?} has no calls");
            } else {
                assert_eq!(counts.calls, expected, "window starting at {start}");
            }
        }
    }

    #[test]
    fn cpg_calls_follow_query_offsets_through_clips_and_indels() {
        let mut read = read_cigar(10, "2S3=2I4=1D5=", Some(b"CGCGACGTCGACGCGC".to_vec()));
        let contig = contig(BasePair(100));
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        let mut coverage = LaneCoverage::for_paint(14, read.start, 1, true);
        let cases = [
            (
                Strand::Forward,
                true,
                vec![(0, 31), (2, 224), (5, 45), (11, 172), (11, 11), (13, 250), (15, 244)],
                vec![(10, 224), (14, 0), (18, 172), (20, 250)],
            ),
            (
                Strand::Reverse,
                false,
                vec![(1, 130), (3, 110), (6, 41), (12, 202), (14, 93)],
                vec![(11, 110), (19, 202), (21, 93)],
            ),
        ];
        for (strand, implicit_unmodified, calls, expected) in cases {
            read.strand = strand;
            read.mod_5mc = Some(BaseMod5mC {
                implicit_unmodified,
                calls,
            });
            coverage.reset(14, read.start, 1);
            coverage.classify_read_against(&read, 14, ReferenceLookup::new(&mut reference, &contig));
            let actual: Vec<_> = (10..24)
                .filter_map(|bp| coverage.mod_5mc_prob_at_bp(BasePair(bp)).map(|prob| (bp, prob)))
                .collect();
            assert_eq!(actual, expected);
        }

        let mut lane_mode = LaneCoverage::for_paint(14, read.start, 1, false);
        lane_mode.classify_read_against(&read, 14, ReferenceLookup::new(&mut reference, &contig));
        assert_eq!(lane_mode.mod_5mc_prob_at_bp(BasePair(11)), None);
    }

    fn classify(coverage: &mut LaneCoverage, read: &Read, content_width: u16) {
        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        coverage.classify_read_against(
            read,
            content_width,
            ReferenceLookup::new(&mut reference, &contig(BasePair(1_000_000))),
        );
    }

    #[test]
    fn visible_classification_resolves_mismatches_from_the_shared_reference_cache() {
        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut seq = (0..5)
            .map(|offset| {
                reference
                    .base_at("chr1", BasePair(offset), BasePair(1_000_000))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        seq[3] = match seq[3] {
            b'A' => b'C',
            _ => b'A',
        };
        let read = read_cigar(0, "5M", Some(seq));
        let mut coverage = LaneCoverage::for_paint(5, BasePair(0), 1, false);

        coverage.classify_read_against(
            &read,
            5,
            ReferenceLookup::new(&mut reference, &contig(BasePair(1_000_000))),
        );

        assert_eq!(coverage.kind_at_bp(BasePair(3)), BaseKind::Mismatch);
        assert_eq!(coverage.kind_at_bp(BasePair(2)), BaseKind::Match);
    }

    #[test]
    fn reset_clears_coverage_and_column_bounds_for_reuse() {
        let mut coverage = LaneCoverage::for_paint(20, BasePair(0), 2, false);
        let read = read_cigar(4, "4M", None);
        classify(&mut coverage, &read, 20);
        assert_eq!(coverage.first_col, 2);
        assert_eq!(coverage.past_last_col, 4);

        coverage.reset(20, BasePair(100), 2);

        assert_eq!(coverage.first_col, 20);
        assert_eq!(coverage.past_last_col, 0);
        assert_eq!(coverage.covered_bp_range(40), 0..0);
        assert!(coverage.kinds.iter().all(|&kind| kind == BaseKind::Uncovered));
        assert!(coverage.base_letters.iter().all(Option::is_none));
        assert!(coverage.digits.iter().all(Option::is_none));
        assert!(coverage.indel_sizes.iter().all(Option::is_none));
    }

    #[test]
    fn annotated_alignment_columns_project_through_the_derived_cigar() {
        enum Event {
            Clip(Vec<u8>),
            Pair { reference: u8, query: u8, op: char },
            Insertion(Vec<u8>),
            Deletion(usize),
        }

        const BASES: [u8; 4] = *b"ACGT";
        let mut reference_cache = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        for seed in 0..12u64 {
            let start = 100 + seed * 20;
            let mut events = vec![Event::Clip(vec![b'T'; (seed % 3) as usize + 1])];
            let pair_count = (seed as usize % 5) + 5;
            let insertion_after = (seed as usize % pair_count) + 1;
            let deletion_after = (seed as usize % (pair_count - 1)) + 2;
            if seed.is_multiple_of(4) {
                events.push(Event::Insertion(vec![b'C'; (seed % 2) as usize + 1]));
            }
            for column in 0..pair_count {
                let ref_base = if column + 2 == pair_count && seed.is_multiple_of(3) {
                    b'N'
                } else {
                    BASES[(column + seed as usize) % BASES.len()]
                };
                let query = if column + 1 == pair_count && seed.is_multiple_of(5) {
                    b'N'
                } else if (column + seed as usize).is_multiple_of(3) && ref_base != b'N' {
                    BASES[(column + seed as usize + 1) % BASES.len()]
                } else {
                    ref_base
                };
                let op = if column + 2 == pair_count && seed.is_multiple_of(2) {
                    '='
                } else if query != ref_base {
                    'X'
                } else {
                    'M'
                };
                events.push(Event::Pair {
                    reference: ref_base,
                    query,
                    op,
                });
                if column + 1 == insertion_after {
                    events.push(Event::Insertion(vec![b'G'; (seed % 3) as usize + 1]));
                }
                if column + 1 == deletion_after {
                    events.push(Event::Deletion((seed % 3) as usize + 1));
                }
            }
            if seed.is_multiple_of(2) {
                events.push(Event::Insertion(vec![b'C'; (seed % 2) as usize + 1]));
            }
            events.push(Event::Clip(vec![b'A'; (seed % 2) as usize + 1]));

            let mut cigar = String::new();
            let mut sequence = Vec::new();
            let mut reference_bases = Vec::new();
            let mut expected: Vec<(BaseKind, Option<u8>)> = Vec::new();
            let mut ref_offset = 0usize;
            let mut insertion_after = false;
            for event in &events {
                match event {
                    Event::Clip(bases) => {
                        cigar.push_str(&format!("{}S", bases.len()));
                        sequence.extend_from_slice(bases);
                    }
                    Event::Pair { reference, query, op } => {
                        cigar.push_str(&format!("1{op}"));
                        sequence.push(*query);
                        reference_bases.push(*reference);
                        let mismatch = *op != '=' && *reference != b'N' && *query != b'N' && *reference != *query;
                        let kind = if insertion_after {
                            insertion_after = false;
                            BaseKind::InsertionFlank
                        } else if mismatch {
                            BaseKind::Mismatch
                        } else {
                            BaseKind::Match
                        };
                        expected.push((kind, Some(*query)));
                        ref_offset += 1;
                    }
                    Event::Insertion(bases) => {
                        cigar.push_str(&format!("{}I", bases.len()));
                        sequence.extend_from_slice(bases);
                        if ref_offset > 0 {
                            expected[ref_offset - 1].0 = BaseKind::InsertionFlank;
                        }
                        insertion_after = true;
                    }
                    Event::Deletion(length) => {
                        cigar.push_str(&format!("{length}D"));
                        for _ in 0..*length {
                            reference_bases.push(BASES[(ref_offset + seed as usize) % BASES.len()]);
                            let kind = if insertion_after {
                                insertion_after = false;
                                BaseKind::InsertionFlank
                            } else {
                                BaseKind::Deletion
                            };
                            expected.push((kind, None));
                            ref_offset += 1;
                        }
                    }
                }
            }

            reference_cache.seed_chunk("chr1", BasePair(start), &reference_bases);
            let mut coverage = LaneCoverage::for_paint(16, BasePair(start), 1, false);
            let read = read_cigar(start, &cigar, Some(sequence));
            coverage.classify_read_against(
                &read,
                16,
                ReferenceLookup::new(&mut reference_cache, &contig(BasePair(start + 100))),
            );

            for (offset, (kind, query_base)) in expected.into_iter().enumerate() {
                let bp = BasePair(start + offset as u64);
                assert_eq!(coverage.kind_at_bp(bp), kind, "seed {seed}, alignment column {bp:?}");
                assert_eq!(
                    coverage.base_letter_at_bp(bp),
                    query_base,
                    "seed {seed}, query identity at {bp:?}"
                );
            }
            assert_eq!(
                coverage.kind_at_bp(BasePair(start + ref_offset as u64)),
                BaseKind::Uncovered
            );
        }
    }

    #[test]
    fn indel_sizes_match_each_operation_and_its_affected_reference_positions() {
        let mut coverage = LaneCoverage::for_paint(210, BasePair(0), 1, false);
        let mut sequence = vec![b'A'; 17];
        sequence[0] = b'C';
        let read = read_cigar(0, "5M2I5M3D150D30N5M", Some(sequence));
        let mut reference = ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn());
        reference.seed_chunk("chr1", BasePair(0), &[b'A'; 210]);
        coverage.classify_read_against(&read, 210, ReferenceLookup::new(&mut reference, &contig(BasePair(210))));

        for (bp, size) in [
            (0, None),
            (4, Some(BasePair::length(2))),
            (5, Some(BasePair::length(2))),
            (9, None),
            (193, None),
            (200, None),
        ] {
            assert_eq!(coverage.indel_size_at_bp(BasePair(bp)), size, "bp {bp}");
        }
        for bp in 10..13 {
            assert_eq!(coverage.indel_size_at_bp(BasePair(bp)), Some(BasePair::length(3)));
        }
        for bp in 13..163 {
            assert_eq!(
                coverage.indel_size_at_bp(BasePair(bp)),
                Some(BasePair::length(150)),
                "bp {bp}"
            );
        }
        for bp in 163..193 {
            assert_eq!(coverage.kind_at_bp(BasePair(bp)), BaseKind::Skip, "bp {bp}");
            assert_eq!(
                coverage.indel_size_at_bp(BasePair(bp)),
                Some(BasePair::length(30)),
                "bp {bp}"
            );
        }
    }

    #[test]
    fn off_screen_bases_do_not_trigger_reference_fetches_and_keep_boundary_insertions() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        // Seed only chunk 1 ([32_768, 65_536)); chunks 0 and 2 remain absent.
        reference.seed_chunk("chr1", BasePair(32_768), &vec![b'A'; 32_768]);

        // Read spans chunks 0, 1, and 2: [10_000, 68_010), with an insertion right at view_end
        // (33_010) whose left flank lands on the last visible base (33_009).
        let mut seq = vec![b'A'; 23_010 + 2 + 35_000];
        seq[23_002] = b'C'; // genomic 33_002 -> mismatch against seeded 'A'
        let read = read_cigar(10_000, "23010M2I35000M", Some(seq));

        let mut coverage = LaneCoverage::for_paint(10, BasePair(33_000), 1, false);
        coverage.classify_read_against(
            &read,
            10,
            ReferenceLookup::new(&mut reference, &contig(BasePair(248_400))),
        );

        assert_eq!(
            reference.pending_chunk_count(),
            0,
            "off-screen read bases must not request reference chunks"
        );
        assert_eq!(coverage.kind_at_bp(BasePair(33_000)), BaseKind::Match);
        assert_eq!(coverage.kind_at_bp(BasePair(33_002)), BaseKind::Mismatch);
        assert_eq!(coverage.kind_at_bp(BasePair(33_009)), BaseKind::InsertionFlank);
        assert_eq!(coverage.indel_size_at_bp(BasePair(33_009)), Some(BasePair::length(2)));
    }
}
