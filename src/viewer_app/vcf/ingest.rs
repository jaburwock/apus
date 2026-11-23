//! VCF reading. Every noodles type stays inside this module; the viewer above
//! sees only display strings and [`BasePair`].

use std::io;
use std::path::Path;
use std::sync::Arc;

use noodles::vcf;

use crate::basepair::BasePair;
use crate::vcf_file::alternate_sample_indices;

/// The seven fixed VCF fields, in file order. `INFO` is absent because the
/// header's `##INFO` declarations expand into columns of their own.
pub const FIXED_COLUMNS: [&str; 7] = ["CHROM", "POS", "ID", "REF", "ALT", "QUAL", "FILTER"];

const SAMPLES_HEADING: &str = "Samples";

/// The widest an INFO column may be drawn. A `CLNREVSTAT` runs to ~50
/// characters and would otherwise eat the screen on its own.
const INFO_COLUMN_WIDTH_CAP: u16 = 12;

/// Width for a declared key the preview never saw a value for. The header
/// defines the column set, so such a column exists and must still be readable.
const INFO_COLUMN_DEFAULT_WIDTH: u16 = 6;

/// The visible heading used when a file's header declares no usable `##INFO` lines.
const RAW_INFO_COLUMN: &str = "INFO";

/// Extracts the whole raw INFO blob for the fallback column.
///
/// A NUL cannot occur in a real INFO key, including the empty key a malformed
/// `=5` field produces. It stays distinct from [`ABSENT_COLUMN_ID`]: the two
/// sentinels deliberately mean opposite things when a cell is requested.
const RAW_INFO_COLUMN_ID: &str = "\u{0}raw-info";

/// Stands in for a column index the table no longer has, so the window still
/// yields one cell per index asked for.
///
/// Deliberately not the empty string: a malformed blob can carry an empty key
/// (`=5` splits to `("", "5")`), and an empty-id placeholder would match it and
/// draw another field's value in a column that should be blank.
const ABSENT_COLUMN_ID: &str = "\u{0}";

/// One INFO key's column: what to pull out of the blob, and how wide to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoColumn {
    /// The `##INFO` ID matched against raw-blob keys, or a private sentinel for
    /// a synthetic column.
    pub id: String,
    /// Measured from the preview and then fixed. Re-measuring as chunks arrive
    /// would make a column's width depend on scroll position, so a value wider
    /// than the preview saw is truncated instead.
    pub width: u16,
}

/// One variant, already reduced to what the table draws.
///
/// **Every field must stay bounded.** Two chunks of `chunks::CHUNK_ROWS` rows are
/// resident at once, so a field that scales with the file's width — an uncapped
/// INFO, or `FORMAT`/per-sample genotypes, which a 50k-sample VCF makes enormous —
/// multiplies straight into the buffer. Only the alternate-sample count is
/// retained. `VCFViewer` measures the preview against `ROW_BYTES_BUDGET` to
/// catch a row that grows anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcfRow {
    pub contig: String,
    /// 0-based internally, like every other position in the app (invariant 1).
    pub position: BasePair,
    pub id: String,
    pub reference_bases: String,
    pub alternate_bases: String,
    pub quality: String,
    pub filter: String,
    alternate_sample_count: usize,
    /// The raw INFO field, stored whole and split per column at draw.
    ///
    /// Kept raw rather than pre-split into a per-key map for two reasons. It
    /// keeps this row's cost the file's own INFO size instead of that size times
    /// the column count, which is what `ROW_BYTES_BUDGET` measures. And it keeps
    /// [`row_from_record`] header-free, which [`super::line_index::read_rows`]
    /// requires: it seeks to a checkpoint and reads records without ever reading
    /// a header.
    pub info: String,
}

