//! Applies BAM replacements and asynchronous GTF loads for `GenomeViewer`.
//!
//! `start_load` submits foreground work to the shared `IoWorkerHandle`, preserving the distinction
//! between cache-only loading and build-if-missing loading. `drain_tracked_jobs` polls without
//! blocking, removes completed or disconnected jobs, then applies results after releasing the
//! tracked-job borrow. Successful indexes update both the locus index and search corpus; failures
//! become viewer notifications, while in-flight jobs drive shell progress and redraw activity.

use crate::Instant;
use std::io;
use std::path::{Path, PathBuf};

use crate::basepair::BasePair;
use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{JobHandle, JobPoll};
use crate::locus::FeatureIndex;
use crate::locus::LoadResult;

use super::{GenomePopover, GenomeViewer, alignment::OnDiskBamSource, local_area::OnDiskSummarySource};

pub(super) struct BamLoad {
    path: PathBuf,
    locus: Option<(String, BasePair)>,
    handle: JobHandle<io::Result<BamSources>>,
    pub(super) started_at: Instant,
}

struct BamSources {
    pileup: OnDiskBamSource,
    summary: OnDiskSummarySource,
}

impl BamSources {
    fn open(path: &Path, worker: IoWorkerHandle) -> io::Result<Self> {
        Ok(Self {
            pileup: OnDiskBamSource::from_path(path, worker)?,
            summary: OnDiskSummarySource::from_path(path)?,
        })
    }
}

fn bam_load_error(path: &Path, error: io::Error) -> String {
    format!("Failed to load BAM {}: {error}", path.display())
}

/// A background GTF load.
pub(crate) enum TrackedJob {
    GtfLoad {
        handle: JobHandle<io::Result<LoadResult>>,
        label: &'static str,
        started_at: Instant,
    },
}

impl TrackedJob {
    pub(super) fn label(&self) -> &'static str {
        match self {
            TrackedJob::GtfLoad { label, .. } => label,
        }
    }

    pub(super) fn started_at(&self) -> Instant {
        match self {
            TrackedJob::GtfLoad { started_at, .. } => *started_at,
        }
    }
}

/// A completed job waiting to be applied after polling.
pub(super) enum DrainedJob {
    GtfLoad(io::Result<LoadResult>),
}

impl GenomeViewer {
    pub(super) fn add_bam(&mut self, path: PathBuf) {
        if !self.bam_paths.contains(&path) {
            self.bam_paths.push(path);
            if let Some(GenomePopover::Bams(switcher)) = &mut self.popover {
                switcher.refresh(&self.bam_paths, self.io_worker.as_ref());
            }
        }
    }

    /// Typed and replayed commands return their load errors to the dispatcher.
    pub(super) fn load_active_bam(&mut self, path: PathBuf) -> Result<(), String> {
        let Some(worker) = self.io_worker.clone() else {
            return Err("Loading BAM files is unavailable in this demo".to_string());
        };
        self.pending_bam = None;
        self.add_bam(path.clone());
        let sources = BamSources::open(&path, worker).map_err(|error| bam_load_error(&path, error))?;
        self.install_bam(path, sources);
        Ok(())
    }

    pub(super) fn switch_bam(&mut self, path: PathBuf, locus: Option<(String, BasePair)>) -> Result<(), String> {
        let Some(worker) = &self.io_worker else {
            return Err("Loading BAM files is unavailable in this demo".to_string());
        };
        self.pending_bam = None;
        if self.active_bam.as_ref() == Some(&path) {
            if let Some((contig, position)) = locus {
                self.jump_to_locus(&contig, position)?;
            }
            return Ok(());
        }
        let bam_path = path.clone();
        let source_worker = worker.clone();
        let handle = worker.submit(IoSchedule::Foreground, move || {
            BamSources::open(&bam_path, source_worker)
        });
        self.pending_bam = Some(BamLoad {
            path,
            locus,
            handle,
            started_at: Instant::now(),
        });
        Ok(())
    }

