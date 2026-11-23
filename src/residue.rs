//! Converts raw nucleotide bytes into the residue vocabulary used by rendering and palettes.
//!
//! `BaseResidue` represents A, C, G, T, N, plus `Null` for a cell with no sequence data.
//! Conversion is ASCII-case-insensitive and deliberately distinguishes recognized but unsupported
//! IUPAC ambiguity codes from unrecognized bytes.
//!
//! This module reports conversion results only; callers decide whether an error should fall back
//! to `Null`, be ignored, or be surfaced.

/// A nucleotide base or a no-data marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseResidue {
    A,
    C,
    G,
    T,
    N,
    /// No sequence data at this cell.
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidueError {
    /// A legal but unsupported IUPAC ambiguity code.
    UnimplementedIupacCode(u8),
    /// An unrecognized byte.
    Unrecognized(u8),
}

impl TryFrom<u8> for BaseResidue {
    type Error = ResidueError;

    fn try_from(base: u8) -> Result<Self, ResidueError> {
        match base.to_ascii_uppercase() {
            b'A' => Ok(Self::A),
            b'C' => Ok(Self::C),
            b'G' => Ok(Self::G),
            b'T' => Ok(Self::T),
            b'N' => Ok(Self::N),
            b'R' | b'Y' | b'W' | b'S' | b'K' | b'M' | b'B' | b'D' | b'H' | b'V' => {
                Err(ResidueError::UnimplementedIupacCode(base))
            }
            other => Err(ResidueError::Unrecognized(other)),
        }
    }
}

impl BaseResidue {
    pub const fn as_char(self) -> char {
        match self {
            Self::A => 'A',
            Self::C => 'C',
            Self::G => 'G',
            Self::T => 'T',
            Self::N => 'N',
            Self::Null => '-',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residue_conversion_round_trips_and_classifies_unsupported_bytes() {
        for base in *b"ACGTN" {
            assert_eq!(BaseResidue::try_from(base).unwrap().as_char(), base as char);
        }
        assert_eq!(BaseResidue::try_from(b'a'), Ok(BaseResidue::A));
        assert_eq!(BaseResidue::try_from(b'?'), Err(ResidueError::Unrecognized(b'?')));
        assert_eq!(
            BaseResidue::try_from(b'R'),
            Err(ResidueError::UnimplementedIupacCode(b'R'))
        );
    }
}