impl VcfRow {
    /// Heap bytes this row holds beyond its own struct, for the budget check.
    pub fn heap_bytes(&self) -> usize {
        self.contig.len()
            + self.id.len()
            + self.reference_bases.len()
            + self.alternate_bases.len()
            + self.quality.len()
            + self.filter.len()
            + self.info.len()
    }

    /// The row's fixed cells, in [`FIXED_COLUMNS`] order.
    fn fixed_cells(&self) -> [String; FIXED_COLUMNS.len()] {
        [
            self.contig.clone(),
            self.position.to_display().to_string(),
            self.id.clone(),
            self.reference_bases.clone(),
            self.alternate_bases.clone(),
            self.quality.clone(),
            self.filter.clone(),
        ]
    }

    /// The cells for a window of the table's columns, named by declared-column
    /// index: fixed fields, optional Samples count, then INFO in header order.
    ///
    /// Takes indices rather than a start offset because the visible set need not
    /// be contiguous once columns can be hidden. Still a *window*, not the whole
    /// set: the work stays proportional to what is on screen, so a file declaring
    /// hundreds of INFO keys does not cost hundreds of extracted values per row
    /// per frame. The INFO blob is scanned once for whichever keys are asked for,
    /// however many that is — one scan, not one per column.
    ///
    /// An index past the last column yields an empty cell rather than panicking,
    /// so a stale window cannot take the table down mid-frame. `wanted` is in
    /// ascending order, as the table's own column order; it is partitioned rather
    /// than searched, so an unsorted list would silently split in the wrong place.
    pub fn cells(&self, wanted: &[usize], table: &VcfTable) -> Vec<String> {
        debug_assert!(
            wanted.windows(2).all(|pair| pair[0] < pair[1]),
            "column indices must be ascending and distinct, got {wanted:?}"
        );
        let fixed = self.fixed_cells();
        let info_start = table.info_start();
        let split = wanted.partition_point(|column| *column < info_start);
        let (leading_wanted, info_wanted) = wanted.split_at(split);

        let mut cells: Vec<String> = leading_wanted
            .iter()
            .map(|column| {
                fixed
                    .get(*column)
                    .cloned()
                    .unwrap_or_else(|| self.alternate_sample_count.to_string())
            })
            .collect();

        // One scan of the blob for the whole window, so the sub-slice handed to
        // `info_values` carries the wanted keys in the order they are drawn.
        //
        // An out-of-range index becomes a column that matches no key rather than
        // being dropped: dropping it would return fewer cells than the caller
        // asked for, shifting every later value one column left — the same
        // misalignment as a double-applied offset, arrived at from the other end.
        let asked: Vec<InfoColumn> = info_wanted
            .iter()
            .map(|column| {
                table
                    .info_columns
                    .get(column - info_start)
                    .cloned()
                    .unwrap_or(InfoColumn {
                        id: ABSENT_COLUMN_ID.to_string(),
                        width: 0,
                    })
            })
            .collect();
        if !asked.is_empty() {
            cells.extend(info_values(&self.info, &asked));
        }
        cells
    }
}

/// Pulls each wanted key's value out of a raw INFO blob, in the order asked for.
///
/// INFO is `;`-separated `KEY=VALUE` pairs, with a bare `KEY` for a flag. Fields
/// are unordered and optional, so a key is looked up rather than taken by
/// position, and one absent from the blob yields an empty cell.
///
/// Pipe-joined values (ClinVar's `MC`, VEP's `CSQ`) stay in one cell; splitting
/// those into subcolumns is deliberately a later step.
fn info_values(blob: &str, wanted: &[InfoColumn]) -> Vec<String> {
    let mut values: Vec<String> = wanted
        .iter()
        .map(|column| {
            if column.id == RAW_INFO_COLUMN_ID {
                blob.to_string()
            } else {
                String::new()
            }
        })
        .collect();
    for field in blob.split(';') {
        let (key, value) = match field.split_once('=') {
            Some((key, value)) => (key, value),
            // A bare key is a flag: its presence is the whole value.
            None => (field, "present"),
        };
        for (slot, column) in values.iter_mut().zip(wanted) {
            if column.id != RAW_INFO_COLUMN_ID && column.id == key {
                *slot = value.to_string();
            }
        }
    }
    values
}

