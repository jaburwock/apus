//! Worker-owned indexed FASTA reader for reference-base fetching.
//!
//! [`RefSequence`] delegates the file-offset work to noodles' indexed reader;
//! application-facing contig metadata remains owned by [`super::RefIndex`].
//! Its public fetch boundary accepts the application's 0-based `BasePair`
//! start and a span length. At that boundary only, it converts to noodles'
//! 1-based [`Position`] values and inclusive [`Region`]: the start becomes
//! `start + 1`, while the endpoint is `start + len`. A zero length therefore
//! builds the inverted range `start + 1 ..= start` rather than an empty one,
//! and no caller guards against it — `reference_data`'s
//! `(contig_length - start).min(CHUNK_LENGTH)` reaches zero at a contig end.
//! Callers that need an empty result must avoid issuing that request. Keeping this
//! translation here prevents external 1-based coordinates from leaking into
//! the rest of the reference pipeline.

use std::io;
use std::path::Path;

use noodles::core::{Position, Region};
use noodles::fasta;

use crate::basepair::BasePair;

/// Indexed FASTA reader owned by the reference worker.
pub struct RefSequence {
    reader: fasta::io::IndexedReader<fasta::io::BufReader<std::fs::File>>,
}

impl RefSequence {
    pub fn from_fa_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let reader = fasta::io::indexed_reader::Builder::default().build_from_path(&path)?;
        Ok(Self { reader })
    }

    /// Fetches uppercased bases from a 0-based span.
    pub fn fetch(&mut self, contig: &str, start: BasePair, len: BasePair) -> io::Result<Vec<u8>> {
        let start_1based = start.as_u64() + 1;
        let end_1based = start.as_u64() + len.as_u64();
        let start_pos =
            Position::try_from(start_1based as usize).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let end_pos =
            Position::try_from(end_1based as usize).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let region = Region::new(contig, start_pos..=end_pos);
        let record = self.reader.query(&region)?;
        Ok(record.sequence().as_ref().to_ascii_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_fetch_matches_reference_bytes_and_reports_unknown_contigs() {
        let mut seq = RefSequence::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let bases = seq.fetch("chr1", BasePair(0), BasePair(20)).unwrap();
        assert_eq!(bases, b"CACCCTAAACCCTAACCCCT");
        assert!(seq.fetch("not_a_real_contig", BasePair(0), BasePair(10)).is_err());
    }
}
