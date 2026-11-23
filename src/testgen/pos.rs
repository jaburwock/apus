//! Uniform in-range SAM position generation.

use rand::{Rng, RngExt};

use crate::refgenome::RefIndex;

/// Samples a 1-based position for an in-range read.
pub fn gen_pos<R: Rng + ?Sized>(rng: &mut R, ref_index: &RefIndex, contig: &str, read_len: u64) -> u64 {
    let contig_len = ref_index
        .length(contig)
        .unwrap_or_else(|| panic!("unknown contig: {contig}"))
        .as_u64();
    assert!(
        read_len <= contig_len,
        "read_len {read_len} exceeds {contig} length {contig_len}"
    );
    let max_start = contig_len - read_len + 1;
    rng.random_range(1..=max_start)
}