/// A loaded VCF's header facts and the rows read so far.
#[derive(Debug, Clone, Default)]
pub struct VcfTable {
    /// Header contigs retained for ingress-equivalence tests.
    #[cfg(test)]
    pub(crate) contigs: Vec<String>,
    pub rows: Vec<VcfRow>,
    pub(super) sample_names: Arc<[String]>,
    /// One column per `##INFO` key, in header order, measured against `rows`.
    /// A single raw `INFO` column when the header declares none.
    pub info_columns: Vec<InfoColumn>,
    /// Set when `rows` stopped at the preview limit rather than end of file.
    pub truncated: bool,
}

impl VcfTable {
    /// Mean heap bytes per row, the sample the buffer's memory bound is checked
    /// against. Zero for an empty table.
    pub fn mean_row_bytes(&self) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        self.rows.iter().map(VcfRow::heap_bytes).sum::<usize>() / self.rows.len()
    }

    fn info_start(&self) -> usize {
        FIXED_COLUMNS.len() + usize::from(self.sample_names.len() > 1)
    }

    pub fn total_columns(&self) -> usize {
        self.info_start() + self.info_columns.len()
    }

    pub fn heading(&self, column: usize) -> &str {
        FIXED_COLUMNS
            .get(column)
            .copied()
            .or_else(|| self.extra_column(column).map(|(heading, _)| heading))
            .unwrap_or_default()
    }

    pub(super) fn extra_column(&self, column: usize) -> Option<(&str, u16)> {
        let info_start = self.info_start();
        if column == FIXED_COLUMNS.len() && column < info_start {
            let digits = self.sample_names.len().ilog10() as u16 + 1;
            return Some((SAMPLES_HEADING, digits.max(SAMPLES_HEADING.len() as u16)));
        }
        let info = self.info_columns.get(column.checked_sub(info_start)?)?;
        let heading = if info.id == RAW_INFO_COLUMN_ID {
            RAW_INFO_COLUMN
        } else {
            &info.id
        };
        Some((heading, info.width))
    }
}

/// Builds the INFO column set: one per declared key, in header order, each
/// measured against `rows`.
///
/// The **header** decides which columns exist, so a key none of `rows` happens
/// to carry still gets one — the column set is a property of the file, not of
/// whichever records were sampled. Only the widths come from `rows`.
///
/// Falls back to a single raw `INFO` column when the header declares nothing
/// usable. Positional `Column1`/`Column2` headings are deliberately not used:
/// INFO fields are unordered and optional, so column N is a different key on
/// different rows and such a heading would lie.
fn info_columns(mut columns: Vec<InfoColumn>, rows: &[VcfRow]) -> Vec<InfoColumn> {
    if columns.is_empty() {
        let width = rows.iter().map(|row| display_width(&row.info)).max().unwrap_or(0);
        return vec![InfoColumn {
            id: RAW_INFO_COLUMN_ID.to_string(),
            width: width.clamp(INFO_COLUMN_DEFAULT_WIDTH, INFO_COLUMN_WIDTH_CAP),
        }];
    }

    for column in &mut columns {
        column.width = INFO_COLUMN_DEFAULT_WIDTH;
    }
    measure_info_widths(rows, &mut columns);
    columns
}

