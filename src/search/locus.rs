//! Genome-viewer ownership and search resolution for the loaded feature index.
//!
//! `LocusIndex` keeps the optional [`FeatureIndex`] beside the viewer that renders
//! and navigates it. It implements [`SearchCorpus`] by exposing feature keys to the
//! corpus-blind fuzzy worker, then joining returned keys back to the current index.
//!
//! Resolution is deliberately separate from fuzzy ranking: this module supplies
//! contig, interval, and transcript destination data, while [`crate::locus`] owns
//! feature parsing and indexing. Unknown or stale keys are dropped rather than
//! converted into destinations from another index.
//!
//! Coordinates come from the existing feature index and remain
//! [`crate::basepair::BasePair`] values;
//! this module does not parse display coordinates or perform a second conversion.

use crate::locus::FeatureIndex;
use crate::search::{MatchTarget, ResolvedMatch, SearchCorpus};

/// Viewer-owned loaded feature index.
#[derive(Default)]
pub struct LocusIndex {
    inner: Option<FeatureIndex>,
}

impl LocusIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, index: FeatureIndex) {
        self.inner = Some(index);
    }

    pub fn active(&self) -> Option<&FeatureIndex> {
        self.inner.as_ref()
    }
}

impl SearchCorpus for LocusIndex {
    fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    fn keys(&self) -> Vec<std::sync::Arc<str>> {
        self.inner.as_ref().map(|fi| fi.keys().collect()).unwrap_or_default()
    }

    /// Resolves search matches against the current index.
    fn resolve_matches(&self, matches: &[crate::search::SearchMatch]) -> Vec<ResolvedMatch> {
        let Some(fi) = self.inner.as_ref() else {
            return Vec::new();
        };
        matches
            .iter()
            .filter_map(|m| {
                let (index, locus) = fi.locus_by_key(m.key.as_ref())?;
                let contig = fi.contig_for_index(index)?.to_string();
                Some(ResolvedMatch {
                    key: m.key.clone(),
                    bold_char_indices: m.indices.clone(),
                    destination: MatchTarget::Locus {
                        contig,
                        start: locus.start,
                        end: locus.end,
                        transcript_id: locus.transcript_id().to_string(),
                    },
                })
            })
            .collect()
    }
}
