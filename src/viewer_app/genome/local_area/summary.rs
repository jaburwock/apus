//! The local-area summary source is the viewport-scale BAM model beneath the context chart.
//! It sits beside the alignment read cache: this layer reduces CIGARs to depth and large-indel
//! presence, and only reads modification tags and SEQ where reference CpG counts qualify a bin.
//! An indexed BAM query is reduced directly into a bounded, globally anchored bin grid, with the
//! BAI index and header retained for the source lifetime. Each query streams completed bins by
//! genomic range so a compatible scroll can keep work already in flight.
//! Each read contributes at most once per indel kind in a bin, preserving the partition between
//! plain, deletion, and insertion depth that the chart relies on. The CIGAR walk keeps internal
//! coordinates 0-based and converts the BAM position at the ingest boundary (invariant 1).
//! A superseded grid may donate bins only when the contig, bin width and data requirements still
//! agree. Missing or failed bins stay unavailable so the renderer shows fog.

use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use noodles::{bam, bgzf, sam};

use crate::basepair::BasePair;
use crate::cigar::{CigarKind, CigarOp};
use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{Batch, Generation, JobHandle, JobPoll};

#[cfg(feature = "resident-demo")]
use super::super::alignment::ResidentBamSource;
use super::super::bam_query::BamRegionReader;
use super::super::geometry::AreaSpan;
use super::super::mod_5mc::{BaseMod5mC, decode_5mc_from_record};
use super::super::packed_seq::PackedSeqRef;
use super::super::reference_data::{ColumnBases, ReferenceCounts, ReferenceLookup};
use crate::viewer_app::bam_index::SharedIndex;

const MAX_BINS: u64 = 512;
const FLUSH_STRIDE: usize = 32;
/// Indels shorter than this stay plain. At viewport scale the track answers "is there a
/// large event around here", so identity is not modelled: a bin counts carriers per kind
/// and the renderer paints one layer each.
const MIN_INDEL_LENGTH: u64 = 100;

/// Carrier counts rather than event records. A fuzz-matched cluster table was rejected: evicting a
/// cluster folded its carriers back into plain depth, so a clear deletion in a busy bin vanished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::viewer_app::genome) struct SummaryBin {
    pub(in crate::viewer_app::genome) spanning_depth: u64,
    pub(in crate::viewer_app::genome) deletion_depth: u64,
    pub(in crate::viewer_app::genome) insertion_depth: u64,
    pub(super) insertion_bp: u64,
    pub(super) mod_5mc_prob_sum: u64,
    pub(in crate::viewer_app::genome) mod_5mc_call_count: u64,
}

impl SummaryBin {
    /// Counts one read once per kind, however many qualifying indels it carries here.
    /// Per-read rather than per-event is what keeps `plain + deletion + insertion ==
    /// spanning`, so the stack stays a partition of real depth and a read carrying three
    /// deletions cannot paint more coverage than exists. `insertion_bp` accumulates the
    /// read's qualifying inserted bases in this bin so the chart can derive mean inserted
    /// length per carrier read (`insertion_bp / insertion_depth`).
    fn add_record(&mut self, deletion: bool, insertion_bp: u64) {
        self.spanning_depth += 1;
        self.deletion_depth += u64::from(deletion);
        if insertion_bp > 0 {
            self.insertion_depth += 1;
            self.insertion_bp = self.insertion_bp.saturating_add(insertion_bp);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::viewer_app::genome) enum SummaryMode {
    CoverageOnly,
    FiveMc { min_cpgs: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::viewer_app::genome) struct GridKey {
    contig: String,
    start: BasePair,
    end: BasePair,
    bin_width: BasePair,
    pub(in crate::viewer_app::genome) mode: SummaryMode,
}

impl GridKey {
    pub(in crate::viewer_app::genome) fn for_span(contig: &str, span: AreaSpan) -> Self {
        let width_for_cap = span.width().as_u64().div_ceil(MAX_BINS - 1).max(1);
        let bin_width = BasePair::length(width_for_cap.checked_next_power_of_two().unwrap_or(u64::MAX));
        let start = BasePair(span.start().as_u64() / bin_width.as_u64() * bin_width.as_u64());
        let end = BasePair(
            span.end()
                .as_u64()
                .div_ceil(bin_width.as_u64())
                .saturating_mul(bin_width.as_u64()),
        );
        Self {
            contig: contig.to_string(),
            start,
            end,
            bin_width,
            mode: SummaryMode::CoverageOnly,
        }
    }

    pub(super) fn reference_fits(&self, reference: &ReferenceLookup<'_>) -> bool {
        reference.reference_band_fits(
            self.start,
            self.end.min(reference.contig_length()),
            ReferenceCounts::BasesAndCpg,
        )
    }

    pub(in crate::viewer_app::genome) fn bin_start(&self, index: usize) -> BasePair {
        self.start + self.bin_width * BasePair(index as u64)
    }

    pub(in crate::viewer_app::genome) fn bin_count(&self) -> usize {
        (self.end - self.start).as_u64().div_ceil(self.bin_width.as_u64()) as usize
    }

    fn bin_index(&self, position: BasePair) -> usize {
        ((position - self.start).as_u64() / self.bin_width.as_u64()) as usize
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.contig == other.contig && self.mode == other.mode && self.start < other.end && other.start < self.end
    }

    fn for_run(&self, run: Range<usize>) -> Self {
        Self {
            start: self.bin_start(run.start),
            end: self.bin_start(run.end),
            ..self.clone()
        }
    }
}

struct SummaryGrid {
    key: GridKey,
    bins: Vec<Option<SummaryBin>>,
    eligible_5mc: Option<Vec<Option<bool>>>,
}

impl SummaryGrid {
    fn new<'a>(key: GridKey, donors: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut grid = Self {
            bins: vec![None; key.bin_count()],
            eligible_5mc: match key.mode {
                SummaryMode::CoverageOnly => None,
                SummaryMode::FiveMc { .. } => Some(vec![None; key.bin_count()]),
            },
            key,
        };
        for donor in donors {
            grid.fill_from(donor);
        }
        grid
    }

    /// Fills empty bins from a grid whose bins mean the same thing here: same contig and width,
    /// and a mode carrying at least the data this grid needs.
    fn fill_from(&mut self, previous: &Self) {
        if previous.key.contig != self.key.contig
            || previous.key.bin_width != self.key.bin_width
            || !(previous.key.mode == self.key.mode || self.key.mode == SummaryMode::CoverageOnly)
        {
            return;
        }
        for index in 0..self.bins.len() {
            let start = self.key.bin_start(index);
            if start < previous.key.start || start >= previous.key.end {
                continue;
            }
            let old = previous.key.bin_index(start);
            if let (Some(eligible), Some(previous_eligible)) = (&mut self.eligible_5mc, &previous.eligible_5mc) {
                eligible[index] = eligible[index].or(previous_eligible[old]);
            }
            if self.bins[index].is_some() {
                continue;
            }
            let mut bin = previous.bins[old];
            if self.eligible_5mc.is_none()
                && previous.eligible_5mc.is_some()
                && let Some(bin) = &mut bin
            {
                // The same 5mC chart can use coverage-only bins when the reference is too wide.
                bin.mod_5mc_prob_sum = 0;
                bin.mod_5mc_call_count = 0;
            }
            self.bins[index] = bin;
        }
    }

    fn resolve_5mc_eligibility(&mut self, mut reference: ReferenceLookup<'_>) {
        let (SummaryMode::FiveMc { min_cpgs }, Some(eligible)) = (self.key.mode, &mut self.eligible_5mc) else {
            return;
        };
        for (index, result) in eligible.iter_mut().enumerate().filter(|(_, result)| result.is_none()) {
            let start = self.key.bin_start(index);
            let end = self.key.bin_start(index + 1).min(reference.contig_length());
            *result = match reference.column_bases(start, end, ReferenceCounts::BasesAndCpg) {
                ColumnBases::Complete(histogram) => Some(u64::from(histogram.cpg) >= min_cpgs),
                ColumnBases::Failed | ColumnBases::NoReference | ColumnBases::OutOfContig => Some(false),
                ColumnBases::Partial => None,
            };
        }
    }

    fn covers(&self, start: BasePair, end: BasePair) -> bool {
        if start < self.key.start || end > self.key.end || start >= end {
            return false;
        }
        let first = self.key.bin_index(start);
        let last = self.key.bin_index(end - BasePair(1));
        self.bins[first..=last].iter().all(Option::is_some)
    }

    fn next_missing_run(&self) -> Option<Range<usize>> {
        let ready = |index: usize| {
            self.bins[index].is_none()
                && self
                    .eligible_5mc
                    .as_ref()
                    .is_none_or(|eligible| eligible[index].is_some())
        };
        let first = (0..self.bins.len()).find(|&index| ready(index))?;
        let end = (first + 1..self.bins.len())
            .find(|&index| !ready(index))
            .unwrap_or(self.bins.len());
        Some(first..end)
    }

    #[cfg(feature = "resident-demo")]
    fn fill_resident(&mut self, source: &ResidentBamSource, window_at: impl Fn(BasePair) -> Option<Range<BasePair>>) {
        let mut index = 0;
        while index < self.bins.len() {
            let start = self.key.bin_start(index);
            let Some(window) = window_at(start).filter(|window| start + self.key.bin_width <= window.end) else {
                index += 1;
                continue;
            };
            if self.bins[index].is_some() {
                index += 1;
                continue;
            }
            let first = index;
            index += 1;
            while index < self.bins.len() && self.bins[index].is_none() && self.key.bin_start(index + 1) <= window.end {
                index += 1;
            }
            let key = self.key.for_run(first..index);
            let mut bins = vec![SummaryBin::default(); index - first];
            for read in source.reads(&key.contig) {
                if read.start < key.end && key.start < read.start + read.length {
                    let Ok(()) = add_alignment(
                        &mut bins,
                        &key,
                        read.start,
                        read.cigar.ops().iter().copied().map(Ok::<_, std::convert::Infallible>),
                        None,
                        || None,
                    );
                }
            }
            for (slot, bin) in self.bins[first..index].iter_mut().zip(bins) {
                *slot = Some(bin);
            }
        }
    }
}

pub(crate) struct OnDiskSummarySource {
    io_worker: IoWorkerHandle,
    bam_path: PathBuf,
    index: SharedIndex,
    header: Arc<noodles::sam::Header>,
    batch: Batch,
}

impl OnDiskSummarySource {
    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        let index = SharedIndex::read(path)?;
        let header = File::open(path).and_then(|file| bam::io::Reader::new(file).read_header())?;
        Ok(Self {
            io_worker: IoWorkerHandle::spawn(),
            bam_path: path.to_path_buf(),
            index,
            header: Arc::new(header),
            batch: Batch::new(),
        })
    }
}

