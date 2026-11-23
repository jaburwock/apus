//! Viewer-owned fuzzy-search infrastructure backed by a persistent worker.
//!
//! The worker ranks opaque `Arc<str>` keys; `SearchCorpus` supplies those keys and resolves them
//! back into viewer-specific destinations. Keeping ranking corpus-blind lets the same protocol
//! serve genome loci without coupling the viewer to the command layer. Numeric `/` input is
//! routed as a VCF row goto by `command::line` before a corpus is consulted.
//!
//! `SearchSession` owns the worker handle and applies latest-wins queries, generation checks, and
//! non-blocking result polling. A disconnected worker remains distinct from an empty result so
//! the UI cannot report stale work as successful, and results from an old corpus cannot overwrite
//! current state.

pub(crate) mod locus;
pub(crate) mod resolved;
pub(crate) mod state;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::jobs::{JobHandle, JobPoll, Worker, WorkerHandle};

pub use resolved::{MatchTarget, ResolvedMatch};

/// One ranked search result: a matched key alongside the sorted,
/// deduplicated character offsets the fuzzy match touched.
#[derive(Clone, Debug)]
pub(crate) struct SearchMatch {
    pub(crate) key: Arc<str>,
    pub(crate) indices: Vec<u32>,
}

/// A viewer-owned set of keys the shared worker ranks against, and the knowledge
/// of what those keys mean.
///
/// The worker is corpus-blind: it holds `Arc<str>` and scores them. This trait is
/// the other half — it supplies the keys and turns a ranked key back into a
/// destination. The current consumer is the genome locus corpus; VCF value lookup
/// now belongs to SQL projections rather than a second fuzzy corpus.
pub(crate) trait SearchCorpus {
    /// Whether anything is loaded. A corpus that is not active is not searched,
    /// and `SearchState` reports no matches rather than spawning a worker for it.
    fn is_active(&self) -> bool;

    fn keys(&self) -> Vec<Arc<str>>;

    /// Attaches this corpus's destination to each ranked key. A key the corpus no
    /// longer holds is dropped rather than resolved, so a match cannot outlive the
    /// index it came from.
    fn resolve_matches(&self, matches: &[SearchMatch]) -> Vec<ResolvedMatch>;
}

/// Number of top search matches returned per query.
pub(crate) const SEARCH_MATCH_LIMIT: usize = 5;

const SCAN_CHUNK_SIZE: usize = 256;

struct QueryResponse {
    generation: u64,
    matches: Vec<SearchMatch>,
}

enum SearchRequest {
    Rebuild {
        generation: u64,
        keys: Vec<Arc<str>>,
    },
    Query {
        generation: u64,
        query: String,
        reply: Sender<QueryResponse>,
    },
}

struct RankedMatch {
    key: Arc<str>,
    score: u32,
}

/// Uses `nucleo-matcher` directly over the keys. The high-level `Nucleo` wrapper was rejected: it
/// builds a per-entry corpus and a private Rayon pool the first time `/` opens.
struct SearchWorker {
    keys: Vec<Arc<str>>,
    matcher: Matcher,
}

