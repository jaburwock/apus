//! Feature intervals and the indexes used to find them by position or key.
//!
//! [`Locus`] is the source-side feature model: a contig-relative, half-open
//! interval with the display key and strand information needed by navigation,
//! feature rendering, and search resolution. [`FeatureIndex`] owns the sorted
//! collection and its derived contig, key, transcript-stem, and overlap
//! indexes. The viewer's [`crate::search::locus::LocusIndex`] owns whether an
//! index is currently installed and joins fuzzy-search results back to this
//! model; this module does not own viewer or worker state.
//!
//! The private [`gtf`] module is the ingest and cache boundary. Its public
//! exports are the loading results and the cache/command helpers used by the
//! command layer, so callers can distinguish loading an existing index from
//! building one without reaching into the parser. GTF coordinates are
//! converted to the crate's 0-based [`crate::basepair::BasePair`] convention
//! at that boundary, and remain half-open internally; callers must not repeat
//! a display-base conversion or reinterpret an interval's exclusive end.

mod gtf;

pub use gtf::{LoadResult, cache_path_for, index_command_for, load_only, load_or_build};

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::basepair::BasePair;

/// Direction carried by a GTF transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeatureStrand {
    Forward,
    Reverse,
    Unknown,
}

impl FeatureStrand {
    pub(crate) fn from_gtf(value: &str) -> Self {
        match value {
            "+" => Self::Forward,
            "-" => Self::Reverse,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn cache_tag(self) -> u8 {
        match self {
            Self::Forward => 0,
            Self::Reverse => 1,
            Self::Unknown => 2,
        }
    }

    pub(crate) fn from_cache_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Forward),
            1 => Some(Self::Reverse),
            2 => Some(Self::Unknown),
            _ => None,
        }
    }

    pub(crate) fn arrow(self) -> Option<&'static str> {
        match self {
            Self::Forward => Some(">"),
            Self::Reverse => Some("<"),
            Self::Unknown => None,
        }
    }
}

/// One searchable feature interval and its display key.
#[derive(Debug, Clone, PartialEq)]
pub struct Locus {
    pub start: BasePair,
    pub end: BasePair,
    pub key: Arc<str>,
    pub(crate) strand: FeatureStrand,
}

impl Locus {
    fn key_for(gene_name: &str, transcript_id: &str, exon_number: u16) -> Arc<str> {
        format!("{gene_name}:{transcript_id}:ex{exon_number}").into()
    }

    /// Returns the transcript segment of the key.
    pub fn transcript_id(&self) -> &str {
        self.key.split(':').nth(1).unwrap_or_default()
    }

    /// Returns the gene/transcript part of the GTF display key.
    pub(crate) fn feature_stem(&self) -> &str {
        feature_key_parts(&self.key).0
    }

    /// Returns the numeric exon part of the GTF display key.
    pub(crate) fn exon_number(&self) -> Option<&str> {
        feature_key_parts(&self.key).1
    }

    pub(crate) fn strand(&self) -> FeatureStrand {
        self.strand
    }
}

fn feature_key_parts(key: &str) -> (&str, Option<&str>) {
    let Some((stem, exon)) = key.rsplit_once(":ex") else {
        return (key, None);
    };
    if stem.is_empty() || exon.is_empty() || !exon.bytes().all(|byte| byte.is_ascii_digit()) {
        (key, None)
    } else {
        (stem, Some(exon))
    }
}

/// Finds the next matching feature start in a sorted range.
fn next_locus_start(loci: &[Locus], range: Range<usize>, after: BasePair, filter: &FeatureFilter) -> Option<BasePair> {
    let slice = &loci[range];
    let i = slice.partition_point(|locus| locus.start <= after);
    slice[i..]
        .iter()
        .find(|locus| filter.matches(locus))
        .map(|locus| locus.start)
}

/// Finds the previous matching feature start in a sorted range.
fn prev_locus_start(loci: &[Locus], range: Range<usize>, before: BasePair, filter: &FeatureFilter) -> Option<BasePair> {
    let slice = &loci[range];
    let i = slice.partition_point(|locus| locus.start < before);
    slice[..i]
        .iter()
        .rev()
        .find(|locus| filter.matches(locus))
        .map(|locus| locus.start)
}

