//! Shared BAM decoding and classification for same-strand 5mC calls.

use std::ffi::CStr;
use std::ops::Range;

use noodles::bam;

use super::packed_seq::PackedSeqRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Call {
    Hypo,
    Uncertain,
    Hyper,
}

impl Call {
    /// Uses the SAM ML byte's 1/256 steps. Below 25% is hypo and 75% or more is hyper.
    pub(super) fn from_prob(prob: u8) -> Self {
        const HYPO_BELOW: u8 = 64;
        const HYPER_FROM: u8 = 192;
        match prob {
            ..HYPO_BELOW => Self::Hypo,
            HYPO_BELOW..HYPER_FROM => Self::Uncertain,
            HYPER_FROM.. => Self::Hyper,
        }
    }
}

/// Decoded same-strand 5mC calls for one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseMod5mC {
    /// Skipped cytosines have low modification probability when the status is `.` or omitted.
    pub implicit_unmodified: bool,
    /// Explicit calls sorted by 0-based BAM SEQ offset, in CIGAR query order.
    pub calls: Vec<(u32, u8)>,
}

impl BaseMod5mC {
    pub(super) fn cpg_cursor<'a>(&'a self, seq: PackedSeqRef<'a>, reverse: bool) -> CpGCallCursor<'a> {
        CpGCallCursor {
            seq,
            reverse,
            implicit_unmodified: self.implicit_unmodified,
            explicit: &self.calls,
        }
    }
}

/// Visits aligned query spans in CIGAR order, skipping calls in clips and insertions.
pub(super) struct CpGCallCursor<'a> {
    seq: PackedSeqRef<'a>,
    reverse: bool,
    implicit_unmodified: bool,
    explicit: &'a [(u32, u8)],
}

impl CpGCallCursor<'_> {
    pub(super) fn in_span(&mut self, query: Range<usize>) -> impl Iterator<Item = (usize, u8)> {
        let mut query = query.start..query.end.min(self.seq.len());
        let skip = self
            .explicit
            .partition_point(|&(offset, _)| (offset as usize) < query.start);
        self.explicit = &self.explicit[skip..];
        if !self.implicit_unmodified
            && self
                .explicit
                .first()
                .is_none_or(|&(offset, _)| offset as usize >= query.end)
        {
            query.end = query.start;
        }

        query.filter_map(move |offset| {
            if !self.seq.is_cpg_cytosine(offset, self.reverse) {
                return None;
            }
            while self.explicit.first().is_some_and(|&(pos, _)| (pos as usize) < offset) {
                self.explicit = &self.explicit[1..];
            }
            let prob = self
                .explicit
                .first()
                .filter(|&&(pos, _)| pos as usize == offset)
                .map(|&(_, prob)| prob)
                .or(self.implicit_unmodified.then_some(0))?;
            Some((offset, prob))
        })
    }
}

pub(super) fn decode_5mc_from_record(record: &bam::Record) -> Option<BaseMod5mC> {
    let seq = record.sequence();
    if seq.is_empty() {
        return None;
    }
    let (mm, ml) = modification_tags(record.data().as_bytes(), seq.len())?;
    decode_5mc(
        mm,
        ml,
        PackedSeqRef::from_bam_bytes(seq.as_bytes(), seq.len()),
        record.flags().is_reverse_complemented(),
    )
}

type AuxValue<'a> = (u8, &'a [u8]);

#[derive(Default)]
struct ModTags<'a> {
    mm: Option<AuxValue<'a>>,
    ml: Option<AuxValue<'a>>,
}

fn modification_tags(mut aux: &[u8], seq_len: usize) -> Option<(&[u8], &[u8])> {
    let (mut modern, mut legacy) = (ModTags::default(), ModTags::default());
    let mut mn = None;
    while !aux.is_empty() {
        let (&[first, second, ty], rest) = aux.split_first_chunk()?;
        aux = rest;
        let value = (ty, take_aux_payload(ty, &mut aux)?);
        match [first, second] {
            [b'M', b'M'] => modern.mm = Some(value),
            [b'M', b'L'] => modern.ml = Some(value),
            [b'M', b'm'] => legacy.mm = Some(value),
            [b'M', b'l'] => legacy.ml = Some(value),
            [b'M', b'N'] => mn = Some(value),
            _ => {}
        }
    }

    // The MM spelling selects the pair even if its type is wrong or its ML is absent.
    let tags = if modern.mm.is_some() { modern } else { legacy };
    let (b'Z', mm) = tags.mm? else {
        return None;
    };
    let ml = match tags.ml {
        Some((b'B', [b'C', _, _, _, _, values @ ..])) => values,
        None => &[],
        _ => return None,
    };
    if let Some(value) = mn
        && aux_int(value)? != seq_len as i64
    {
        return None;
    }
    Some((mm, ml))
}