impl SearchWorker {
    fn new(keys: Vec<Arc<str>>) -> Self {
        Self {
            keys,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    fn rebuild(&mut self, keys: Vec<Arc<str>>) {
        self.keys = keys;
    }

    /// Keeps only the newest pending query and applies rebuilds in order.
    fn drain_pending(&mut self, receiver: &Receiver<SearchRequest>) -> Option<SearchRequest> {
        let mut latest_query = None;
        while let Ok(request) = receiver.try_recv() {
            match request {
                SearchRequest::Rebuild {
                    generation: _generation,
                    keys,
                } => {
                    self.rebuild(keys);
                    latest_query = None;
                }
                request @ SearchRequest::Query { .. } => latest_query = Some(request),
            }
        }
        latest_query
    }

    fn query(
        &mut self,
        generation: u64,
        query: String,
        reply: Sender<QueryResponse>,
        receiver: &Receiver<SearchRequest>,
    ) -> Option<SearchRequest> {
        let pattern = Pattern::parse(&query, CaseMatching::Ignore, Normalization::Smart);
        let mut haystack_buf = Vec::new();
        let mut ranked: Vec<RankedMatch> = Vec::with_capacity(SEARCH_MATCH_LIMIT);

        for chunk_start in (0..self.keys.len()).step_by(SCAN_CHUNK_SIZE) {
            if let Some(next) = self.drain_pending(receiver) {
                return Some(next);
            }

            let chunk_end = (chunk_start + SCAN_CHUNK_SIZE).min(self.keys.len());
            for index in chunk_start..chunk_end {
                let key = &self.keys[index];
                let haystack = Utf32Str::new(key.as_ref(), &mut haystack_buf);
                let Some(score) = pattern.score(haystack, &mut self.matcher) else {
                    continue;
                };

                let candidate = RankedMatch {
                    key: Arc::clone(key),
                    score,
                };
                let insertion = ranked
                    .iter()
                    .position(|existing| candidate.score > existing.score)
                    .unwrap_or(ranked.len());
                if insertion < SEARCH_MATCH_LIMIT {
                    ranked.insert(insertion, candidate);
                    ranked.truncate(SEARCH_MATCH_LIMIT);
                }
            }
        }

        let matches = ranked
            .into_iter()
            .map(|candidate| {
                let mut indices = Vec::new();
                let haystack = Utf32Str::new(candidate.key.as_ref(), &mut haystack_buf);
                let _ = pattern.indices(haystack, &mut self.matcher, &mut indices);
                indices.sort_unstable();
                indices.dedup();
                SearchMatch {
                    key: candidate.key,
                    indices,
                }
            })
            .collect();

        let _ = reply.send(QueryResponse { generation, matches });
        self.drain_pending(receiver)
    }
}

impl Worker for SearchWorker {
    type Request = SearchRequest;

    fn run(mut self, receiver: Receiver<Self::Request>) {
        let mut next = None;
        loop {
            let request = match next.take().or_else(|| receiver.recv().ok()) {
                Some(request) => request,
                None => return,
            };
            match request {
                SearchRequest::Rebuild {
                    generation: _generation,
                    keys,
                } => self.rebuild(keys),
                SearchRequest::Query {
                    generation,
                    query,
                    reply,
                } => next = self.query(generation, query, reply, &receiver),
            }
        }
    }
}

/// A fuzzy-search session with one persistent dedicated worker thread.
pub(crate) struct SearchSession {
    worker: WorkerHandle<SearchWorker>,
    pending: Option<JobHandle<QueryResponse>>,
    generation: u64,
    last_sent: Option<String>,
    /// The most recent ranked matches from a completed query.
    pub(crate) latest_matches: Vec<SearchMatch>,
}

impl SearchSession {
    /// Starts the dedicated worker and installs its initial corpus.
    pub(crate) fn spawn(keys: Vec<Arc<str>>) -> Self {
        Self {
            worker: WorkerHandle::spawn(SearchWorker::new(keys)),
            pending: None,
            generation: 0,
            last_sent: None,
            latest_matches: Vec::new(),
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Replaces the corpus without restarting the worker thread.
    pub fn rebuild(&mut self, keys: Vec<Arc<str>>) {
        let generation = self.next_generation();
        self.pending = None;
        self.last_sent = None;
        self.latest_matches.clear();
        let _ = self.worker.send(SearchRequest::Rebuild { generation, keys });
    }

    /// Sends one query if it differs from the last query.
    pub fn query(&mut self, query: &str) -> bool {
        if self.last_sent.as_deref() == Some(query) {
            return false;
        }

        self.last_sent = Some(query.to_string());
        let generation = self.next_generation();
        let (reply, handle) = JobHandle::channel();
        self.pending = Some(handle);
        let _ = self.worker.send(SearchRequest::Query {
            generation,
            query: query.to_string(),
            reply,
        });
        true
    }

    /// Non-blocking poll for the current query's result.
    pub fn try_recv(&mut self) -> Option<Result<Vec<SearchMatch>, String>> {
        let pending = self.pending.as_mut()?;
        match pending.try_recv() {
            JobPoll::Empty => None,
            JobPoll::Stale => {
                self.pending = None;
                None
            }
            JobPoll::Disconnected => {
                self.pending = None;
                Some(Err("Search worker disconnected".to_string()))
            }
            JobPoll::Ready(response) => {
                self.pending = None;
                (response.generation == self.generation).then_some(Ok(response.matches))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn search_key(gene: &str) -> Arc<str> {
        format!("{gene}:NM_000000.1:ex1").into()
    }

    fn settle(search: &mut SearchSession, query: &str) -> Vec<SearchMatch> {
        search.query(query);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(Ok(matches)) = search.try_recv() {
                search.latest_matches = matches.clone();
                return matches;
            }
            assert!(std::time::Instant::now() < deadline, "query did not converge in time");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_query_converges_and_returns_highlight_indices() {
        let keys = vec![search_key("IL9R"), search_key("BPY2C"), search_key("TP53")];
        let mut search = SearchSession::spawn(keys);
        let matches = settle(&mut search, "IL9R");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].key.as_ref(), "IL9R:NM_000000.1:ex1");
        assert!(!matches[0].indices.is_empty());
    }

    #[test]
    fn top_matches_are_bounded_and_equal_scores_keep_original_order() {
        let keys = (0..8)
            .map(|index| format!("gene{index}:NM_000000.1:ex1").into())
            .collect();
        let mut search = SearchSession::spawn(keys);
        let matches = settle(&mut search, "");
        let keys: Vec<_> = matches.iter().map(|matched| matched.key.as_ref()).collect();
        assert_eq!(keys.len(), SEARCH_MATCH_LIMIT);
        assert_eq!(
            keys,
            vec![
                "gene0:NM_000000.1:ex1",
                "gene1:NM_000000.1:ex1",
                "gene2:NM_000000.1:ex1",
                "gene3:NM_000000.1:ex1",
                "gene4:NM_000000.1:ex1",
            ]
        );
    }

    #[test]
    fn rebuilding_reuses_the_session_for_a_new_corpus() {
        let mut search = SearchSession::spawn(vec![search_key("IL9R")]);
        assert_eq!(settle(&mut search, "IL9R").len(), 1);
        search.rebuild(vec![search_key("ZZZONLY")]);
        let matches = settle(&mut search, "ZZZONLY");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].key.as_ref(), "ZZZONLY:NM_000000.1:ex1");
    }

    #[test]
    fn newer_queries_replace_older_pending_results() {
        let keys = (0..20_000)
            .map(|index| format!("gene{index}:NM_000000.1:ex1").into())
            .collect();
        let mut search = SearchSession::spawn(keys);
        assert!(search.query("gene1"));
        assert!(search.query("gene19999"));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(Ok(matches)) = search.try_recv() {
                assert_eq!(
                    matches.first().map(|matched| matched.key.as_ref()),
                    Some("gene19999:NM_000000.1:ex1")
                );
                return;
            }
            assert!(std::time::Instant::now() < deadline, "newest query did not converge");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_disconnected_search_worker_is_not_an_empty_match_result() {
        let mut search = SearchSession::spawn(vec![search_key("IL9R")]);
        let (reply, handle) = JobHandle::channel();
        drop(reply);
        search.pending = Some(handle);

        assert!(matches!(
            search.try_recv(),
            Some(Err(error)) if error == "Search worker disconnected"
        ));
    }
}