/// Include/exclude substring filtering for locus keys.
#[derive(Debug, Clone, Default)]
pub struct FeatureFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl FeatureFilter {
    pub fn none() -> Self {
        Self::default()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Returns whether a locus passes this filter.
    ///
    /// A non-empty `include` wins over `exclude`: showing too much beats hiding a feature the user
    /// expected.
    pub fn matches(&self, locus: &Locus) -> bool {
        let key = locus.key.as_ref();
        let included = self.include.is_empty() || self.include.iter().any(|s| key.contains(s.as_str()));
        if !included {
            return false;
        }
        if !self.include.is_empty() {
            return true;
        }
        self.exclude.is_empty() || !self.exclude.iter().any(|s| key.contains(s.as_str()))
    }
}

/// Queryable locus collection with contig and key indexes.
struct FeatureStemSpan {
    stem: Arc<str>,
    start: BasePair,
    end: BasePair,
}

pub struct FeatureIndex {
    loci: Vec<Locus>,
    loci_by_contig: HashMap<String, Range<usize>>,
    loci_by_key: HashMap<Arc<str>, usize>,
    loci_by_stem: HashMap<Arc<str>, Vec<usize>>,
    spans_by_contig: HashMap<String, Vec<FeatureStemSpan>>,
    /// Running maximum end coordinate for each contig range. `loci` is sorted by start only, so a
    /// search on start misses a long feature that begins before the window; this is monotonic and
    /// gives `overlapping` its real lower bound.
    max_end_prefix: Vec<BasePair>,
}

impl FeatureIndex {
    /// Builds secondary indexes from sorted loci and contig ranges.
    pub fn new(loci: Vec<Locus>, loci_by_contig: HashMap<String, Range<usize>>) -> Self {
        let loci_by_key = loci
            .iter()
            .enumerate()
            .map(|(i, locus)| (locus.key.clone(), i))
            .collect();
        let mut loci_by_stem: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        for (i, locus) in loci.iter().enumerate() {
            loci_by_stem.entry(Arc::from(locus.feature_stem())).or_default().push(i);
        }
        let mut spans_by_contig = HashMap::new();
        for (contig, range) in &loci_by_contig {
            let mut spans: HashMap<Arc<str>, (BasePair, BasePair)> = HashMap::new();
            for locus in &loci[range.clone()] {
                let stem = Arc::from(locus.feature_stem());
                let span = spans.entry(stem).or_insert((locus.start, locus.end));
                span.0 = span.0.min(locus.start);
                span.1 = span.1.max(locus.end);
            }
            let mut spans: Vec<_> = spans
                .into_iter()
                .map(|(stem, (start, end))| FeatureStemSpan { stem, start, end })
                .collect();
            spans.sort_by(|left, right| left.start.cmp(&right.start).then(left.stem.cmp(&right.stem)));
            spans_by_contig.insert(contig.clone(), spans);
        }

        let mut max_end_prefix = vec![BasePair::new(0); loci.len()];
        for range in loci_by_contig.values() {
            let mut running_max = BasePair::new(0);
            for i in range.clone() {
                running_max = running_max.max(loci[i].end);
                max_end_prefix[i] = running_max;
            }
        }

        Self {
            loci,
            loci_by_contig,
            loci_by_key,
            loci_by_stem,
            spans_by_contig,
            max_end_prefix,
        }
    }

    /// Returns the number of loci.
    pub fn len(&self) -> usize {
        self.loci.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.loci.is_empty()
    }

    /// Returns loci overlapping the half-open interval `[start, end)`.
    pub fn overlapping(&self, contig: &str, start: BasePair, end: BasePair) -> &[Locus] {
        let Some(range) = self.loci_by_contig.get(contig) else {
            return &[];
        };
        let prefix = &self.max_end_prefix[range.clone()];
        let slice = &self.loci[range.clone()];

        let lo = prefix.partition_point(|&max_end| max_end <= start);
        let hi = slice.partition_point(|locus| locus.start < end);
        if lo >= hi { &[] } else { &slice[lo..hi] }
    }

    /// Returns the next matching feature start.
    pub fn next_start(&self, contig: &str, after: BasePair, filter: &FeatureFilter) -> Option<BasePair> {
        let range = self.loci_by_contig.get(contig)?;
        next_locus_start(&self.loci, range.clone(), after, filter)
    }

    /// Returns the previous matching feature start.
    pub fn prev_start(&self, contig: &str, before: BasePair, filter: &FeatureFilter) -> Option<BasePair> {
        let range = self.loci_by_contig.get(contig)?;
        prev_locus_start(&self.loci, range.clone(), before, filter)
    }

    /// Looks up a locus by its search key and returns its index.
    pub fn locus_by_key(&self, key: &str) -> Option<(usize, &Locus)> {
        let &index = self.loci_by_key.get(key)?;
        Some((index, &self.loci[index]))
    }

    /// Returns every exon for a transcript stem on one contig, including off-screen exons.
    pub(crate) fn loci_for_stem(&self, contig: &str, stem: &str) -> Vec<&Locus> {
        let Some(range) = self.loci_by_contig.get(contig) else {
            return Vec::new();
        };
        self.loci_by_stem
            .get(stem)
            .into_iter()
            .flatten()
            .filter(|&&index| range.contains(&index))
            .map(|&index| &self.loci[index])
            .collect()
    }

