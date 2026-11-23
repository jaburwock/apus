//! Indexed reads from coordinate-sorted BAMs, using one reusable record.

use std::io;
use std::ops::Range;

use noodles::core::Position;
use noodles::csi::BinningIndex;
use noodles::{bam, bgzf, csi, sam};

use crate::basepair::BasePair;
use crate::cigar::CigarOp;

use crate::viewer_app::bam_index::SharedIndex;

pub(super) struct BamRegionReader<'a, R> {
    reader: bam::io::Reader<csi::io::Query<'a, R>>,
    record: bam::Record,
    ref_id: usize,
    range: Range<BasePair>,
    done: bool,
}

impl<'a, R: bgzf::io::BufRead + bgzf::io::Seek> BamRegionReader<'a, R> {
    pub(super) fn query(
        reader: &'a mut R,
        index: &SharedIndex,
        header: &sam::Header,
        contig: &str,
        range: Range<BasePair>,
    ) -> io::Result<Self> {
        let (ref_id, chunks) = if range.is_empty() {
            (0, Vec::new())
        } else {
            let ref_id = header
                .reference_sequences()
                .get_index_of(contig.as_bytes())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("unknown BAM contig: {contig}")))?;
            // Re-tag as a coordinate: `to_display` adds 1 only to coordinates, not lengths.
            let start = bam_position(BasePair(range.start.as_u64()).to_display())?;
            let end = bam_position(range.end.as_u64())?;
            (ref_id, index.query(ref_id, (start..=end).into())?)
        };
        Ok(Self {
            reader: bam::io::Reader::from(csi::io::Query::new(reader, chunks)),
            record: bam::Record::default(),
            ref_id,
            range,
            done: false,
        })
    }

    /// Returns records overlapping the half-open range. Cancellation ends the query like EOF.
    pub(super) fn next_record(&mut self, cancelled: impl Fn() -> bool) -> io::Result<Option<(BasePair, &bam::Record)>> {
        while !self.done && !cancelled() && self.reader.read_record(&mut self.record)? != 0 {
            let Some(ref_id) = self.record.reference_sequence_id().transpose()? else {
                continue;
            };
            if ref_id > self.ref_id {
                break;
            }
            if ref_id < self.ref_id {
                continue;
            }
            let Some(pos) = self.record.alignment_start().transpose()? else {
                continue;
            };
            let start = BasePair::from_1based(usize::from(pos) as u64);
            if start >= self.range.end {
                break;
            }
            // Only upstream starts need a span check. Downstream callers can walk CIGAR once.
            if start < self.range.start && start + record_span(&self.record)? <= self.range.start {
                continue;
            }
            return Ok(Some((start, &self.record)));
        }
        self.done = true;
        Ok(None)
    }
}

fn bam_position(pos: u64) -> io::Result<Position> {
    usize::try_from(pos)
        .ok()
        .and_then(Position::new)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "BAM query position is out of range"))
}

pub(super) fn record_span(record: &bam::Record) -> io::Result<BasePair> {
    let cigar = record.cigar();
    let mut span = BasePair(0);
    for op in CigarOp::iter_bam_bytes(cigar.as_bytes())? {
        let op = op?;
        if op.kind().consumes_reference() {
            span = span + BasePair::length(op.len() as u64);
        }
    }
    // BAM indexes an alignment with no reference-consuming ops as a single position.
    Ok(span.max(BasePair(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodles::core::Region;
    use std::fs::{self, File};

    #[test]
    fn region_reader_matches_noodles_at_alignment_and_contig_edges() {
        let (path, header) = crate::testgen::write_indexed_test_bam(
            "region_reader",
            concat!(
                "left\t0\tchr1\t1\t60\t4M\t*\t0\t0\t*\t*\n",
                "span\t0\tchr1\t3\t60\t10M\t*\t0\t0\t*\t*\n",
                "point\t0\tchr1\t6\t60\t1I\t*\t0\t0\t*\t*\n",
                "right\t0\tchr1\t9\t60\t3M\t*\t0\t0\t*\t*\n",
                "last\t0\tchr1\t17\t60\t2M\t*\t0\t0\t*\t*\n",
                "other\t0\tchr2\t1\t60\t5M\t*\t0\t0\t*\t*\n",
            ),
        );
        let index = SharedIndex::read(&path).unwrap();
        let mut bgzf = bgzf::io::Reader::new(File::open(&path).unwrap());
        let mut noodles = bam::io::IndexedReader::new(File::open(&path).unwrap(), index.clone());
        for (contig, start, end, names) in [
            ("chr1", 0, 1, &["left"][..]),
            ("chr1", 4, 8, &["span", "point"]),
            ("chr1", 6, 8, &["span"]),
            ("chr1", 8, 12, &["span", "right"]),
            ("chr1", 12, 16, &[]),
            ("chr1", 16, 30, &["last"]),
            ("chr2", 0, 30, &["other"]),
        ] {
            let interval = Position::try_from(start + 1).unwrap()..=Position::try_from(end).unwrap();
            let oracle: Vec<_> = noodles
                .query(&header, &Region::new(contig, interval))
                .unwrap()
                .records()
                .map(|record| record.unwrap().name().unwrap().to_string())
                .collect();
            let range = BasePair(start as u64)..BasePair(end as u64);
            let mut query = BamRegionReader::query(&mut bgzf, &index, &header, contig, range.clone()).unwrap();
            let mut got = Vec::new();
            while let Some((start, record)) = query.next_record(|| false).unwrap() {
                assert!(start < range.end);
                got.push(record.name().unwrap().to_string());
            }
            assert_eq!(got, oracle, "{contig}:{start}..{end}");
            assert_eq!(got, names, "{contig}:{start}..{end}");
        }
        let empty = BasePair(5)..BasePair(5);
        let mut query = BamRegionReader::query(&mut bgzf, &index, &header, "chr1", empty).unwrap();
        assert!(query.next_record(|| false).unwrap().is_none());
        assert!(matches!(
            BamRegionReader::query(&mut bgzf, &index, &header, "missing", BasePair(0)..BasePair(1)),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
