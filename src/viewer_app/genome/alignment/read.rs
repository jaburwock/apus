//! Rendering-domain representation of one aligned read.
//!
//! This module is the ingest boundary's small domain value: BAM/`noodles` types stop in
//! `read_source`, and rendering receives a 0-based genomic start, a tagged reference span, parsed
//! CIGAR operations, optional sequence, and a domain-level strand. The sequence is optional because
//! it is needed for close-zoom letters and mismatch comparison, not for every source or renderer.
//!
//! `ReadId` is an internal locator for deduplication and lane persistence, never a user-facing name;
//! `Strand` keeps file-format flags out of the painting layer. The CIGAR's reference and query
//! cursors must remain distinct downstream, since indels make a read offset diverge from its
//! genomic position.

use crate::basepair::BasePair;
use crate::cigar::Cigar;

use super::super::mod_5mc::BaseMod5mC;
use super::super::packed_seq::PackedSeq;

/// Compact identity for a BAM read name.
///
/// The identity is used only for lane assignment and duplicate suppression; read names are never
/// rendered or exposed to the user. A 64-bit hash makes collisions negligible for the resident
/// cache sizes, but not impossible: a collision can make two reads share an internal identity and
/// therefore perturb their lane assignment without causing memory unsafety or a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ReadId(u64);

impl ReadId {
    pub(crate) fn from_name(name: &[u8]) -> Self {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// Which strand a read aligned to.
///
/// Derived from the BAM reverse-complement flag at ingest, so the noodles flag type stops at the
/// file boundary and rendering sees a domain value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strand {
    Forward,
    Reverse,
}

impl Strand {
    pub(crate) fn is_reverse(self) -> bool {
        matches!(self, Self::Reverse)
    }
}

/// Pileup data needed to draw one read.
#[derive(Clone)]
pub(crate) struct Read {
    /// 0-based genomic start position.
    pub start: BasePair,
    /// Read span — `BasePair::length()`, never a position.
    pub length: BasePair,
    /// Compact identity derived from the source record name.
    pub id: ReadId,
    /// Parsed CIGAR.
    pub cigar: Cigar,
    /// Optional BAM sequence for close-zoom letters.
    pub(in crate::viewer_app::genome) seq: Option<PackedSeq>,
    /// Aligned strand, drawn by the `direction` read-colour mode.
    pub strand: Strand,
    pub mod_5mc: Option<BaseMod5mC>,
}
