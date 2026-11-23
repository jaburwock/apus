//! Regenerates `testdata/bam/medium.bam` and its `.bai` from the committed reference subset.
//!
//! A fixed RNG seed and draw order pin every generated column except QNAME, which `gen_qname`
//! mints from `Uuid::new_v4()` — OS entropy, outside the seeded `StdRng`. Output is therefore
//! structurally stable but not byte-stable, so don't regenerate without a reason.

use noodles::bam;
use noodles::core::Position;
use noodles::csi::binning_index::Indexer;
use noodles::csi::binning_index::index::reference_sequence::bin::Chunk;
use noodles::csi::binning_index::index::reference_sequence::index::LinearIndex;
use noodles::sam::alignment::RecordBuf;
use noodles::sam::alignment::io::Write as _;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use apus::{RefIndex, RefSeq, generate_sam_line, header_from_ref_index};

const TEST_FASTA: &str = "testdata/reference/hs1_subset.fa";

fn fixture() -> (RefIndex, RefSeq) {
    let ref_index = RefIndex::from_fai_path(format!("{TEST_FASTA}.fai")).expect("test fixture must load");
    let ref_seq = RefSeq::from_fa_path(TEST_FASTA).expect("test fixture must load");
    (ref_index, ref_seq)
}

fn record_buf_from_sam_line(header: &noodles::sam::Header, line: &str) -> RecordBuf {
    let mut reader = noodles::sam::io::Reader::new(line.as_bytes());
    let mut record = RecordBuf::default();
    reader
        .read_record_buf(header, &mut record)
        .expect("a freshly generated SAM line must parse against its own header");
    record
}

fn alignment_context(ref_id: usize, record: &RecordBuf) -> Option<(usize, Position, Position, bool)> {
    let start = record.alignment_start()?;
    let end = record.alignment_end()?;
    let is_mapped = !record.flags().is_unmapped();
    Some((ref_id, start, end, is_mapped))
}

fn main() {
    let (ref_index, mut ref_seq) = fixture();
    let header = header_from_ref_index(&ref_index);

    const MEDIUM_BAM_SEED: u64 = 20260804;
    let mut rng = StdRng::seed_from_u64(MEDIUM_BAM_SEED);

    let mut records: Vec<(usize, RecordBuf)> = Vec::new();
    for _ in 0..50 {
        let read_len = rng.random_range(75..=150u64);
        let pos_0based = rng.random_range(0..=50u64);
        let line = generate_sam_line(
            &mut rng,
            &ref_index,
            &mut ref_seq,
            read_len,
            Some(("chr1", pos_0based + 1)),
        );
        let chr1_ref_id = ref_index
            .contig_names()
            .position(|n| n == "chr1")
            .expect("chr1 must exist");
        records.push((chr1_ref_id, record_buf_from_sam_line(&header, &line)));
    }
    records.sort_by_key(|(ref_id, record)| (*ref_id, record.alignment_start()));

    let dir = std::path::Path::new("testdata/bam");
    std::fs::create_dir_all(dir).expect("must create testdata/bam");
    let bam_path = dir.join("medium.bam");

    let bam_file = std::fs::File::create(&bam_path).expect("must create medium.bam");
    let mut indexer = Indexer::<LinearIndex>::default();
    {
        let mut writer = bam::io::Writer::new(bam_file);
        writer.write_header(&header).expect("must write header");
        let mut start_position = writer.get_ref().virtual_position();
        for (ref_id, record) in &records {
            writer
                .write_alignment_record(&header, record)
                .expect("must write record");
            let end_position = writer.get_ref().virtual_position();
            let chunk = Chunk::new(start_position, end_position);
            let ctx = alignment_context(*ref_id, record);
            indexer.add_record(ctx, chunk).expect("must add to indexer");
            start_position = end_position;
        }
        writer.try_finish().expect("must finish BAM");
    }
    let index = indexer.build(header.reference_sequences().len());
    bam::bai::fs::write(dir.join("medium.bam.bai"), &index).expect("must write BAI");
}
