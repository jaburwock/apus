//! The resident row window: a lazy sliding pair of chunks.
//!
//! Rows are read through [`LineIndex`] a chunk at a time and only two chunks stay
//! resident, so memory is bounded by the chunk size rather than the file's record
//! count. The pair slides: the chunk holding the viewport's top edge is always
//! held, and its neighbour is fetched once the top edge comes within
//! `PREFETCH_MARGIN` rows of the boundary — before the viewport reaches it, so
//! an ordinary crossing never shows the loading line.
//!
//! Eviction is by **distance from the top edge**, not by age. That is what makes
//! the rule symmetric: scrolling back up recycles the buffer that scrolling down
//! had just filled, with no memory of which direction the user came from.

use crate::Instant;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{Batch, Generation, JobHandle, JobPoll};

use super::ingest::VcfRow;
use super::line_index::{INDEX_STRIDE, LineIndex};

/// Rows per chunk.
///
/// Defined as [`INDEX_STRIDE`] so a chunk always begins exactly on a checkpoint:
/// the fetch seeks straight there and reads forward zero records, making the
/// worst-case read the best case. Any other size reintroduces the skip.
pub const CHUNK_ROWS: usize = INDEX_STRIDE;

/// How close the top edge must come to a boundary before the neighbour is
/// fetched.
///
/// Runway rather than a cache-size knob: no key moves the table more than
/// `PAGE_STEP` rows, so the viewport cannot cross this margin in one action and
/// the neighbour has always landed by the time it is reached.
const PREFETCH_MARGIN: usize = 1_000;

/// How many chunks stay resident. The whole cache policy.
const RESIDENT_CHUNKS: usize = 2;

/// One buffer of the pair.
///
/// `Stale` is a state rather than an empty `Vec` because the two are genuinely
/// different: a stale slot holds nothing and may be recycled freely, while a
/// resident slot with no rows is a chunk that really is empty at end of file and
/// must not be refetched forever.
enum Slot {
    Stale,
    Loading {
        chunk: usize,
        handle: JobHandle<io::Result<Vec<VcfRow>>>,
        started_at: Instant,
        /// Rows already in hand for this chunk, kept on screen while the fetch
        /// runs. Non-empty only when topping a seeded partial chunk up: dropping
        /// them would blank the very rows the seed exists to preserve.
        partial: Vec<VcfRow>,
    },
    Resident {
        chunk: usize,
        rows: Vec<VcfRow>,
        /// False for rows seeded from the preview, which cover only part of the
        /// chunk. A fetched chunk is complete by definition — however few rows it
        /// returned, that is what the file holds there — so only a seeded slot is
        /// ever topped up, and no chunk can be refetched forever.
        complete: bool,
    },
}

impl Slot {
    /// The chunk this slot is holding or fetching, if any.
    fn chunk(&self) -> Option<usize> {
        match self {
            Self::Stale => None,
            Self::Loading { chunk, .. } | Self::Resident { chunk, .. } => Some(*chunk),
        }
    }

    /// The rows this slot can serve now, whether settled or still filling out.
    fn rows(&self) -> &[VcfRow] {
        match self {
            Self::Stale => &[],
            Self::Loading { partial, .. } => partial,
            Self::Resident { rows, .. } => rows,
        }
    }

    /// The mutable counterpart of [`Self::rows`], for test-only row injection.
    #[cfg(test)]
    fn rows_mut(&mut self) -> &mut [VcfRow] {
        match self {
            Self::Stale => &mut [],
            Self::Loading { partial, .. } => partial,
            Self::Resident { rows, .. } => rows,
        }
    }
}

