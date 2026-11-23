//! Shared, nonblocking reference access for the genome tracks. Cache hits return
//! immediately; embedded and fallback bases fill locally. Indexed FASTA misses queue worker
//! requests and become visible after `poll`.

use std::io;
use std::sync::Arc;

use crate::basepair::BasePair;
use crate::jobs::{JobHandle, JobPoll};
#[cfg(feature = "resident-demo")]
use crate::refgenome::resident::ReferenceWindow;
use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};

use super::ring_buffer_cache::insert_mru;
use super::track::Contig;

const CACHE_CAPACITY: usize = 20;
const CHUNK_LENGTH: BasePair = BasePair(32 * 1024);
/// Stride for per-chunk base/CpG prefix sums. Keeps prefix tables ~12 KB per 32 KB chunk
/// while capping per-column edge scanning to at most two partial blocks.
const PREFIX_BLOCK: usize = 64;
/// The local-area band's share of the cache. The rest stays free for the detail tracks, so a wide
/// band cannot evict their chunks every frame; a span needing more draws no band at all.
const REFERENCE_BAND_CHUNK_BUDGET: usize = CACHE_CAPACITY - 2;
const BASES: [u8; 4] = *b"ACGT";

/// Base counts for one fully resident local-area column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct BaseHistogram {
    pub acgt: [u32; 4],
    pub invalid: u32,
    /// Counts each CpG in the interval containing its cytosine.
    pub cpg: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReferenceCounts {
    Bases,
    BasesAndCpg,
}

impl ReferenceCounts {
    fn fetch_end(self, end: BasePair, contig_length: BasePair) -> BasePair {
        match self {
            Self::Bases => end,
            Self::BasesAndCpg => (end + BasePair(1)).min(contig_length),
        }
    }
}

/// Availability of every reference base contributing to one local-area column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColumnBases {
    Complete(BaseHistogram),
    Partial,
    Failed,
    /// CpG counts were requested without a loaded FASTA (bundled placeholder sequence only).
    NoReference,
    OutOfContig,
}

