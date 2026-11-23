//! Non-blocking read sources for the alignment track.
//!
//! [`EmptySource`] gives a newly constructed track a valid, blank source until a BAM is loaded.
//! [`OnDiskBamSource`] adapts an indexed BAM to the same contract: viewport requests are submitted
//! to the shared I/O worker, and polling distinguishes pending work, an empty successful window,
//! and a settled failure. A failure must stay distinct from “no reads”, so the viewer can report it
//! and the cache can suppress repeated requests for the same resident window.
//!
//! Each fetch opens its own reader and reuses the parsed shared index and header. The source builds
//! coverage beside the reads and cancels work once its window leaves the viewport.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use noodles::bam;
use noodles::bgzf;

use crate::basepair::BasePair;
use crate::cigar::{Cigar, ref_len};
use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{Batch, Generation, JobHandle, JobPoll};

use super::super::bam_query::{BamRegionReader, record_span};
use super::super::mod_5mc::decode_5mc_from_record;
use super::super::packed_seq::PackedSeq;
use super::insertions::with_insertion_context;
use super::read::{Read, ReadId, Strand};
use super::read_cache::{DEFAULT_WINDOW_SIZING, LANE_TILE_LEN, ReadSource, WindowData, WindowState};
use crate::viewer_app::bam_index::SharedIndex;

const READ_SAMPLE_LIMIT: usize = 64;

/// Empty source used before a BAM is loaded.
pub(crate) struct EmptySource;

impl ReadSource for EmptySource {
    fn take_window(&mut self, _contig: &str, start: BasePair, end: BasePair) -> WindowState {
        WindowState::Ready(WindowData::from_reads(
            Vec::new(),
            start,
            end.checked_sub(start).unwrap_or(BasePair(0)),
        ))
    }

    fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

    fn is_fetching(&self) -> bool {
        false
    }
}

#[cfg(feature = "resident-demo")]
#[derive(Clone)]
pub(in crate::viewer_app::genome) struct ResidentBamSource {
    reads: Arc<HashMap<String, Vec<Read>>>,
}

#[cfg(feature = "resident-demo")]
impl ResidentBamSource {
    pub(in crate::viewer_app::genome) fn read(bytes: &[u8]) -> io::Result<Self> {
        let mut reader = bam::io::Reader::new(bytes);
        let header = reader.read_header()?;
        let mut reads = HashMap::<String, Vec<Read>>::new();
        let mut record = bam::Record::default();
        while reader.read_record(&mut record)? != 0 {
            if record.flags().is_unmapped() {
                continue;
            }
            let Some(id) = record.reference_sequence_id().transpose()? else {
                continue;
            };
            let Some(pos) = record.alignment_start().transpose()? else {
                continue;
            };
            let (name, _) = header
                .reference_sequences()
                .get_index(id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "BAM reference ID is outside the header"))?;
            let start = BasePair::from_1based(usize::from(pos) as u64);
            if let Some(read) = read_from_record(start, &record)? {
                let contig = String::from_utf8_lossy(name.as_ref()).into_owned();
                reads.entry(contig).or_default().push(read);
            }
        }
        Ok(Self { reads: Arc::new(reads) })
    }

    pub(in crate::viewer_app::genome) fn reads(&self, contig: &str) -> &[Read] {
        self.reads.get(contig).map_or(&[], Vec::as_slice)
    }
}

#[cfg(feature = "resident-demo")]
impl ReadSource for ResidentBamSource {
    fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState {
        let range = with_insertion_context(start..end);
        let reads = self
            .reads(contig)
            .iter()
            .filter(|read| read.start < range.end && range.start < read.start + read.length)
            .cloned()
            .collect();
        WindowState::Ready(WindowData::from_reads(reads, start, end - start))
    }

    fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

    fn is_fetching(&self) -> bool {
        false
    }
}

/// Indexed BAM source queried through the I/O worker.
pub(crate) struct OnDiskBamSource {
    io_worker: IoWorkerHandle,
    batch: Batch,
    bam_path: PathBuf,
    /// Parsed once, then shared with every window fetch. Re-reading a real BAI per window
    /// cost hundreds of milliseconds each, which is what made fast movement lag: every new
    /// viewport queues a window, and each one re-parsed the whole index.
    index: SharedIndex,
    header: Arc<noodles::sam::Header>,
    window_sizing: (usize, BasePair),
    generation: Generation,
    in_flight: Vec<PendingWindow>,
    completed: HashMap<(String, BasePair), Result<WindowData, String>>,
}

struct PendingWindow {
    key: (String, BasePair),
    handle: JobHandle<io::Result<WindowData>>,
}