/// Rows for the visible span, fetched a chunk at a time.
pub struct ChunkBuffer {
    slots: [Slot; RESIDENT_CHUNKS],
    /// One generation covers all chunks requested by one viewport advance.
    batch: Batch,
    /// Always [`CHUNK_ROWS`] outside tests, which shrink it so a small fixture
    /// still crosses real boundaries.
    chunk_rows: usize,
    /// Always [`PREFETCH_MARGIN`] outside tests. Scaled with `chunk_rows` there,
    /// since a margin wider than a chunk would make every position "near" a
    /// boundary and erase the mid-chunk case entirely.
    margin: usize,
}

impl Default for ChunkBuffer {
    fn default() -> Self {
        Self {
            slots: [Slot::Stale, Slot::Stale],
            batch: Batch::new(),
            chunk_rows: CHUNK_ROWS,
            margin: PREFETCH_MARGIN,
        }
    }
}

impl ChunkBuffer {
    /// A buffer with a smaller chunk, so a small fixture crosses real boundaries.
    /// The margin keeps its production share of the chunk, so the proximity rule
    /// under test is the one that ships.
    #[cfg(test)]
    pub(super) fn with_chunk_rows(chunk_rows: usize) -> Self {
        Self {
            chunk_rows,
            margin: chunk_rows * PREFETCH_MARGIN / CHUNK_ROWS,
            ..Self::default()
        }
    }

    /// Which chunk holds an absolute row number.
    fn chunk_of(&self, row: usize) -> usize {
        row / self.chunk_rows
    }

    /// The row, if its chunk is resident.
    ///
    /// `None` covers "not fetched", "in flight", and "past the end of a partially
    /// filled chunk" alike — the caller draws all three as the loading line and
    /// asks [`Self::is_fetching`] only when it must tell them apart.
    pub fn row(&self, absolute: usize) -> Option<&VcfRow> {
        let wanted = self.chunk_of(absolute);
        self.slots
            .iter()
            .filter(|slot| slot.chunk() == Some(wanted))
            .find_map(|slot| slot.rows().get(absolute - wanted * self.chunk_rows))
    }

    /// Replaces a resident row's contig, so a test can inject a value the fixture cannot hold.
    ///
    /// Test-only: nothing in the app rewrites ingested data, and the shapes that matter for command
    /// generation (a contig carrying the batch separator) cannot be spelled in a valid VCF.
    #[cfg(test)]
    pub(crate) fn overwrite_contig_for_test(&mut self, absolute: usize, contig: String) {
        let wanted = self.chunk_of(absolute);
        let offset = absolute - wanted * self.chunk_rows;
        for slot in self.slots.iter_mut().filter(|slot| slot.chunk() == Some(wanted)) {
            if let Some(row) = slot.rows_mut().get_mut(offset) {
                row.contig = contig;
                return;
            }
        }
    }

    /// `count` rows from `first`, reading across both buffers so a span
    /// straddling a boundary comes back as one contiguous run. `None` marks a row
    /// whose chunk is not resident.
    pub fn slice(&self, first: usize, count: usize) -> Vec<Option<&VcfRow>> {
        (first..first + count).map(|row| self.row(row)).collect()
    }

    pub fn is_fetching(&self) -> bool {
        self.slots.iter().any(|slot| matches!(slot, Slot::Loading { .. }))
    }

