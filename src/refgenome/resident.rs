//! Ingest of a small FASTA containing named chromosome slices with original coordinates.

use std::io::{self, BufReader};
use std::ops::Range;

use crate::basepair::BasePair;

use super::RefIndex;

pub(crate) struct ReferenceWindow {
    pub(crate) contig: String,
    pub(crate) span: Range<BasePair>,
    pub(crate) bases: Vec<u8>,
}

pub(crate) fn read_windows(bytes: &[u8], index: &RefIndex) -> io::Result<Vec<ReferenceWindow>> {
    let mut reader = noodles::fasta::io::Reader::new(BufReader::new(flate2::read::MultiGzDecoder::new(bytes)));
    let mut windows = Vec::new();
    let invalid = |message| io::Error::new(io::ErrorKind::InvalidData, message);
    for record in reader.records() {
        let record = record?;
        let name = std::str::from_utf8(record.name()).map_err(|_| invalid("Reference slice name is not UTF-8"))?;
        let (contig, coordinates) = name
            .rsplit_once(':')
            .ok_or_else(|| invalid("Reference slice needs a range"))?;
        let (first, last) = coordinates
            .split_once('-')
            .ok_or_else(|| invalid("Reference slice needs both ends"))?;
        let first = first
            .parse::<u64>()
            .map_err(|_| invalid("Invalid reference slice start"))?;
        let last = last
            .parse::<u64>()
            .map_err(|_| invalid("Invalid reference slice end"))?;
        if first == 0 || first > last {
            return Err(invalid("Reference slice range must be 1-based and inclusive"));
        }
        let span = BasePair::from_1based(first)..(BasePair::from_1based(last) + BasePair(1));
        let bases = record.sequence().as_ref().to_vec();
        if index.length(contig).is_none_or(|length| span.end > length)
            || (span.end - span.start).as_u64() != bases.len() as u64
        {
            return Err(invalid("Reference slice length does not match its range or contig"));
        }
        windows.push(ReferenceWindow {
            contig: contig.to_owned(),
            span,
            bases,
        });
    }
    windows.sort_unstable_by(|a, b| a.contig.cmp(&b.contig).then(a.span.start.cmp(&b.span.start)));
    if windows.is_empty()
        || windows
            .windows(2)
            .any(|pair| pair[0].contig == pair[1].contig && pair[0].span.end > pair[1].span.start)
    {
        return Err(invalid("Reference slices must be present and disjoint"));
    }
    Ok(windows)
}