impl OnDiskBamSource {
    pub(crate) fn from_path(bam_path: &Path, io_worker: IoWorkerHandle) -> io::Result<Self> {
        let index = SharedIndex::read(bam_path)?;
        let mut reader = bam::io::Reader::new(File::open(bam_path)?);
        let header = reader.read_header()?;
        let window_sizing = sample_window_sizing(&mut reader);
        let batch = Batch::new();
        Ok(Self {
            io_worker,
            generation: batch.next(),
            batch,
            bam_path: bam_path.to_path_buf(),
            index,
            header: Arc::new(header),
            window_sizing,
            in_flight: Vec::new(),
            completed: HashMap::new(),
        })
    }

    pub(super) fn window_sizing(&self) -> (usize, BasePair) {
        self.window_sizing
    }
}

impl ReadSource for OnDiskBamSource {
    fn retain_windows(&mut self, contig: &str, starts: &[BasePair]) {
        let visible = |key: &(String, BasePair)| key.0 == contig && starts.contains(&key.1);
        let had_in_flight = !self.in_flight.is_empty();
        self.in_flight.retain(|pending| visible(&pending.key));
        self.completed.retain(|key, _| visible(key));
        if had_in_flight && self.in_flight.is_empty() {
            self.generation = self.batch.next();
        }
    }

    fn poll(&mut self) -> bool {
        let mut finished: Vec<((String, BasePair), Result<WindowData, String>)> = Vec::new();
        self.in_flight.retain_mut(|pw| match pw.handle.try_recv() {
            JobPoll::Ready(result) => {
                finished.push((pw.key.clone(), result.map_err(|error| error.to_string())));
                false
            }
            JobPoll::Empty => true,
            JobPoll::Stale => false,
            JobPoll::Disconnected => {
                finished.push((pw.key.clone(), Err("the I/O worker stopped".to_string())));
                false
            }
        });
        let settled = !finished.is_empty();
        for (key, data) in finished {
            self.completed.insert(key, data);
        }
        settled
    }

    fn take_window(&mut self, contig: &str, start: BasePair, _end: BasePair) -> WindowState {
        self.poll();
        let key = (contig.to_string(), start);
        let Some(data) = self.completed.remove(&key) else {
            return WindowState::Pending;
        };
        match data {
            Ok(data) => WindowState::Ready(data),
            Err(error) => WindowState::Failed(error),
        }
    }

    fn request_window(&mut self, contig: &str, start: BasePair, end: BasePair) {
        let key = (contig.to_string(), start);
        if self.in_flight.iter().any(|pw| pw.key == key) {
            return;
        }
        if self.completed.contains_key(&key) {
            return;
        }
        let bam_path = self.bam_path.clone();
        let contig_str = contig.to_string();
        let index = self.index.clone();
        let header = Arc::clone(&self.header);
        let handle = self
            .io_worker
            .submit_cancellable(IoSchedule::Viewport(self.generation.clone()), move |cancel| {
                fetch_window(&bam_path, &index, &header, &contig_str, start, end, || {
                    cancel.is_cancelled()
                })
            });
        self.in_flight.push(PendingWindow { key, handle });
    }

