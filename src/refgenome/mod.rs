//! Reference-genome metadata and the boundary to indexed sequence access.
//!
//! [`IndexedFasta`] is the small descriptor passed through the viewer: it carries the reference
//! name, the `.fai`-derived contig index, and an optional FASTA path. Native builds generate
//! placeholder bases for the bundled fallback reference. The demo supplies resident reference
//! slices alongside its pathless descriptor. A real path is validated while its metadata is loaded,
//! then its indexed reader is opened and retained by [`ReferenceWorkerHandle`].
//!
//! This split keeps noodles and blocking FASTA I/O below the viewer and leaves cache ownership to
//! `genome::reference_data`. Positions and spans crossing into noodles are still `BasePair`
//! values; the reader performs the one sanctioned conversion to noodles' 1-based positions at
//! that external boundary.

use std::path::PathBuf;

mod refindex;
mod refsequence;
#[cfg(feature = "resident-demo")]
pub(crate) mod resident;
mod worker;

pub use refindex::RefIndex;
pub(crate) use refsequence::RefSequence;
pub(crate) use worker::ReferenceWorkerHandle;

/// A reference name, contig index, and optional sequence path.
#[derive(Clone)]
pub struct IndexedFasta {
    pub(crate) name: String,
    pub(crate) index: RefIndex,
    /// Absent for the fallback and for embedded reference slices.
    pub(crate) path: Option<PathBuf>,
}

/// Compiled fallback `.fai` text for hs1.
const HS1_FAI: &str = "\
chr1\t248387328\t6\t50\t51
chr2\t242696752\t253355087\t50\t51
chr3\t201105948\t500905781\t50\t51
chr4\t193574945\t706033854\t50\t51
chr5\t182045439\t903480304\t50\t51
chr6\t172126628\t1089166658\t50\t51
chr7\t160567428\t1264735825\t50\t51
chrX\t154259566\t1428514608\t50\t51
chr9\t150617247\t1585859372\t50\t51
chr8\t146259331\t1739488970\t50\t51
chr11\t135127769\t1888673495\t50\t51
chr10\t134758134\t2026503827\t50\t51
chr12\t133324548\t2163957131\t50\t51
chr13\t113566686\t2299948177\t50\t51
chr14\t101161492\t2415786204\t50\t51
chr15\t99753195\t2518970933\t50\t51
chr16\t96330374\t2620719199\t50\t51
chr17\t84276897\t2718976188\t50\t51
chr18\t80542538\t2804938630\t50\t51
chr20\t66210255\t2887092026\t50\t51
chrY\t62460029\t2954626493\t50\t51
chr19\t61707364\t3018335730\t50\t51
chr22\t51324926\t3081277249\t50\t51
chr21\t45090682\t3133628681\t50\t51
chrM\t16569\t3179621183\t50\t51
";

impl IndexedFasta {
    /// Parses `.fai` text into reference metadata.
    pub(crate) fn new(name: String, fai_data: &str) -> std::io::Result<Self> {
        let index = RefIndex::from_fai_str(fai_data)?;
        Ok(Self {
            name,
            index,
            path: None,
        })
    }

    /// Loads reference metadata from a FASTA path and its sibling `.fai`.
    pub(crate) fn from_file<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<Self> {
        let path = path.as_ref();
        let fai_path = format!("{}.fai", path.display());
        let index = RefIndex::from_fai_path(&fai_path)?;
        std::fs::File::open(path)?;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Ok(Self {
            name,
            index,
            path: Some(path.to_path_buf()),
        })
    }

    /// Returns the bundled hs1 fallback.
    pub fn hs1_fallback() -> Self {
        Self::new("hs1".to_string(), HS1_FAI).expect("bundled hs1.fa.fai must parse")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_reference_metadata_matches_its_index_and_reports_missing_files() {
        let loaded = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        assert_eq!(loaded.index.length("chr1"), Some(crate::basepair::BasePair(248_400)));
        assert_eq!(loaded.index.length("chrM"), Some(crate::basepair::BasePair(16_569)));
        assert_eq!(loaded.index.length("chr_does_not_exist"), None);
        assert!(IndexedFasta::from_file("testdata/does_not_exist.fa").is_err());
    }
}
