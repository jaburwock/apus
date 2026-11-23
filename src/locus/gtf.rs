//! Converts supported gzipped GTF exon rows into the sorted, indexed `FeatureIndex` consumed by
//! `locus`.
//!
//! GTF coordinates cross the ingest boundary in `parse_exon_row`: 1-based inclusive starts become
//! 0-based `BasePair`s, while ends remain exclusive, so downstream queries use internal half-open
//! ranges (invariants 1–3). Only complete `NM_` exon records with the attributes needed for a locus
//! key survive parsing; the result is sorted by contig and start before its contig ranges are built.
//!
//! `load_or_build` reads the compressed cache when it is valid, and rebuilds it from the GTF when the
//! cache reports invalid data. Other I/O or decoding errors are returned rather than being treated
//! as permission to rebuild. `load_only` is the deliberate no-build path behind `:load`; its cache
//! check stays inside the load operation so a race cannot turn refusal into an unexpected build.
//! Cache validity is checked by its format marker, serialized fields, and strand tags, not by
//! comparing the cache with GTF source metadata.
//!
//! The recovery command is constructed beside `load_only`: it uses the resolved, encoded path so the
//! advised `:load_index` line remains pasteable and cannot split a command batch.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::basepair::BasePair;

use super::{FeatureIndex, FeatureStrand, Locus};

/// Contig ranges into the sorted locus vector.
type LociByContig = HashMap<String, Range<usize>>;

const CACHE_MAGIC: &[u8; 8] = b"APUSGTF1";

/// Attributes used to build a locus key.
struct Attributes {
    gene_name: Option<String>,
    transcript_id: Option<String>,
    exon_number: Option<u16>,
}

/// Parses the attributes needed by this module.
fn parse_attributes(column: &str) -> Attributes {
    let mut gene_name = None;
    let mut transcript_id = None;
    let mut exon_number = None;

    for field in column.split(';') {
        let field = field.trim();
        let Some((key, rest)) = field.split_once(' ') else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        match key {
            "gene_name" => gene_name = Some(value.to_string()),
            "transcript_id" => transcript_id = Some(value.to_string()),
            "exon_number" => exon_number = value.parse().ok(),
            _ => {}
        }
    }

    Attributes {
        gene_name,
        transcript_id,
        exon_number,
    }
}

/// Parsed exon row before sorting and indexing.
struct RawEntry {
    contig: String,
    locus: Locus,
}

/// Parses one tab-split exon row.
fn parse_exon_row(fields: &[&str]) -> Option<RawEntry> {
    let contig = fields[0].to_string();
    let start: u64 = fields[3].parse().ok()?;
    let end: u64 = fields[4].parse().ok()?;

    let attrs = parse_attributes(fields[8]);
    let gene_name = attrs.gene_name?;
    let transcript_id = attrs.transcript_id?;
    let exon_number = attrs.exon_number?;

    if !transcript_id.starts_with("NM_") {
        return None;
    }

    // GTF start is 1-based inclusive; convert it to a 0-based start. The raw end
    // is the matching exclusive bound for the internal half-open range.
    let key = Locus::key_for(&gene_name, &transcript_id, exon_number);

    Some(RawEntry {
        contig,
        locus: Locus {
            start: BasePair::from_1based(start),
            end: BasePair::new(end),
            key,
            strand: FeatureStrand::from_gtf(fields[6]),
        },
    })
}

/// Parses supported exon rows from a gzipped GTF.
fn parse_gtf(gtf_gz_path: &Path) -> io::Result<Vec<RawEntry>> {
    let file = File::open(gtf_gz_path)?;
    let reader = BufReader::new(GzDecoder::new(file));

    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 9 || fields[2] != "exon" {
            continue;
        }
        if let Some(entry) = parse_exon_row(&fields) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Sorts entries and records each contig's range.
fn sort_and_index(mut entries: Vec<RawEntry>) -> (Vec<Locus>, LociByContig) {
    entries.sort_by(|a, b| (a.contig.as_str(), a.locus.start).cmp(&(b.contig.as_str(), b.locus.start)));

    let mut by_contig: LociByContig = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        by_contig
            .entry(entry.contig.clone())
            .and_modify(|range| range.end = i + 1)
            .or_insert(i..i + 1);
    }

    let loci = entries.into_iter().map(|e| e.locus).collect();
    (loci, by_contig)
}