    pub(super) fn poll_bam_load(&mut self) -> bool {
        let Some(mut pending) = self.pending_bam.take() else {
            return false;
        };
        let result = match pending.handle.try_recv() {
            JobPoll::Empty => {
                self.pending_bam = Some(pending);
                return true;
            }
            JobPoll::Ready(result) => result,
            JobPoll::Disconnected | JobPoll::Stale => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "I/O worker disconnected"))
            }
        };
        match result {
            Ok(sources) => {
                if let Some((contig, position)) = pending.locus
                    && let Err(message) = self.jump_to_locus(&contig, position)
                {
                    self.notify.error(message);
                } else {
                    self.install_bam(pending.path, sources);
                }
            }
            Err(error) => self.notify.error(bam_load_error(&pending.path, error)),
        }
        true
    }

    fn install_bam(&mut self, path: PathBuf, sources: BamSources) {
        self.track_align.set_source(sources.pileup);
        self.track_local_area.set_source(sources.summary);
        self.active_bam = Some(path);
    }

    /// Starts an asynchronous GTF load, building a missing cache only if `build_missing`.
    ///
    /// The load-only path composes its own failure text here rather than in `apply_load_result`,
    /// which sees only an `io::Error` and cannot tell the two paths apart. Naming the resolved path
    /// is what makes the message a command line the user can paste straight back into the bar.
    pub(super) fn start_load(
        &mut self,
        gtf_path: PathBuf,
        cache_path: PathBuf,
        build_missing: bool,
    ) -> Result<(), String> {
        let Some(worker) = &self.io_worker else {
            return Err("Loading GTF files is unavailable in this demo".to_string());
        };
        self.tracked_jobs.push(TrackedJob::GtfLoad {
            handle: worker.submit(IoSchedule::Foreground, move || {
                if build_missing {
                    return crate::locus::load_or_build(&gtf_path, &cache_path);
                }
                crate::locus::load_only(&cache_path).map_err(|e| {
                    let advice = match crate::locus::index_command_for(&gtf_path) {
                        Ok(command) => format!("run {command} to build it"),
                        Err(error) => format!("cannot construct a command to build it: {error}"),
                    };
                    io::Error::new(e.kind(), format!("{e} — {advice}"))
                })
            }),
            label: "Loading GTF annotations",
            started_at: Instant::now(),
        });
        Ok(())
    }

    /// Returns whether a GTF load is in flight.
    #[cfg(test)]
    pub(super) fn gtf_load_in_flight(&self) -> bool {
        self.tracked_jobs
            .iter()
            .any(|j| matches!(j, TrackedJob::GtfLoad { .. }))
    }

    /// Installs a completed load and reports its result.
    fn apply_load_result(&mut self, result: io::Result<LoadResult>) {
        match result {
            Ok(LoadResult::FromCache { index, elapsed }) => {
                let message = format!(
                    "Loaded {} entries from cache in {:.1}ms",
                    index.len(),
                    elapsed.as_secs_f64() * 1000.0
                );
                self.install_loaded_index(index, message);
            }
            Ok(LoadResult::Built { index, elapsed }) => {
                let message = format!("Built cache ({} entries) in {:.2}s", index.len(), elapsed.as_secs_f64());
                self.install_loaded_index(index, message);
            }
            Err(e) => self.notify.error(format!("Failed to load GTF: {e}")),
        }
    }

    /// Installs a loaded index and refreshes search state.
    pub(super) fn install_loaded_index(&mut self, index: FeatureIndex, message: String) {
        self.locus_index.set(index);
        self.search.install_corpus(&self.locus_index);
        self.notify.info(message);
    }

    /// Polls tracked jobs and returns whether any were pending.
    pub(super) fn drain_tracked_jobs(&mut self) -> bool {
        let animating = !self.tracked_jobs.is_empty();
        let mut drained: Vec<DrainedJob> = Vec::new();
        self.tracked_jobs.retain_mut(|job| match job {
            TrackedJob::GtfLoad { handle, .. } => match handle.try_recv() {
                JobPoll::Ready(result) => {
                    drained.push(DrainedJob::GtfLoad(result));
                    false
                }
                JobPoll::Empty => true,
                JobPoll::Stale => false,
                JobPoll::Disconnected => {
                    drained.push(DrainedJob::GtfLoad(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "I/O worker disconnected",
                    ))));
                    false
                }
            },
        });
        for done in drained {
            match done {
                DrainedJob::GtfLoad(result) => self.apply_load_result(result),
            }
        }
        animating
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{new_viewer, settle_search, viewer_with_loaded_entries};
    use crate::basepair::BasePair;
    use crate::locus::{FeatureIndex, FeatureStrand, Locus};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn draining_clears_the_queue() {
        use crate::viewer::StatusProvider;

        let mut viewer = new_viewer();
        viewer.notify.info("test message");

        let drained = viewer.drain_notifications();

        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "test message");
        assert!(
            viewer.drain_notifications().is_empty(),
            "a second drain must find nothing left"
        );
    }

    #[test]
    fn a_fresh_load_rebuilds_the_matcher_against_the_new_index() {
        let mut viewer = viewer_with_loaded_entries("search_reset_on_load");
        settle_search(&mut viewer, "IL9R");
        assert!(!viewer.search.resolved().is_empty(), "fixture must match IL9R");

        let replacement = FeatureIndex::new(
            vec![Locus {
                key: Arc::from("ZZZONLY:NM_999999.9:ex1"),
                start: BasePair::new(100),
                end: BasePair::new(200),
                strand: FeatureStrand::Unknown,
            }],
            HashMap::from([("chr1".to_string(), 0..1)]),
        );
        viewer.install_loaded_index(replacement, "replaced".to_string());

        settle_search(&mut viewer, "ZZZONLY");
        let resolved = viewer.search.resolved();
        assert!(
            resolved.iter().any(|m| m.key.as_ref() == "ZZZONLY:NM_999999.9:ex1"),
            "a key from the newly loaded index must be searchable, got {:?}",
            resolved.iter().map(|m| m.key.as_ref()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn installing_a_loaded_index_eagerly_creates_the_search_session() {
        let mut viewer = new_viewer();
        assert!(!viewer.search.has_matcher());

        let index = FeatureIndex::new(
            vec![Locus {
                key: Arc::from("EAGER:NM_000001.1:ex1"),
                start: BasePair::new(100),
                end: BasePair::new(200),
                strand: FeatureStrand::Unknown,
            }],
            HashMap::from([("chr1".to_string(), 0..1)]),
        );
        viewer.install_loaded_index(index, "loaded".to_string());

        assert!(viewer.search.has_matcher());
    }
}