    fn is_fetching(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

/// Samples a bounded prefix of records after the header. Sizing is best effort so a truncated
/// prefix still leaves actual fetch errors to the affected window.
pub(in crate::viewer_app::genome) fn sample_window_sizing(
    reader: &mut bam::io::Reader<impl io::Read>,
) -> (usize, BasePair) {
    let mut record = bam::Record::default();
    let mut spans = Vec::with_capacity(READ_SAMPLE_LIMIT);
    for _ in 0..READ_SAMPLE_LIMIT {
        if !matches!(reader.read_record(&mut record), Ok(n) if n > 0) {
            break;
        }
        if !record.flags().is_unmapped()
            && let Ok(span) = record_span(&record)
        {
            spans.push(span);
        }
    }
    if spans.is_empty() {
        return DEFAULT_WINDOW_SIZING;
    }
    spans.sort_unstable();
    let median = spans[(spans.len() - 1) / 2]
        .as_u64()
        .midpoint(spans[spans.len() / 2].as_u64());
    let (capacity, len) = match median {
        ..1_000 => (8, BasePair(8_000)),
        1_000..5_000 => (6, BasePair(16_000)),
        _ => (4, BasePair(32_000)),
    };
    (capacity, len.div_ceil(LANE_TILE_LEN) * LANE_TILE_LEN)
}

/// Queries one window and returns reads with coverage.
pub(in crate::viewer_app::genome) fn fetch_window(
    bam_path: &Path,
    index: &SharedIndex,
    header: &noodles::sam::Header,
    contig: &str,
    start: BasePair,
    end: BasePair,
    cancelled: impl Fn() -> bool,
) -> io::Result<WindowData> {
    let window_len = end
        .checked_sub(start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "BAM window end precedes its start"))?;
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut range = with_insertion_context(start..end);
    // The last aligned window can run past the BAM header's contig, even beyond the BAI limit.
    if let Some(sequence) = header.reference_sequences().get(contig.as_bytes()) {
        range.end = range.end.min(BasePair(sequence.length().get() as u64));
    }
    let mut bgzf = bgzf::io::Reader::new(File::open(bam_path)?);
    let mut reader = BamRegionReader::query(&mut bgzf, index, header, contig, range)?;
    let mut reads = Vec::new();
    while let Some((start, record)) = reader.next_record(&cancelled)? {
        if let Some(read) = read_from_record(start, record)? {
            reads.push(read);
        }
    }
    if cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    Ok(WindowData::from_reads(reads, start, window_len))
}

/// Converts a BAM record into a pileup read.
fn read_from_record(start: BasePair, record: &bam::Record) -> io::Result<Option<Read>> {
    let Some(name) = record.name() else {
        return Ok(None);
    };
    let cigar = Cigar::from_bam_bytes(record.cigar().as_bytes())?;
    // Match the indexed query's single-position span for an empty reference CIGAR.
    let length = BasePair::length(ref_len(&cigar).max(1));
    let strand = if record.flags().is_reverse_complemented() {
        Strand::Reverse
    } else {
        Strand::Forward
    };
    let seq = record.sequence();
    let seq = (!seq.is_empty()).then(|| PackedSeq::from_bam_bytes(seq.as_bytes(), seq.len()));
    let mut mod_5mc = decode_5mc_from_record(record);
    if let Some(mods) = &mut mod_5mc {
        // A long read can occupy several cached windows. Drop the spare capacity before caching.
        mods.calls.shrink_to_fit();
    }
    Ok(Some(Read {
        start,
        length,
        id: ReadId::from_name(name.as_ref()),
        cigar,
        mod_5mc,
        seq,
        strand,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testgen::bam_records_from_sam;

    #[test]
    fn window_sizing_grows_with_mapped_read_span_and_falls_back_without_one() {
        let sizing = |name: &str, sam: &str| {
            let (path, _) = crate::testgen::write_indexed_test_bam(&format!("window_size_{name}"), sam);
            let sizing = OnDiskBamSource::from_path(&path, IoWorkerHandle::spawn())
                .unwrap()
                .window_sizing();
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
            sizing
        };
        // A deletion sets the reference span without needing a matching SEQ.
        let sized: Vec<_> = [500, 2_000, 10_000]
            .into_iter()
            .map(|span: u64| {
                let sam = format!("r\t0\tchr1\t1\t60\t1M{}D1M\t*\t0\t0\tAC\t*\n", span - 2);
                sizing(&format!("span_{span}"), &sam)
            })
            .collect();
        for &(capacity, len) in &sized {
            assert!(capacity > 0);
            assert_eq!(len % LANE_TILE_LEN, BasePair(0));
        }
        assert!(sized.windows(2).all(|pair| pair[0].1 <= pair[1].1), "{sized:?}");
        assert!(sized[0].1 < sized[2].1, "{sized:?}");

        let unmapped = (1..=READ_SAMPLE_LIMIT)
            .map(|pos| format!("u{pos}\t4\tchr1\t{pos}\t0\t*\t*\t0\t0\t*\t*\n"))
            .collect::<String>();
        assert_eq!(sizing("empty", ""), DEFAULT_WINDOW_SIZING);
        assert_eq!(
            sizing(
                "unmapped",
                &(unmapped + "late\t0\tchr1\t65\t60\t5000M\t*\t0\t0\t*\t*\n")
            ),
            DEFAULT_WINDOW_SIZING
        );
    }

    #[test]
    fn scrolling_retains_shared_window_requests_and_cancels_windows_that_leave_view() {
        let mut source = OnDiskBamSource::from_path(Path::new("testdata/bam/tiny.bam"), IoWorkerHandle::spawn())
            .expect("tiny.bam fixture");
        let (_, len) = source.window_sizing();
        let (_first_tx, first, first_cancel) = JobHandle::cancellable_channel(Some(source.generation.clone()));
        let (_second_tx, second, second_cancel) = JobHandle::cancellable_channel(Some(source.generation.clone()));
        source.in_flight = vec![
            PendingWindow {
                key: ("chr1".to_owned(), BasePair(0)),
                handle: first,
            },
            PendingWindow {
                key: ("chr1".to_owned(), len),
                handle: second,
            },
        ];
        source.retain_windows("chr1", &[len, len + len]);
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert_eq!(source.in_flight.len(), 1);
        assert!(source.in_flight[0].handle.is_current());

        source.retain_windows("chr2", &[len]);
        assert!(second_cancel.is_cancelled());
        assert!(!source.is_fetching());
    }

    #[test]
    fn bam_record_conversion_preserves_span_cigar_name_and_strand() {
        use super::super::window_counts::BaseCounts;

        let sam = concat!(
            "forward\t0\tchr1\t2\t60\t2M1I2M\t*\t0\t0\tACGTT\tIIIII\tMM:Z:C+m?,0;\tML:B:C,147\n",
            "reverse\t16\tchr1\t2\t60\t2M1I2M\t*\t0\t0\tACGTT\tIIIII\n",
        );
        let (path, header) = crate::testgen::write_indexed_test_bam("pileup_window", sam);
        let index = SharedIndex::read(&path).unwrap();
        let data = fetch_window(&path, &index, &header, "chr1", BasePair(2), BasePair(4), || false).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(
            data.counts.bases,
            [
                BaseCounts {
                    c: 2,
                    ..BaseCounts::default()
                },
                BaseCounts {
                    t: 2,
                    ..BaseCounts::default()
                }
            ]
        );
        let reads = data.reads;
        assert_eq!(reads.len(), 2);
        for (read, (name, strand)) in reads
            .iter()
            .zip([("forward", Strand::Forward), ("reverse", Strand::Reverse)])
        {
            assert_eq!(read.id, ReadId::from_name(name.as_bytes()));
            assert_eq!(read.start, BasePair(1));
            assert_eq!(read.length, BasePair::length(4));
            assert_eq!(crate::cigar::format_ops(&read.cigar), "2M1I2M");
            assert_eq!(read.strand, strand);
            let seq = read.seq.as_ref().unwrap().as_view();
            for (i, base) in b"ACGTT".iter().copied().enumerate() {
                assert_eq!(seq.base_at(i), Some(base));
            }
            assert_eq!(seq.base_at(5), None);
        }
        assert_eq!(reads[0].mod_5mc.as_ref().unwrap().calls, [(1, 147)]);
        assert!(reads[1].mod_5mc.is_none());
    }

    #[test]
    fn missing_bam_sequence_keeps_the_single_position_span() {
        let records = bam_records_from_sam("missing\t0\tchr1\t2\t60\t1I\t*\t0\t0\t*\t*\n");
        let read = read_from_record(BasePair(1), &records[0]).unwrap().unwrap();
        assert!(read.seq.is_none());
        assert_eq!(read.start, BasePair(1));
        assert_eq!(read.length, BasePair::length(1));
    }

    #[test]
    fn reads_without_cigar_contribute_unknown_bases_over_their_spans() {
        let reads = vec![
            Read {
                start: BasePair(1),
                length: BasePair::length(3),
                id: ReadId::from_name(b"first"),
                cigar: Cigar::default(),
                seq: None,
                strand: Strand::Forward,
                mod_5mc: None,
            },
            Read {
                start: BasePair(3),
                length: BasePair::length(2),
                id: ReadId::from_name(b"second"),
                cigar: Cigar::default(),
                seq: None,
                strand: Strand::Reverse,
                mod_5mc: None,
            },
        ];
        let data = WindowData::from_reads(reads, BasePair(0), BasePair(8));

        let unknown = |n| super::super::window_counts::BaseCounts {
            n,
            ..Default::default()
        };
        assert_eq!(data.counts.bases, [0, 1, 1, 2, 1, 0, 0, 0].map(unknown));
    }

    #[test]
    fn unknown_bam_contig_is_a_fetch_error_not_an_empty_window() {
        let path = Path::new("testdata/bam/tiny.bam");
        let index = SharedIndex::read(path).expect("bai fixture");
        let header = File::open(path)
            .and_then(|file| bam::io::Reader::new(file).read_header())
            .expect("header");
        let result = fetch_window(
            path,
            &index,
            &header,
            "not_a_real_contig",
            BasePair(0),
            BasePair(50),
            || false,
        );

        assert!(
            result.is_err(),
            "an unknown contig must not masquerade as an empty region"
        );
    }

    #[test]
    fn poll_settles_in_flight_fetches_without_drawing() {
        let mut source = OnDiskBamSource::from_path(Path::new("testdata/bam/tiny.bam"), IoWorkerHandle::spawn())
            .expect("tiny.bam fixture must be readable");
        source.request_window("chr1", BasePair(0), BasePair(2_000));
        assert!(source.is_fetching());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut settled = false;
        while source.is_fetching() {
            assert!(
                std::time::Instant::now() < deadline,
                "BAM window fetch did not settle via poll()"
            );
            settled |= source.poll();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        assert!(settled, "settling a window via poll() must request a redraw");
        assert!(matches!(
            source.take_window("chr1", BasePair(0), BasePair(2_000)),
            WindowState::Ready(data) if !data.reads.is_empty()
        ));
    }
}