fn write_len_prefixed(writer: &mut impl Write, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    writer.write_all(&(bytes.len() as u16).to_le_bytes())?;
    writer.write_all(bytes)
}

fn read_len_prefixed(reader: &mut impl Read) -> io::Result<String> {
    let mut len_buf = [0u8; 2];
    reader.read_exact(&mut len_buf)?;
    let len = u16::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes the sorted loci and contig ranges to a gzipped cache.
fn save_cache(loci: &[Locus], by_contig: &LociByContig, cache_path: &Path) -> io::Result<()> {
    let mut contig_at = vec![""; loci.len()];
    for (contig, range) in by_contig {
        for slot in &mut contig_at[range.clone()] {
            *slot = contig;
        }
    }

    let file = File::create(cache_path)?;
    let mut writer = BufWriter::new(GzEncoder::new(file, Compression::default()));
    writer.write_all(CACHE_MAGIC)?;
    writer.write_all(&(loci.len() as u64).to_le_bytes())?;
    for (locus, contig) in loci.iter().zip(contig_at) {
        write_len_prefixed(&mut writer, contig)?;
        write_len_prefixed(&mut writer, &locus.key)?;
        writer.write_all(&locus.start.as_u64().to_le_bytes())?;
        writer.write_all(&locus.end.as_u64().to_le_bytes())?;
        writer.write_all(&[locus.strand.cache_tag()])?;
    }
    writer.into_inner()?.finish()?;
    Ok(())
}

/// Reads a cache written by `save_cache`.
fn load_cache(cache_path: &Path) -> io::Result<(Vec<Locus>, LociByContig)> {
    let file = File::open(cache_path)?;
    let mut reader = BufReader::new(GzDecoder::new(file));

    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != CACHE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "feature cache has an unsupported format; rebuild the GTF index",
        ));
    }

    let mut count_buf = [0u8; 8];
    reader.read_exact(&mut count_buf)?;
    let count = u64::from_le_bytes(count_buf) as usize;

    let mut loci = Vec::with_capacity(count);
    let mut by_contig: LociByContig = HashMap::new();
    for i in 0..count {
        let contig = read_len_prefixed(&mut reader)?;
        let key = read_len_prefixed(&mut reader)?;

        let mut u64_buf = [0u8; 8];
        reader.read_exact(&mut u64_buf)?;
        let start = BasePair::new(u64::from_le_bytes(u64_buf));
        reader.read_exact(&mut u64_buf)?;
        let end = BasePair::new(u64::from_le_bytes(u64_buf));
        let mut strand_buf = [0u8; 1];
        reader.read_exact(&mut strand_buf)?;
        let strand = FeatureStrand::from_cache_tag(strand_buf[0]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "feature cache contains an invalid strand tag",
            )
        })?;

        by_contig
            .entry(contig)
            .and_modify(|range| range.end = i + 1)
            .or_insert(i..i + 1);
        loci.push(Locus {
            start,
            end,
            key: key.into(),
            strand,
        });
    }
    Ok((loci, by_contig))
}

/// Returns the cache path beside a GTF, with `.apuscache` appended.
pub fn cache_path_for(gtf_gz_path: &Path) -> PathBuf {
    let mut name = gtf_gz_path.file_name().unwrap_or_default().to_os_string();
    name.push(".apuscache");
    gtf_gz_path.with_file_name(name)
}

/// Returns whether a cache file exists.
pub fn cache_exists(cache_path: &Path) -> bool {
    cache_path.is_file()
}

/// Result of loading from or building a cache.
pub enum LoadResult {
    FromCache { index: FeatureIndex, elapsed: Duration },
    Built { index: FeatureIndex, elapsed: Duration },
}

