//! A sparse record-number → file-offset index over a VCF.
//!
//! Tabix answers *coordinate* questions (`contig:start-end`). A scrolling table
//! asks *row-offset* ones — "rows 40,000–40,050" — which tabix cannot serve at
//! all: its bins are coordinate-keyed and carry no record counts, so nothing in
//! a `.tbi` turns a row number into a file offset. This module builds the other
//! half by scanning the file once and checkpointing every [`INDEX_STRIDE`]
//! records; [`super::chunks`] reads through it to scroll the table. Each
//! checkpoint also keeps its coordinate so a locus can be mapped back to a row
//! without building a second, dense coordinate index. Ingress tests retain a
//! separate indexed coordinate query for comparison.
//!
//! Like `ingest`, every noodles type stays inside this module.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use noodles::vcf;

use crate::basepair::BasePair;
use crate::vcf_file::{RecordOffset, Source};

use super::ingest::{VcfRow, coordinate_from_record, row_from_record};

/// Records between checkpoints, chosen from a seek-latency budget: a seek reads
/// forward at most one stride, so the stride alone fixes the worst case and file
/// size does not enter into it. Reason in that frame when changing this. Measured
/// on ClinVar (release build): a skip costs ~320ns per record, scan time is flat
/// across strides because decompression dominates, and the worst-case seek is
/// ~1.5ms, about a tenth of a frame. Memory is what scales with file size (64
/// bytes a checkpoint, ~2.6MB for a 50GB VCF), so don't buy latency below
/// perception at a multiple of it.
///
/// Sized against a `read_record` that skips by scanning lines; a cheaper skip
/// path would change the budget, so re-derive rather than assuming this value
/// still holds.
///
/// `chunks::CHUNK_ROWS` is defined as this value so a chunk boundary is always a
/// checkpoint; changing one changes the other deliberately.
pub(super) const INDEX_STRIDE: usize = 5_000;

/// A record's row number and where it starts in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecordPointer {
    row_number: usize,
    offset: RecordOffset,
}

/// A row's starting point and coordinate, sampled every [`INDEX_STRIDE`] rows.
#[derive(Debug, Clone)]
struct Checkpoint {
    pointer: RecordPointer,
    contig_rank: usize,
    position: BasePair,
}

/// Checkpoints into one VCF, enough to start reading at an arbitrary record or
/// coordinate.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Every [`INDEX_STRIDE`] records and always including record 0. Coordinates
    /// are ordered by VCF contig order, then position, for the locus lookup.
    checkpoints: Vec<Checkpoint>,
    /// First-seen record order, which is the only order the lookup can rely on.
    /// Deliberately *not* the header's `##contig` order: writers routinely emit
    /// those sorted by length or not at all. Clair3 can declare
    /// `chrX` between `chr7` and `chr9` while its records run in numeric order.
    /// Deliberately not lexical either — `chr10` follows `chr2` in a real file.
    contig_ranks: HashMap<String, usize>,
    total_records: usize,
    /// The first breach of the sorted-record assumption, if the scan saw one.
    disorder: Option<String>,
}

impl LineIndex {
    pub fn total_records(&self) -> usize {
        self.total_records
    }

    /// Describes the first out-of-order record the scan found, if any. Locus
    /// lookups on such a file may miss; row scrolling is unaffected.
    pub fn disorder(&self) -> Option<&str> {
        self.disorder.as_deref()
    }

