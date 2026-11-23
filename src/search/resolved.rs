//! What a ranked search match resolves to, across every corpus.
//!
//! The fuzzy worker ranks bare keys and knows nothing about what a key means, so
//! the destination is attached here, on the way back out — by whichever corpus
//! owns the key.

use std::sync::Arc;

use crate::basepair::BasePair;

/// A search match resolved against the corpus that produced it.
#[derive(Debug, Clone)]
pub struct ResolvedMatch {
    pub key: Arc<str>,
    pub bold_char_indices: Vec<u32>,
    pub destination: MatchTarget,
}

/// Where selecting a match takes the viewer that owns it.
///
/// One enum rather than one type per corpus because the shell carries these
/// between `search_matches` and `search_enter` ([`crate::viewer::ViewerApp`])
/// without looking inside: a viewer reads the variant its own axis is
/// denominated in and never sees another's. The same reasoning as
/// [`crate::command::protocol::CommandOutcome::Goto`], which carries two readings of one
/// input for the same reason — resolution is app-global, so it cannot know which
/// viewer will receive the value.
#[derive(Debug, Clone)]
pub enum MatchTarget {
    /// A genomic interval, for a corpus of loci.
    Locus {
        contig: String,
        start: BasePair,
        end: BasePair,
        transcript_id: String,
    },
    /// A 0-based record index, for a corpus of one VCF column's values.
    Row(usize),
}