pub(in crate::viewer_app::genome) struct SummaryBatch {
    start: BasePair,
    pub(in crate::viewer_app::genome) bins: Vec<SummaryBin>,
}

struct PendingScan {
    /// The scan's remaining span, advanced as batches land.
    key: GridKey,
    batches: mpsc::Receiver<SummaryBatch>,
    completion: JobHandle<io::Result<()>>,
}

impl PendingScan {
    fn overlaps_missing(&self, grid: &SummaryGrid) -> bool {
        let key = &grid.key;
        let start = self.key.start.max(key.start);
        let end = self.key.end.min(key.end);
        if self.key.contig != key.contig
            || self.key.mode != key.mode
            || self.key.bin_width != key.bin_width
            || start >= end
        {
            return false;
        }
        let first = key.bin_index(start);
        let last = key.bin_index(end);
        grid.bins[first..last].iter().any(Option::is_none)
    }

    fn drain_into(&mut self, grid: &mut SummaryGrid) -> bool {
        let mut updated = false;
        while let Ok(batch) = self.batches.try_recv() {
            self.key.start = batch.start + self.key.bin_width * BasePair(batch.bins.len() as u64);
            for (offset, bin) in batch.bins.into_iter().enumerate() {
                let start = batch.start + self.key.bin_width * BasePair(offset as u64);
                if start >= grid.key.start && start < grid.key.end {
                    let index = grid.key.bin_index(start);
                    grid.bins[index] = Some(bin);
                    updated = true;
                }
            }
        }
        updated
    }
}

pub(in crate::viewer_app::genome) struct SummaryCache {
    source: Option<OnDiskSummarySource>,
    #[cfg(feature = "resident-demo")]
    resident: Option<ResidentBamSource>,
    grid: Option<SummaryGrid>,
    /// The last grid at another bin width, so a zoom round trip restores it instead of rescanning.
    spare: Option<SummaryGrid>,
    generation: Option<Generation>,
    pending: Option<PendingScan>,
    failed: Option<GridKey>,
    errors: Vec<String>,
}

impl SummaryCache {
    pub(in crate::viewer_app::genome) fn new() -> Self {
        Self {
            source: None,
            #[cfg(feature = "resident-demo")]
            resident: None,
            grid: None,
            spare: None,
            generation: None,
            pending: None,
            failed: None,
            errors: Vec::new(),
        }
    }

    pub(super) fn set_source(&mut self, source: OnDiskSummarySource) {
        self.source = Some(source);
        #[cfg(feature = "resident-demo")]
        {
            self.resident = None;
        }
        self.clear();
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn set_resident_source(&mut self, source: ResidentBamSource) {
        self.source = None;
        self.resident = Some(source);
        self.clear();
    }

    pub(super) fn supports_5mc(&self) -> bool {
        #[cfg(feature = "resident-demo")]
        {
            self.resident.is_none()
        }
        #[cfg(not(feature = "resident-demo"))]
        {
            true
        }
    }

    pub(super) fn clear(&mut self) {
        self.grid = None;
        self.spare = None;
        self.generation = None;
        self.pending = None;
        self.failed = None;
        self.errors.clear();
    }

    pub(super) fn ensure(&mut self, desired: GridKey, reference: Option<ReferenceLookup<'_>>) {
        if self.failed.as_ref().is_some_and(|failed| failed.overlaps(&desired)) {
            return;
        }
        self.failed = None;
        if self.grid.as_ref().is_none_or(|grid| grid.key != desired) {
            if let (Some(pending), Some(grid)) = (&mut self.pending, &mut self.grid) {
                pending.drain_into(grid);
            }
            let grid = SummaryGrid::new(desired, self.grid.iter().chain(&self.spare));
            #[cfg(feature = "resident-demo")]
            let grid = {
                let mut grid = grid;
                if let (Some(source), Some(reference)) = (&self.resident, reference.as_ref()) {
                    grid.fill_resident(source, |start| reference.resident_span(start));
                }
                grid
            };
            if !self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.overlaps_missing(&grid))
            {
                self.pending = None;
                self.generation = None;
            }
            let width = grid.key.bin_width;
            if let Some(previous) = self.grid.replace(grid)
                && previous.key.bin_width != width
            {
                self.spare = Some(previous);
            }
        }
        if self.generation.is_none() {
            self.generation = self.source.as_mut().map(|source| source.batch.next());
        }
        self.prepare_scan(reference);
    }

