//! BAM SEQ in query order, with the even-indexed base in each byte's high nibble.

const BASES: &[u8; 16] = b"=ACMGRSVTWYHKDBN";

#[derive(Clone)]
pub(super) struct PackedSeq {
    bytes: Box<[u8]>,
    len: u32,
}

impl PackedSeq {
    pub(super) fn from_bam_bytes(bytes: &[u8], base_count: usize) -> Self {
        Self {
            bytes: bytes[..base_count.div_ceil(2)].into(),
            len: base_count as u32, // BAM's l_seq is a 32-bit field.
        }
    }

    pub(super) fn as_view(&self) -> PackedSeqRef<'_> {
        PackedSeqRef {
            bytes: &self.bytes,
            len: self.len as usize,
        }
    }

    #[cfg(any(test, feature = "profiling"))]
    pub(super) fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    #[cfg(test)]
    pub(super) fn from_ascii(seq: &[u8]) -> Self {
        let encode = |base: u8| {
            BASES
                .iter()
                .position(|&code| code.eq_ignore_ascii_case(&base))
                .expect("fixture base must have a BAM nibble") as u8
        };
        let bytes = seq
            .chunks(2)
            .map(|pair| (encode(pair[0]) << 4) | pair.get(1).copied().map_or(0, encode))
            .collect();
        Self {
            bytes,
            len: seq.len().try_into().expect("fixture must fit in BAM SEQ"),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct PackedSeqRef<'a> {
    bytes: &'a [u8],
    len: usize,
}

impl<'a> PackedSeqRef<'a> {
    pub(super) fn from_bam_bytes(bytes: &'a [u8], len: usize) -> Self {
        Self {
            bytes: &bytes[..len.div_ceil(2)],
            len,
        }
    }

    pub(super) fn len(self) -> usize {
        self.len
    }

    pub(super) fn nibble_at(self, i: usize) -> Option<u8> {
        if i >= self.len {
            return None;
        }
        let packed = self.bytes[i / 2];
        Some(if i.is_multiple_of(2) { packed >> 4 } else { packed & 0xf })
    }

    pub(super) fn base_at(self, i: usize) -> Option<u8> {
        self.nibble_at(i).map(|code| BASES[code as usize])
    }

    /// The cytosine of a reverse alignment is stored as the G of the CpG.
    pub(super) fn is_cpg_cytosine(self, query_index: usize, reverse: bool) -> bool {
        let Some(start) = query_index.checked_sub(usize::from(reverse)) else {
            return false;
        };
        if start >= self.len.saturating_sub(1) {
            return false;
        }
        let byte_index = start / 2;
        let packed = self.bytes[byte_index];
        if start.is_multiple_of(2) {
            packed == 0x24
        } else {
            packed & 0xf == 2 && self.bytes[byte_index + 1] >> 4 == 4
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bam_alphabet_round_trips_across_nibble_offsets_and_sequence_ends() {
        let alphabet = b"=ACMGRSVTWYHKDBN";
        let packed = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x0f];
        for len in 0..=17 {
            let ascii: Vec<_> = alphabet.iter().copied().cycle().take(len).collect();
            let from_ascii = PackedSeq::from_ascii(&ascii);
            let from_bam = PackedSeq::from_bam_bytes(&packed, len);
            assert_eq!(from_ascii.byte_len(), len.div_ceil(2));
            assert_eq!(from_bam.byte_len(), len.div_ceil(2));
            for seq in [from_ascii.as_view(), from_bam.as_view()] {
                let bases: Vec<_> = (0..len).map(|i| seq.base_at(i).unwrap()).collect();
                assert_eq!(bases, ascii);
                assert_eq!(seq.base_at(len), None);
            }
        }
    }

    #[test]
    fn packed_cpg_checks_match_ascii_in_both_strands() {
        let ascii = b"cgAcgCGTncCgGCc";
        for len in 0..=ascii.len() {
            let ascii = &ascii[..len];
            let packed = PackedSeq::from_ascii(ascii);
            for reverse in [false, true] {
                for index in 0..=len + 1 {
                    let expected = index
                        .checked_sub(usize::from(reverse))
                        .and_then(|start| ascii.get(start..start + 2))
                        .is_some_and(|pair| pair.eq_ignore_ascii_case(b"CG"));
                    assert_eq!(packed.as_view().is_cpg_cytosine(index, reverse), expected);
                }
            }
        }
    }
}
