//! Samples mapped reference names from a reference index.

use rand::{Rng, RngExt};

use crate::refgenome::RefIndex;

/// Samples a uniformly random contig name from the reference.
pub fn gen_rname<R: Rng + ?Sized>(rng: &mut R, ref_index: &RefIndex) -> String {
    ref_index.contig_name(rng.random_range(0..ref_index.contig_count()))
}