    pub(super) fn prepare_scan(&mut self, reference: Option<ReferenceLookup<'_>>) {
        if let (Some(_), Some(grid), Some(reference)) = (&self.generation, &mut self.grid, reference) {
            grid.resolve_5mc_eligibility(reference);
        }
        self.submit_next();
    }

    #[cfg(test)]
    pub(super) fn covers(&self, start: BasePair, end: BasePair) -> bool {
        self.grid.as_ref().is_some_and(|grid| grid.covers(start, end))
    }

    /// Visits every resident bin intersecting a half-open range.
    ///
    /// The active grid is the only answer source. `false` keeps a partially
    /// resident terminal column behind fog rather than turning missing bins
    /// into a plausible zero-depth dropout. The visitor keeps projection
    /// allocation-free even when a terminal half-column crosses many bins.
    pub(super) fn visit_resident_bins(
        &self,
        start: BasePair,
        end: BasePair,
        mut visit: impl FnMut(&SummaryBin, BasePair, BasePair, u64),
    ) -> bool {
        let Some(grid) = self.grid.as_ref() else {
            return false;
        };
        if !grid.covers(start, end) {
            return false;
        }
        let first = grid.key.bin_index(start);
        let last = grid.key.bin_index(end - BasePair(1));
        for index in first..=last {
            let bin_start = grid.key.bin_start(index);
            let bin_end = bin_start + grid.key.bin_width;
            let overlap_start = start.max(bin_start);
            let overlap_end = end.min(bin_end);
            let Some(bin) = grid.bins[index].as_ref() else {
                return false;
            };
            visit(bin, bin_start, bin_end, (overlap_end - overlap_start).as_u64());
        }
        true
    }

    pub(super) fn tick(&mut self, enabled: bool) -> bool {
        if !enabled {
            self.generation = None;
            if let (Some(mut pending), Some(grid)) = (self.pending.take(), &mut self.grid) {
                pending.drain_into(grid);
                return true;
            }
            return false;
        }
        let Some(mut pending) = self.pending.take() else {
            return false;
        };
        // Completion is sent after the final batch. Poll it first so the drain sees that batch
        // even if the worker finishes during this tick.
        let result = pending.completion.try_recv();
        let updated = self.grid.as_mut().is_some_and(|grid| pending.drain_into(grid));
        match result {
            JobPoll::Empty => {
                self.pending = Some(pending);
                updated
            }
            JobPoll::Stale => updated,
            JobPoll::Disconnected => {
                self.fail("its worker stopped; reload the BAM to restart it".to_string());
                self.source = None;
                true
            }
            JobPoll::Ready(Err(error)) => {
                self.fail(error.to_string());
                true
            }
            JobPoll::Ready(Ok(())) => {
                self.submit_next();
                true
            }
        }
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.pending.is_some() || self.is_waiting_for_reference()
    }

    pub(super) fn is_waiting_for_reference(&self) -> bool {
        self.pending.is_none()
            && self.generation.is_some()
            && self
                .grid
                .as_ref()
                .is_some_and(|grid| grid.eligible_5mc.is_some() && grid.bins.iter().any(Option::is_none))
    }

    pub(super) fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    #[cfg(test)]
    pub(super) fn has_source(&self) -> bool {
        self.source.is_some()
    }

    #[cfg(test)]
    pub(super) fn seed_prefix(&mut self, contig: &str, span: AreaSpan, count: usize) {
        let mut grid = SummaryGrid::new(GridKey::for_span(contig, span), []);
        for bin in grid.bins.iter_mut().take(count) {
            *bin = Some(SummaryBin::default());
        }
        self.grid = Some(grid);
    }

    #[cfg(any(test, feature = "profiling"))]
    pub(in crate::viewer_app::genome) fn seed_bins(&mut self, contig: &str, span: AreaSpan, bins: Vec<SummaryBin>) {
        let mut grid = SummaryGrid::new(GridKey::for_span(contig, span), []);
        for (slot, bin) in grid.bins.iter_mut().zip(bins) {
            *slot = Some(bin);
        }
        self.grid = Some(grid);
    }

    fn fail(&mut self, error: String) {
        self.failed = self.grid.as_ref().map(|grid| grid.key.clone());
        self.errors
            .push(format!("Failed to load local-area BAM summary: {error}"));
        self.generation = None;
    }

    fn submit_next(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let (Some(source), Some(generation), Some(grid)) = (&self.source, &self.generation, &self.grid) else {
            return;
        };
        let Some(run) = grid.next_missing_run() else {
            return;
        };
        let qualifying = grid
            .eligible_5mc
            .as_ref()
            .map(|eligible| {
                eligible[run.clone()]
                    .iter()
                    .map(|&flag| flag == Some(true))
                    .collect::<Vec<_>>()
            })
            .filter(|flags| flags.iter().any(|&flag| flag));
        let key = grid.key.for_run(run);
        let path = source.bam_path.clone();
        let index = source.index.clone();
        let header = Arc::clone(&source.header);
        let scan_key = key.clone();
        let (send, batches) = mpsc::channel();
        let completion = source
            .io_worker
            .submit_cancellable(IoSchedule::Viewport(generation.clone()), move |cancel| {
                scan_summary(
                    &path,
                    &index,
                    &header,
                    &scan_key,
                    qualifying.as_deref(),
                    || cancel.is_cancelled(),
                    |batch| {
                        let _ = send.send(batch);
                    },
                )
            });
        self.pending = Some(PendingScan {
            key,
            batches,
            completion,
        });
    }
}

pub(in crate::viewer_app::genome) fn scan_summary(
    path: &Path,
    index: &SharedIndex,
    header: &sam::Header,
    key: &GridKey,
    qualifying: Option<&[bool]>,
    cancelled: impl Fn() -> bool,
    mut emit: impl FnMut(SummaryBatch),
) -> io::Result<()> {
    if cancelled() || key.start >= key.end {
        return Ok(());
    }
    let mut bgzf = bgzf::io::Reader::new(File::open(path)?);
    let mut reader = BamRegionReader::query(&mut bgzf, index, header, &key.contig, key.start..key.end)?;
    let mut bins = vec![SummaryBin::default(); key.bin_count()];
    let mut flushed = 0;

    while let Some((start, record)) = reader.next_record(&cancelled)? {
        // Sorted starts let us finalize a bin even while earlier reads span later batches.
        while flushed + FLUSH_STRIDE <= bins.len() && key.bin_start(flushed + FLUSH_STRIDE) <= start {
            let end = flushed + FLUSH_STRIDE;
            emit(SummaryBatch {
                start: key.bin_start(flushed),
                bins: bins[flushed..end].to_vec(),
            });
            flushed = end;
            if cancelled() {
                return Ok(());
            }
        }
        add_record(&mut bins, key, start, record, qualifying)?;
    }
    for (offset, batch) in bins[flushed..].chunks(FLUSH_STRIDE).enumerate() {
        if cancelled() {
            break;
        }
        emit(SummaryBatch {
            start: key.bin_start(flushed + offset * FLUSH_STRIDE),
            bins: batch.to_vec(),
        });
    }
    Ok(())
}

