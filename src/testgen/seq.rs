//! Builds SAM `SEQ` strings from an indexed reference and a CIGAR.
//!
//! [`RefSeq`] owns the FASTA reader used for base fetching; contig metadata remains with the
//! reference index. `gen_seq` first loads the reference span consumed by the CIGAR, then walks
//! the complete operation vocabulary so query and reference consumption stay explicit.
//!
//! Reference-backed operations copy or deliberately substitute bases according to their CIGAR
//! kind and the supplied mismatch offsets. Insertions and soft clips are filled with random bases,
//! while operations that consume no query emit nothing. The fetch position is a SAM 1-based
//! coordinate, and malformed text CIGARs remain an internal invariant rather than a user parse path.

use std::io;
use std::path::Path;

use noodles::core::{Position, Region};
use noodles::fasta;
use rand::{Rng, RngExt};

use crate::cigar::{CigarKind, parse_ops, ref_len};

const BASES: [u8; 4] = *b"ACGT";

/// Indexed FASTA reader used for SEQ generation.
pub struct RefSeq {
    reader: fasta::io::IndexedReader<fasta::io::BufReader<std::fs::File>>,
}

impl RefSeq {
    pub fn from_fa_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let reader = fasta::io::indexed_reader::Builder::default().build_from_path(path)?;
        Ok(Self { reader })
    }

    /// Fetches a 1-based reference span.
    fn fetch(&mut self, contig: &str, pos: u64, len: u64) -> io::Result<Vec<u8>> {
        let start = Position::try_from(pos as usize).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let end =
            Position::try_from((pos + len - 1) as usize).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let region = Region::new(contig, start..=end);
        let record = self.reader.query(&region)?;
        Ok(record.sequence().as_ref().to_ascii_uppercase())
    }
}

/// Builds SEQ by walking CIGAR operations and applying mismatch offsets.
pub fn gen_seq<R: Rng + ?Sized>(
    rng: &mut R,
    ref_seq: &mut RefSeq,
    contig: &str,
    pos: u64,
    cigar: &str,
    mismatch_offsets: &[u64],
) -> io::Result<String> {
    let parsed = parse_ops(cigar);
    let ref_bases = ref_seq.fetch(contig, pos, ref_len(&parsed))?;
    let mismatches: std::collections::HashSet<u64> = mismatch_offsets.iter().copied().collect();

    let mut seq = Vec::new();
    let mut ref_pos: usize = 0;
    let mut query_pos: u64 = 0;

    for op in &parsed {
        let len = op.len() as u64;
        match op.kind() {
            CigarKind::Match => {
                for _ in 0..len {
                    let ref_base = ref_bases[ref_pos];
                    ref_pos += 1;
                    let base = if mismatches.contains(&query_pos) {
                        mismatched_base(rng, ref_base)
                    } else {
                        ref_base
                    };
                    seq.push(base);
                    query_pos += 1;
                }
            }
            CigarKind::Insertion | CigarKind::SoftClip => {
                for _ in 0..len {
                    seq.push(BASES[rng.random_range(0..BASES.len())]);
                    query_pos += 1;
                }
            }
            CigarKind::Deletion | CigarKind::Skip => {
                ref_pos += len as usize;
            }
            CigarKind::SequenceMatch => {
                for _ in 0..len {
                    seq.push(ref_bases[ref_pos]);
                    ref_pos += 1;
                    query_pos += 1;
                }
            }
            CigarKind::SequenceMismatch => {
                for _ in 0..len {
                    let base = mismatched_base(rng, ref_bases[ref_pos]);
                    seq.push(base);
                    ref_pos += 1;
                    query_pos += 1;
                }
            }
            CigarKind::HardClip | CigarKind::Pad => {}
        }
    }

    Ok(String::from_utf8(seq).expect("bases must be valid UTF-8"))
}

/// Samples a base different from `ref_base`.
fn mismatched_base<R: Rng + ?Sized>(rng: &mut R, ref_base: u8) -> u8 {
    loop {
        let candidate = BASES[rng.random_range(0..BASES.len())];
        if candidate != ref_base {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cigar::parse_ops;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn generated_sequence_matches_cigar_consumption_and_base_alphabet() {
        let mut ref_seq = RefSeq::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let mut rng = StdRng::seed_from_u64(3);

        let cigar = "5M2I3M1D4M";
        let mismatch_offsets = [];
        let seq = gen_seq(&mut rng, &mut ref_seq, "chrM", 100, cigar, &mismatch_offsets).unwrap();

        let query_len: usize = parse_ops(cigar)
            .ops()
            .iter()
            .filter(|op| op.kind().consumes_read())
            .map(|op| op.len())
            .sum();
        assert_eq!(seq.len(), query_len);
        assert!(seq.bytes().all(|b| matches!(b, b'A' | b'C' | b'G' | b'T')));
    }

    #[test]
    fn mismatch_offsets_produce_different_base_than_reference() {
        let mut ref_seq = RefSeq::from_fa_path("testdata/reference/hs1_subset.fa").unwrap();
        let mut rng = StdRng::seed_from_u64(3);

        let cigar = "20M";
        let mismatch_offsets = [3, 7];
        let ref_bases = ref_seq.fetch("chrM", 100, 20).unwrap();
        let seq = gen_seq(&mut rng, &mut ref_seq, "chrM", 100, cigar, &mismatch_offsets).unwrap();

        for &offset in &mismatch_offsets {
            let i = offset as usize;
            assert_ne!(seq.as_bytes()[i], ref_bases[i]);
        }
    }
}
