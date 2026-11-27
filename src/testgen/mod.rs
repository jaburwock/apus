//! Composes the private column generators into fixture-ready SAM records.
//!
//! `generate_sam_line` is the integration seam that keeps the independently generated columns
//! consistent: CIGAR establishes query consumption and mismatch offsets, SEQ consumes that CIGAR
//! against an indexed reference, and QUAL uses the requested read length. Random loci come from
//! the reference index, while fixed-locus callers provide the SAM coordinate explicitly.
//!
//! `header_from_ref_index` supplies reference sequences and declares coordinate sort order; it
//! does not sort records, so the fixture binaries retain that responsibility. Component generators
//! stay private, while this module exposes the small façade used by fixture binaries and tests.
//! Seeded content is repeatable except for QNAMEs, which are fresh UUIDs by design.
//! `testdata/README.md` documents the public HG002 calibration sample.

mod cigar;
mod flag;
mod pos;
mod qname;
mod qual;
mod rname;
mod seq;

pub(crate) use cigar::gen_cigar;
pub(crate) use flag::gen_flag;
pub(crate) use pos::gen_pos;
pub(crate) use qname::gen_qname;
pub(crate) use qual::gen_qual;
pub(crate) use rname::gen_rname;
pub use seq::RefSeq;

use rand::{Rng, RngExt};

use crate::refgenome::RefIndex;

// The calibration sample is stratified at its modal mapping quality.
const MAPQ: u8 = 60;

/// Exact genomic split (`CigarKind::Skip` / `N`) offered to eligible generated reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitSpec<'a> {
    pub contig: &'a str,
    /// 1-based SAM coordinate of the first skipped reference base (matches `fixed_locus`).
    pub start: u64,
    /// Exact skipped reference length in bp; must be positive.
    pub len: u64,
}

