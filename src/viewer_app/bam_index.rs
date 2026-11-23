//! A BAI index parsed once and shared across the fetches that query it.
//!
//! A BAI can be tens of megabytes. A real whole-genome nanopore index took 313 ms to parse.
//! Alignment windows and local-area scans reuse the parsed index for the lifetime of their source.
//!
//! `noodles` takes the index by value (`IndexedReader::new<I: BinningIndex + 'static>`) and boxes
//! it, so sharing needs a type that is cheap to clone and still implements `BinningIndex`. The
//! trait's blanket impl delegates through `Deref`, which is what lets this newtype forward every
//! method to the one parsed copy behind an `Arc`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use noodles::bam;
use noodles::bgzf;
use noodles::core::region::Interval;
use noodles::csi::BinningIndex;
use noodles::csi::binning_index::ReferenceSequence;
use noodles::csi::binning_index::index::Header;
use noodles::csi::binning_index::index::reference_sequence::bin::Chunk;

/// A shared, cheaply cloned BAI index.
#[derive(Clone)]
pub(crate) struct SharedIndex(Arc<bam::bai::Index>);

pub(super) fn bai_path(bam_path: &Path) -> PathBuf {
    let mut path = bam_path.as_os_str().to_os_string();
    path.push(".bai");
    path.into()
}

impl SharedIndex {
    pub(crate) fn read(bam_path: &Path) -> io::Result<Self> {
        bam::bai::fs::read(bai_path(bam_path)).map(|index| Self(Arc::new(index)))
    }
}

impl BinningIndex for SharedIndex {
    fn min_shift(&self) -> u8 {
        self.0.min_shift()
    }

    fn depth(&self) -> u8 {
        self.0.depth()
    }

    fn header(&self) -> Option<&Header> {
        self.0.header()
    }

    fn reference_sequences(&self) -> Box<dyn Iterator<Item = &dyn ReferenceSequence> + '_> {
        // `bai::Index` has an inherent `reference_sequences` returning a slice, which shadows
        // the trait method here; name the trait explicitly to forward rather than recurse.
        BinningIndex::reference_sequences(&*self.0)
    }

    fn unplaced_unmapped_record_count(&self) -> Option<u64> {
        self.0.unplaced_unmapped_record_count()
    }

    fn query(&self, reference_sequence_id: usize, interval: Interval) -> io::Result<Vec<Chunk>> {
        self.0.query(reference_sequence_id, interval)
    }

    fn last_first_record_start_position(&self) -> Option<bgzf::VirtualPosition> {
        self.0.last_first_record_start_position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodles::core::Position;

    fn chr1_reference_sequence_id() -> usize {
        crate::refgenome::RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai")
            .expect("test fixture must load")
            .contig_names()
            .position(|name| name == "chr1")
            .expect("tiny BAM reference must exist")
    }

    /// Sharing must not change what the index answers.
    ///
    /// `SharedIndex` reimplements `BinningIndex` by delegation, so a method forwarded to the
    /// wrong place would silently return different chunks and the viewer would draw the wrong
    /// reads rather than fail. Each delegated index property and full-reference query is compared
    /// with the freshly parsed tiny fixture.
    #[test]
    fn a_shared_index_answers_exactly_as_a_freshly_parsed_one() {
        let path = std::path::Path::new("testdata/bam/tiny.bam");
        let shared = SharedIndex::read(path).expect("tiny BAI fixture");
        let direct = bam::bai::fs::read(format!("{}.bai", path.display())).expect("tiny BAI fixture");

        assert_eq!(shared.min_shift(), direct.min_shift());
        assert_eq!(shared.depth(), direct.depth());
        assert_eq!(
            shared.unplaced_unmapped_record_count(),
            direct.unplaced_unmapped_record_count()
        );
        assert_eq!(
            shared.last_first_record_start_position(),
            direct.last_first_record_start_position()
        );

        let reference_count = BinningIndex::reference_sequences(&direct).count();
        assert_eq!(BinningIndex::reference_sequences(&shared).count(), reference_count);
        let chr1 = chr1_reference_sequence_id();
        assert!(chr1 < reference_count, "chr1 must be in the tiny BAI");
        let shared_chunks = BinningIndex::query(&shared, chr1, Interval::from(..)).expect("shared");
        let direct_chunks = BinningIndex::query(&direct, chr1, Interval::from(..)).expect("direct");
        assert_eq!(shared_chunks, direct_chunks, "tiny BAM chr1 chunks");
    }

    /// A clone keeps answering with the index that was parsed from this BAM.
    #[test]
    fn cloned_shared_index_answers_the_same_queries() {
        let shared = SharedIndex::read(std::path::Path::new("testdata/bam/tiny.bam")).expect("tiny BAI fixture");
        let cloned = shared.clone();
        let chr1 = chr1_reference_sequence_id();
        let narrow = Interval::from(Position::try_from(1).unwrap()..=Position::try_from(10).unwrap());
        for interval in [Interval::from(..), narrow] {
            assert_eq!(
                BinningIndex::query(&cloned, chr1, interval).expect("cloned index"),
                BinningIndex::query(&shared, chr1, interval).expect("shared index")
            );
        }
    }
}