fn take_aux_payload<'a>(ty: u8, aux: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = match ty {
        b'A' | b'c' | b'C' => 1,
        b's' | b'S' => 2,
        b'i' | b'I' | b'f' => 4,
        b'Z' | b'H' => {
            let value = CStr::from_bytes_until_nul(aux).ok()?;
            *aux = &aux[value.to_bytes_with_nul().len()..];
            return Some(value.to_bytes());
        }
        b'B' => {
            let (header, _) = aux.split_first_chunk::<5>()?;
            let &[subtype, count @ ..] = header;
            let width = match subtype {
                b'c' | b'C' => 1,
                b's' | b'S' => 2,
                b'i' | b'I' | b'f' => 4,
                _ => return None,
            };
            let count = usize::try_from(u32::from_le_bytes(count)).ok()?;
            header.len().checked_add(count.checked_mul(width)?)?
        }
        _ => return None,
    };
    aux.split_off(..len)
}

fn aux_int((ty, bytes): AuxValue<'_>) -> Option<i64> {
    Some(match ty {
        b'c' => i64::from(i8::from_le_bytes(bytes.try_into().ok()?)),
        b'C' => i64::from(u8::from_le_bytes(bytes.try_into().ok()?)),
        b's' => i64::from(i16::from_le_bytes(bytes.try_into().ok()?)),
        b'S' => i64::from(u16::from_le_bytes(bytes.try_into().ok()?)),
        b'i' => i64::from(i32::from_le_bytes(bytes.try_into().ok()?)),
        b'I' => i64::from(u32::from_le_bytes(bytes.try_into().ok()?)),
        _ => return None,
    })
}

fn decode_5mc(mut mm: &[u8], mut ml: &[u8], seq: PackedSeqRef<'_>, reverse: bool) -> Option<BaseMod5mC> {
    let mut mods: Option<BaseMod5mC> = None;
    let mut needs_sort = false;
    while !mm.is_empty() {
        let (&[base, strand], rest) = mm.split_first_chunk()?;
        mm = rest;
        // SEQ has no U nibble, so U is matched as T.
        let nibble = match (base, reverse) {
            (b'A', false) | (b'T' | b'U', true) => Some(1),
            (b'C', false) | (b'G', true) => Some(2),
            (b'G', false) | (b'C', true) => Some(4),
            (b'T' | b'U', false) | (b'A', true) => Some(8),
            // N counts all positions.
            (b'N', _) => None,
            _ => return None,
        };
        if !matches!(strand, b'+' | b'-') {
            return None;
        }
        let (stride, mod_index) = modification_codes(&mut mm)?;
        let implicit_unmodified = !mm.starts_with(b"?");
        if let Some((b'.' | b'?', rest)) = mm.split_first() {
            mm = rest;
        }
        let mut target = if base == b'C' && strand == b'+' {
            mod_index.map(|index| {
                let mods = mods.get_or_insert_with(|| BaseMod5mC {
                    implicit_unmodified: true,
                    calls: Vec::new(),
                });
                mods.implicit_unmodified &= implicit_unmodified;
                (index, mods.calls.len(), &mut mods.calls)
            })
        } else {
            None
        };
        let matches_base = |&i: &usize| seq.nibble_at(i) == nibble;
        let mut positions = 0..seq.len();
        while let Some(rest) = mm.strip_prefix(b",") {
            mm = rest;
            let skip = take_decimal(&mut mm)?;
            let probs = ml.split_off(..stride)?;
            let i = match (base, reverse) {
                (b'N', false) => positions.nth(skip),
                (b'N', true) => positions.nth_back(skip),
                (_, false) => positions.by_ref().filter(matches_base).nth(skip),
                (_, true) => positions.by_ref().filter(matches_base).nth_back(skip),
            }?;
            if let Some((index, _, calls)) = &mut target {
                calls.push((i as u32, probs[*index]));
            }
        }
        mm = mm.strip_prefix(b";")?;
        if let Some((_, start, calls)) = target {
            needs_sort |= start != 0 && start != calls.len();
            if reverse {
                calls[start..].reverse();
            }
        }
    }
    if !ml.is_empty() {
        return None;
    }
    let mut mods = mods?;
    if needs_sort {
        mods.calls.sort_by_key(|&(pos, _)| pos);
    }
    Some(mods)
}