    /// The last checkpoint at or before `record`.
    fn checkpoint_for(&self, record: usize) -> Option<RecordPointer> {
        let following = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.pointer.row_number <= record);
        self.checkpoints
            .get(following.checked_sub(1)?)
            .map(|checkpoint| checkpoint.pointer)
    }

    pub(super) fn seek_start(&self, record: usize) -> Option<(RecordOffset, usize)> {
        let pointer = self.checkpoint_for(record)?;
        Some((pointer.offset, record - pointer.row_number))
    }

    /// Resolves a requested name to one file contig rank. Exact spelling wins;
    /// otherwise matching is case-insensitive, then the reciprocal `chr`/bare
    /// alias is tried. Multiple case-insensitive or alias matches are refused
    /// rather than choosing a contig by hash-map order.
    fn resolve_contig_rank(&self, requested: &str) -> Option<usize> {
        let requested = requested.trim();
        if requested.is_empty() {
            return None;
        }
        if let Some(&rank) = self.contig_ranks.get(requested) {
            return Some(rank);
        }
        if let Some(rank) = self.unique_contig_rank(|name| name.eq_ignore_ascii_case(requested)) {
            return Some(rank);
        }

        let alias = requested
            .get(..3)
            .filter(|prefix| prefix.eq_ignore_ascii_case("chr"))
            .map_or_else(|| format!("chr{requested}"), |bare| requested[bare.len()..].to_string());
        self.unique_contig_rank(|name| name.eq_ignore_ascii_case(&alias))
    }

    /// Returns a rank only when all matching names identify the same contig.
    fn unique_contig_rank(&self, matches: impl Fn(&str) -> bool) -> Option<usize> {
        let mut found = None;
        for (name, &rank) in &self.contig_ranks {
            if !matches(name) {
                continue;
            }
            if found.is_some_and(|previous| previous != rank) {
                return None;
            }
            found = Some(rank);
        }
        found
    }

    /// Reads `count` records starting at `first`, seeking to the nearest
    /// checkpoint and skipping forward from there. Returns fewer rows than
    /// asked for only at end of file.
    ///
    /// No header read is needed after the seek: `read_record` reads a line into
    /// the record's buffer and parses no fields, so it is header-independent.
    pub fn read_rows(&self, path: &Path, first: usize, count: usize) -> io::Result<Vec<VcfRow>> {
        let Some(pointer) = self.checkpoint_for(first) else {
            return Ok(Vec::new());
        };

        let mut source = Source::open(path)?;
        source.seek(pointer.offset)?;
        let mut reader = vcf::io::Reader::new(source.as_buf_read());

        let mut record = vcf::Record::default();
        let mut rows = Vec::with_capacity(count);
        for number in pointer.row_number..first.saturating_add(count) {
            if reader.read_record(&mut record)? == 0 {
                break;
            }
            if number >= first {
                rows.push(row_from_record(&record));
            }
        }
        Ok(rows)
    }

    /// Finds the row nearest to `position` on `contig`.
    ///
    /// The VCF must be coordinate-sorted in the order its records appear; see
    /// [`LineIndex::disorder`] for what the build reports when it is not. A
    /// sparse checkpoint search selects one window, and [`read_rows`] reads it
    /// through the existing plain/BGZF path. The next checkpoint's coordinate
    /// is compared as the window's right boundary without reading it twice.
    /// This keeps the lookup to one seek and at most one stride of record scans.
    /// The read is blocking; callers keep it on the I/O worker with `Fifo`.
    ///
    /// A present contig clamps to its first or last row. An absent contig is
    /// never substituted with a neighbouring contig.
    pub fn nearest_row(&self, path: &Path, contig: &str, position: BasePair) -> io::Result<Option<usize>> {
        let Some(contig_rank) = self.resolve_contig_rank(contig) else {
            return Ok(None);
        };
        if self.checkpoints.is_empty() {
            return Ok(None);
        }

        let following = self.checkpoints.partition_point(|checkpoint| {
            checkpoint.contig_rank < contig_rank
                || (checkpoint.contig_rank == contig_rank && checkpoint.position <= position)
        });
        let start_index = following.saturating_sub(1);
        let boundary_index = start_index + 1;
        let start = &self.checkpoints[start_index];
        let end = self
            .checkpoints
            .get(boundary_index)
            .map_or(self.total_records, |checkpoint| checkpoint.pointer.row_number);
        let rows = self.read_rows(path, start.pointer.row_number, end - start.pointer.row_number)?;

        let mut best: Option<(usize, BasePair)> = None;
        for (offset, row) in rows.iter().enumerate() {
            if self.contig_ranks.get(&row.contig).copied() != Some(contig_rank) {
                continue;
            }
            let row_number = start.pointer.row_number + offset;
            let distance = row.position.as_u64().abs_diff(position.as_u64());
            let replace = best.is_none_or(|(_, best_position)| {
                let best_distance = best_position.as_u64().abs_diff(position.as_u64());
                distance < best_distance || (distance == best_distance && row.position < best_position)
            });
            if replace {
                best = Some((row_number, row.position));
            }
        }

        if let Some(boundary) = self.checkpoints.get(boundary_index)
            && boundary.contig_rank == contig_rank
        {
            let distance = boundary.position.as_u64().abs_diff(position.as_u64());
            let replace = best.is_none_or(|(_, best_position)| {
                let best_distance = best_position.as_u64().abs_diff(position.as_u64());
                distance < best_distance || (distance == best_distance && boundary.position < best_position)
            });
            if replace {
                best = Some((boundary.pointer.row_number, boundary.position));
            }
        }

        Ok(best.map(|(row, _)| row))
    }
}