fn add_record(
    bins: &mut [SummaryBin],
    key: &GridKey,
    record_start: BasePair,
    record: &bam::Record,
    qualifying: Option<&[bool]>,
) -> io::Result<()> {
    if record.name().is_none() {
        return Ok(());
    }
    let cigar = record.cigar();
    add_alignment(
        bins,
        key,
        record_start,
        CigarOp::iter_bam_bytes(cigar.as_bytes())?,
        qualifying,
        || {
            let mods = decode_5mc_from_record(record)?;
            let seq = record.sequence();
            let seq = PackedSeqRef::from_bam_bytes(seq.as_bytes(), seq.len());
            Some((mods, seq, record.flags().is_reverse_complemented()))
        },
    )
}

fn add_alignment<'a, E>(
    bins: &mut [SummaryBin],
    key: &GridKey,
    record_start: BasePair,
    cigar: impl Iterator<Item = Result<CigarOp, E>>,
    qualifying: Option<&[bool]>,
    mut modification_data: impl FnMut() -> Option<(BaseMod5mC, PackedSeqRef<'a>, bool)>,
) -> Result<(), E> {
    let mut cursor = record_start;
    let mut query_index = 0;
    let mut modifications = None;
    let mut deletions: Vec<(BasePair, BasePair)> = Vec::new();
    let mut insertions: Vec<(BasePair, BasePair)> = Vec::new();

    for op in cigar {
        let op = op?;
        let kind = op.kind();
        let length = BasePair::length(op.len() as u64);
        let op_end = cursor + length;
        match (kind, qualifying) {
            (CigarKind::Insertion, _) if length.as_u64() >= MIN_INDEL_LENGTH => {
                // Insertions have no reference span and belong to the preceding base.
                insertions.push((cursor.checked_sub(BasePair(1)).unwrap_or(record_start), length));
            }
            (CigarKind::Deletion, _) if length.as_u64() >= MIN_INDEL_LENGTH => deletions.push((cursor, op_end)),
            (CigarKind::Match | CigarKind::SequenceMatch | CigarKind::SequenceMismatch, Some(qualifying)) => {
                let mut start = cursor.max(key.start);
                let end = op_end.min(key.end);
                while start < end {
                    let index = key.bin_index(start);
                    let stop = end.min(key.bin_start(index + 1));
                    if qualifying[index] {
                        let mods = modifications.get_or_insert_with(&mut modification_data);
                        let Some((mods, seq, reverse)) = mods else {
                            break;
                        };
                        let from = query_index + (start - cursor).as_u64() as usize;
                        let to = query_index + (stop - cursor).as_u64() as usize;
                        add_5mc_calls(&mut bins[index], mods, *seq, *reverse, from..to);
                    }
                    start = stop;
                }
            }
            _ => {}
        }
        if kind.consumes_reference() {
            cursor = op_end;
        }
        if kind.consumes_read() {
            query_index += op.len();
        }
    }
    let record_end = cursor.min(key.end);
    let first_base = record_start.max(key.start);
    if first_base >= record_end {
        return Ok(());
    }
    let first = key.bin_index(first_base);
    let last = key.bin_index(record_end - BasePair(1));

    for (offset, bin) in bins[first..=last].iter_mut().enumerate() {
        let bin_start = key.bin_start(first + offset);
        let bin_end = bin_start + key.bin_width;
        let deletion = deletions.iter().any(|&(start, end)| start < bin_end && end > bin_start);
        let insertion_bp = insertions
            .iter()
            .filter(|(start, _)| *start >= bin_start && *start < bin_end)
            .fold(0u64, |acc, (_, length)| acc.saturating_add(length.as_u64()));
        bin.add_record(deletion, insertion_bp);
    }
    Ok(())
}

