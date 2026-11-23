//! Application-facing contig metadata parsed from a `.fai` index.
//!
//! [`RefIndex`] owns the parsed noodles index as well as the lookup used by the
//! rest of the application for contig names and lengths. The full index keeps
//! the source ordering and record access needed by selectors; the length map
//! avoids making those callers depend on noodles types. Sequence bytes belong
//! to [`super::RefSequence`], which wraps the indexed FASTA reader separately.
//! A `.fai` therefore supplies metadata here, while this module does not fetch
//! or interpret bases.

use std::collections::HashMap;
use std::io;
use std::io::Cursor;
use std::path::Path;

use noodles::fasta::fai;

use crate::basepair::BasePair;

/// Reference contig names and lengths.
#[derive(Clone)]
pub struct RefIndex {
    index: fai::Index,
    lengths: HashMap<String, BasePair>,
}

impl RefIndex {
    /// Parses `.fai` text.
    pub(crate) fn from_fai_str(fai_data: &str) -> io::Result<Self> {
        let index = fai::io::Reader::new(Cursor::new(fai_data.as_bytes())).read_index()?;
        Self::from_index(index)
    }

    pub fn from_fai_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let index = fai::fs::read(path)?;
        Self::from_index(index)
    }

    fn from_index(index: fai::Index) -> io::Result<Self> {
        let lengths = index
            .as_ref()
            .iter()
            .map(|record| (record.name().to_string(), BasePair(record.length())))
            .collect();
        Ok(Self { index, lengths })
    }

    pub fn contig_names(&self) -> impl Iterator<Item = String> {
        self.index
            .as_ref()
            .iter()
            .map(|record| String::from_utf8_lossy(record.name()).into_owned())
    }

    #[cfg(any(test, feature = "testgen"))]
    pub(crate) fn contig_count(&self) -> usize {
        self.index.as_ref().len()
    }

    /// Returns the contig name at `index`.
    pub(crate) fn contig_name(&self, index: usize) -> String {
        let record = &self.index.as_ref()[index];
        String::from_utf8_lossy(record.name()).into_owned()
    }

    pub(crate) fn length(&self, contig: &str) -> Option<BasePair> {
        self.lengths.get(contig).copied()
    }
}
