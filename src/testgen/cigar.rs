//! Generates fixture CIGAR strings using aggregate HG002 alignment statistics.
//!
//! The walk consumes exactly the requested query length while sampling indel starts, indel
//! lengths, and single-base errors. Adjacent operations are coalesced, and the generator emits
//! `M`/`I`/`D` plus an optional `N` (`Skip`) at an exact relative reference offset.
//!
//! Mismatches are represented as `M` operations plus 0-based query offsets; the assembly layer
//! carries those offsets in the generated `XN` tag so sequence synthesis can substitute the
//! corresponding reference bases. Query consumption and offset placement are coupled invariants.

use rand::{Rng, RngExt};

use crate::cigar::{Cigar, CigarKind, CigarOp, format_ops};

/// HG002 indel-length buckets: `(lo, hi, insertion_count, deletion_count)`.
const INDEL_LENGTH_BUCKETS: [(u32, u32, u64, u64); 9] = [
    (1, 1, 934_211, 1_246_687),
    (2, 2, 236_200, 362_879),
    (3, 3, 80_936, 142_696),
    (4, 5, 55_339, 108_665),
    (6, 10, 19_270, 43_611),
    (11, 20, 6_135, 9_647),
    (21, 50, 3_370, 4_314),
    (51, 100, 1_063, 847),
    (101, 300, 1_622, 424),
];

const ALIGNED_BASES: f64 = 913_635_440.0;
const INDEL_EVENTS: f64 = 3_257_916.0;
// Each aligned base or in-range indel run accounts for one draw in the walk.
const INDEL_START_RATE: f64 = INDEL_EVENTS / (ALIGNED_BASES + INDEL_EVENTS);

const MISMATCH_RATE: f64 = 3_305_277.0 / ALIGNED_BASES;

enum Step {
    Match,
    Mismatch,
    Insertion(u32),
    Deletion(u32),
}

/// Samples an indel kind and length from the bucket table.
fn gen_indel<R: Rng + ?Sized>(rng: &mut R) -> Step {
    let total: u64 = INDEL_LENGTH_BUCKETS.iter().map(|(_, _, ins, del)| ins + del).sum();
    let mut x = rng.random_range(0..total);
    for (lo, hi, ins, del) in INDEL_LENGTH_BUCKETS {
        let bucket_total = ins + del;
        if x < bucket_total {
            let len = rng.random_range(lo..=hi);
            return if x < ins {
                Step::Insertion(len)
            } else {
                Step::Deletion(len)
            };
        }
        x -= bucket_total;
    }
    unreachable!()
}

/// Samples the next CIGAR step without exceeding the query length or crossing a pending split.
fn gen_step<R: Rng + ?Sized>(
    rng: &mut R,
    query_remaining: u64,
    query_used: u64,
    max_del_before_split: Option<u64>,
    require_aligned_base: bool,
) -> Step {
    if require_aligned_base {
        return if rng.random_bool(MISMATCH_RATE) {
            Step::Mismatch
        } else {
            Step::Match
        };
    }
    loop {
        let step = if rng.random_bool(INDEL_START_RATE) {
            gen_indel(rng)
        } else if rng.random_bool(MISMATCH_RATE) {
            Step::Mismatch
        } else {
            Step::Match
        };
        match step {
            Step::Match | Step::Mismatch => return step,
            Step::Insertion(len) if u64::from(len) <= query_remaining => return step,
            Step::Deletion(len)
                if max_del_before_split.is_none_or(|limit| query_used > 0 && u64::from(len) < limit) =>
            {
                return step;
            }
            _ => {}
        }
    }
}

/// Appends an operation, coalescing adjacent operations of the same kind.
fn push_op(ops: &mut Vec<CigarOp>, kind: CigarKind, len: u64) {
    if let Some(last) = ops.last_mut()
        && last.kind() == kind
    {
        *last = CigarOp::new(kind, last.len() + len as usize);
        return;
    }
    ops.push(CigarOp::new(kind, len as usize));
}