/// The command line that would build `gtf_gz_path`'s index.
///
/// Lives here, beside [`load_only`], so the advice and the refusal it explains cannot drift apart:
/// the test that proves this line still resolves calls this function rather than restating its
/// format.
///
/// Callers pass the *resolved* path, which is what makes the line pasteable when the user's original
/// argument was relative or `~`-prefixed. The value is passed through the shared command encoder,
/// so a path containing the batch separator remains one escaped command argument.
pub fn index_command_for(gtf_gz_path: &Path) -> Result<String, String> {
    Ok(format!(
        ":load_index {}",
        crate::command::encode_arg(&gtf_gz_path.display().to_string())?
    ))
}

/// Reads a cache that is known to exist into a `FromCache` result.
fn read_cache(cache_path: &Path) -> io::Result<LoadResult> {
    let start = Instant::now();
    let (loci, by_contig) = load_cache(cache_path)?;
    Ok(LoadResult::FromCache {
        index: FeatureIndex::new(loci, by_contig),
        elapsed: start.elapsed(),
    })
}

/// Loads an existing cache, and never builds one.
///
/// Takes no GTF path: nothing is parsed, so the file the cache came from is irrelevant. The
/// `NotFound` error carries no user-facing advice either — composing the command line that would
/// build the cache is the caller's job, since only it knows the path as the user gave it.
///
/// The single [`cache_exists`] check here is deliberately the only one on this path. Checking it in
/// `apply_outcome` and then dispatching a build-capable job would test on the UI thread and build on
/// the worker, so a cache vanishing in between would build anyway — the one guarantee that separates
/// `:load` from `:load_index`.
pub fn load_only(cache_path: &Path) -> io::Result<LoadResult> {
    if !cache_exists(cache_path) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no index cache at {}", cache_path.display()),
        ));
    }
    read_cache(cache_path)
}