fn modification_codes(mm: &mut &[u8]) -> Option<(usize, Option<usize>)> {
    match mm.first()? {
        b'0'..=b'9' => {
            let id = u32::try_from(take_decimal(mm)?).ok()?;
            Some((1, (id == 27551).then_some(0)))
        }
        // SAM also defines uppercase single-letter codes for unspecified modifications.
        b'A' | b'C' | b'G' | b'T' | b'U' | b'N' => {
            *mm = &mm[1..];
            Some((1, None))
        }
        b'a'..=b'z' => {
            let len = mm.iter().take_while(|b| b.is_ascii_lowercase()).count();
            let codes = mm.split_off(..len)?;
            Some((len, codes.iter().position(|&code| code == b'm')))
        }
        _ => None,
    }
}

fn take_decimal(mm: &mut &[u8]) -> Option<usize> {
    let (&first @ b'0'..=b'9', rest) = mm.split_first()? else {
        return None;
    };
    *mm = rest;
    let mut n = usize::from(first - b'0');
    while let Some((&digit @ b'0'..=b'9', rest)) = mm.split_first() {
        n = n.checked_mul(10)?.checked_add(usize::from(digit - b'0'))?;
        *mm = rest;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::bam_records_from_sam;
    use crate::viewer_app::genome::packed_seq::PackedSeq;
    use noodles::sam::alignment::record_buf::Sequence;
    use noodles::sam::record::data::field::value::BaseModifications;
    use noodles::sam::record::data::field::value::base_modifications::group::{
        Status, Strand, UnmodifiedBase,
        modification::{FIVE_METHYLCYTOSINE, Modification},
    };

    #[test]
    fn call_classes_use_the_quarter_and_three_quarter_boundaries() {
        for prob in u8::MIN..=u8::MAX {
            let lower_bound = f32::from(prob) / 256.0;
            let expected = if lower_bound < 0.25 {
                Call::Hypo
            } else if lower_bound < 0.75 {
                Call::Uncertain
            } else {
                Call::Hyper
            };
            assert_eq!(Call::from_prob(prob), expected, "ML byte {prob}");
        }
    }

    #[test]
    fn bam_5mc_calls_keep_their_probabilities_in_seq_order() {
        let cases = [
            (
                0,
                "C+h.,0;C+m.,1,0;",
                "ML:B:C,12,184,237",
                true,
                vec![(3, 184), (5, 237)],
            ),
            (16, "C+hm?,0,1;", "ML:B:C,33,72,8,218", false, vec![(2, 218), (6, 72)]),
            (0, "N+n,3;C+27551,2;", "ML:B:C,16,199", true, vec![(5, 199)]),
            (0, "C+m.;", "MN:i:8", true, vec![]),
        ];
        let sam: String = cases
            .iter()
            .map(|(flag, mm, tags, ..)| {
                format!("read\t{flag}\tchr1\t2\t60\t8M\t*\t0\t0\tACGCGCGT\t*\tMM:Z:{mm}\t{tags}\n")
            })
            .collect();
        let records = bam_records_from_sam(&sam);
        assert_eq!(records.len(), cases.len());
        for (record, (_, _, _, implicit_unmodified, calls)) in records.iter().zip(cases) {
            assert_eq!(
                decode_5mc_from_record(record),
                Some(BaseMod5mC {
                    implicit_unmodified,
                    calls
                })
            );
        }
    }

    #[test]
    fn malformed_mod_tags_do_not_decode_and_legacy_tags_still_work() {
        let cases = [
            ("Mm:Z:C+m?,0;\tMl:B:C,147", Some(147)),
            ("Mm:Z:C+m?,0;\tML:B:C,17\tMl:B:C,147", Some(147)),
            ("Mm:Z:C+m?,0;\tMl:B:C,17\tMM:Z:C+m?,0;\tML:B:C,203", Some(203)),
            ("Mm:Z:C+m?,0;\tMl:B:C,147\tMM:i:1", None),
            ("MM:Z:C+m?,0;\tMl:B:C,147", None),
            ("MM:Z:C+m?,0;\tML:B:c,47", None),
            ("MM:Z:C+m?,0;\tML:B:C,147,245", None),
            ("MM:Z:C+m?,0,0;\tML:B:C,147", None),
            ("MM:Z:C+m?,0;", None),
            ("MM:Z:C+m?,7;\tML:B:C,147", None),
        ];
        let sam: String = cases
            .iter()
            .map(|(tags, _)| format!("read\t0\tchr1\t2\t60\t8M\t*\t0\t0\tACGCGCGT\t*\t{tags}\n"))
            .collect();
        let records = bam_records_from_sam(&sam);
        assert_eq!(records.len(), cases.len());
        for (record, (tags, probability)) in records.iter().zip(cases) {
            assert_eq!(
                decode_5mc_from_record(record),
                probability.map(|prob| BaseMod5mC {
                    implicit_unmodified: false,
                    calls: vec![(1, prob)]
                }),
                "{tags}",
            );
        }
    }

    #[test]
    fn missing_or_stale_mod_tags_or_missing_seq_have_no_calls() {
        let records = bam_records_from_sam(concat!(
            "read\t0\tchr1\t2\t60\t8M\t*\t0\t0\tACGCGCGT\t*\n",
            "read\t0\tchr1\t2\t60\t8M\t*\t0\t0\tACGCGCGT\t*\tMM:Z:C+m?,0;\tML:B:C,147\tMN:i:9\n",
            "read\t0\tchr1\t2\t60\t8M\t*\t0\t0\t*\t*\tMM:Z:C+m.;\n",
        ));
        assert_eq!(records.len(), 3);
        for record in &records {
            assert_eq!(decode_5mc_from_record(record), None);
        }
    }

    #[test]
    fn cpg_cytosine_uses_the_strand_specific_query_offset() {
        let packed = PackedSeq::from_ascii(b"GcGcNcgC");
        let seq = packed.as_view();
        let offsets = |reverse| {
            (0..=seq.len())
                .filter(|&index| seq.is_cpg_cytosine(index, reverse))
                .collect::<Vec<_>>()
        };
        assert_eq!(offsets(false), [1, 5]);
        assert_eq!(offsets(true), [2, 6]);
    }

    #[test]
    fn raw_aux_scan_skips_all_field_types_and_rejects_truncation() {
        let mut aux = b"XAA!Xcc\xffXCC\x80Xss\x00\x01XSS\x01\x00Xii\x01\x00\x00\x00\
                        XII\x00\x01\x00\x00Xff\x00\x00\x00\x00XZZtext\0XHHCAFE\0"
            .to_vec();
        for (subtype, width) in [
            (b'c', 1),
            (b'C', 1),
            (b's', 2),
            (b'S', 2),
            (b'i', 4),
            (b'I', 4),
            (b'f', 4),
        ] {
            aux.extend_from_slice(&[b'X', subtype, b'B', subtype, 2, 0, 0, 0]);
            aux.extend(std::iter::repeat_n(0, 2 * width));
        }
        aux.extend_from_slice(b"MLBC\x01\x00\x00\x00\x93MMZC+m?,0;\0");
        let expected = Some((&b"C+m?,0;"[..], &[147][..]));
        assert_eq!(modification_tags(&aux, 8), expected);
        for (ty, value) in [
            (b'c', &b"\x08"[..]),
            (b'C', &b"\x08"[..]),
            (b's', &b"\x08\x00"[..]),
            (b'S', &b"\x08\x00"[..]),
            (b'i', &b"\x08\x00\x00\x00"[..]),
            (b'I', &b"\x08\x00\x00\x00"[..]),
        ] {
            let mut data = aux.clone();
            data.extend_from_slice(&[b'M', b'N', ty]);
            data.extend_from_slice(value);
            assert_eq!(modification_tags(&data, 8), expected);
        }
        for suffix in [
            &b"X"[..],
            b"Xxq",
            b"Xxf\0\0\0",
            b"XxZunterminated",
            b"XxBA\0\0\0\0",
            b"XxBC\x01\0\0",
            b"XxBI\xff\xff\xff\xff",
            b"MLBC\x02\0\0\0\xff",
            b"MNc\xf8",
            b"MNZ8\0",
        ] {
            let mut data = aux.clone();
            data.extend_from_slice(suffix);
            assert_eq!(modification_tags(&data, 8), None, "{suffix:?}");
        }
    }

    #[test]
    fn streaming_decoder_matches_noodles_position_oracle() {
        let ascii = b"ACGTACGNCTGAC";
        let packed = PackedSeq::from_ascii(ascii);
        let seq = packed.as_view();
        let sequence = Sequence::from(ascii);
        // Noodles 0.86 matches literal Ns, so use a same-length wildcard sequence for N groups.
        let any_base = Sequence::from(vec![b'N'; ascii.len()]);
        for reverse in [false, true] {
            for (mm, n_probs) in [
                (&b"C+hm?,0,1;A+a.,1;C+h,0;N+n,2,9;C+27551.,0,0;C-m,0;"[..], 11),
                (b"C+m,0;T-g,1;G+o,2;C+027551,1;C+m?;", 4),
                (b"C+m.;", 0),
                (b"C+m?,0;A+a,13;", 2),
                (b"C+m?,0;N+n,13;", 2),
                (b"C+m?,0;N+n,0", 2),
                (b"C+m?,0;A+a,;", 1),
                (b"C+m?,0;C+4294967296,0;", 2),
                (b"C+m?,0;C-e,99999999999999999999;", 2),
                (b"C+m?,0;C+hm,0;", 2),
                (b"C+m?,0;A+a,0;", 3),
            ] {
                let ml: Vec<_> = (10..).step_by(17).take(n_probs).collect();
                let expected = noodles_5mc(mm, &ml, reverse, &sequence, &any_base);
                assert_eq!(decode_5mc(mm, &ml, seq, reverse), expected, "{mm:?}, reverse={reverse}");
            }
        }
        // SAM ambiguity codes are valid even though the noodles group parser rejects them.
        assert_eq!(
            decode_5mc(b"C+C,0;C+m,1;", &[17, 212], seq, false),
            Some(BaseMod5mC {
                implicit_unmodified: true,
                calls: vec![(5, 212)],
            })
        );
        for reverse in [false, true] {
            let results: Vec<_> = (0..4)
                .map(|delta| {
                    let as_t = format!("C+m,0;T+e,{delta};");
                    let as_u = as_t.replace('T', "U");
                    let decoded = decode_5mc(as_u.as_bytes(), &[17, 212], seq, reverse);
                    assert_eq!(decoded, decode_5mc(as_t.as_bytes(), &[17, 212], seq, reverse));
                    decoded
                })
                .collect();
            // Deltas past the last matching base must reject the read, or the comparison is blind.
            assert!(results.iter().any(Option::is_some) && results.iter().any(Option::is_none));
        }
    }

    fn noodles_5mc(
        mm: &[u8],
        mut ml: &[u8],
        reverse: bool,
        sequence: &Sequence,
        any_base: &Sequence,
    ) -> Option<BaseMod5mC> {
        let mut target = None;
        for encoded in mm.split_inclusive(|&b| b == b';') {
            let group_seq = if encoded.starts_with(b"N") { any_base } else { sequence };
            let groups = BaseModifications::parse(encoded, reverse, group_seq).ok()?;
            let group = groups.as_ref().first()?;
            let stride = group.modifications().len();
            let probs = ml.split_off(..group.positions().len() * stride)?;
            if group.unmodified_base() == UnmodifiedBase::C
                && group.strand() == Strand::Forward
                && let Some(index) = group
                    .modifications()
                    .iter()
                    .position(|&m| matches!(m, FIVE_METHYLCYTOSINE | Modification::ChebiId(27551)))
            {
                let mods = target.get_or_insert(BaseMod5mC {
                    implicit_unmodified: true,
                    calls: Vec::new(),
                });
                mods.implicit_unmodified &= group.status() != Some(Status::Explicit);
                mods.calls.extend(
                    group
                        .positions()
                        .iter()
                        .zip(probs.chunks_exact(stride))
                        .map(|(&pos, probs)| (pos as u32, probs[index])),
                );
            }
        }
        if !ml.is_empty() {
            return None;
        }
        let mut mods = target?;
        mods.calls.sort_by_key(|&(pos, _)| pos);
        Some(mods)
    }
}