/// Generates one SAM record, optionally at a fixed locus and with an optional split site.
///
/// When `split` is supplied, reads on `split.contig` starting strictly before `split.start` with
/// room on the contig roll 50/50 to incorporate `{split.len}N` at `split.start` and continue
/// aligning on the downstream side. Reads starting inside or after the split stay unspliced.
pub fn generate_sam_line<R: Rng + ?Sized>(
    rng: &mut R,
    ref_index: &RefIndex,
    ref_seq: &mut RefSeq,
    read_len: u64,
    fixed_locus: Option<(&str, u64)>,
    split: Option<SplitSpec<'_>>,
) -> String {
    let qname = gen_qname();
    let flag = gen_flag(rng);
    let (rname, pos) = match fixed_locus {
        Some((rname, pos)) => (rname.to_string(), pos),
        None => {
            let rname = gen_rname(rng, ref_index);
            let pos = gen_pos(rng, ref_index, &rname, read_len);
            (rname, pos)
        }
    };

    let cigar_split = match split {
        Some(spec) => {
            assert!(spec.len > 0, "split length must be positive");
            let contig_len = ref_index
                .length(spec.contig)
                .unwrap_or_else(|| panic!("unknown split contig: {}", spec.contig))
                .as_u64();
            assert!(
                spec.start >= 1 && spec.start.saturating_add(spec.len) - 1 <= contig_len,
                "split {}:{}+{} exceeds contig length {contig_len}",
                spec.contig,
                spec.start,
                spec.len
            );
            let fits_contig = pos.saturating_add(read_len).saturating_add(spec.len).saturating_sub(1) <= contig_len;
            if rname == spec.contig && pos < spec.start && fits_contig && rng.random_bool(0.5) {
                Some((spec.start - pos, spec.len))
            } else {
                None
            }
        }
        None => None,
    };

    let (cigar, mismatch_offsets) = gen_cigar(rng, read_len, cigar_split);
    let xn = if mismatch_offsets.is_empty() {
        "NA".to_string()
    } else {
        mismatch_offsets
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let seq = seq::gen_seq(rng, ref_seq, &rname, pos, &cigar, &mismatch_offsets)
        .expect("reference fetch for SEQ generation must succeed");
    let qual = gen_qual(rng, read_len);

    format!("{qname}\t{flag}\t{rname}\t{pos}\t{MAPQ}\t{cigar}\t*\t0\t0\t{seq}\t{qual}\tXN:Z:{xn}")
}

/// Builds a coordinate-sorted SAM header from a reference index.
pub fn header_from_ref_index(ref_index: &RefIndex) -> noodles::sam::Header {
    use noodles::sam::header::record::value::{
        Map,
        map::{ReferenceSequence, header::sort_order::COORDINATE, header::tag::SORT_ORDER},
    };

    let mut hd = Map::default();
    hd.other_fields_mut().insert(SORT_ORDER, COORDINATE.into());
    let mut builder = noodles::sam::Header::builder().set_header(hd);
    for name in ref_index.contig_names() {
        let length = ref_index
            .length(&name)
            .expect("contig from RefIndex must have a length");
        let length = std::num::NonZero::new(length.as_u64() as usize).expect("contig length must be nonzero");
        builder = builder.add_reference_sequence(name, Map::<ReferenceSequence>::new(length));
    }
    builder.build()
}

#[cfg(test)]
pub(crate) fn bam_records_from_sam(sam: &str) -> Vec<noodles::bam::Record> {
    use noodles::bam;

    let ref_index = RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai").expect("test fixture must load");
    let header = header_from_ref_index(&ref_index);
    let mut bam_bytes = Vec::new();
    write_bam_from_sam(&mut bam_bytes, &header, sam).unwrap();

    let mut reader = bam::io::Reader::new(bam_bytes.as_slice());
    reader.read_header().unwrap();
    reader.records().map(|record| record.expect("BAM record")).collect()
}

#[cfg(test)]
pub(crate) fn write_bam_from_sam(
    output: impl std::io::Write,
    header: &noodles::sam::Header,
    sam: &str,
) -> std::io::Result<()> {
    use noodles::sam::alignment::io::Write as _;

    let mut writer = noodles::bam::io::Writer::new(output);
    writer.write_header(header)?;
    let mut reader = noodles::sam::io::Reader::new(sam.as_bytes());
    let mut record = noodles::sam::alignment::RecordBuf::default();
    while reader.read_record_buf(header, &mut record)? != 0 {
        writer.write_alignment_record(header, &record)?;
    }
    writer.try_finish()
}

#[cfg(test)]
pub(crate) fn write_indexed_test_bam(test_name: &str, sam: &str) -> (std::path::PathBuf, noodles::sam::Header) {
    use noodles::bam;
    use std::fs::{self, File};

    let dir = std::env::temp_dir().join(format!("apus_{test_name}_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("reads.bam");
    let ref_index = RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai").unwrap();
    let header = header_from_ref_index(&ref_index);
    write_bam_from_sam(File::create(&path).unwrap(), &header, sam).unwrap();
    bam::bai::fs::write(dir.join("reads.bam.bai"), &bam::fs::index(&path).unwrap()).unwrap();
    (path, header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodles::sam::alignment::io::Write as _;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn sam_assembly_preserves_generated_fields_and_locus_coordinates() {
        let ref_index = RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai").unwrap();
        let mut ref_seq = RefSeq::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let mut rng = StdRng::seed_from_u64(42);
        for (read_len, fixed_locus) in [(128, None), (512, Some(("chrM", 100)))] {
            let line = generate_sam_line(&mut rng, &ref_index, &mut ref_seq, read_len, fixed_locus, None);
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 12);
            let contig_len = ref_index.length(fields[2]).expect("RNAME must be indexed").as_u64();
            let pos: u64 = fields[3].parse().expect("POS must be an integer");
            assert!(pos >= 1 && pos + read_len - 1 <= contig_len);
            if let Some((contig, position)) = fixed_locus {
                assert_eq!(fields[2], contig);
                assert_eq!(fields[3].parse::<u64>().unwrap(), position);
            }

            let flag: u16 = fields[1].parse().expect("FLAG must be an integer");
            assert!(flag <= 4095);
            assert_eq!(fields[9].len(), read_len as usize);
            assert_eq!(fields[10].len(), read_len as usize);
            assert!(fields[11].starts_with("XN:Z:"));
        }
    }

    #[test]
    fn split_generation_produces_both_spliced_and_unspliced_reads_at_exact_locus() {
        use crate::cigar::{CigarKind, parse_ops};

        let ref_index = RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai").unwrap();
        let mut ref_seq = RefSeq::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let mut rng = StdRng::seed_from_u64(19);
        let split = SplitSpec {
            contig: "chr1",
            start: 32,
            len: 30,
        };

        let mut spliced = 0usize;
        let mut unspliced = 0usize;
        for pos in 1..=51 {
            let read_len = 100;
            let line = generate_sam_line(
                &mut rng,
                &ref_index,
                &mut ref_seq,
                read_len,
                Some(("chr1", pos)),
                Some(split),
            );
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields[9].len(), read_len as usize);
            assert_eq!(fields[10].len(), read_len as usize);

            let parsed = parse_ops(fields[5]);
            let mut ref_cursor = pos;
            let mut skips = Vec::new();
            for op in &parsed {
                let len = op.len() as u64;
                if op.kind() == CigarKind::Skip {
                    skips.push((ref_cursor, len));
                }
                if op.kind().consumes_reference() {
                    ref_cursor += len;
                }
            }

            if pos >= split.start {
                assert!(
                    skips.is_empty(),
                    "read starting at {pos} inside/after split must stay unspliced"
                );
            } else if skips.is_empty() {
                unspliced += 1;
            } else {
                assert_eq!(skips, vec![(split.start, split.len)]);
                spliced += 1;
            }
        }
        assert!(
            spliced > 0 && unspliced > 0,
            "expected mix of spliced ({spliced}) and unspliced ({unspliced})"
        );
    }

    #[test]
    fn round_trips_generated_reads_through_a_bam_file() {
        let ref_index = RefIndex::from_fai_path("testdata/reference/hs1_subset.fa.fai").unwrap();
        let mut ref_seq = RefSeq::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let mut rng = StdRng::seed_from_u64(7);

        let header = header_from_ref_index(&ref_index);

        let mut sam_lines = Vec::new();
        for read_len in [128, 512, 1_024] {
            sam_lines.push(generate_sam_line(
                &mut rng,
                &ref_index,
                &mut ref_seq,
                read_len,
                None,
                None,
            ));
        }
        sam_lines.push(generate_sam_line(
            &mut rng,
            &ref_index,
            &mut ref_seq,
            100,
            Some(("chr1", 10)),
            Some(SplitSpec {
                contig: "chr1",
                start: 32,
                len: 30,
            }),
        ));

        let sam_text = sam_lines.join("\n");
        let mut sam_reader = noodles::sam::io::Reader::new(sam_text.as_bytes());
        let expected: Vec<noodles::sam::alignment::RecordBuf> = sam_lines
            .iter()
            .map(|_| {
                let mut record = noodles::sam::alignment::RecordBuf::default();
                sam_reader.read_record_buf(&header, &mut record).unwrap();
                record
            })
            .collect();

        let mut buf = Vec::new();
        {
            let mut writer = noodles::bam::io::Writer::new(&mut buf);
            writer.write_header(&header).unwrap();
            for record in &expected {
                writer.write_alignment_record(&header, record).unwrap();
            }
            writer.try_finish().unwrap();
        }

        let mut bam_reader = noodles::bam::io::Reader::new(buf.as_slice());
        let read_back_header = bam_reader.read_header().unwrap();
        assert_eq!(read_back_header, header);

        let actual: Vec<noodles::sam::alignment::RecordBuf> = bam_reader
            .record_bufs(&read_back_header)
            .map(|result| result.unwrap())
            .collect();

        assert_eq!(actual, expected);
    }
}