/// Loads a cache or builds one from the GTF.
pub fn load_or_build(gtf_gz_path: &Path, cache_path: &Path) -> io::Result<LoadResult> {
    if cache_exists(cache_path) {
        // An old or corrupt cache cannot supply strand data. Rebuild it here; `load_only` keeps
        // refusing to build and reports the same invalid-data error to the caller.
        match read_cache(cache_path) {
            Ok(result) => return Ok(result),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(error) => return Err(error),
        }
    }

    let start = Instant::now();
    let entries = parse_gtf(gtf_gz_path)?;
    let (loci, by_contig) = sort_and_index(entries);
    save_cache(&loci, &by_contig, cache_path)?;
    let index = FeatureIndex::new(loci, by_contig);
    Ok(LoadResult::Built {
        index,
        elapsed: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_raw() -> Vec<RawEntry> {
        parse_gtf(Path::new("testdata/gtf/gtf_subset.gtf.gz")).unwrap()
    }

    fn fixture_sorted() -> (Vec<Locus>, LociByContig) {
        sort_and_index(fixture_raw())
    }

    fn exon_number(locus: &Locus) -> u16 {
        locus
            .key
            .rsplit(':')
            .next()
            .and_then(|seg| seg.strip_prefix("ex"))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn gtf_parser_keeps_supported_rows_and_skips_malformed_rows() {
        let raw = fixture_raw();
        assert!(raw.iter().all(|e| e.locus.transcript_id().starts_with("NM_")));
        assert!(!raw.iter().any(|e| e.locus.transcript_id().starts_with("NR_")));
        let raw = fixture_raw();
        let forward = raw
            .iter()
            .map(|entry| &entry.locus)
            .find(|locus| locus.transcript_id() == "NM_002186.3" && exon_number(locus) == 1)
            .expect("fixture has a forward transcript");
        let reverse = raw
            .iter()
            .map(|entry| &entry.locus)
            .find(|locus| locus.transcript_id() == "NM_001002761.1" && exon_number(locus) == 9)
            .expect("fixture has a reverse transcript");

        assert_eq!(forward.strand(), FeatureStrand::Forward);
        assert_eq!(reverse.strand(), FeatureStrand::Reverse);
        assert!(
            !raw.iter()
                .any(|e| e.locus.transcript_id() == "NM_001002761.1" && exon_number(&e.locus) == 6)
        );
        assert!(
            raw.iter()
                .any(|e| e.locus.transcript_id() == "NM_001002761.1" && exon_number(&e.locus) == 7)
        );
    }

    #[test]
    fn gtf_coordinates_convert_at_ingest() {
        let raw = fixture_raw();
        let first_exon = raw
            .iter()
            .map(|e| &e.locus)
            .find(|l| l.transcript_id() == "NM_002186.3" && exon_number(l) == 1)
            .expect("fixture has NM_002186.3 exon 1");
        assert_eq!(first_exon.start, BasePair::new(62_422_686));
        assert_eq!(first_exon.end, BasePair::new(62_422_778));
    }

    #[test]
    fn parsed_key_preserves_gene_transcript_and_exon_identity() {
        let raw = fixture_raw();
        let locus = raw
            .iter()
            .map(|e| &e.locus)
            .find(|l| l.transcript_id() == "NM_002186.3" && exon_number(l) == 1)
            .expect("fixture has NM_002186.3 exon 1");
        assert_eq!(&*locus.key, "IL9R:NM_002186.3:ex1");
        assert_eq!(locus.key.split(':').next(), Some("IL9R"));
    }

    #[test]
    fn cache_round_trips() {
        let (loci, by_contig) = fixture_sorted();
        let dir = std::env::temp_dir();
        let cache_path = dir.join(format!("apus_gtf_cache_test_{}.cache", std::process::id()));

        save_cache(&loci, &by_contig, &cache_path).unwrap();
        let (loaded_loci, loaded_by_contig) = load_cache(&cache_path).unwrap();
        std::fs::remove_file(&cache_path).ok();

        assert_eq!(loaded_loci, loci);
        assert_eq!(loaded_by_contig, by_contig);
    }

    #[test]
    fn cache_lifecycle_distinguishes_load_build_and_rebuild() {
        let dir = std::env::temp_dir().join(format!("apus_gtf_cache_lifecycle_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gtf_path = dir.join("gtf_subset.gtf.gz");
        std::fs::copy("testdata/gtf/gtf_subset.gtf.gz", &gtf_path).unwrap();
        let cache_path = cache_path_for(&gtf_path);

        let missing_cache = match load_only(&cache_path) {
            Err(error) => error,
            Ok(_) => panic!("load_only must not build a missing cache"),
        };
        assert_eq!(missing_cache.kind(), io::ErrorKind::NotFound);
        assert!(!cache_path.exists());

        let built = load_or_build(&gtf_path, &cache_path).expect("missing cache must build");
        let built_index = match built {
            LoadResult::Built { index, .. } => index,
            LoadResult::FromCache { .. } => panic!("first call against a fresh path must build"),
        };
        assert!(!built_index.is_empty());
        let key = built_index.keys().next().expect("fixture must contain a locus");
        assert!(built_index.locus_by_key(&key).is_some());
        let cached = load_or_build(&gtf_path, &cache_path).expect("valid cache must load");
        let cached_index = match cached {
            LoadResult::FromCache { index, .. } => index,
            LoadResult::Built { .. } => panic!("second call must use the written cache"),
        };
        assert_eq!(cached_index.len(), built_index.len());

        {
            let file = File::create(&cache_path).unwrap();
            let mut writer = GzEncoder::new(file, Compression::default());
            writer.write_all(&0u64.to_le_bytes()).unwrap();
            writer.finish().unwrap();
        }

        let rebuilt = load_or_build(&gtf_path, &cache_path).expect("the stale cache must be rebuilt");
        assert!(matches!(rebuilt, LoadResult::Built { .. }));
        assert!(matches!(load_only(&cache_path), Ok(LoadResult::FromCache { .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn by_contig_ranges_are_contiguous_and_sorted_by_start() {
        let (loci, by_contig) = fixture_sorted();
        for range in by_contig.values() {
            let slice = &loci[range.clone()];
            assert!(
                slice.windows(2).all(|w| w[0].start <= w[1].start),
                "range must be start-sorted"
            );
        }
        let mut covered: Vec<usize> = by_contig.values().flat_map(|r| r.clone()).collect();
        covered.sort_unstable();
        let expected: Vec<usize> = (0..loci.len()).collect();
        assert_eq!(covered, expected, "every index must belong to exactly one contig range");
    }
}