/// Generates a CIGAR and 0-based query offsets for seeded mismatches;
/// the offsets are emitted through the generated `XN` tag.
///
/// When `split` is `Some((ref_offset, skip_len))`, a `{skip_len}N` operation is inserted once the
/// reference cursor reaches `ref_offset` with query bases still remaining for the downstream exon.
pub fn gen_cigar<R: Rng + ?Sized>(rng: &mut R, read_len: u64, split: Option<(u64, u64)>) -> (String, Vec<u64>) {
    let mut ops: Vec<CigarOp> = Vec::new();
    let mut mismatch_offsets: Vec<u64> = Vec::new();
    let mut query_used: u64 = 0;
    let mut ref_used: u64 = 0;
    let mut pending_split = split.map(|(ref_offset, skip_len)| {
        assert!(ref_offset > 0, "split offset must be positive");
        assert!(skip_len > 0, "split length must be positive");
        (ref_offset, skip_len)
    });
    let mut require_aligned_base = false;

    while query_used < read_len {
        if let Some((ref_offset, skip_len)) = pending_split
            && ref_used == ref_offset
        {
            debug_assert!(query_used > 0);
            push_op(&mut ops, CigarKind::Skip, skip_len);
            ref_used += skip_len;
            pending_split = None;
            require_aligned_base = true;
            continue;
        }

        let max_del_before_split = pending_split.map(|(ref_offset, _)| ref_offset - ref_used);
        let step = gen_step(
            rng,
            read_len - query_used,
            query_used,
            max_del_before_split,
            require_aligned_base,
        );
        require_aligned_base = false;
        match step {
            Step::Match => {
                push_op(&mut ops, CigarKind::Match, 1);
                query_used += 1;
                ref_used += 1;
            }
            Step::Mismatch => {
                mismatch_offsets.push(query_used);
                push_op(&mut ops, CigarKind::Match, 1);
                query_used += 1;
                ref_used += 1;
            }
            Step::Insertion(len) => {
                push_op(&mut ops, CigarKind::Insertion, len as u64);
                query_used += len as u64;
            }
            Step::Deletion(len) => {
                push_op(&mut ops, CigarKind::Deletion, len as u64);
                ref_used += len as u64;
            }
        }
    }

    (format_ops(&Cigar::new(ops)), mismatch_offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cigar::parse_ops;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn query_len(cigar: &str) -> u64 {
        parse_ops(cigar)
            .ops()
            .iter()
            .filter(|op| op.kind().consumes_read())
            .map(|op| op.len() as u64)
            .sum()
    }

    #[test]
    fn generated_cigars_consume_bounded_reads_with_basic_operations() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut read_lengths: Vec<u64> = (1..=8).collect();
        read_lengths.extend((0..6).map(|_| rng.random_range(128..=512)));
        for read_len in read_lengths {
            let (cigar_str, _) = gen_cigar(&mut rng, read_len, None);
            let parsed = parse_ops(&cigar_str);
            assert_eq!(query_len(&cigar_str), read_len, "cigar {cigar_str} for {read_len}");
            assert!(
                parsed
                    .ops()
                    .iter()
                    .all(|op| matches!(op.kind(), CigarKind::Match | CigarKind::Insertion | CigarKind::Deletion))
            );
        }
    }

    #[test]
    fn mismatch_offsets_fall_within_m_ops() {
        let mut rng = StdRng::seed_from_u64(11);
        for _ in 0..6 {
            let read_len = 512;
            let (cigar_str, offsets) = gen_cigar(&mut rng, read_len, None);
            let cigar = parse_ops(&cigar_str);
            let mut query_pos = 0u64;
            let mut match_ranges = Vec::new();
            for op in &cigar {
                let len = op.len() as u64;
                if op.kind() == CigarKind::Match {
                    match_ranges.push(query_pos..query_pos + len);
                }
                if op.kind().consumes_read() {
                    query_pos += len;
                }
            }
            for &offset in &offsets {
                assert!(
                    offset < read_len && match_ranges.iter().any(|range| range.contains(&offset)),
                    "offset {offset} not covered by any M op in cigar {cigar_str}"
                );
            }
        }
    }

    #[test]
    fn split_cigar_inserts_exact_skip_and_preserves_query_and_mismatch_invariants() {
        let mut rng = StdRng::seed_from_u64(29);
        for (read_len, ref_offset, skip_len) in [
            (2, 1, 30),
            (10, 5, 30),
            (80, 1, 50),
            (100, 31, 30),
            (150, 75, 1_000),
            (20, 20, 30),
            (20, 40, 30),
        ] {
            for _ in 0..8 {
                let (cigar_str, offsets) = gen_cigar(&mut rng, read_len, Some((ref_offset, skip_len)));
                let parsed = parse_ops(&cigar_str);
                let ops = parsed.ops();
                assert_eq!(query_len(&cigar_str), read_len, "cigar {cigar_str}");
                assert_ne!(ops.first().map(|op| op.kind()), Some(CigarKind::Skip));
                assert_ne!(ops.last().map(|op| op.kind()), Some(CigarKind::Skip));

                let mut ref_pos = 0u64;
                let mut query_pos = 0u64;
                let mut match_ranges = Vec::new();
                let mut skips = Vec::new();
                for (idx, op) in ops.iter().enumerate() {
                    let len = op.len() as u64;
                    if op.kind() == CigarKind::Skip {
                        skips.push((ref_pos, len));
                        assert_ne!(
                            ops[idx - 1].kind(),
                            CigarKind::Deletion,
                            "deletion before N: {cigar_str}"
                        );
                        assert_eq!(ops[idx + 1].kind(), CigarKind::Match, "expected M after N: {cigar_str}");
                    }
                    if op.kind() == CigarKind::Match {
                        match_ranges.push(query_pos..query_pos + len);
                    }
                    if op.kind().consumes_reference() {
                        ref_pos += len;
                    }
                    if op.kind().consumes_read() {
                        query_pos += len;
                    }
                }

                if ref_pos > ref_offset {
                    assert_eq!(skips, vec![(ref_offset, skip_len)], "cigar {cigar_str}");
                } else {
                    assert!(skips.is_empty(), "short read should not emit N: {cigar_str}");
                }

                for &offset in &offsets {
                    assert!(
                        offset < read_len && match_ranges.iter().any(|range| range.contains(&offset)),
                        "offset {offset} not covered by any M op in cigar {cigar_str}"
                    );
                }
            }
        }
    }
}