    /// Returns transcript stems whose exon span overlaps a viewport, including intron-only views.
    pub(crate) fn stems_covering(&self, contig: &str, start: BasePair, end: BasePair) -> Vec<&str> {
        self.spans_by_contig
            .get(contig)
            .into_iter()
            .flatten()
            .filter(|span| span.start < end && span.end > start)
            .map(|span| span.stem.as_ref())
            .collect()
    }

    /// Returns the contig containing a locus index.
    pub fn contig_for_index(&self, index: usize) -> Option<&str> {
        self.loci_by_contig
            .iter()
            .find(|(_, range)| range.contains(&index))
            .map(|(contig, _)| contig.as_str())
    }

    /// Returns cloned search keys.
    pub fn keys(&self) -> impl Iterator<Item = Arc<str>> + '_ {
        self.loci_by_key.keys().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locus(start: u64, end: u64, key: &str) -> Locus {
        Locus {
            start: BasePair::new(start),
            end: BasePair::new(end),
            key: key.into(),
            strand: FeatureStrand::Unknown,
        }
    }

    fn index(loci: Vec<Locus>) -> FeatureIndex {
        let by_contig = HashMap::from([("chr1".to_string(), 0..loci.len())]);
        FeatureIndex::new(loci, by_contig)
    }

    #[test]
    fn overlap_query_handles_boundaries_and_unknown_contigs() {
        let fi = index(vec![
            locus(0, 50, "before"),
            locus(50, 150, "left_overhang"),
            locus(150, 250, "right_overhang"),
            locus(300, 400, "after"),
        ]);
        let hits = fi.overlapping("chr1", BasePair::new(100), BasePair::new(200));
        assert_eq!(
            hits.iter().map(|hit| &*hit.key).collect::<Vec<_>>(),
            ["left_overhang", "right_overhang"]
        );
        assert!(
            fi.overlapping("chr2", BasePair::new(100), BasePair::new(200))
                .is_empty()
        );
    }

    #[test]
    fn navigation_skips_filtered_loci_and_stops_at_range_edges() {
        let fi = index(vec![
            locus(10, 20, "GENE:NM_1.1:ex1"),
            locus(20, 30, "GENE:NR_1.1:ex1"),
            locus(30, 40, "GENE:NM_2.1:ex1"),
        ]);
        let filter = FeatureFilter {
            include: vec!["NM_".to_string()],
            exclude: vec![],
        };
        assert_eq!(
            fi.next_start("chr1", BasePair::new(10), &filter),
            Some(BasePair::new(30))
        );
        assert_eq!(
            fi.prev_start("chr1", BasePair::new(30), &filter),
            Some(BasePair::new(10))
        );
        assert_eq!(fi.next_start("chr1", BasePair::new(30), &filter), None);
        assert_eq!(fi.prev_start("chr1", BasePair::new(10), &filter), None);
    }

    #[test]
    fn feature_filter_applies_include_and_exclude_rules() {
        let included = locus(0, 10, "GENE:NM_000001.1:ex1");
        let filter = FeatureFilter {
            include: vec!["NM_000001".to_string()],
            exclude: vec!["NM_000001".to_string()],
        };
        assert!(filter.matches(&included), "include should win when both lists match");
        let excluded = locus(0, 10, "GENE:XR_000001.1:ex1");
        assert!(
            !FeatureFilter {
                include: vec![],
                exclude: vec!["XR_".to_string()],
            }
            .matches(&excluded)
        );

        let other = locus(0, 10, "GENE:NR_1.1:ex1");
        let neither = locus(0, 10, "GENE:XM_1.1:ex1");
        let any_include = FeatureFilter {
            include: vec!["NM_".to_string(), "NR_".to_string()],
            exclude: vec![],
        };
        assert!(any_include.matches(&other));
        assert!(!any_include.matches(&neither));
        assert!(FeatureFilter::none().matches(&neither));
    }

    #[test]
    fn feature_key_parts_expose_the_stem_and_numeric_exon() {
        let valid = locus(0, 10, "GENE12:TRANSCRIPT_12345:ex7");
        assert_eq!(valid.feature_stem(), "GENE12:TRANSCRIPT_12345");
        assert_eq!(valid.exon_number(), Some("7"));

        let malformed = locus(0, 10, "not_a_gtf_key");
        assert_eq!(malformed.feature_stem(), "not_a_gtf_key");
        assert_eq!(malformed.exon_number(), None);
    }

    #[test]
    fn locus_key_lookup_joins_indexes_and_returns_none_for_unknown_keys() {
        let fi = index(vec![locus(10, 20, "a"), locus(30, 40, "b")]);
        let (index, found) = fi.locus_by_key("b").expect("key exists");
        assert_eq!(index, 1);
        assert_eq!(&*found.key, "b");
        assert_eq!(fi.contig_for_index(index), Some("chr1"));
        assert_eq!(fi.locus_by_key("nope"), None);
    }
}