/// Scans the whole file, checkpointing every [`INDEX_STRIDE`] records.
///
/// Blocking and proportional to file size — this belongs on the I/O worker.
pub fn build(path: &Path) -> io::Result<LineIndex> {
    build_with_stride(path, INDEX_STRIDE)
}

/// [`build`] with the stride named, so a test can force a small fixture to
/// produce several checkpoints and actually exercise seeking.
fn build_with_stride(path: &Path, stride: usize) -> io::Result<LineIndex> {
    let mut source = Source::open(path)?;
    vcf::io::Reader::new(source.as_buf_read()).read_header()?;

    let mut contig_ranks: HashMap<String, usize> = HashMap::new();
    let mut checkpoints = Vec::new();
    let mut record = vcf::Record::default();
    let mut total_records: usize = 0;
    let mut previous: Option<(usize, BasePair)> = None;
    let mut disorder: Option<String> = None;
    loop {
        // Sampled before the read, so a checkpoint names where its record starts.
        // Only taken on stride boundaries: on a plain file this is a seek syscall.
        let start = total_records
            .is_multiple_of(stride)
            .then(|| source.position())
            .transpose()?;

        if vcf::io::Reader::new(source.as_buf_read()).read_record(&mut record)? == 0 {
            break;
        }
        let record_contig = record.reference_sequence_name();
        let next_rank = contig_ranks.len();
        let contig_rank = *contig_ranks
            .entry(record_contig.to_string())
            .or_insert_with(|| next_rank);

        let (_, position) = coordinate_from_record(&record);
        // The lookup binary-searches on (rank, position), so record order is the
        // invariant that must hold. Report the first breach rather than failing:
        // the row-offset half of the index stays correct either way, and refusing
        // to open a file over a coordinate quirk is worse than a warning.
        if disorder.is_none()
            && let Some(previous) = previous
            && previous > (contig_rank, position)
        {
            disorder = Some(if previous.0 == contig_rank {
                format!(
                    "{record_contig} positions are out of order at record {} ({} after {})",
                    total_records + 1,
                    position.to_display(),
                    previous.1.to_display()
                )
            } else {
                format!(
                    "{record_contig} records are interleaved with another contig's at record {}",
                    total_records + 1
                )
            });
        }
        previous = Some((contig_rank, position));

        if let Some(offset) = start {
            checkpoints.push(Checkpoint {
                pointer: RecordPointer {
                    row_number: total_records,
                    offset,
                },
                contig_rank,
                position,
            });
        }
        total_records += 1;
    }

    Ok(LineIndex {
        checkpoints,
        contig_ranks,
        total_records,
        disorder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer_app::vcf::ingest::read_preview;

    const FIXTURE: &str = "testdata/vcf/sample.vcf";
    const FIXTURE_GZ: &str = "testdata/vcf/sample.vcf.gz";
    /// Declares contigs by length, as clair3 does, so header order contradicts
    /// record order.
    const MISMATCHED_HEADER: &str = "testdata/vcf/header_order_mismatch.vcf";
    /// Positions that jump backwards within one contig.
    const UNSORTED: &str = "testdata/vcf/unsorted.vcf";

    /// The rows a sequential read produces — the oracle every indexed read is
    /// checked against. Built from `read_preview`, not from this module.
    fn sequential(path: &str) -> Vec<VcfRow> {
        read_preview(Path::new(path), usize::MAX).unwrap().rows
    }

    #[test]
    fn an_indexed_read_of_any_span_matches_a_sequential_read_of_the_same_span() {
        for path in [FIXTURE, FIXTURE_GZ] {
            let expected = sequential(path);
            // Stride 1 so every record is a checkpoint and every read seeks.
            let index = build_with_stride(Path::new(path), 1).unwrap();
            assert_eq!(index.total_records(), expected.len(), "{path} record count");

            for first in 0..expected.len() {
                for count in 0..=expected.len() - first {
                    assert_eq!(
                        index.read_rows(Path::new(path), first, count).unwrap(),
                        expected[first..first + count],
                        "{path} rows {first}..{}",
                        first + count
                    );
                }
            }
        }
    }

    #[test]
    fn a_read_starting_between_checkpoints_still_lands_on_the_right_record() {
        // Stride 2 puts odd-numbered records strictly after their checkpoint, so
        // reaching them requires skipping forward rather than seeking exactly.
        let expected = sequential(FIXTURE_GZ);
        let index = build_with_stride(Path::new(FIXTURE_GZ), 2).unwrap();
        for first in (1..expected.len()).step_by(2) {
            assert_eq!(
                index.read_rows(Path::new(FIXTURE_GZ), first, 1).unwrap(),
                expected[first..first + 1],
                "record {first} sits between checkpoints"
            );
        }
    }

    #[test]
    fn a_corrupted_offset_is_caught_rather_than_silently_returning_other_records() {
        let expected = sequential(FIXTURE);
        let mut index = build_with_stride(Path::new(FIXTURE), 1).unwrap();
        // Point the last checkpoint at the first record's offset. Proves the test
        // above reads through the offsets rather than re-deriving them.
        let first_offset = index.checkpoints[0].pointer.offset;
        let last = index.checkpoints.len() - 1;
        index.checkpoints[last].pointer.offset = first_offset;

        assert_ne!(
            index.read_rows(Path::new(FIXTURE), last, 1).unwrap(),
            expected[last..last + 1],
            "a wrong offset must change what is read"
        );
    }

    #[test]
    fn both_compressions_index_the_same_records_with_their_own_offset_kinds() {
        let plain = build_with_stride(Path::new(FIXTURE), 1).unwrap();
        let compressed = build_with_stride(Path::new(FIXTURE_GZ), 1).unwrap();
        assert_eq!(plain.total_records(), compressed.total_records());
        assert_eq!(
            plain.read_rows(Path::new(FIXTURE), 0, plain.total_records()).unwrap(),
            compressed
                .read_rows(Path::new(FIXTURE_GZ), 0, compressed.total_records())
                .unwrap()
        );
        assert!(
            plain
                .checkpoints
                .iter()
                .all(|checkpoint| matches!(checkpoint.pointer.offset, RecordOffset::Plain(_)))
        );
        assert!(
            compressed
                .checkpoints
                .iter()
                .all(|checkpoint| matches!(checkpoint.pointer.offset, RecordOffset::Bgzf(_)))
        );
    }

    #[test]
    fn reading_past_the_end_stops_rather_than_erroring() {
        let index = build(Path::new(FIXTURE)).unwrap();
        let total = index.total_records();
        assert_eq!(index.read_rows(Path::new(FIXTURE), 0, total + 50).unwrap().len(), total);
        assert!(index.read_rows(Path::new(FIXTURE), total + 10, 5).unwrap().is_empty());
    }

    #[test]
    fn a_missing_file_is_an_error_rather_than_a_panic() {
        assert!(build(Path::new("testdata/vcf/does_not_exist.vcf")).is_err());
    }

    fn linear_nearest(rows: &[VcfRow], contig: &str, position: BasePair) -> Option<usize> {
        let mut best: Option<(usize, BasePair)> = None;
        for (row_number, row) in rows.iter().enumerate() {
            if row.contig != contig {
                continue;
            }
            let distance = row.position.as_u64().abs_diff(position.as_u64());
            let replace = best.is_none_or(|(_, best_position)| {
                let best_distance = best_position.as_u64().abs_diff(position.as_u64());
                distance < best_distance || (distance == best_distance && row.position < best_position)
            });
            if replace {
                best = Some((row_number, row.position));
            }
        }
        best.map(|(row, _)| row)
    }

    #[test]
    fn checkpoints_keep_the_coordinate_of_the_row_they_start() {
        for path in [FIXTURE, FIXTURE_GZ] {
            let expected = sequential(path);
            let index = build_with_stride(Path::new(path), 2).unwrap();
            assert!(
                index.checkpoints.len() > 1,
                "the forced stride must create several checkpoints"
            );

            for checkpoint in &index.checkpoints {
                let row = &expected[checkpoint.pointer.row_number];
                assert_eq!(
                    checkpoint.contig_rank, index.contig_ranks[&row.contig],
                    "{path} row {} contig",
                    checkpoint.pointer.row_number
                );
                assert_eq!(
                    checkpoint.position, row.position,
                    "{path} row {} position",
                    checkpoint.pointer.row_number
                );
            }
        }
    }

    #[test]
    fn nearest_row_matches_a_linear_oracle_across_both_compressions() {
        for path in [FIXTURE, FIXTURE_GZ] {
            let expected = sequential(path);
            let index = build_with_stride(Path::new(path), 2).unwrap();

            for contig in ["chr1", "chr2"] {
                let positions: Vec<u64> = expected
                    .iter()
                    .filter(|row| row.contig == contig)
                    .map(|row| row.position.as_u64())
                    .collect();
                let mut queries = vec![positions[0].saturating_sub(1), *positions.last().unwrap() + 1];
                let mut sampled_rows = vec![0, positions.len() / 2, positions.len() - 1];
                if positions.len() > 1 {
                    sampled_rows.push(1);
                }
                for row in sampled_rows {
                    queries.push(positions[row]);
                    if row + 1 < positions.len() {
                        queries.push(positions[row] + (positions[row + 1] - positions[row]) / 2);
                    }
                }
                queries.sort_unstable();
                queries.dedup();

                for value in queries {
                    let position = BasePair(value);
                    let actual = index.nearest_row(Path::new(path), contig, position).unwrap();
                    let oracle = linear_nearest(&expected, contig, position);
                    assert_eq!(actual, oracle, "{path} {contig}:{value}");
                    if let Some(row_number) = actual {
                        assert_eq!(
                            expected[row_number].contig, contig,
                            "lookup crossed contigs at {path} {contig}:{value}"
                        );
                    }
                }
            }

            for contig in ["chr0", "chr3"] {
                assert_eq!(
                    index.nearest_row(Path::new(path), contig, BasePair(200)).unwrap(),
                    None,
                    "{path} must not substitute a neighbouring contig for {contig}"
                );
            }
        }
    }

    #[test]
    fn nearest_row_handles_exact_midpoint_and_clamping_rules() {
        for path in [FIXTURE, FIXTURE_GZ] {
            let expected = sequential(path);
            let index = build_with_stride(Path::new(path), 2).unwrap();
            let row_at = |contig: &str, position: u64| {
                expected
                    .iter()
                    .position(|row| row.contig == contig && row.position == BasePair(position))
            };

            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(174)).unwrap(),
                row_at("chr1", 174),
                "exact hits should return their row"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(161)).unwrap(),
                row_at("chr1", 149),
                "the closest side should win a non-tie midpoint"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(124)).unwrap(),
                row_at("chr1", 99),
                "equal distances should resolve to the lower position"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(0)).unwrap(),
                row_at("chr1", 99),
                "before the first row should clamp to the first row"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(1_000)).unwrap(),
                row_at("chr1", 899),
                "after the last row should clamp to the last row on that contig"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "chr1", BasePair(199)).unwrap(),
                row_at("chr1", 174),
                "a different contig's exact coordinate must not win"
            );
        }
    }

    #[test]
    fn nearest_row_propagates_a_read_error() {
        let index = build_with_stride(Path::new(FIXTURE), 2).unwrap();
        assert!(
            index
                .nearest_row(Path::new("testdata/vcf/does_not_exist.vcf"), "chr1", BasePair(0))
                .is_err()
        );
    }

    #[test]
    fn nearest_row_accepts_reciprocal_case_insensitive_contig_aliases() {
        for path in [FIXTURE, FIXTURE_GZ] {
            let index = build_with_stride(Path::new(path), 2).unwrap();
            let exact = index.nearest_row(Path::new(path), "chr1", BasePair(174)).unwrap();

            assert_eq!(
                index.nearest_row(Path::new(path), "CHR1", BasePair(174)).unwrap(),
                exact,
                "case-insensitive exact names must resolve"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "1", BasePair(174)).unwrap(),
                exact,
                "bare names must resolve to chr-prefixed records"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "CHr1", BasePair(174)).unwrap(),
                exact,
                "mixed-case aliases must resolve"
            );
            assert_eq!(
                index.nearest_row(Path::new(path), "missing", BasePair(174)).unwrap(),
                None,
                "an absent name must not cross to a present contig"
            );
        }
    }

    #[test]
    fn an_ambiguous_alias_is_refused_instead_of_crossing_contigs() {
        let path = Path::new(FIXTURE);
        let mut index = build_with_stride(path, 2).unwrap();
        let chr2_rank = index.contig_ranks["chr2"];
        index.contig_ranks.insert("CHR1".to_string(), chr2_rank);

        assert_eq!(
            index.nearest_row(path, "1", BasePair(174)).unwrap(),
            None,
            "an alias matching two different file contigs must be rejected"
        );
    }

    /// Ranking from `##contig` order silently mis-sorts real files: clair3 emits
    /// its declarations by contig length, so `chrX` is declared between `chr7`
    /// and `chr9` while the records themselves run in numeric order. Ranks must
    /// come from the records.
    #[test]
    fn contig_ranks_follow_record_order_not_the_header_declaration() {
        let path = Path::new(MISMATCHED_HEADER);
        let index = build_with_stride(path, 2).unwrap();
        let rows = index.read_rows(path, 0, index.total_records).unwrap();

        let ranks: Vec<usize> = rows.iter().map(|row| index.contig_ranks[&row.contig]).collect();
        assert!(
            ranks.windows(2).all(|pair| pair[0] <= pair[1]),
            "record order must be non-decreasing in rank, got {ranks:?}"
        );
        assert!(index.disorder().is_none(), "this fixture's records are sorted");

        // Every row must be findable at its own coordinate — the property the
        // binary search needs, and the one header ranking breaks.
        for (row_number, row) in rows.iter().enumerate() {
            assert_eq!(
                index.nearest_row(path, &row.contig, row.position).unwrap(),
                Some(row_number),
                "{}:{} must find its own row",
                row.contig,
                row.position.to_display()
            );
        }
    }

    /// A file the lookup cannot serve is still worth opening, so the breach is
    /// reported rather than refused.
    #[test]
    fn out_of_order_records_are_reported_without_failing_the_build() {
        let index = build_with_stride(Path::new(FIXTURE), 2).unwrap();
        assert!(index.disorder().is_none(), "the sorted fixture must not warn");

        let unsorted = build_with_stride(Path::new(UNSORTED), 2).unwrap();
        let disorder = unsorted.disorder().expect("descending positions must be reported");
        assert!(
            disorder.contains("chr1") && disorder.contains("out of order"),
            "the warning must name the contig and the breach: {disorder:?}"
        );
        assert_eq!(
            unsorted.total_records, 4,
            "a disordered file still indexes every row for scrolling"
        );
    }
}