fn add_5mc_calls(bin: &mut SummaryBin, mods: &BaseMod5mC, seq: PackedSeqRef<'_>, reverse: bool, query: Range<usize>) {
    let first = mods.calls.partition_point(|&(index, _)| (index as usize) < query.start);
    let end = first + mods.calls[first..].partition_point(|&(index, _)| (index as usize) < query.end);
    let mut explicit_count = 0;
    for calls in mods.calls[first..end].chunk_by(|a, b| a.0 == b.0) {
        let (index, prob) = calls[0];
        if seq.is_cpg_cytosine(index as usize, reverse) {
            bin.mod_5mc_prob_sum += u64::from(prob);
            explicit_count += 1;
        }
    }
    bin.mod_5mc_call_count += if mods.implicit_unmodified {
        (query.start..query.end.min(seq.len()))
            .filter(|&index| seq.is_cpg_cytosine(index, reverse))
            .count() as u64
    } else {
        explicit_count
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodles::sam::alignment::io::Write as _;

    fn span_at(centre: u64, width: u64, scale: u64) -> AreaSpan {
        AreaSpan::for_viewport(
            crate::viewer_app::viewport::ViewPosition::new(BasePair(centre), BasePair(width)),
            scale,
            BasePair(20_000_000),
        )
    }

    #[cfg(feature = "resident-demo")]
    #[test]
    fn resident_bins_match_indexed_depth_and_keep_incomplete_bins_unavailable() {
        let sam = concat!(
            "left\t0\tchr1\t1\t60\t20M104D20M131I40M\t*\t0\t0\t*\t*\n",
            "overhang\t0\tchr1\t350\t60\t100M\t*\t0\t0\t*\t*\n",
            "right\t0\tchr1\t580\t60\t40M\t*\t0\t0\t*\t*\n",
        );
        let (path, header) = crate::testgen::write_indexed_test_bam("resident_summary", sam);
        let index = SharedIndex::read(&path).unwrap();
        let source = ResidentBamSource::read(&std::fs::read(&path).unwrap()).unwrap();
        let windows = [BasePair(5)..BasePair(413), BasePair(516)..BasePair(703)];
        for width in [8, 16, 32] {
            let key = GridKey {
                contig: "chr1".to_string(),
                start: BasePair(0),
                end: BasePair(768),
                bin_width: BasePair::length(width),
                mode: SummaryMode::CoverageOnly,
            };
            let mut indexed = Vec::new();
            scan_summary(
                &path,
                &index,
                &header,
                &key,
                None,
                || false,
                |batch| indexed.extend(batch.bins),
            )
            .unwrap();
            let mut grid = SummaryGrid::new(key, []);
            grid.fill_resident(&source, |start| {
                windows.iter().find(|window| window.contains(&start)).cloned()
            });

            for (i, bin) in indexed.into_iter().enumerate() {
                let start = grid.key.bin_start(i);
                let end = grid.key.bin_start(i + 1);
                let expected = windows.iter().any(|window| window.start <= start && end <= window.end);
                assert_eq!(
                    grid.bins[i],
                    expected.then_some(bin),
                    "bin at {start:?} with width {width}"
                );
            }
            assert!(grid.bins.contains(&Some(SummaryBin::default())));
            assert!(grid.bins.iter().flatten().any(|bin| bin.deletion_depth > 0));
            assert!(grid.bins.iter().flatten().any(|bin| bin.insertion_depth > 0));
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn attach_pending_scan(
        cache: &mut SummaryCache,
        key: GridKey,
    ) -> (
        mpsc::Sender<SummaryBatch>,
        mpsc::Sender<io::Result<()>>,
        crate::jobs::Cancellation,
    ) {
        let (send, batches) = mpsc::channel();
        let (complete, completion, cancellation) = JobHandle::cancellable_channel(cache.generation.clone());
        cache.pending = Some(PendingScan {
            key,
            batches,
            completion,
        });
        (send, complete, cancellation)
    }

    #[test]
    fn a_disconnected_scan_worker_reports_once_and_stops_fetching() {
        let mut cache = SummaryCache::new();
        cache.set_source(OnDiskSummarySource::from_path(Path::new("testdata/bam/tiny.bam")).expect("BAM fixture"));
        let mut key = GridKey::for_span("chr1", span_at(10_000_000, 320, 500));
        // Without a reference lookup, 5mC lets the test supply the completion channel.
        key.mode = SummaryMode::FiveMc { min_cpgs: 1 };
        cache.ensure(key.clone(), None);
        let (_send, complete, _) = attach_pending_scan(&mut cache, key);
        drop(complete);

        assert!(cache.tick(true));
        assert_eq!(
            cache.take_errors(),
            ["Failed to load local-area BAM summary: its worker stopped; reload the BAM to restart it"]
        );

        cache.ensure(GridKey::for_span("chr1", span_at(15_000_000, 320, 500)), None);
        assert!(!cache.is_fetching());
        assert!(!cache.tick(true));
        assert!(cache.take_errors().is_empty());
    }

    #[test]
    fn scrolling_donates_landed_bins_and_keeps_the_scan_while_its_tail_overlaps() {
        let key = GridKey {
            contig: "chr1".to_string(),
            start: BasePair(400),
            end: BasePair(800),
            bin_width: BasePair::length(4),
            mode: SummaryMode::CoverageOnly,
        };
        let mut cache = SummaryCache::new();
        cache.ensure(key.clone(), None);
        let (send, _complete, cancellation) = attach_pending_scan(&mut cache, key.clone());
        let send_batch = |first| {
            send.send(SummaryBatch {
                start: key.bin_start(first),
                bins: (first..first + FLUSH_STRIDE)
                    .map(|i| SummaryBin {
                        spanning_depth: 7 * i as u64 + 1,
                        ..SummaryBin::default()
                    })
                    .collect(),
            })
            .unwrap();
        };
        send_batch(0);
        assert!(cache.tick(true));
        assert!(cache.covers(key.start, key.bin_start(FLUSH_STRIDE)));
        assert!(!cache.covers(key.start, key.end));

        let shifted = |bins| {
            let offset = key.bin_width * BasePair(bins);
            GridKey {
                start: key.start + offset,
                end: key.end + offset,
                ..key.clone()
            }
        };
        cache.ensure(shifted(5), None);
        assert_eq!(cache.grid.as_ref().unwrap().bins[0].unwrap().spanning_depth, 36);
        let shift = FLUSH_STRIDE + 5;
        cache.ensure(shifted(shift as u64), None);
        assert!(!cancellation.is_cancelled());
        send_batch(FLUSH_STRIDE);
        assert!(cache.tick(true));
        assert_eq!(
            cache.grid.as_ref().unwrap().bins[0].unwrap().spanning_depth,
            7 * shift as u64 + 1
        );

        // The grids still overlap, but this jump leaves no unscanned bins in common.
        let left = key.bin_width * BasePair(45);
        cache.ensure(
            GridKey {
                start: key.start - left,
                end: key.end - left,
                ..key
            },
            None,
        );
        assert!(cancellation.is_cancelled());
        assert!(cache.pending.is_none());
        assert!(cache.take_errors().is_empty());
    }

    /// Work continues to completion once movement stops.
    #[test]
    fn the_grid_fills_completely_after_a_scroll_settles() {
        let contig = BasePair(248_000_000);
        let viewport = BasePair(320);
        let span_for = |centre: u64| {
            AreaSpan::for_viewport(
                crate::viewer_app::viewport::ViewPosition::new(BasePair(centre), viewport),
                500,
                contig,
            )
        };
        let mut cache = SummaryCache::new();
        cache.set_source(OnDiskSummarySource::from_path(Path::new("testdata/bam/tiny.bam")).expect("BAM fixture"));
        for step in 0..=8u64 {
            cache.ensure(
                GridKey::for_span("chr1", span_for(10_000_000 + step * viewport.as_u64() * 2)),
                None,
            );
        }
        for _ in 0..5_000 {
            if !cache.is_fetching() {
                break;
            }
            cache.tick(true);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let grid = cache.grid.as_ref().expect("grid");
        assert!(
            grid.bins.iter().all(Option::is_some),
            "every bin must be resident once movement stops"
        );
    }

    #[test]
    fn a_scale_change_cancels_the_in_flight_scan() {
        let mut cache = SummaryCache::new();
        let before = GridKey::for_span("chr1", span_at(10_000_000, 320, 500));
        let after = GridKey::for_span("chr1", span_at(10_000_000, 320, 20));
        assert_ne!(before.bin_width, after.bin_width);
        cache.ensure(before.clone(), None);
        let (_send, _complete, cancellation) = attach_pending_scan(&mut cache, before);
        cache.ensure(after.clone(), None);
        assert!(cancellation.is_cancelled());
        assert!(cache.pending.is_none());
        assert_eq!(cache.grid.as_ref().unwrap().key, after);
    }

    /// Coverage answers for the grid being drawn, never for the one it replaced.
    ///
    /// A scale change re-bins, so the superseded grid holds real depth at a width the
    /// renderer no longer uses. Reporting it as covered would clear the fog from a
    /// column the new grid has nothing for, drawing zero depth as a dropout — the
    /// stale-summary failure the whole grid key exists to prevent.
    #[test]
    fn coverage_follows_the_current_bin_width_and_survives_a_same_width_scroll() {
        let narrow = span_at(5_000, 100, 1);
        let mut cache = SummaryCache::new();
        cache.seed_prefix("chr1", narrow, usize::MAX);
        let narrow_key = cache.grid.as_ref().expect("seeded grid").key.clone();
        assert!(
            cache.covers(narrow_key.start, narrow_key.end),
            "the seeded grid covers itself"
        );

        cache.ensure(GridKey::for_span("chr1", span_at(5_000, 100, 10_000)), None);
        assert_ne!(
            cache.grid.as_ref().expect("re-binned grid").key.bin_width,
            narrow_key.bin_width,
            "the scale change must re-bin, or this proves nothing"
        );
        assert!(
            !cache.covers(narrow_key.start, narrow_key.end),
            "a re-binned grid must not inherit coverage at the old bin width"
        );

        // A scroll that keeps the bin width does reuse bins, so the fog must not
        // return for the overlap — the donor copy is what makes progressive fill work.
        let mut cache = SummaryCache::new();
        cache.seed_prefix("chr1", narrow, usize::MAX);
        let overlapping = span_at(5_020, 100, 1);
        cache.ensure(GridKey::for_span("chr1", overlapping), None);
        let grid = cache.grid.as_ref().expect("scrolled grid");
        assert_eq!(grid.key.bin_width, narrow_key.bin_width, "a scroll must not re-bin");
        let shared_end = narrow_key.end.min(grid.key.end);
        assert!(
            cache.covers(grid.key.start, shared_end),
            "bins that keep their genomic meaning must carry across a scroll"
        );
    }

    #[test]
    fn a_zoom_round_trip_restores_bins_scanned_at_the_original_width() {
        let close = span_at(5_000, 100, 1);
        let close_key = GridKey::for_span("chr1", close);
        let seeded: Vec<_> = (1..=close_key.bin_count() as u64)
            .map(|depth| SummaryBin {
                spanning_depth: depth,
                ..SummaryBin::default()
            })
            .collect();
        let mut cache = SummaryCache::new();
        cache.seed_bins("chr1", close, seeded.clone());

        let mid = GridKey::for_span("chr1", span_at(5_000, 100, 10_000));
        assert_ne!(mid.bin_width, close_key.bin_width, "the zoom must re-bin");
        cache.ensure(mid, None);
        cache.ensure(close_key, None);
        let restored = &cache.grid.as_ref().unwrap().bins;
        assert!(restored.iter().zip(&seeded).all(|(bin, seeded)| *bin == Some(*seeded)));
    }

    #[test]
    fn summary_grids_align_cover_and_cap_generated_viewports() {
        let mut state = 0x91a2_4f03_77c1_08d5u64;
        for _ in 0..512 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let contig_length = 1 + state % 20_000_000;
            let width = (state.rotate_left(17) % 10_000_000).max(1);
            let centre = state.rotate_left(31) % (contig_length + 1);
            let position = crate::viewer_app::viewport::ViewPosition::new(BasePair(centre), BasePair(width));
            let span = AreaSpan::for_viewport(position, 1 + state % 1_000, BasePair(contig_length));
            let key = GridKey::for_span("chr1", span);
            assert!(key.bin_width.as_u64().is_power_of_two());
            assert!(key.bin_count() <= MAX_BINS as usize);
            assert!(key.start.as_u64().is_multiple_of(key.bin_width.as_u64()));
            assert!(key.end.as_u64().is_multiple_of(key.bin_width.as_u64()));
            assert!(key.start <= span.start() && key.end >= span.end());
            assert!(span.start() - key.start < key.bin_width);
            assert!(key.end - span.end() < key.bin_width);
            assert!(key.end > key.start);
        }
    }

    fn bam_record_for_cigar(header: &noodles::sam::Header, cigar: &str, query_length: usize) -> bam::Record {
        let sequence = "A".repeat(query_length);
        let quality = "I".repeat(query_length);
        let sam_line = format!("r1\t0\tchr1\t1\t60\t{cigar}\t*\t0\t0\t{sequence}\t{quality}\n");
        let mut bytes = Vec::new();
        {
            let mut writer = bam::io::Writer::new(&mut bytes);
            writer.write_header(header).expect("header");
            let mut reader = noodles::sam::io::Reader::new(sam_line.as_bytes());
            let mut record = noodles::sam::alignment::RecordBuf::default();
            reader.read_record_buf(header, &mut record).expect("SAM record");
            writer.write_alignment_record(header, &record).expect("BAM record");
            writer.try_finish().expect("BAM finish");
        }
        let mut reader = bam::io::Reader::new(bytes.as_slice());
        reader.read_header().expect("BAM header");
        reader.records().next().expect("one record").expect("valid record")
    }

    fn add_test_record(
        bins: &mut [SummaryBin],
        start: BasePair,
        bin_width: BasePair,
        record: &bam::Record,
        qualifying: Option<&[bool]>,
    ) -> io::Result<()> {
        let key = GridKey {
            contig: "chr1".to_string(),
            start,
            end: start + bin_width * BasePair(bins.len() as u64),
            bin_width,
            mode: SummaryMode::CoverageOnly,
        };
        let pos = record.alignment_start().unwrap()?;
        add_record(
            bins,
            &key,
            BasePair::from_1based(usize::from(pos) as u64),
            record,
            qualifying,
        )
    }

    fn summarize_cigar(
        header: &noodles::sam::Header,
        cigar: &str,
        query_length: usize,
        bin_width: u64,
        bin_count: usize,
    ) -> Vec<SummaryBin> {
        let record = bam_record_for_cigar(header, cigar, query_length);
        let mut bins = vec![SummaryBin::default(); bin_count];
        add_test_record(&mut bins, BasePair(0), BasePair::length(bin_width), &record, None).expect("summary");
        bins
    }

    #[test]
    fn insertion_bp_accumulates_only_qualifying_insertions_while_keeping_depth_per_read() {
        let reference = crate::refgenome::RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai")
            .expect("reference fixture must load");
        let header = crate::testgen::header_from_ref_index(&reference);

        let below = summarize_cigar(&header, "100M99I100M", 299, 1_000, 1);
        assert_eq!(
            (below[0].spanning_depth, below[0].insertion_depth, below[0].insertion_bp),
            (1, 0, 0),
            "insertions shorter than MIN_INDEL_LENGTH must not contribute depth or bp"
        );

        let at_threshold = summarize_cigar(&header, "100M100I100M", 300, 1_000, 1);
        assert_eq!(
            (
                at_threshold[0].spanning_depth,
                at_threshold[0].insertion_depth,
                at_threshold[0].insertion_bp
            ),
            (1, 1, 100)
        );

        let multi_in_one_read = summarize_cigar(&header, "100M200I100M300I100M", 800, 10_000, 1);
        assert_eq!(
            (
                multi_in_one_read[0].spanning_depth,
                multi_in_one_read[0].insertion_depth,
                multi_in_one_read[0].insertion_bp
            ),
            (1, 1, 500),
            "multiple qualifying insertions in one read keep insertion_depth per-read while summing bp"
        );

        let deletion = summarize_cigar(&header, "100M500D100M", 200, 100, 7);
        assert!(deletion.iter().all(|bin| bin.insertion_bp == 0));

        let skip = summarize_cigar(&header, "100M500N100M", 200, 100, 7);
        assert!(skip.iter().all(|bin| bin.insertion_bp == 0));

        let mut bins = vec![SummaryBin::default(); 2];
        for (cigar, query_length) in [("100M200I100M", 400), ("100M600I100M", 800)] {
            let record = bam_record_for_cigar(&header, cigar, query_length);
            add_test_record(&mut bins, BasePair(0), BasePair::length(100), &record, None).expect("summary");
        }
        assert_eq!(
            (bins[0].spanning_depth, bins[0].insertion_depth, bins[0].insertion_bp),
            (2, 2, 800)
        );
        assert_eq!(
            (bins[1].spanning_depth, bins[1].insertion_depth, bins[1].insertion_bp),
            (2, 0, 0)
        );
    }

    #[test]
    fn cigar_summary_preserves_half_open_bins_and_read_kind_partitioning() {
        let reference = crate::refgenome::RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai")
            .expect("reference fixture must load");
        let header = crate::testgen::header_from_ref_index(&reference);
        let both = summarize_cigar(&header, "100M500D100M300I100M", 600, 1_000, 1);
        let bin = both[0];
        assert_eq!(
            (bin.spanning_depth, bin.deletion_depth, bin.insertion_depth),
            (1, 1, 1),
            "one read carrying both kinds counts once in each"
        );

        let twice = summarize_cigar(&header, "100M200D100M200D100M", 300, 10_000, 1);
        assert_eq!(
            (twice[0].spanning_depth, twice[0].deletion_depth),
            (1, 1),
            "a read is one deletion carrier however many deletions it has here"
        );
        assert!(twice[0].deletion_depth <= twice[0].spanning_depth);

        let deletion = summarize_cigar(&header, "100M500D100M", 200, 100, 7);
        let deletion_oracle: [(u64, u64); 7] = [(1, 0), (1, 1), (1, 1), (1, 1), (1, 1), (1, 1), (1, 0)];
        assert_eq!(
            deletion
                .iter()
                .map(|bin| (bin.spanning_depth, bin.deletion_depth))
                .collect::<Vec<_>>(),
            deletion_oracle
        );

        let insertion = summarize_cigar(&header, "100M500I100M", 700, 100, 2);
        assert_eq!(
            insertion
                .iter()
                .map(|bin| (bin.spanning_depth, bin.insertion_depth))
                .collect::<Vec<_>>(),
            vec![(1u64, 1u64), (1, 0)]
        );

        let skip = summarize_cigar(&header, "100M500N100M", 200, 100, 7);
        assert!(
            skip.iter()
                .all(|bin| bin.spanning_depth == 1 && bin.deletion_depth == 0 && bin.insertion_depth == 0)
        );

        for (cigar, query_length, expected) in [
            ("200M", 200, vec![1, 1, 0, 0]),
            ("250M", 250, vec![1, 1, 1, 0]),
            ("50M", 50, vec![1, 0, 0, 0]),
        ] {
            let bins = summarize_cigar(&header, cigar, query_length, 100, 4);
            assert_eq!(
                bins.iter().map(|bin| bin.spanning_depth).collect::<Vec<_>>(),
                expected,
                "{cigar}"
            );
        }
    }

    #[test]
    fn methylation_calls_follow_aligned_cigar_positions_and_qualifying_bins() {
        let records = crate::testgen::bam_records_from_sam(concat!(
            "forward\t0\tchr1\t11\t60\t2H2S4M2I2=2D2X2N4M2S2H\t*\t0\t0\tCGCGCGCGCGCGCGCGCG\t*\tMM:Z:C+m.,0,0,1,0,0,0,0,0;\tML:B:C,240,11,241,32,63,80,105,242\n",
            "reverse\t16\tchr1\t11\t60\t2H2S4M2I2=2D2X2N4M2S2H\t*\t0\t0\tCGCGCGCGCGCGCGCGCG\t*\tMM:Z:C+m?,0,0,0,1,0,0,1;\tML:B:C,243,121,144,56,244,18,245\n",
            "stale\t0\tchr1\t11\t60\t2H2S4M2I2=2D2X2N4M2S2H\t*\t0\t0\tCGCGCGCGCGCGCGCGCG\t*\tMM:Z:C+m?,6;\tML:B:C,250\tMN:i:19\n",
        ));
        let expected = [(106, 4), (63, 1), (224, 2), (226, 2)];
        for qualifying in [
            &[true, true, true, true][..],
            &[true, false, true],
            &[false, false, false],
        ] {
            let mut bins = vec![SummaryBin::default(); qualifying.len()];
            for record in &records {
                add_test_record(&mut bins, BasePair(12), BasePair(4), record, Some(qualifying)).unwrap();
            }
            for ((bin, &qualifies), (sum, count)) in bins.iter().zip(qualifying).zip(expected) {
                assert_eq!(bin.spanning_depth, 3);
                assert_eq!(
                    (bin.mod_5mc_prob_sum, bin.mod_5mc_call_count),
                    if qualifies { (sum, count) } else { (0, 0) },
                );
            }
        }
    }

    #[test]
    fn methylation_grid_reuses_only_compatible_bins_and_pending_work() {
        let span = span_at(10_000_000, 2_000, 1);
        let mut key = GridKey::for_span("chr1", span);
        key.mode = SummaryMode::FiveMc { min_cpgs: 1 };
        let sample = SummaryBin {
            spanning_depth: 12,
            deletion_depth: 3,
            mod_5mc_prob_sum: 246,
            mod_5mc_call_count: 1,
            ..SummaryBin::default()
        };
        let mut cache = SummaryCache::new();
        cache.set_source(OnDiskSummarySource::from_path(Path::new("testdata/bam/tiny.bam")).unwrap());
        cache.ensure(key.clone(), None);
        assert!(cache.is_fetching());
        assert!(
            cache.pending.is_none(),
            "unresolved gates must keep BAM work off the queue"
        );
        let grid = cache.grid.as_mut().unwrap();
        let midpoint = grid.bins.len() / 2;
        grid.bins[midpoint] = Some(sample);
        grid.eligible_5mc.as_mut().unwrap().fill(Some(true));
        let (_send, _complete, cancellation) = attach_pending_scan(&mut cache, key.clone());

        let mut shifted = GridKey::for_span("chr1", span_at(10_000_000 + key.bin_width.as_u64(), 2_000, 1));
        shifted.mode = key.mode;
        cache.ensure(shifted.clone(), None);
        let grid = cache.grid.as_ref().unwrap();
        assert_eq!(grid.bins[midpoint - 1], Some(sample));
        assert_eq!(grid.eligible_5mc.as_ref().unwrap()[midpoint - 1], Some(true));
        assert_eq!(grid.eligible_5mc.as_ref().unwrap().last(), Some(&None));
        assert!(!cancellation.is_cancelled());

        let mut new_threshold = shifted.clone();
        new_threshold.mode = SummaryMode::FiveMc { min_cpgs: 2 };
        cache.ensure(new_threshold, None);
        assert!(cancellation.is_cancelled());
        let grid = cache.grid.as_ref().unwrap();
        assert!(grid.bins.iter().all(Option::is_none));
        assert!(grid.eligible_5mc.as_ref().unwrap().iter().all(Option::is_none));
        assert!(
            cache.pending.is_none(),
            "a threshold change invalidates in-flight 5mC work"
        );

        cache.ensure(shifted.clone(), None);
        cache.grid.as_mut().unwrap().bins[midpoint - 1] = Some(sample);
        shifted.mode = SummaryMode::CoverageOnly;
        cache.ensure(shifted, None);
        assert_eq!(
            cache.grid.as_ref().unwrap().bins[midpoint - 1],
            Some(SummaryBin {
                spanning_depth: 12,
                deletion_depth: 3,
                ..SummaryBin::default()
            })
        );
    }

    #[test]
    fn missing_runs_stop_at_resident_bins_and_unresolved_5mc_gates() {
        let key = GridKey {
            contig: "chr1".to_string(),
            start: BasePair(20),
            end: BasePair(40),
            bin_width: BasePair::length(2),
            mode: SummaryMode::FiveMc { min_cpgs: 1 },
        };
        let mut grid = SummaryGrid::new(key, []);
        let qualifying = grid.eligible_5mc.as_mut().unwrap();
        qualifying[1..7].fill(Some(true));
        qualifying[7] = Some(false);
        qualifying[9] = Some(false);
        grid.bins[4] = Some(SummaryBin::default());

        for run in [1..4, 5..8, 9..10] {
            assert_eq!(grid.next_missing_run(), Some(run.clone()));
            grid.bins[run].fill(Some(SummaryBin::default()));
        }
        assert_eq!(grid.next_missing_run(), None);
    }

    #[test]
    fn failed_ranges_retry_only_after_the_view_leaves_them() {
        let first = AreaSpan::for_viewport(
            crate::viewer_app::viewport::ViewPosition::new(BasePair(500), BasePair(100)),
            1,
            BasePair(10_000),
        );
        let mut cache = SummaryCache::new();
        cache.ensure(GridKey::for_span("chr1", first), None);
        cache.fail("broken".to_string());
        let failed = cache.failed.clone().expect("failed range");

        cache.ensure(GridKey::for_span("chr1", first), None);
        assert_eq!(
            cache.failed.as_ref(),
            Some(&failed),
            "an overlapping draw must not retry"
        );

        let disjoint = AreaSpan::for_viewport(
            crate::viewer_app::viewport::ViewPosition::new(BasePair(5_000), BasePair(100)),
            1,
            BasePair(10_000),
        );
        cache.ensure(GridKey::for_span("chr1", disjoint), None);
        assert!(cache.failed.is_none());
    }

    #[test]
    fn tiny_bam_streams_to_complete_bins_without_materialising_reads() {
        let mut cache = SummaryCache::new();
        cache.set_source(OnDiskSummarySource::from_path(Path::new("testdata/bam/tiny.bam")).expect("tiny BAM source"));
        let span = AreaSpan::for_viewport(
            crate::viewer_app::viewport::ViewPosition::new(BasePair(32), BasePair(64)),
            1,
            BasePair(1_000),
        );
        cache.ensure(GridKey::for_span("chr1", span), None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while cache.is_fetching() {
            assert!(std::time::Instant::now() < deadline, "summary fetch timed out");
            cache.tick(true);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(cache.take_errors().is_empty());
        assert!(cache.covers(span.start(), span.end()));
    }

    #[test]
    fn streamed_batches_match_independent_per_bin_queries() {
        use noodles::core::{Position, Region};

        let sam = format!(
            concat!(
                "long\t0\tchr1\t1\t60\t320M105D140M140I220M\t*\t0\t0\t{}\t*",
                "\tMM:Z:C+m?,4,18,140,1,74,36,13,30,52,26;\tML:B:C,13,31,59,79,83,103,127,157,199,251\n",
                "boundary1\t0\tchr1\t331\t60\t15M\t*\t0\t0\t*\t*\n",
                "boundary2\t0\tchr1\t651\t60\t35M\t*\t0\t0\t*\t*\n",
                "tail\t0\tchr1\t806\t60\t10M\t*\t0\t0\t*\t*\n",
                "outside\t0\tchr1\t811\t60\t10M\t*\t0\t0\t*\t*\n",
            ),
            "CG".repeat(410),
        );
        let (path, header) = crate::testgen::write_indexed_test_bam("streamed_summary", &sam);
        let index = SharedIndex::read(&path).unwrap();
        let key = GridKey {
            contig: "chr1".to_string(),
            start: BasePair(10),
            end: BasePair(810),
            bin_width: BasePair::length(10),
            mode: SummaryMode::FiveMc { min_cpgs: 1 },
        };
        let qualifying: Vec<_> = (0..key.bin_count()).map(|i| i % 7 != 4).collect();
        let mut bins = Vec::new();
        scan_summary(
            &path,
            &index,
            &header,
            &key,
            Some(&qualifying),
            || false,
            |batch| {
                assert_eq!(batch.start, key.bin_start(bins.len()));
                assert_eq!(batch.bins.len(), FLUSH_STRIDE.min(key.bin_count() - bins.len()));
                bins.extend(batch.bins);
            },
        )
        .unwrap();
        assert_eq!(bins.len(), key.bin_count());

        let mut noodles = bam::io::IndexedReader::new(File::open(&path).unwrap(), index);
        let aligned_calls = [
            (8, 13),
            (46, 31),
            (433, 59),
            (437, 79),
            (611, 157),
            (717, 199),
            (771, 251),
        ];
        for (i, (got, eligible)) in bins.into_iter().zip(qualifying).enumerate() {
            let start = key.bin_start(i);
            let end = key.bin_start(i + 1);
            let interval = Position::try_from(start.to_display() as usize).unwrap()
                ..=Position::try_from(end.as_u64() as usize).unwrap();
            let region = Region::new("chr1", interval);
            let mut want = SummaryBin::default();
            for record in noodles.query(&header, &region).unwrap().records() {
                let record = record.unwrap();
                want.spanning_depth += 1;
                if record.name().unwrap() != b"long" {
                    continue;
                }
                if start < BasePair(425) && end > BasePair(320) {
                    want.deletion_depth = 1;
                }
                if start <= BasePair(564) && BasePair(564) < end {
                    want.insertion_depth = 1;
                    want.insertion_bp = 140;
                }
                for (position, prob) in aligned_calls {
                    if eligible && start <= BasePair(position) && BasePair(position) < end {
                        want.mod_5mc_prob_sum += prob;
                        want.mod_5mc_call_count += 1;
                    }
                }
            }
            assert_eq!(got, want, "bin {i}");
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn cancelling_a_scan_keeps_finalized_bins_and_skips_its_tail() {
        let path = Path::new("testdata/bam/tiny.bam");
        let index = SharedIndex::read(path).unwrap();
        let header = bam::io::Reader::new(File::open(path).unwrap()).read_header().unwrap();
        let key = GridKey::for_span("chr1", span_at(100_000, 200_000, 1));
        assert!(key.bin_count() > 2 * FLUSH_STRIDE);
        let cancelled = std::cell::Cell::new(false);
        let mut bins = Vec::new();
        scan_summary(
            path,
            &index,
            &header,
            &key,
            None,
            || cancelled.get(),
            |batch| {
                assert_eq!(batch.start, key.start);
                bins.extend(batch.bins);
                cancelled.set(true);
            },
        )
        .unwrap();
        assert_eq!(bins.len(), FLUSH_STRIDE);
    }

    #[test]
    fn final_aligned_bin_may_extend_past_the_contig() {
        let key = GridKey {
            contig: "chr1".to_string(),
            start: BasePair(248_384),
            end: BasePair(248_416),
            bin_width: BasePair::length(32),
            mode: SummaryMode::CoverageOnly,
        };
        let path = Path::new("testdata/bam/tiny.bam");
        let index = SharedIndex::read(path).expect("bai fixture");
        let header = File::open(path)
            .and_then(|file| bam::io::Reader::new(file).read_header())
            .expect("header");
        let mut bins = Vec::new();
        scan_summary(
            path,
            &index,
            &header,
            &key,
            None,
            || false,
            |batch| bins.extend(batch.bins),
        )
        .expect("the indexed query must tolerate an aligned final bin");
        assert_eq!(bins.len(), 1);
    }

    #[test]
    fn disabling_drops_pending_work_and_reenable_resumes_the_same_grid() {
        let mut cache = SummaryCache::new();
        cache.set_source(OnDiskSummarySource::from_path(Path::new("testdata/bam/tiny.bam")).expect("tiny BAM source"));
        let span = AreaSpan::for_viewport(
            crate::viewer_app::viewport::ViewPosition::new(BasePair(500), BasePair(1_000)),
            1,
            BasePair(10_000),
        );
        let key = GridKey::for_span("chr1", span);
        cache.ensure(key.clone(), None);
        let (send, _complete, cancellation) = attach_pending_scan(&mut cache, key.clone());
        send.send(SummaryBatch {
            start: key.start,
            bins: vec![SummaryBin::default(); FLUSH_STRIDE],
        })
        .unwrap();

        cache.tick(false);
        assert!(cancellation.is_cancelled());
        assert!(!cache.is_fetching());
        assert!(cache.generation.is_none());
        let next = key.bin_start(FLUSH_STRIDE);
        assert!(cache.covers(key.start, next));

        cache.ensure(key, None);
        assert_eq!(cache.pending.as_ref().unwrap().key.start, next);
    }
}