    /// When the oldest in-flight fetch started, for the spinner's phase. Taking
    /// the earliest keeps the animation continuous when a second fetch joins a
    /// first rather than restarting it.
    pub fn started_at(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Loading { started_at, .. } => Some(*started_at),
                _ => None,
            })
            .min()
    }

    /// The chunks that should be resident for a viewport whose top edge is at
    /// `top_edge`: the edge's own chunk, plus a neighbour once the edge is within
    /// [`PREFETCH_MARGIN`] of that boundary.
    ///
    /// Mid-chunk this is one chunk and nothing is prefetched. Near a boundary it
    /// is two — which after a jump means both are fetched at once, there being no
    /// runway left to earn the second one later.
    fn wanted(&self, top_edge: usize, total_rows: usize) -> Vec<usize> {
        let chunk = self.chunk_of(top_edge);
        let mut wanted = vec![chunk];

        let offset = top_edge - chunk * self.chunk_rows;
        let last = self.chunk_of(total_rows.saturating_sub(1));
        if offset < self.margin && chunk > 0 {
            wanted.push(chunk - 1);
        } else if self.chunk_rows - offset <= self.margin && chunk < last {
            wanted.push(chunk + 1);
        }
        wanted
    }

    /// Brings the pair in line with what the viewport needs, fetching what is
    /// missing and recycling whatever sits furthest from the top edge.
    ///
    /// Called every frame and on every scroll; a no-op once the wanted chunks are
    /// held or in flight.
    pub fn advance(
        &mut self,
        top_edge: usize,
        total_rows: usize,
        index: &Arc<LineIndex>,
        path: &Path,
        io_worker: &IoWorkerHandle,
    ) {
        let mut generation: Option<Generation> = None;
        for chunk in self.wanted(top_edge, total_rows) {
            // A seeded chunk covers only the preview's span, so "present" is not
            // enough — it is refetched once to fill the chunk out.
            if self.holds_complete(chunk) || self.is_loading(chunk) {
                continue;
            }
            let Some(victim) = self.recyclable(top_edge, chunk) else {
                continue;
            };
            // Whatever the victim already has for this chunk stays visible while
            // the fetch runs; for any other chunk it is not ours to show.
            let partial = match self.slots[victim].chunk() {
                Some(held) if held == chunk => self.slots[victim].rows().to_vec(),
                _ => Vec::new(),
            };
            let request_generation = if let Some(generation) = generation.as_ref() {
                generation.clone()
            } else {
                let next = self.batch.next();
                generation = Some(next.clone());
                next
            };
            self.slots[victim] = self.fetch(chunk, partial, request_generation, index, path, io_worker);
        }
    }

    fn is_loading(&self, chunk: usize) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, Slot::Loading { chunk: held, .. } if *held == chunk))
    }

    /// Which slot may be recycled to hold `incoming`: the one furthest from the
    /// top edge, preferring a stale slot and never displacing an in-flight fetch
    /// the viewport still wants.
    fn recyclable(&self, top_edge: usize, incoming: usize) -> Option<usize> {
        // A partial copy of the incoming chunk is replaced in place, so topping it
        // up cannot leave both slots claiming the same chunk.
        if let Some(partial) = self.slots.iter().position(|slot| slot.chunk() == Some(incoming)) {
            return Some(partial);
        }
        if let Some(stale) = self.slots.iter().position(|slot| matches!(slot, Slot::Stale)) {
            return Some(stale);
        }
        let current = self.chunk_of(top_edge);
        let (slot, held) = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.chunk().map(|chunk| (i, chunk)))
            .max_by_key(|(_, chunk)| self.distance(top_edge, *chunk))?;

        // The top edge's own chunk always displaces something: the viewport
        // cannot be left unservable. A prefetch is more polite and only evicts a
        // chunk strictly further away, so the two can never trade places frame
        // after frame.
        (incoming == current || self.distance(top_edge, held) > self.distance(top_edge, incoming)).then_some(slot)
    }

    /// How far `chunk` sits from the top edge, in rows.
    ///
    /// Measured in rows rather than chunk indices because the two neighbours are
    /// equidistant by index from anywhere in a chunk, which would leave the
    /// prefetch unable to displace the chunk behind it at the very boundary it is
    /// preparing for. Rows carry which side of the chunk the edge is on, and so
    /// break that tie the way the viewport is actually moving.
    fn distance(&self, top_edge: usize, chunk: usize) -> usize {
        let start = chunk * self.chunk_rows;
        let end = start + self.chunk_rows - 1;
        if top_edge < start {
            start - top_edge
        } else {
            top_edge.saturating_sub(end)
        }
    }

    /// Issues one chunk fetch under the generation for this viewport advance.
    fn fetch(
        &mut self,
        chunk: usize,
        partial: Vec<VcfRow>,
        generation: Generation,
        index: &Arc<LineIndex>,
        path: &Path,
        io_worker: &IoWorkerHandle,
    ) -> Slot {
        Slot::Loading {
            chunk,
            partial,
            handle: io_worker.submit(IoSchedule::Viewport(generation), {
                let path = path.to_path_buf();
                let index = Arc::clone(index);
                let first = chunk * self.chunk_rows;
                let count = self.chunk_rows;
                move || index.read_rows(&path, first, count)
            }),
            started_at: Instant::now(),
        }
    }

    /// Installs rows already in hand as `chunk`, without a fetch.
    ///
    /// The preview's verified rows arrive this way, which is what makes the
    /// preview → indexed swap seamless: the buffer is already serving the rows on
    /// screen before `line_index` switches the viewer over to it. The chunk may
    /// be short of `chunk_rows`; [`Self::advance`] tops it up.
    pub fn seed(&mut self, chunk: usize, rows: Vec<VcfRow>) {
        self.slots[0] = Slot::Resident {
            chunk,
            rows,
            complete: false,
        };
    }

    /// Whether `chunk` is resident and known complete — false for a seeded
    /// partial chunk that still needs topping up.
    fn holds_complete(&self, chunk: usize) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, Slot::Resident { chunk: held, complete: true, .. } if *held == chunk))
    }

    /// Polls both slots, installing what has landed. Returns an error message
    /// once, for the viewer to flash.
    pub fn poll(&mut self) -> Option<String> {
        let mut reported = None;
        for slot in &mut self.slots {
            let Slot::Loading {
                chunk, handle, partial, ..
            } = slot
            else {
                continue;
            };
            // A fetch that will not arrive falls back to whatever rows the slot
            // already had: stale if it had none, and otherwise the seeded rows,
            // which are still correct — only the top-up was lost. `advance` then
            // refetches if the viewport still wants the chunk.
            let abandoned = |partial: &mut Vec<VcfRow>, chunk: usize| match std::mem::take(partial) {
                rows if rows.is_empty() => Slot::Stale,
                rows => Slot::Resident {
                    chunk,
                    rows,
                    complete: false,
                },
            };
            match handle.try_recv() {
                JobPoll::Empty => {}
                // Rows that arrived are installed whether or not a later scroll has
                // superseded them: they are the file's contents, which do not go out
                // of date, and dropping them would blank a completed prefetch.
                JobPoll::Ready(Ok(rows)) => {
                    *slot = Slot::Resident {
                        chunk: *chunk,
                        rows,
                        complete: true,
                    };
                }
                JobPoll::Ready(Err(err)) => {
                    *slot = abandoned(partial, *chunk);
                    reported = reported.or(Some(format!("Reading VCF rows failed: {err}")));
                }
                // Superseded before the worker got to it, so no reply is coming and
                // nothing failed — the scroll that replaced it is not an error.
                JobPoll::Stale => {
                    *slot = abandoned(partial, *chunk);
                }
                JobPoll::Disconnected => {
                    *slot = abandoned(partial, *chunk);
                    reported = reported.or(Some("Reading VCF rows failed: the I/O worker stopped".to_string()));
                }
            }
        }
        reported
    }

    /// Drops everything, for a newly loaded file.
    pub fn clear(&mut self) {
        self.slots = [Slot::Stale, Slot::Stale];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer_app::vcf::line_index;

    const FIXTURE: &str = "testdata/vcf/sample.vcf";

    /// Chunk size for the policy tests. Small enough that a few hundred rows span
    /// several chunks; the margin scales with it, keeping the shipped proportion.
    const TEST_CHUNK: usize = 100;

    /// A file large enough for the chunk arithmetic to have somewhere to go. The
    /// policy tests never read it — they exercise `wanted`/`recyclable`, which are
    /// functions of position and slot state alone.
    const TEST_TOTAL: usize = TEST_CHUNK * 10;

    fn buffer() -> ChunkBuffer {
        ChunkBuffer::with_chunk_rows(TEST_CHUNK)
    }

    /// One row, enough to tell a populated slot from an empty one.
    fn a_row() -> VcfRow {
        super::super::ingest::read_preview(Path::new(FIXTURE), 1)
            .unwrap()
            .rows
            .remove(0)
    }

    /// Installs distinct rows so a wrong offset cannot look like a cache hit.
    fn resident(buffer: &mut ChunkBuffer, at: usize, chunk: usize, rows: usize) {
        let seed = a_row();
        buffer.slots[at] = Slot::Resident {
            chunk,
            rows: (0..rows)
                .map(|offset| {
                    let mut row = seed.clone();
                    row.id = format!("row-{}", chunk * buffer.chunk_rows + offset);
                    row
                })
                .collect(),
            complete: true,
        };
    }

    /// The chunks the pair is holding or fetching, ascending.
    fn held(buffer: &ChunkBuffer) -> Vec<usize> {
        let mut chunks: Vec<usize> = buffer.slots.iter().filter_map(Slot::chunk).collect();
        chunks.sort_unstable();
        chunks
    }

    /// How many in-flight fetches are still current — i.e. were issued by the
    /// most recent `advance`. Asks what the generation is *for* rather than what
    /// number it reached.
    fn current_fetches(buffer: &ChunkBuffer) -> usize {
        buffer
            .slots
            .iter()
            .filter(|slot| match slot {
                Slot::Loading { handle, .. } => handle.is_current(),
                _ => false,
            })
            .count()
    }

    /// A worker and index for tests that only need `advance` to *decide*; the
    /// fetches it starts are never awaited.
    fn policy_inputs() -> (IoWorkerHandle, Arc<LineIndex>) {
        (
            IoWorkerHandle::spawn(),
            Arc::new(line_index::build(Path::new(FIXTURE)).unwrap()),
        )
    }

    #[test]
    fn a_span_crossing_a_boundary_reads_as_one_contiguous_run() {
        let mut buffer = buffer();
        resident(&mut buffer, 0, 1, TEST_CHUNK);
        resident(&mut buffer, 1, 2, TEST_CHUNK);

        let first = 2 * TEST_CHUNK - 3;
        let slice = buffer.slice(first, 6);
        let actual: Vec<_> = slice.iter().map(|row| row.map(|row| row.id.clone())).collect();
        let expected: Vec<_> = (first..first + 6).map(|row| Some(format!("row-{row}"))).collect();
        assert_eq!(actual, expected, "a boundary slice must preserve absolute row identity");
    }

    #[test]
    fn a_slice_reaching_outside_the_resident_pair_is_holed_not_shifted() {
        let mut buffer = buffer();
        resident(&mut buffer, 0, 1, TEST_CHUNK);

        // Chunk 0 is absent, so the first half of this span is missing and the
        // second half must still land at its own offsets.
        let slice = buffer.slice(TEST_CHUNK - 2, 4);
        assert_eq!(
            slice.iter().map(Option::is_some).collect::<Vec<_>>(),
            vec![false, false, true, true],
            "missing rows must be holes in place, not close the gap"
        );
    }

    #[test]
    fn landing_mid_chunk_wants_one_chunk_and_landing_near_a_boundary_wants_two() {
        let buffer = buffer();
        let mid = 3 * TEST_CHUNK + TEST_CHUNK / 2;
        assert_eq!(buffer.wanted(mid, TEST_TOTAL), vec![3], "mid-chunk must not prefetch");

        // Beside the 3|4 boundary, with no runway left to earn the second buffer
        // later — so one call must want both.
        let ahead = 4 * TEST_CHUNK - 1;
        assert_eq!(
            buffer.wanted(ahead, TEST_TOTAL),
            vec![3, 4],
            "approaching a boundary must want the neighbour too"
        );
        // And symmetrically on the way back up.
        let behind = 4 * TEST_CHUNK;
        assert_eq!(
            buffer.wanted(behind, TEST_TOTAL),
            vec![4, 3],
            "the rule must be symmetric, so scrolling up prefetches as well"
        );
    }

    #[test]
    fn the_file_edges_have_no_neighbour_to_prefetch() {
        let buffer = buffer();
        assert_eq!(
            buffer.wanted(0, TEST_TOTAL),
            vec![0],
            "there is nothing before the first row"
        );
        assert_eq!(
            buffer.wanted(TEST_TOTAL - 1, TEST_TOTAL),
            vec![9],
            "nor anything after the last"
        );
    }

    #[test]
    fn approaching_a_boundary_fetches_the_neighbour_before_a_row_of_it_is_wanted() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        resident(&mut buffer, 0, 3, TEST_CHUNK);

        let approaching = 4 * TEST_CHUNK - buffer.margin;
        assert_eq!(
            buffer.chunk_of(approaching),
            3,
            "still inside chunk 3, so this is a prefetch"
        );
        buffer.advance(approaching, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);

        assert!(
            held(&buffer).contains(&4),
            "the neighbour must be requested on proximity alone"
        );
    }

    #[test]
    fn a_boundary_advance_uses_one_generation_for_both_prefetches() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        let approaching = 4 * TEST_CHUNK - 1;

        buffer.advance(approaching, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);

        assert_eq!(held(&buffer), vec![3, 4], "the pair must survive the enqueue collapse");
        assert_eq!(
            current_fetches(&buffer),
            2,
            "neither request may supersede the other: one advance is one batch"
        );
    }

    #[test]
    fn the_top_edges_chunk_always_displaces_a_more_distant_one() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        // Both slots far from where the viewport is about to be.
        resident(&mut buffer, 0, 0, TEST_CHUNK);
        resident(&mut buffer, 1, 1, TEST_CHUNK);

        let jumped = 7 * TEST_CHUNK + TEST_CHUNK / 2;
        buffer.advance(jumped, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);

        assert!(
            held(&buffer).contains(&7),
            "the viewport's own chunk must be fetched even when both slots are busy"
        );
        assert!(held(&buffer).len() <= RESIDENT_CHUNKS, "and the pair must not grow");
    }

    #[test]
    fn the_pair_recycles_whichever_buffer_is_furthest_from_the_top_edge() {
        let mut buffer = buffer();
        resident(&mut buffer, 0, 4, TEST_CHUNK);
        resident(&mut buffer, 1, 5, TEST_CHUNK);

        // Once the edge has crossed into 5, chunk 4 is the more distant of the
        // pair and chunk 6 the one being approached.
        let edge = 6 * TEST_CHUNK - 1;
        assert_eq!(buffer.recyclable(edge, 6), Some(0), "the far side is the one recycled");

        // Scrolling back the other way reverses which is far, with no direction
        // memory involved.
        let back = 4 * TEST_CHUNK;
        assert_eq!(buffer.recyclable(back, 3), Some(1), "and the rule is symmetric");
    }

    #[test]
    fn a_prefetch_never_evicts_a_chunk_nearer_the_viewport_than_itself() {
        let mut buffer = buffer();
        resident(&mut buffer, 0, 5, TEST_CHUNK);
        resident(&mut buffer, 1, 6, TEST_CHUNK);

        // A speculative chunk 4 is further from the edge than either held chunk;
        // taking a slot would mean the two swap places every frame.
        let edge = 6 * TEST_CHUNK;
        assert_eq!(
            buffer.recyclable(edge, 4),
            None,
            "a prefetch further away than what is held must yield"
        );
    }

    #[test]
    fn a_complete_resident_chunk_is_never_refetched_even_when_short() {
        let (worker, index) = policy_inputs();
        for (rows, total) in [(TEST_CHUNK, TEST_TOTAL), (1, 1)] {
            let mut buffer = buffer();
            resident(&mut buffer, 0, 0, rows);
            buffer.advance(0, total, &index, Path::new(FIXTURE), &worker);
            assert!(
                !buffer.is_fetching(),
                "a complete chunk must not be fetched again, regardless of its row count"
            );
        }
    }

    #[test]
    fn an_in_flight_chunk_is_not_requested_twice() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();

        let deep = 3 * TEST_CHUNK + TEST_CHUNK / 2;
        buffer.advance(deep, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);
        assert_eq!(current_fetches(&buffer), 1, "the chunk is requested once");

        // advance runs every frame; a repeat must neither re-request the chunk nor
        // start a generation that would strand the fetch already in flight.
        buffer.advance(deep + 1, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);
        assert_eq!(
            current_fetches(&buffer),
            1,
            "the in-flight fetch must still belong to the newest batch"
        );
    }

    #[test]
    fn a_fetched_chunk_holds_the_rows_the_index_reads_for_it() {
        let worker = IoWorkerHandle::spawn();
        let index = Arc::new(line_index::build(Path::new(FIXTURE)).unwrap());
        let total = index.total_records();
        // The fixture's real size, so chunk 0 is the whole file and the arithmetic
        // is checked against rows that actually exist.
        let mut buffer = ChunkBuffer::with_chunk_rows(total);

        buffer.advance(0, total, &index, Path::new(FIXTURE), &worker);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while buffer.is_fetching() {
            assert!(Instant::now() < deadline, "the chunk fetch did not finish");
            assert_eq!(buffer.poll(), None, "a readable fixture must not error");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let expected = index.read_rows(Path::new(FIXTURE), 0, total).unwrap();
        assert!(!expected.is_empty(), "the fixture must have rows");
        for (row, want) in expected.iter().enumerate() {
            assert_eq!(buffer.row(row), Some(want), "row {row}");
        }
    }

    #[test]
    fn a_seeded_chunk_serves_its_rows_and_is_topped_up_rather_than_trusted() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        let table = super::super::ingest::read_preview(Path::new(FIXTURE), usize::MAX).unwrap();
        let seeded = table.rows.len();
        buffer.seed(0, table.rows);

        assert!(buffer.row(0).is_some(), "seeded rows must be served immediately");
        assert!(
            buffer.row(seeded).is_none(),
            "past a partial chunk's rows is missing, not end of file"
        );

        buffer.advance(0, TEST_TOTAL, &index, Path::new(FIXTURE), &worker);
        assert!(buffer.is_fetching(), "a partial chunk must be filled out");
        assert!(
            buffer.row(0).is_some(),
            "and its rows must stay on screen while that happens — the point of seeding"
        );
    }

    #[test]
    fn a_chunk_that_cannot_be_read_reports_once_and_stops_fetching() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        buffer.advance(0, TEST_TOTAL, &index, Path::new("testdata/vcf/nope.vcf"), &worker);

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut reported = None;
        while buffer.is_fetching() {
            assert!(Instant::now() < deadline, "the failing fetch did not report");
            reported = reported.or(buffer.poll());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        assert!(reported.is_some(), "a failed read must be reported");
        assert_eq!(buffer.poll(), None, "and must not keep reporting");
    }

    #[test]
    fn a_failed_top_up_keeps_the_rows_it_already_had() {
        let (worker, index) = policy_inputs();
        let mut buffer = buffer();
        buffer.seed(0, vec![a_row()]);
        buffer.advance(0, TEST_TOTAL, &index, Path::new("testdata/vcf/nope.vcf"), &worker);

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while buffer.is_fetching() {
            assert!(Instant::now() < deadline, "the failing fetch did not report");
            buffer.poll();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        assert!(
            buffer.row(0).is_some(),
            "losing the top-up must not cost the rows that were already correct"
        );
    }
}