/// Sizes each column to the widest value `rows` holds for it, capped.
///
/// Derives everything from the rows it is handed — it must never consult how
/// many records the preview reads, so that retuning that count cannot change a
/// column's width. Width is a function of the *values* present, not of how many
/// rows were sampled.
///
/// A column with no value in `rows` keeps [`INFO_COLUMN_DEFAULT_WIDTH`] rather
/// than collapsing to nothing. Widths are measured once, from the preview; later
/// chunks truncate wider values rather than re-measuring, which would make the
/// layout shift with scroll position.
fn measure_info_widths(rows: &[VcfRow], columns: &mut [InfoColumn]) {
    let mut widest = vec![0u16; columns.len()];
    for row in rows {
        for (seen, value) in widest.iter_mut().zip(info_values(&row.info, columns)) {
            *seen = (*seen).max(display_width(&value));
        }
    }
    for (column, seen) in columns.iter_mut().zip(widest) {
        if seen > 0 {
            column.width = seen.clamp(INFO_COLUMN_DEFAULT_WIDTH, INFO_COLUMN_WIDTH_CAP);
        }
    }
}

/// Terminal columns a value occupies, saturating at the cap it will be clamped
/// to anyway. Counts characters, not bytes, so a multi-byte value is not
/// over-measured.
fn display_width(value: &str) -> u16 {
    u16::try_from(value.chars().count()).unwrap_or(u16::MAX)
}

/// Reads the header and up to `limit` records from the start of the file.
///
/// The fast path: no index needed, so it works on a plain `.vcf` as readily as a
/// `.vcf.gz`, and populates the table synchronously the moment a VCF is set.
pub fn read_preview(path: &Path, limit: usize) -> io::Result<VcfTable> {
    let reader = vcf::io::reader::Builder::default().build_from_path(path)?;
    read_table(reader, limit)
}

#[cfg(feature = "resident-demo")]
pub(super) fn read_bgzf(bytes: &[u8]) -> io::Result<VcfTable> {
    read_table(vcf::io::Reader::new(noodles::bgzf::io::Reader::new(bytes)), usize::MAX)
}

fn read_table(mut reader: vcf::io::Reader<impl io::BufRead>, limit: usize) -> io::Result<VcfTable> {
    let header = reader.read_header()?;

    #[cfg(test)]
    let contigs = header.contigs().keys().map(|name| name.to_string()).collect();
    // `Infos` is an `IndexMap`, so this is the order the header declares.
    let header_columns: Vec<InfoColumn> = header
        .infos()
        .keys()
        .map(|id| InfoColumn {
            id: id.to_string(),
            width: INFO_COLUMN_DEFAULT_WIDTH,
        })
        .collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    for result in reader.records() {
        if rows.len() == limit {
            // One record past the limit distinguishes "more to come" from a file
            // that happens to hold exactly `limit` records.
            truncated = true;
            break;
        }
        rows.push(row_from_record(&result?));
    }

    // Measured against the rows just read — the preview is the sample, one
    // concept at one size, so nothing here consults how many records that was.
    let info_columns = info_columns(header_columns, &rows);

    Ok(VcfTable {
        #[cfg(test)]
        contigs,
        rows,
        sample_names: header.sample_names().iter().cloned().collect(),
        info_columns,
        truncated,
    })
}

/// Queries one region through the index for ingress-equivalence tests.
#[cfg(test)]
pub(crate) fn query_window(path: &Path, contig: &str, start: BasePair, end: BasePair) -> io::Result<Vec<VcfRow>> {
    use noodles::core::{Position, Region};

    let mut reader = vcf::io::indexed_reader::Builder::default().build_from_path(path)?;
    let header = reader.read_header()?;

    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid query {what}"));
    let region_start = Position::try_from(start.as_u64() as usize + 1).map_err(|_| invalid("start"))?;
    let region_end = Position::try_from(end.as_u64() as usize).map_err(|_| invalid("end"))?;
    let region = Region::new(contig, region_start..=region_end);

    reader
        .query(&header, &region)?
        .records()
        .map(|result| result.map(|record| row_from_record(&record)))
        .collect()
}

