//! Validated BAM CIGAR operations shared by alignment and fixture generation.

use std::io;

#[derive(Debug, Clone, Default)]
pub struct Cigar(Box<[CigarOp]>);

impl Cigar {
    pub fn from_bam_bytes(bytes: &[u8]) -> io::Result<Self> {
        let parsed = CigarOp::iter_bam_bytes(bytes)?;
        let mut ops = Vec::with_capacity(parsed.len());
        for op in parsed {
            ops.push(op?);
        }
        Ok(Self(ops.into_boxed_slice()))
    }

    #[cfg(any(test, feature = "testgen"))]
    pub fn new(ops: Vec<CigarOp>) -> Self {
        Self(ops.into_boxed_slice())
    }

    pub fn ops(&self) -> &[CigarOp] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Cigar {
    type Item = &'a CigarOp;
    type IntoIter = std::slice::Iter<'a, CigarOp>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops().iter()
    }
}

/// A BAM operation word with the length in the upper 28 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CigarOp(u32);

impl CigarOp {
    pub(crate) fn iter_bam_bytes(bytes: &[u8]) -> io::Result<impl ExactSizeIterator<Item = io::Result<Self>> + '_> {
        let (chunks, []) = bytes.as_chunks() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated BAM CIGAR operation",
            ));
        };
        Ok(chunks.iter().map(|chunk| {
            let word = u32::from_le_bytes(*chunk);
            if word & 0xf > CigarKind::SequenceMismatch as u32 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid BAM CIGAR opcode"));
            }
            Ok(Self(word))
        }))
    }

    #[cfg(any(test, feature = "testgen"))]
    pub fn new(kind: CigarKind, len: usize) -> Self {
        assert!(
            len <= (u32::MAX >> 4) as usize,
            "CIGAR operation exceeds the BAM length limit"
        );
        Self(((len as u32) << 4) | kind as u32)
    }

    pub fn kind(self) -> CigarKind {
        match self.0 & 0xf {
            0 => CigarKind::Match,
            1 => CigarKind::Insertion,
            2 => CigarKind::Deletion,
            3 => CigarKind::Skip,
            4 => CigarKind::SoftClip,
            5 => CigarKind::HardClip,
            6 => CigarKind::Pad,
            7 => CigarKind::SequenceMatch,
            8 => CigarKind::SequenceMismatch,
            _ => unreachable!("CIGAR opcodes are validated at construction"),
        }
    }

    pub fn len(self) -> usize {
        (self.0 >> 4) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CigarKind {
    Match = 0,
    Insertion = 1,
    Deletion = 2,
    Skip = 3,
    SoftClip = 4,
    HardClip = 5,
    Pad = 6,
    SequenceMatch = 7,
    SequenceMismatch = 8,
}

impl CigarKind {
    pub fn consumes_reference(self) -> bool {
        match self {
            Self::Match | Self::Deletion | Self::Skip | Self::SequenceMatch | Self::SequenceMismatch => true,
            Self::Insertion | Self::SoftClip | Self::HardClip | Self::Pad => false,
        }
    }

    pub fn consumes_read(self) -> bool {
        match self {
            Self::Match | Self::Insertion | Self::SoftClip | Self::SequenceMatch | Self::SequenceMismatch => true,
            Self::Deletion | Self::Skip | Self::HardClip | Self::Pad => false,
        }
    }
}

/// Parses internal fixture CIGAR text, which must be well-formed and BAM-encodable.
#[cfg(any(test, feature = "testgen"))]
pub fn parse_ops(cigar: &str) -> Cigar {
    use noodles::sam::alignment::record::cigar::op::Kind;
    use noodles::sam::record::Cigar as NoodlesCigar;

    let ops = NoodlesCigar::new(cigar.as_bytes())
        .iter()
        .map(|op| {
            let op = op.expect("cigar must be well-formed");
            let kind = match op.kind() {
                Kind::Match => CigarKind::Match,
                Kind::Insertion => CigarKind::Insertion,
                Kind::Deletion => CigarKind::Deletion,
                Kind::Skip => CigarKind::Skip,
                Kind::SoftClip => CigarKind::SoftClip,
                Kind::HardClip => CigarKind::HardClip,
                Kind::Pad => CigarKind::Pad,
                Kind::SequenceMatch => CigarKind::SequenceMatch,
                Kind::SequenceMismatch => CigarKind::SequenceMismatch,
            };
            CigarOp::new(kind, op.len())
        })
        .collect();
    Cigar::new(ops)
}

/// Returns the reference bases consumed by the CIGAR.
pub fn ref_len(cigar: &Cigar) -> u64 {
    cigar
        .ops()
        .iter()
        .filter(|op| op.kind().consumes_reference())
        .map(|op| op.len() as u64)
        .sum()
}

#[cfg(any(test, feature = "testgen"))]
pub fn format_ops(cigar: &Cigar) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    for op in cigar {
        let code = match op.kind() {
            CigarKind::Match => 'M',
            CigarKind::Insertion => 'I',
            CigarKind::Deletion => 'D',
            CigarKind::Skip => 'N',
            CigarKind::SoftClip => 'S',
            CigarKind::HardClip => 'H',
            CigarKind::Pad => 'P',
            CigarKind::SequenceMatch => '=',
            CigarKind::SequenceMismatch => 'X',
        };
        write!(text, "{}{code}", op.len()).expect("writing to String cannot fail");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bam_operations_round_trip_with_reference_and_read_lengths() {
        let bytes = [
            0x50, 0, 0, 0, // 5M
            0x21, 0, 0, 0, // 2I
            0x12, 0, 0, 0, // 1D
            0x33, 0, 0, 0, // 3N
            0x44, 0, 0, 0, // 4S
            0x55, 0, 0, 0, // 5H
            0x66, 0, 0, 0, // 6P
            0x77, 0, 0, 0, // 7=
            0xf8, 0xff, 0xff, 0xff, // max length X
        ];
        let max_len = (1 << 28) - 1;
        let cigar = Cigar::from_bam_bytes(&bytes).unwrap();
        let text = format_ops(&cigar);
        assert_eq!(text, format!("5M2I1D3N4S5H6P7={max_len}X"));
        assert_eq!(parse_ops(&text).ops(), cigar.ops());
        assert_eq!(ref_len(&cigar), 5 + 1 + 3 + 7 + max_len);
        let read_len: usize = cigar
            .ops()
            .iter()
            .filter(|op| op.kind().consumes_read())
            .map(|op| op.len())
            .sum();
        assert_eq!(read_len as u64, 5 + 2 + 4 + 7 + max_len);
    }

    #[test]
    fn malformed_bam_operations_are_rejected() {
        for opcode in 9..=15 {
            assert_eq!(
                Cigar::from_bam_bytes(&[0x10 | opcode, 0, 0, 0]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for len in 1..4 {
            assert_eq!(
                Cigar::from_bam_bytes(&[0x10, 0, 0, 0, 0x20, 0, 0][..4 + len])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn bam_long_cigar_resolves_overflow_before_packing() {
        let pairs = usize::from(u16::MAX) / 2 + 1;
        let text = "1M1I".repeat(pairs);
        let seq = "AC".repeat(pairs);
        let sam = format!("long\t0\tchr1\t1\t60\t{text}\t*\t0\t0\t{seq}\t*\n");
        let records = crate::testgen::bam_records_from_sam(&sam);
        let cigar = Cigar::from_bam_bytes(records[0].cigar().as_bytes()).unwrap();
        assert_eq!(cigar.ops().len(), pairs * 2);
        assert_eq!(ref_len(&cigar), pairs as u64);
        assert_eq!(format_ops(&cigar), text);
    }
}
