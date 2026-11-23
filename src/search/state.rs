//! Viewer-owned orchestration for one searchable corpus.
//!
//! `SearchState` sits between a viewer's [`SearchCorpus`] and the shared
//! search-session mechanics. It creates or rebuilds the session, submits changed
//! queries, polls without blocking, and resolves completed keys against the corpus
//! that still owns them.
//!
//! The viewer owns this state rather than `App`; each viewer therefore keeps its
//! search lifecycle and destination cache isolated. An inactive corpus produces no
//! visible matches, while a worker failure stops convergence and is handed upward as
//! an error without discarding the last matches.
//!
//! Corpus replacement must go through this boundary so pending work, resolved results,
//! and rebuild state cannot be mixed across indexes.

use super::SearchCorpus;
use super::SearchSession;
use super::resolved::ResolvedMatch;

#[derive(Default)]
pub struct SearchState {
    session: Option<SearchSession>,
    resolved_matches: Vec<ResolvedMatch>,
    converging: bool,
    needs_rebuild: bool,
    error: Option<String>,
}

impl SearchState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances the query and returns whether it is converging.
    ///
    /// Generic over the corpus rather than knowing any one of them: the lifecycle
    /// here — spawn lazily, rebuild in place, poll and resolve — is the same
    /// whether the keys are loci or one VCF column's values.
    pub fn tick(&mut self, query: &str, corpus: &impl SearchCorpus) -> bool {
        if !corpus.is_active() {
            self.resolved_matches.clear();
            self.converging = false;
            return false;
        }

        if self.session.is_none() {
            self.session = Some(SearchSession::spawn(corpus.keys()));
            self.needs_rebuild = false;
        } else if self.needs_rebuild {
            if let Some(session) = self.session.as_mut() {
                session.rebuild(corpus.keys());
            }
            self.needs_rebuild = false;
        }

        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if session.query(query) {
            self.converging = true;
        }
        if let Some(result) = session.try_recv() {
            self.converging = false;
            match result {
                Ok(matches) => {
                    session.latest_matches = matches;
                    self.resolved_matches = corpus.resolve_matches(&session.latest_matches);
                }
                Err(error) => self.error = Some(error),
            }
        }
        self.converging
    }

    /// Installs or replaces the corpus.
    pub fn install_corpus(&mut self, corpus: &impl SearchCorpus) {
        if let Some(session) = self.session.as_mut() {
            session.rebuild(corpus.keys());
        } else {
            self.session = Some(SearchSession::spawn(corpus.keys()));
        }
        self.needs_rebuild = false;
        self.resolved_matches.clear();
        self.converging = false;
        self.error = None;
    }

    /// Returns resolved matches.
    pub fn resolved(&self) -> &[ResolvedMatch] {
        &self.resolved_matches
    }

    /// Whether a query is still in flight.
    pub fn is_converging(&self) -> bool {
        self.converging
    }

    /// Takes a terminal search error for the owning viewer to notify.
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    #[cfg(test)]
    pub fn has_matcher(&self) -> bool {
        self.session.is_some()
    }

    #[cfg(test)]
    pub fn latest_matches(&self) -> &[super::SearchMatch] {
        self.session.as_ref().map_or(&[], |session| &session.latest_matches)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::jobs::JobHandle;
    use crate::search::{MatchTarget, SearchMatch};

    use super::*;

    struct ActiveCorpus;

    impl SearchCorpus for ActiveCorpus {
        fn is_active(&self) -> bool {
            true
        }

        fn keys(&self) -> Vec<Arc<str>> {
            vec![Arc::from("gene")]
        }

        fn resolve_matches(&self, _matches: &[SearchMatch]) -> Vec<ResolvedMatch> {
            Vec::new()
        }
    }

    #[test]
    fn a_disconnected_worker_stops_convergence_and_preserves_previous_matches() {
        let corpus = ActiveCorpus;
        let mut state = SearchState::new();
        state.session = Some(SearchSession::spawn(corpus.keys()));
        state.resolved_matches.push(ResolvedMatch {
            key: Arc::from("previous"),
            bold_char_indices: vec![0],
            destination: MatchTarget::Row(0),
        });

        let (reply, handle) = JobHandle::<super::super::QueryResponse>::channel();
        drop(reply);
        let session = state.session.as_mut().unwrap();
        session.last_sent = Some("gene".to_string());
        session.pending = Some(handle);
        state.converging = true;

        assert!(!state.tick("gene", &corpus));
        assert!(!state.is_converging());
        assert_eq!(state.resolved().len(), 1);
        assert_eq!(state.take_error().as_deref(), Some("Search worker disconnected"));

        assert!(!state.tick("gene", &corpus));
        assert!(state.session.as_ref().unwrap().pending.is_none());
        assert_eq!(state.resolved().len(), 1);
    }
}