/// Reduces one record to display strings, converting POS at the boundary.
///
/// Shared with `line_index` so indexed rows are built exactly as preview rows
/// are — which is what makes comparing the two a test of the offsets alone.
pub(super) fn row_from_record(record: &vcf::Record) -> VcfRow {
    let (contig, position) = coordinate_from_record(record);

    let missing_or = |s: &str| if s.is_empty() { ".".to_string() } else { s.to_string() };

    VcfRow {
        contig,
        position,
        id: missing_or(record.ids().as_ref()),
        reference_bases: missing_or(record.reference_bases()),
        alternate_bases: missing_or(record.alternate_bases().as_ref()),
        quality: record
            .quality_score()
            .and_then(|result| result.ok())
            .map_or_else(|| ".".to_string(), |score| score.to_string()),
        filter: missing_or(record.filters().as_ref()),
        alternate_sample_count: alternate_sample_indices(&record.samples()).count(),
        info: record.info().as_ref().to_string(),
    }
}

/// Extracts the coordinate shared by preview rows, indexed rows, and sparse
/// line-index checkpoints. External VCF positions become 0-based here.
pub(super) fn coordinate_from_record(record: &vcf::Record) -> (String, BasePair) {
    let position = record
        .variant_start()
        .and_then(|result| result.ok())
        .map_or(BasePair(0), |p| BasePair::from_1based(usize::from(p) as u64));

    (record.reference_sequence_name().to_string(), position)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "testdata/vcf/sample.vcf";
    const FIXTURE_GZ: &str = "testdata/vcf/sample.vcf.gz";
    const RAW_INFO_FIXTURE: &str = "testdata/vcf/raw_info.vcf";
    const RAW_INFO_BLOB: &str = "DP=10;AF=0.5";

    #[test]
    fn a_records_position_is_stored_zero_based_and_displays_as_written() {
        let table = read_preview(Path::new(FIXTURE), 100).unwrap();
        let first = &table.rows[0];
        // The fixture's first record is at 1-based POS 100.
        assert_eq!(first.position, BasePair(99), "POS must convert to 0-based on ingest");
        assert_eq!(first.position.to_display(), 100, "and round-trip back for display");
    }

    #[test]
    fn the_same_records_are_read_whether_or_not_the_file_is_compressed() {
        let plain = read_preview(Path::new(FIXTURE), 100).unwrap();
        let compressed = read_preview(Path::new(FIXTURE_GZ), 100).unwrap();
        assert_eq!(plain.rows, compressed.rows);
        assert_eq!(plain.contigs, compressed.contigs);
    }

    #[test]
    fn the_preview_stops_at_the_limit_and_says_so() {
        let table = read_preview(Path::new(FIXTURE), 2).unwrap();
        assert_eq!(table.rows.len(), 2);
        assert!(table.truncated, "stopping early must be reported as truncated");
    }

    #[test]
    fn samples_count_each_genotype_with_a_called_alternate_allele_once() {
        let data = concat!(
            "##fileformat=VCFv4.2\n",
            "#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\tFORMAT\ta\tb\tc\td\te\tf\tg\th\ti\n",
            "chr1\t1\t.\tA\tC,T\t.\t.\t.\tGT:DP\t0/0:12\t0|1:9\t./1:15\t./.:0\t2:8\t0|2/2:10\t.\t0/.:1\t0|0:4\n",
            "chr1\t2\t.\tA\tC\t.\t.\t.\tDP\t12\t9\t15\t0\t8\t10\t.\t1\t4\n",
        );
        let table = read_table(vcf::io::Reader::new(data.as_bytes()), 10).unwrap();
        let samples = FIXED_COLUMNS.len();
        assert_eq!(table.rows[0].cells(&[samples], &table), ["4"]);
        assert_eq!(table.rows[1].cells(&[samples], &table), ["0"]);
    }

    #[test]
    fn reading_past_the_end_is_not_truncated() {
        let table = read_preview(Path::new(FIXTURE), 10_000).unwrap();
        assert!(!table.rows.is_empty());
        assert!(!table.truncated, "reaching end of file is not truncation");
    }

    #[test]
    fn contigs_come_from_the_header_so_a_query_has_a_name_to_use() {
        let table = read_preview(Path::new(FIXTURE), 1).unwrap();
        assert!(
            table.contigs.contains(&"chr1".to_string()),
            "header contigs drive the indexed query, got {:?}",
            table.contigs
        );
    }

    #[test]
    fn an_indexed_query_returns_only_records_inside_the_region() {
        let (start, end) = (BasePair(0), BasePair(200));
        let rows = query_window(Path::new(FIXTURE_GZ), "chr1", start, end).unwrap();
        assert!(!rows.is_empty(), "the fixture must have records in chr1:1-200");
        assert!(
            rows.iter().all(|row| row.position >= start && row.position < end),
            "every returned record must lie inside the queried region"
        );
    }

    #[test]
    fn the_indexed_and_sequential_paths_agree_on_the_same_records() {
        let preview = read_preview(Path::new(FIXTURE_GZ), 10_000).unwrap();
        let queried = query_window(Path::new(FIXTURE_GZ), "chr1", BasePair(0), BasePair(1_000_000)).unwrap();
        let sequential: Vec<&VcfRow> = preview.rows.iter().filter(|row| row.contig == "chr1").collect();
        assert_eq!(
            queried.iter().collect::<Vec<_>>(),
            sequential,
            "an indexed query must return what a sequential read of the same span does"
        );
    }

    #[test]
    fn a_missing_file_is_an_error_rather_than_a_panic() {
        assert!(read_preview(Path::new("testdata/vcf/does_not_exist.vcf"), 10).is_err());
    }

    /// The fixture's `##INFO` IDs, read from the file itself rather than from the
    /// code under test, so the two can disagree.
    fn declared_info_ids(path: &str) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("##INFO=<ID=")?.split(',').next().map(str::to_string))
            .collect()
    }

    fn column(id: &str, width: u16) -> InfoColumn {
        InfoColumn {
            id: id.to_string(),
            width,
        }
    }

    #[test]
    fn the_column_set_comes_from_the_header_not_from_the_records() {
        let table = read_preview(Path::new(FIXTURE), 100).unwrap();
        let declared = declared_info_ids(FIXTURE);
        assert!(!declared.is_empty(), "the fixture must declare INFO keys to test this");

        let ids: Vec<String> = table.info_columns.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, declared, "every declared key gets a column, in header order");

        // The claim that separates a header-driven column set from a
        // records-driven one. The fixture declares `UNUSED` precisely so a key no
        // record carries is present here, as ClinVar declares 35 and carries 16.
        let carried_by_none: Vec<&String> = declared
            .iter()
            .filter(|id| table.rows.iter().all(|row| !row.info.contains(id.as_str())))
            .collect();
        assert!(
            !carried_by_none.is_empty(),
            "the fixture must declare a key no record carries, or this proves nothing"
        );
        for absent in carried_by_none {
            assert!(
                ids.contains(absent),
                "{absent} is declared but in no record, and still needs a column"
            );
        }
    }

    #[test]
    fn info_widths_follow_values_within_the_column_bounds() {
        let middle_width = INFO_COLUMN_DEFAULT_WIDTH.saturating_add(2).min(INFO_COLUMN_WIDTH_CAP);
        let middle_value = "x".repeat(usize::from(middle_width));
        let rows = vec![
            row_with_info(&format!("SHORT=ab;MID={middle_value};LONG=abcdefghijklmnopqrstuvwxyz")),
            row_with_info("SHORT=a;MID=1;LONG=abc"),
        ];
        let columns = info_columns(
            vec![
                column("SHORT", 0),
                column("MID", 0),
                column("LONG", 0),
                column("NEVER", 0),
            ],
            &rows,
        );

        assert!(
            columns
                .iter()
                .all(|column| { (INFO_COLUMN_DEFAULT_WIDTH..=INFO_COLUMN_WIDTH_CAP).contains(&column.width) })
        );
        assert_eq!(
            columns[1].width,
            middle_width.clamp(INFO_COLUMN_DEFAULT_WIDTH, INFO_COLUMN_WIDTH_CAP),
            "a value between the bounds keeps its measured width"
        );
        assert_eq!(columns[2].width, INFO_COLUMN_WIDTH_CAP, "a wide value stops at the cap");

        let unseen = info_columns(vec![column("NEVER", 0)], &rows);
        assert!(
            unseen[0].width >= INFO_COLUMN_DEFAULT_WIDTH && unseen[0].width <= INFO_COLUMN_WIDTH_CAP,
            "an unseen key's width must stay within the configured bounds"
        );

        let mut repeated_rows = rows.clone();
        repeated_rows.extend(rows.iter().cloned().cycle().take(100));
        assert_eq!(
            info_columns(columns.clone(), &repeated_rows),
            columns,
            "repeating the same sampled values cannot change measured widths"
        );
    }

    #[test]
    fn a_header_with_no_info_declarations_exposes_its_raw_blob() {
        let table = read_preview(Path::new(RAW_INFO_FIXTURE), 100).unwrap();
        let fallback = FIXED_COLUMNS.len();

        assert_eq!(table.heading(fallback), RAW_INFO_COLUMN);
        assert_eq!(
            table.rows[0].cells(&[fallback], &table),
            vec![RAW_INFO_BLOB],
            "the fallback column must show its row's complete INFO blob"
        );
    }

    #[test]
    fn info_values_match_requested_keys_without_splitting_values() {
        let wanted = [
            column("DP", 6),
            column("AF", 6),
            column("DB", 6),
            column("MC", 30),
            column("GONE", 6),
        ];
        let expected = ["30", "0.5", "present", "SO:0001583|missense_variant", ""];

        for blob in [
            "DP=30;AF=0.5;DB;MC=SO:0001583|missense_variant",
            "MC=SO:0001583|missense_variant;DB;AF=0.5;DP=30",
        ] {
            assert_eq!(info_values(blob, &wanted), expected, "INFO order in {blob:?}");
        }
        assert_eq!(
            info_values("DP=30;MC=SO:0001583|missense_variant", &wanted),
            ["30", "", "", "SO:0001583|missense_variant", ""]
        );
    }

    /// Fed deliberately malformed blobs — the shapes a hand-written or truncated
    /// VCF produces — to prove the scanner never panics and never invents a value.
    #[test]
    fn arbitrary_and_malformed_blobs_are_read_without_panicking() {
        let wanted = [column("DP", 6), column("AF", 6), column("DB", 6)];
        for blob in [
            "",
            ";",
            ";;;;",
            "=",
            "==",
            "=5",
            "DP",
            "DP=",
            "DP=;AF=",
            "DP=1;;AF=2",
            "DP=1=2",
            "dp=1",
            "DPX=1",
            "ααα=βββ",
            "DP=ααα;AF=0.5",
            "DP=1;DP=2",
        ] {
            let values = info_values(blob, &wanted);
            assert_eq!(values.len(), wanted.len(), "{blob:?} must yield one cell per column");
        }

        // A repeated key takes the last occurrence rather than either panicking or
        // concatenating; `DPX`/`dp` must not satisfy the `DP` column.
        assert_eq!(info_values("DP=1;DP=2", &wanted)[0], "2");
        assert_eq!(info_values("DPX=1;dp=2", &wanted)[0], "");
        assert_eq!(
            info_values("DP=", &wanted)[0],
            "",
            "an empty value is empty, not present"
        );
    }

    /// The window need not be contiguous once columns can be hidden, so an
    /// arbitrary subset must come back as exactly those columns, in the order
    /// asked for, with nothing shifted into a neighbour's place.
    ///
    /// The oracle is the full row indexed by the same indices, built independently
    /// of the windowing path under test.
    #[test]
    fn requested_cells_preserve_column_identity_and_placeholders() {
        let table = VcfTable {
            info_columns: vec![column("DP", 6), column("AF", 6), column("DB", 6)],
            sample_names: Arc::from(["sample".to_string()]),
            ..VcfTable::default()
        };
        let row = VcfRow {
            contig: "ctg-101".to_string(),
            position: BasePair(8),
            id: "id-307".to_string(),
            reference_bases: "R-2".to_string(),
            alternate_bases: "A-503".to_string(),
            quality: "Q-61".to_string(),
            filter: "F-811".to_string(),
            alternate_sample_count: 0,
            info: "DP=31;AF=0.57;DB".to_string(),
        };
        let expected_all = [
            "ctg-101", "9", "id-307", "R-2", "A-503", "Q-61", "F-811", "31", "0.57", "present",
        ];
        let all: Vec<_> = (0..table.total_columns()).collect();
        assert_eq!(row.cells(&all, &table), expected_all);

        // Spans both sides of the fixed/INFO boundary, and both edges of each.
        for wanted in [
            vec![0],
            vec![9],
            vec![0, 7],
            vec![1, 3, 8],
            vec![0, 2, 4, 6, 8],
            vec![6, 7],
            vec![7, 8, 9],
        ] {
            let expected: Vec<&str> = wanted.iter().map(|column| expected_all[*column]).collect();
            assert_eq!(row.cells(&wanted, &table), expected, "for columns {wanted:?}");
        }

        assert_eq!(row.cells(&[7, 99], &table), ["31", ""]);
        assert!(row.cells(&[], &table).is_empty());

        // A malformed blob can produce an empty *key* (`=5` splits to
        // `("", "5")`), so the out-of-range placeholder must not be the empty
        // string, or it matches that field and draws its value in a column that
        // should be blank. This failed before `ABSENT_COLUMN_ID` existed.
        let malformed = VcfRow {
            info: "=5;DP=31".to_string(),
            ..row
        };
        assert_eq!(
            malformed.cells(&[7, 99], &table),
            ["31", ""],
            "an out-of-range column must stay empty even against an empty key"
        );
    }

    /// A row built from a raw INFO string, for tests about columns rather than
    /// about reading a file.
    fn row_with_info(info: &str) -> VcfRow {
        VcfRow {
            contig: "chr1".to_string(),
            position: BasePair(0),
            id: ".".to_string(),
            reference_bases: "A".to_string(),
            alternate_bases: "G".to_string(),
            quality: ".".to_string(),
            filter: ".".to_string(),
            alternate_sample_count: 0,
            info: info.to_string(),
        }
    }

    #[test]
    fn a_rows_measured_size_tracks_the_content_it_actually_holds() {
        let table = read_preview(Path::new(FIXTURE), 100).unwrap();
        let row = &table.rows[0];
        let expected = row.contig.len()
            + row.id.len()
            + row.reference_bases.len()
            + row.alternate_bases.len()
            + row.quality.len()
            + row.filter.len()
            + row.info.len();
        assert_eq!(row.heap_bytes(), expected, "every stored string must be counted");

        let mut widened = row.clone();
        widened.info.push_str(&"x".repeat(50));
        assert_eq!(
            widened.heap_bytes(),
            row.heap_bytes() + 50,
            "a wider field must raise the measurement"
        );
    }

    #[test]
    fn the_mean_row_size_is_the_average_over_the_rows_read() {
        let table = read_preview(Path::new(FIXTURE), 100).unwrap();
        let total: usize = table.rows.iter().map(VcfRow::heap_bytes).sum();
        assert_eq!(table.mean_row_bytes(), total / table.rows.len());
        assert_eq!(VcfTable::default().mean_row_bytes(), 0, "an empty table has no mean");
    }
}