/// Placeholder bases for a fallback genome: a SplitMix64 hash of the position, so a base never depends
/// on which chunk fetched it. Seeding a `StdRng` per base cost ~0.3s per chunk in debug builds,
/// which every genome draw test paid.
fn mock_fetch(contig: &str, start: BasePair, len: BasePair) -> Vec<u8> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contig.hash(&mut hasher);
    let salt = hasher.finish();
    (0..len.as_u64())
        .map(|offset| {
            let mut z = salt ^ (start.as_u64() + offset).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            BASES[((z ^ (z >> 31)) % BASES.len() as u64) as usize]
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct PrefixCounts {
    acgt: [u32; 4],
    invalid: u32,
    /// Cumulative count of `CG` dinucleotides starting at indices `< offset` whose `G` also lies
    /// inside this chunk; the final index `bases.len() - 1` is resolved at the chunk seam.
    cpg: u32,
}

struct Chunk {
    contig: String,
    start: BasePair,
    bases: Arc<[u8]>,
    prefix: Box<[PrefixCounts]>,
}

impl Chunk {
    fn new(contig: String, start: BasePair, bases: Vec<u8>) -> Self {
        let prefix = build_prefix(&bases);
        Self {
            contig,
            start,
            bases: Arc::from(bases),
            prefix,
        }
    }

    fn accumulate(&self, histogram: &mut BaseHistogram, first: usize, counted: usize, counts: ReferenceCounts) {
        let end = first + counted;
        if first >= end {
            return;
        }
        let first_block = first.div_ceil(PREFIX_BLOCK);
        let last_block = end / PREFIX_BLOCK;
        if first_block < last_block {
            scan_range(&self.bases, first, first_block * PREFIX_BLOCK, counts, histogram);
            let lo = self.prefix[first_block];
            let hi = self.prefix[last_block];
            for i in 0..4 {
                histogram.acgt[i] += hi.acgt[i] - lo.acgt[i];
            }
            histogram.invalid += hi.invalid - lo.invalid;
            if counts == ReferenceCounts::BasesAndCpg {
                histogram.cpg += hi.cpg - lo.cpg;
            }
            scan_range(&self.bases, last_block * PREFIX_BLOCK, end, counts, histogram);
        } else {
            scan_range(&self.bases, first, end, counts, histogram);
        }
    }
}

enum ChunkEntry {
    Ready(Chunk),
    /// A settled failure in an ordinary MRU slot, for the reason `ReadWindowCache::insert_failure`
    /// gives: no per-frame retry, and eviction later allows one.
    Failed {
        contig: String,
        start: BasePair,
    },
}

impl ChunkEntry {
    fn matches(&self, contig: &str, start: BasePair) -> bool {
        match self {
            Self::Ready(chunk) => chunk.contig == contig && chunk.start == start,
            Self::Failed {
                contig: failed_contig,
                start: failed_start,
            } => failed_contig == contig && *failed_start == start,
        }
    }
}

struct PendingChunk {
    key: (String, BasePair),
    generation: u64,
    handle: JobHandle<io::Result<Vec<u8>>>,
}

/// UI-side cache shared by the reference, alignment and local-area renderers, so none keeps a copy.
pub(super) struct ReferenceData {
    worker: Option<ReferenceWorkerHandle>,
    #[cfg(feature = "resident-demo")]
    resident: Vec<ReferenceWindow>,
    generation: u64,
    fallback: bool,
    chunks: Vec<ChunkEntry>,
    pending: Vec<PendingChunk>,
    open: Option<(u64, JobHandle<io::Result<()>>)>,
    error: Option<String>,
}

impl ReferenceData {
    pub(super) fn new(genome: &IndexedFasta, worker: ReferenceWorkerHandle) -> Self {
        let open = genome.path.as_ref().map(|path| (0, worker.open(0, path.clone())));
        let mut data = Self::with_worker(Some(worker), genome.path.is_none());
        data.open = open;
        data
    }

    fn with_worker(worker: Option<ReferenceWorkerHandle>, fallback: bool) -> Self {
        Self {
            worker,
            #[cfg(feature = "resident-demo")]
            resident: Vec::new(),
            generation: 0,
            fallback,
            chunks: Vec::with_capacity(CACHE_CAPACITY),
            pending: Vec::new(),
            open: None,
            error: None,
        }
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn resident(windows: Vec<ReferenceWindow>) -> Self {
        let mut data = Self::with_worker(None, false);
        data.resident = windows;
        data
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn resident_span(&self, contig: &str, pos: BasePair) -> Option<std::ops::Range<BasePair>> {
        self.resident
            .iter()
            .find(|window| window.contig == contig && window.span.contains(&pos))
            .map(|window| window.span.clone())
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn first_resident_span(&self, contig: &str) -> Option<std::ops::Range<BasePair>> {
        self.resident
            .iter()
            .find(|window| window.contig == contig)
            .map(|window| window.span.clone())
    }

    pub(super) fn replace(&mut self, genome: &IndexedFasta) {
        self.generation = self.generation.wrapping_add(1);
        self.fallback = genome.path.is_none();
        self.chunks.clear();
        self.pending.clear();
        self.error = None;
        self.open = genome
            .path
            .as_ref()
            .zip(self.worker.as_ref())
            .map(|(path, worker)| (self.generation, worker.open(self.generation, path.clone())));
    }

    pub(super) fn poll(&mut self) {
        if let Some((generation, handle)) = &mut self.open {
            match handle.try_recv() {
                JobPoll::Empty => {}
                JobPoll::Stale => self.open = None,
                JobPoll::Ready(Ok(())) => self.open = None,
                JobPoll::Ready(Err(error)) => {
                    if *generation == self.generation {
                        self.error = Some(format!("Failed to open reference sequence: {error}"));
                    }
                    self.open = None;
                }
                JobPoll::Disconnected => {
                    if *generation == self.generation {
                        self.error = Some("Reference worker disconnected".to_string());
                    }
                    self.open = None;
                }
            }
        }

        let generation = self.generation;
        let mut finished = Vec::new();
        let mut disconnected = Vec::new();
        self.pending.retain_mut(|pending| match pending.handle.try_recv() {
            JobPoll::Empty => true,
            JobPoll::Stale => false,
            JobPoll::Ready(result) => {
                if pending.generation == generation {
                    finished.push((pending.key.clone(), result));
                }
                false
            }
            JobPoll::Disconnected => {
                if pending.generation == generation {
                    disconnected.push(pending.key.clone());
                }
                false
            }
        });

        for (contig, start) in disconnected {
            self.settle_failure(contig, start, "Reference worker disconnected".to_string());
        }

        for ((contig, start), result) in finished {
            match result {
                Ok(bases) if !bases.is_empty() => self.insert(contig, start, bases),
                Ok(_) => self.settle_failure(contig, start, "reference worker returned no bases".to_string()),
                Err(error) if error.kind() != io::ErrorKind::Interrupted => {
                    self.settle_failure(contig, start, format!("Failed to fetch reference sequence: {error}"));
                }
                Err(_) => {}
            }
        }
    }

    pub(super) fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.open.is_some() || !self.pending.is_empty()
    }

    pub(super) fn has_real_reference(&self) -> bool {
        !self.fallback
    }

    /// Whether a span leaves two cache slots for the detail tracks.
    pub(super) fn reference_band_fits(
        &self,
        start: BasePair,
        end: BasePair,
        contig_length: BasePair,
        counts: ReferenceCounts,
    ) -> bool {
        chunk_count(start, counts.fetch_end(end, contig_length)) <= REFERENCE_BAND_CHUNK_BUDGET
    }

    /// Counts a fully resident range without materialising its sequence.
    ///
    /// A real miss queues its aligned chunk and remains partial until `poll`.
    /// No mode is computed from only the chunks that happened to arrive first.
    pub(super) fn column_bases(
        &mut self,
        contig: &str,
        start: BasePair,
        end: BasePair,
        contig_length: BasePair,
        counts: ReferenceCounts,
    ) -> ColumnBases {
        if start >= end || start >= contig_length || end > contig_length {
            return ColumnBases::OutOfContig;
        }
        if counts == ReferenceCounts::BasesAndCpg && self.fallback {
            return ColumnBases::NoReference;
        }
        #[cfg(feature = "resident-demo")]
        let contig_length = if self.resident.is_empty() {
            contig_length
        } else if let Some(span) = self.resident_span(contig, start).filter(|span| end <= span.end) {
            span.end
        } else {
            return ColumnBases::OutOfContig;
        };

        let fetch_end = counts.fetch_end(end, contig_length);
        let chunk_starts = chunk_starts(start, fetch_end);
        if chunk_starts.clone().any(|chunk_start| {
            self.chunks
                .iter()
                .any(|entry| matches!(entry, ChunkEntry::Failed { .. }) && entry.matches(contig, chunk_start))
        }) {
            return ColumnBases::Failed;
        }

        for chunk_start in chunk_starts.clone() {
            if self.chunks.iter().any(|entry| entry.matches(contig, chunk_start)) {
                continue;
            }
            if self.fallback {
                let len = (contig_length - chunk_start).min(CHUNK_LENGTH);
                self.insert(contig.to_string(), chunk_start, mock_fetch(contig, chunk_start, len));
            } else {
                self.request_chunk(contig, chunk_start, contig_length);
            }
        }

        // Outstanding work covers both the requests just queued and any left by an
        // earlier draw, so this one scan is the whole incompleteness test.
        if chunk_starts.clone().any(|chunk_start| {
            self.pending
                .iter()
                .any(|pending| pending.key.0 == contig && pending.key.1 == chunk_start)
        }) {
            return ColumnBases::Partial;
        }

        let mut histogram = BaseHistogram::default();
        let mut previous_base = None;
        for chunk_start in chunk_starts {
            let Some(index) = self.chunks.iter().position(|entry| entry.matches(contig, chunk_start)) else {
                return ColumnBases::Partial;
            };
            if index > 0 {
                self.chunks[..=index].rotate_right(1);
            }
            let ChunkEntry::Ready(chunk) = &self.chunks[0] else {
                return ColumnBases::Failed;
            };
            let range_start = start.max(chunk.start);
            let range_end = fetch_end.min(chunk.start + BasePair::from(chunk.bases.len() as u64));
            let expected_end = fetch_end.min(chunk.start + CHUNK_LENGTH);
            if range_start >= range_end || range_end != expected_end {
                return ColumnBases::Failed;
            }
            let first = (range_start - chunk.start).as_u64() as usize;
            let past_last = (range_end - chunk.start).as_u64() as usize;
            let counted = (end.min(range_end) - range_start).as_u64() as usize;
            if counts == ReferenceCounts::BasesAndCpg
                && previous_base == Some(b'C')
                && chunk.bases[first].eq_ignore_ascii_case(&b'G')
            {
                histogram.cpg += 1;
            }
            chunk.accumulate(&mut histogram, first, counted, counts);
            // Prefix/scan counts inspect `bases[i + 1]` within the chunk; if `end == contig_length`
            // ends inside an oversized chunk (e.g. fallback or seeded test chunk), drop the trailing
            // pair whose `G` lies past `contig_length`.
            if counts == ReferenceCounts::BasesAndCpg
                && counted > 0
                && first + counted == past_last
                && past_last < chunk.bases.len()
                && chunk.bases[past_last - 1].eq_ignore_ascii_case(&b'C')
                && chunk.bases[past_last].eq_ignore_ascii_case(&b'G')
            {
                histogram.cpg -= 1;
            }
            previous_base = (counts == ReferenceCounts::BasesAndCpg && counted > 0)
                .then(|| chunk.bases[first + counted - 1].to_ascii_uppercase());
        }
        ColumnBases::Complete(histogram)
    }

    /// Returns a resident base, fills fallback data locally, or queues a real
    /// fetch; a real miss remains unavailable until a later `poll`.
    pub(super) fn base_at(&mut self, contig: &str, pos: BasePair, contig_length: BasePair) -> Option<u8> {
        if pos >= contig_length {
            return None;
        }
        #[cfg(feature = "resident-demo")]
        if !self.resident.is_empty() && self.resident_span(contig, pos).is_none() {
            return None;
        }
        if let Some(base) = self.lookup(contig, pos) {
            return Some(base);
        }

        let start = self.chunk_start_for(pos);
        if self.fallback {
            self.insert(contig.to_string(), start, mock_fetch(contig, start, CHUNK_LENGTH));
            return self.lookup(contig, pos);
        }
        if self.chunks.iter().any(|chunk| chunk.matches(contig, start)) {
            return None;
        }
        self.request_chunk(contig, start, contig_length);
        self.lookup(contig, pos)
    }

    fn lookup(&mut self, contig: &str, pos: BasePair) -> Option<u8> {
        let index = self.chunks.iter().position(|entry| match entry {
            ChunkEntry::Ready(chunk) => {
                chunk.contig == contig
                    && pos >= chunk.start
                    && pos < chunk.start + BasePair::from(chunk.bases.len() as u64)
            }
            ChunkEntry::Failed { .. } => false,
        })?;
        if index > 0 {
            self.chunks[..=index].rotate_right(1);
        }
        let ChunkEntry::Ready(chunk) = &self.chunks[0] else {
            unreachable!("lookup only selects ready reference chunks");
        };
        Some(chunk.bases[(pos - chunk.start).as_u64() as usize])
    }

    fn chunk_start_for(&self, pos: BasePair) -> BasePair {
        BasePair((pos.as_u64() / CHUNK_LENGTH.as_u64()) * CHUNK_LENGTH.as_u64())
    }

    fn insert(&mut self, contig: String, start: BasePair, bases: Vec<u8>) {
        self.chunks.retain(|entry| !entry.matches(&contig, start));
        insert_mru(
            &mut self.chunks,
            CACHE_CAPACITY,
            ChunkEntry::Ready(Chunk::new(contig, start, bases)),
        );
    }

    fn settle_failure(&mut self, contig: String, start: BasePair, error: String) {
        self.chunks.retain(|entry| !entry.matches(&contig, start));
        insert_mru(&mut self.chunks, CACHE_CAPACITY, ChunkEntry::Failed { contig, start });
        self.error = Some(error);
    }

    fn request_chunk(&mut self, contig: &str, start: BasePair, contig_length: BasePair) {
        if self.chunks.iter().any(|entry| entry.matches(contig, start))
            || self
                .pending
                .iter()
                .any(|pending| pending.key.0 == contig && pending.key.1 == start)
        {
            return;
        }
        let Some(worker) = &self.worker else {
            #[cfg(feature = "resident-demo")]
            self.fill_resident_chunk(contig, start);
            return;
        };
        let key = (contig.to_string(), start);
        let len = (contig_length - start).min(CHUNK_LENGTH);
        let handle = worker.fetch(self.generation, key.0.clone(), start, len);
        self.pending.push(PendingChunk {
            key,
            generation: self.generation,
            handle,
        });
    }

    #[cfg(feature = "resident-demo")]
    fn fill_resident_chunk(&mut self, contig: &str, start: BasePair) {
        // Preserve the cache's aligned keys. Lookups reject every position outside a slice
        // before consulting the chunk, including padding at either end.
        let mut bases = vec![0; CHUNK_LENGTH.as_u64() as usize];
        let end = start + CHUNK_LENGTH;
        for window in self.resident.iter().filter(|window| window.contig == contig) {
            let first = start.max(window.span.start);
            let last = end.min(window.span.end);
            if first < last {
                let chunk_range = (first - start).as_u64() as usize..(last - start).as_u64() as usize;
                let window_range =
                    (first - window.span.start).as_u64() as usize..(last - window.span.start).as_u64() as usize;
                bases[chunk_range].copy_from_slice(&window.bases[window_range]);
            }
        }
        self.insert(contig.to_string(), start, bases);
    }

    #[cfg(test)]
    pub(super) fn cached_base(&mut self, contig: &str, pos: BasePair) -> Option<u8> {
        self.lookup(contig, pos)
    }

    #[cfg(test)]
    pub(super) fn seed_chunk(&mut self, contig: &str, start: BasePair, bases: &[u8]) {
        self.insert(contig.to_string(), start, bases.to_vec());
    }

    #[cfg(test)]
    pub(super) fn pending_chunk_count(&self) -> usize {
        self.pending.len()
    }
}

/// The cache bound to the contig being drawn.
///
/// Every reference read needs a name and a length together — the name selects the chunk, the length
/// bounds it — and they are always the two halves of the [`Contig`] the shell already pushes into
/// each track. Passing that pair as loose arguments meant a renderer received a `&Contig`, split it,
/// and handed the halves down through several signatures; bundling here keeps the type whole for the
/// whole call chain.
///
/// Borrowed rather than owning: it lives for one draw, over state the viewer owns.
pub(super) struct ReferenceLookup<'a> {
    data: &'a mut ReferenceData,
    contig: &'a Contig,
}

impl<'a> ReferenceLookup<'a> {
    pub(super) fn new(data: &'a mut ReferenceData, contig: &'a Contig) -> Self {
        Self { data, contig }
    }

    /// Reborrows for a nested call, so one draw can pass the bundle down without moving it.
    pub(super) fn reborrow(&mut self) -> ReferenceLookup<'_> {
        ReferenceLookup {
            data: self.data,
            contig: self.contig,
        }
    }

    pub(super) fn base_at(&mut self, pos: BasePair) -> Option<u8> {
        self.data.base_at(&self.contig.name, pos, self.contig.length)
    }

    pub(super) fn contig_length(&self) -> BasePair {
        self.contig.length
    }

    #[cfg(feature = "resident-demo")]
    pub(super) fn resident_span(&self, pos: BasePair) -> Option<std::ops::Range<BasePair>> {
        self.data.resident_span(&self.contig.name, pos)
    }

    pub(super) fn column_bases(&mut self, start: BasePair, end: BasePair, counts: ReferenceCounts) -> ColumnBases {
        self.data
            .column_bases(&self.contig.name, start, end, self.contig.length, counts)
    }

    pub(super) fn has_real_reference(&self) -> bool {
        self.data.has_real_reference()
    }

    pub(super) fn reference_band_fits(&self, start: BasePair, end: BasePair, counts: ReferenceCounts) -> bool {
        self.data.reference_band_fits(start, end, self.contig.length, counts)
    }
}

fn chunk_starts(start: BasePair, end: BasePair) -> impl Clone + Iterator<Item = BasePair> {
    let first = (start.as_u64() / CHUNK_LENGTH.as_u64()) * CHUNK_LENGTH.as_u64();
    (0..chunk_count(start, end)).map(move |index| BasePair(first + index as u64 * CHUNK_LENGTH.as_u64()))
}

fn chunk_count(start: BasePair, end: BasePair) -> usize {
    if start >= end {
        return 0;
    }
    let first = start.as_u64() / CHUNK_LENGTH.as_u64();
    let last = (end.as_u64() - 1) / CHUNK_LENGTH.as_u64();
    usize::try_from(last - first + 1).unwrap_or(usize::MAX)
}

fn build_prefix(bases: &[u8]) -> Box<[PrefixCounts]> {
    let block_count = bases.len() / PREFIX_BLOCK;
    let mut prefix = Vec::with_capacity(block_count + 1);
    let mut acc = PrefixCounts::default();
    prefix.push(acc);
    for block in 0..block_count {
        let start = block * PREFIX_BLOCK;
        let end = start + PREFIX_BLOCK;
        for i in start..end {
            let base = bases[i].to_ascii_uppercase();
            match base {
                b'A' => acc.acgt[0] += 1,
                b'C' => {
                    acc.acgt[1] += 1;
                    if i + 1 < bases.len() && bases[i + 1].eq_ignore_ascii_case(&b'G') {
                        acc.cpg += 1;
                    }
                }
                b'G' => acc.acgt[2] += 1,
                b'T' => acc.acgt[3] += 1,
                _ => acc.invalid += 1,
            }
        }
        prefix.push(acc);
    }
    prefix.into_boxed_slice()
}

fn scan_range(bases: &[u8], start: usize, end: usize, counts: ReferenceCounts, histogram: &mut BaseHistogram) {
    let want_cpg = counts == ReferenceCounts::BasesAndCpg;
    for i in start..end {
        match bases[i].to_ascii_uppercase() {
            b'A' => histogram.acgt[0] += 1,
            b'C' => {
                histogram.acgt[1] += 1;
                if want_cpg && i + 1 < bases.len() && bases[i + 1].eq_ignore_ascii_case(&b'G') {
                    histogram.cpg += 1;
                }
            }
            b'G' => histogram.acgt[2] += 1,
            b'T' => histogram.acgt[3] += 1,
            _ => histogram.invalid += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_reference_generates_and_reuses_a_chunk() {
        let genome = IndexedFasta::hs1_fallback();
        let worker = ReferenceWorkerHandle::spawn();
        let mut data = ReferenceData::new(&genome, worker);
        let first = data.base_at("chr1", BasePair(10), BasePair(100)).unwrap();
        assert_eq!(data.cached_base("chr1", BasePair(10)), Some(first));
        assert!(!data.is_fetching());
    }

    #[test]
    fn base_at_returns_none_at_and_after_the_contig_end_before_fetching() {
        let genome = IndexedFasta::hs1_fallback();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());

        for pos in [BasePair(100), BasePair(101)] {
            assert_eq!(data.base_at("chr1", pos, BasePair(100)), None);
            assert_eq!(data.cached_base("chr1", pos), None);
        }
        assert!(!data.is_fetching());
    }

    #[test]
    fn fallback_fetch_differs_across_contigs_at_the_same_position() {
        let a = mock_fetch("chr1", BasePair(0), BasePair(50));
        let b = mock_fetch("chr2", BasePair(0), BasePair(50));
        assert_ne!(a, b);
    }

    #[test]
    fn fallback_fetch_only_uses_valid_bases() {
        let bases = mock_fetch("chr1", BasePair(0), BasePair(200));
        assert!(bases.iter().all(|base| matches!(base, b'A' | b'C' | b'G' | b'T')));
    }

    #[test]
    fn fallback_fetch_tiles_seamlessly_across_adjacent_calls() {
        let combined = mock_fetch("chr1", BasePair(500), BasePair(40));
        let first = mock_fetch("chr1", BasePair(500), BasePair(15));
        let second = mock_fetch("chr1", BasePair(515), BasePair(25));
        assert_eq!(combined[..15], first[..]);
        assert_eq!(combined[15..], second[..]);
    }

    #[test]
    fn real_reference_does_not_block_on_a_cache_miss() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let worker = ReferenceWorkerHandle::spawn();
        let mut data = ReferenceData::new(&genome, worker);
        assert_eq!(data.base_at("chrM", BasePair(10), BasePair(16_569)), None);
        assert!(data.is_fetching());
    }

    #[test]
    fn disconnected_fetch_surfaces_an_error_for_the_current_generation() {
        let genome = IndexedFasta::hs1_fallback();
        let worker = ReferenceWorkerHandle::spawn();
        let mut data = ReferenceData::new(&genome, worker);
        let (reply, handle) = JobHandle::<io::Result<Vec<u8>>>::channel();
        drop(reply);
        data.pending.push(PendingChunk {
            key: ("chr1".to_string(), BasePair(0)),
            generation: data.generation,
            handle,
        });

        data.poll();

        assert_eq!(data.take_error().as_deref(), Some("Reference worker disconnected"));
        assert!(!data.is_fetching());
    }

    #[test]
    fn failed_real_chunk_is_not_requested_again_until_replaced() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let worker = ReferenceWorkerHandle::spawn();
        let mut data = ReferenceData::new(&genome, worker);
        data.settle_failure(
            "chrM".to_string(),
            BasePair(0),
            "reference worker returned no bases".to_string(),
        );

        assert_eq!(data.base_at("chrM", BasePair(10), BasePair(16_569)), None);
        assert!(data.pending.is_empty());
        assert_eq!(data.take_error().as_deref(), Some("reference worker returned no bases"));

        data.replace(&IndexedFasta::hs1_fallback());
        assert!(data.base_at("chrM", BasePair(10), BasePair(16_569)).is_some());
    }

    #[test]
    fn an_empty_reference_reply_settles_the_chunk_as_a_failure() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let worker = ReferenceWorkerHandle::spawn();
        let mut data = ReferenceData::new(&genome, worker);
        let (reply, handle) = JobHandle::<io::Result<Vec<u8>>>::channel();
        reply.send(Ok(Vec::new())).unwrap();
        data.pending.push(PendingChunk {
            key: ("chrM".to_string(), BasePair(0)),
            generation: data.generation,
            handle,
        });

        data.poll();

        assert_eq!(data.take_error().as_deref(), Some("reference worker returned no bases"));
        assert_eq!(data.base_at("chrM", BasePair(10), BasePair(16_569)), None);
        assert!(data.pending.is_empty());
    }

    #[test]
    fn column_histogram_counts_acgt_case_insensitively_and_keeps_invalid_separate() {
        let genome = IndexedFasta::hs1_fallback();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        data.seed_chunk("chr1", BasePair(0), b"AaCcGgTtNRY?");

        assert_eq!(
            data.column_bases("chr1", BasePair(0), BasePair(12), BasePair(12), ReferenceCounts::Bases),
            ColumnBases::Complete(BaseHistogram {
                acgt: [2, 2, 2, 2],
                invalid: 4,
                cpg: 0,
            })
        );
    }

    #[test]
    fn column_histogram_folds_across_chunk_boundaries() {
        let genome = IndexedFasta::hs1_fallback();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut first = vec![b'N'; CHUNK_LENGTH.as_u64() as usize];
        let last_two = first.len() - 2;
        first[last_two..].copy_from_slice(b"AC");
        data.seed_chunk("chr1", BasePair(0), &first);
        data.seed_chunk("chr1", CHUNK_LENGTH, b"GGTT");

        assert_eq!(
            data.column_bases(
                "chr1",
                CHUNK_LENGTH - BasePair(2),
                CHUNK_LENGTH + BasePair(4),
                CHUNK_LENGTH + BasePair(4),
                ReferenceCounts::Bases,
            ),
            ColumnBases::Complete(BaseHistogram {
                acgt: [1, 1, 2, 2],
                invalid: 0,
                cpg: 0,
            })
        );
    }

    #[test]
    fn cpg_counts_partition_by_cytosine_across_chunk_and_column_boundaries() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut first = vec![b'N'; CHUNK_LENGTH.as_u64() as usize];
        let tail = first.len() - 3;
        first[tail..].copy_from_slice(b"cGc");
        data.seed_chunk("chr1", BasePair(0), &first);
        data.seed_chunk("chr1", CHUNK_LENGTH, b"gTcG");
        let start = CHUNK_LENGTH - BasePair(3);
        let end = CHUNK_LENGTH + BasePair(4);

        for (offset, (acgt, cpg)) in [
            ([0, 1, 0, 0], 1),
            ([0, 0, 1, 0], 0),
            ([0, 1, 0, 0], 1),
            ([0, 0, 1, 0], 0),
            ([0, 0, 0, 1], 0),
            ([0, 1, 0, 0], 1),
            ([0, 0, 1, 0], 0),
        ]
        .into_iter()
        .enumerate()
        {
            let pos = start + BasePair(offset as u64);
            assert_eq!(
                data.column_bases("chr1", pos, pos + BasePair(1), end, ReferenceCounts::BasesAndCpg),
                ColumnBases::Complete(BaseHistogram { acgt, invalid: 0, cpg }),
            );
        }
        assert_eq!(
            data.column_bases("chr1", start, end, end, ReferenceCounts::BasesAndCpg),
            ColumnBases::Complete(BaseHistogram {
                acgt: [0, 3, 3, 1],
                invalid: 0,
                cpg: 3,
            }),
        );
    }

    #[test]
    fn cpg_counts_require_a_real_reference_even_when_placeholder_bases_are_cached() {
        let genome = IndexedFasta::hs1_fallback();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let start = BasePair(0);
        let end = BasePair(10);
        assert!(!data.has_real_reference());
        assert!(matches!(
            data.column_bases("chr1", start, end, end, ReferenceCounts::Bases),
            ColumnBases::Complete(_)
        ));
        assert_eq!(
            data.column_bases("chr1", start, end, end, ReferenceCounts::BasesAndCpg),
            ColumnBases::NoReference
        );
    }

    #[test]
    fn real_column_misses_are_partial_and_requested_once() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());

        assert_eq!(
            data.column_bases(
                "chrM",
                BasePair(10),
                BasePair(20),
                BasePair(16_569),
                ReferenceCounts::Bases
            ),
            ColumnBases::Partial
        );
        assert_eq!(data.pending.len(), 1);
        assert_eq!(
            data.column_bases(
                "chrM",
                BasePair(10),
                BasePair(20),
                BasePair(16_569),
                ReferenceCounts::Bases
            ),
            ColumnBases::Partial
        );
        assert_eq!(data.pending.len(), 1, "a repeated draw must not duplicate the request");
    }

    /// The mode must not be computed from whichever chunks happened to arrive first:
    /// a band that averages half a span redraws as chunks land, which reads as data
    /// rather than as loading. This is the state a later draw sees, where nothing is
    /// newly requested and only the outstanding request marks the span incomplete.
    #[test]
    fn a_span_half_resident_and_half_outstanding_stays_partial() {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        // chr1 spans several chunks; chrM is shorter than one and would be out of contig.
        let contig_length = BasePair(248_400);
        let span_end = CHUNK_LENGTH + BasePair(4);

        // Seed the first chunk before any query, so it is resident with nothing
        // outstanding against it; the query then requests only the second.
        data.seed_chunk("chr1", BasePair(0), &vec![b'A'; CHUNK_LENGTH.as_u64() as usize]);
        assert_eq!(
            data.column_bases("chr1", BasePair(0), span_end, contig_length, ReferenceCounts::Bases),
            ColumnBases::Partial,
            "a resident chunk beside an outstanding one must not yield a mode"
        );
        assert_eq!(
            data.pending.iter().map(|pending| pending.key.1).collect::<Vec<_>>(),
            vec![CHUNK_LENGTH],
            "only the absent chunk may be requested"
        );

        // A later draw requests nothing new, so the pending scan alone must catch it.
        assert_eq!(
            data.column_bases("chr1", BasePair(0), span_end, contig_length, ReferenceCounts::Bases),
            ColumnBases::Partial,
            "an outstanding request from an earlier draw must still read as loading"
        );
        assert_eq!(data.pending.len(), 1, "the resident chunk must not be re-requested");
    }

    #[test]
    fn failed_and_out_of_contig_columns_stay_distinct() {
        let genome = IndexedFasta::hs1_fallback();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        data.settle_failure("chr1".to_string(), BasePair(0), "failed".to_string());

        assert_eq!(
            data.column_bases("chr1", BasePair(0), BasePair(10), BasePair(100), ReferenceCounts::Bases),
            ColumnBases::Failed
        );
        assert_eq!(
            data.column_bases(
                "chr1",
                BasePair(90),
                BasePair(101),
                BasePair(100),
                ReferenceCounts::Bases
            ),
            ColumnBases::OutOfContig
        );
        assert_eq!(
            data.column_bases(
                "chr1",
                BasePair(10),
                BasePair(10),
                BasePair(100),
                ReferenceCounts::Bases
            ),
            ColumnBases::OutOfContig
        );
    }

    #[test]
    fn reference_band_reserves_two_cache_chunks_at_aligned_and_unaligned_edges() {
        let genome = IndexedFasta::hs1_fallback();
        let data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let budget_end = CHUNK_LENGTH * BasePair(REFERENCE_BAND_CHUNK_BUDGET as u64);

        let longer_contig = budget_end + BasePair(1);

        for start in [BasePair(0), BasePair(1)] {
            assert!(data.reference_band_fits(start, budget_end, longer_contig, ReferenceCounts::Bases));
            assert!(!data.reference_band_fits(start, longer_contig, longer_contig, ReferenceCounts::Bases));
            assert!(data.reference_band_fits(start, budget_end, budget_end, ReferenceCounts::BasesAndCpg));
            assert!(!data.reference_band_fits(start, budget_end, longer_contig, ReferenceCounts::BasesAndCpg));
        }
    }

    #[test]
    fn prefix_backed_column_bases_matches_direct_scan_across_block_and_chunk_seams() {
        let alphabet = b"AcGtCgNcg";
        let chunk_len = CHUNK_LENGTH.as_u64() as usize;
        let full: Vec<u8> = (0..chunk_len * 2)
            .map(|i| alphabet[(i.wrapping_mul(7) + i / 3) % alphabet.len()])
            .collect();
        // Force CpGs directly across block boundaries and the chunk seam.
        let mut seq = full;
        for seam in [PREFIX_BLOCK - 1, PREFIX_BLOCK * 2 - 1, chunk_len - 1, chunk_len + 63] {
            seq[seam] = b'c';
            seq[seam + 1] = b'G';
        }

        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        let mut data = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        data.seed_chunk("chr1", BasePair(0), &seq[..chunk_len]);
        data.seed_chunk("chr1", CHUNK_LENGTH, &seq[chunk_len..]);

        let oracle = |start: usize, end: usize, contig_len: usize, counts: ReferenceCounts| -> BaseHistogram {
            let mut h = BaseHistogram::default();
            for &b in &seq[start..end] {
                match b.to_ascii_uppercase() {
                    b'A' => h.acgt[0] += 1,
                    b'C' => h.acgt[1] += 1,
                    b'G' => h.acgt[2] += 1,
                    b'T' => h.acgt[3] += 1,
                    _ => h.invalid += 1,
                }
            }
            if counts == ReferenceCounts::BasesAndCpg {
                let lookahead_end = (end + 1).min(contig_len);
                h.cpg = seq[start..lookahead_end]
                    .windows(2)
                    .filter(|pair| pair.eq_ignore_ascii_case(b"CG"))
                    .count() as u32;
            }
            h
        };

        let cases = [
            (0, 10, seq.len()),
            (PREFIX_BLOCK - 1, PREFIX_BLOCK + 1, seq.len()),
            (3, PREFIX_BLOCK * 3 + 17, seq.len()),
            (chunk_len - 130, chunk_len, seq.len()),
            (chunk_len - 130, chunk_len + 130, seq.len()),
            (chunk_len - 1, chunk_len + 1, seq.len()),
            (0, PREFIX_BLOCK, PREFIX_BLOCK),
            (chunk_len - 10, chunk_len, chunk_len),
        ];
        for (start, end, contig_len) in cases {
            for counts in [ReferenceCounts::Bases, ReferenceCounts::BasesAndCpg] {
                assert_eq!(
                    data.column_bases(
                        "chr1",
                        BasePair(start as u64),
                        BasePair(end as u64),
                        BasePair(contig_len as u64),
                        counts,
                    ),
                    ColumnBases::Complete(oracle(start, end, contig_len, counts)),
                    "range {start}..{end} (contig_len={contig_len})"
                );
            }
        }
    }
}
