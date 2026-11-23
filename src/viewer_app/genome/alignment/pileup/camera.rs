//! Converts genomic read spans into bounded pileup columns.
//!
//! These helpers are the pileup camera's geometry seam: they translate 0-based [`BasePair`] spans
//! using the active horizontal scale and clamp the result through shared genome geometry.

use crate::basepair::BasePair;
use crate::viewer_app::genome::geometry::clamp_column;

/// Returns the visible screen columns for a read.
pub(super) fn visible_columns(
    read: &super::super::read::Read,
    offset: BasePair,
    width: u16,
    bp_per_column: u64,
) -> std::ops::Range<u16> {
    let to_col = |bp: BasePair| clamp_column((bp / bp_per_column).as_u64(), 0, width);
    let x_start = read.start.checked_sub(offset).map_or(0, to_col);
    let x_end = (read.start + read.length).checked_sub(offset).map_or(0, to_col);
    x_start..x_end.max(x_start)
}

#[cfg(test)]
mod tests {
    use crate::basepair::BasePair;

    use super::*;

    #[test]
    fn visible_columns_shifts_read_by_offset_and_clamps_to_width() {
        let read = super::super::super::read::Read {
            start: BasePair(10),
            length: BasePair::length(20),
            id: super::super::super::read::ReadId::from_name(b"test"),
            cigar: crate::cigar::Cigar::default(),
            seq: None,
            strand: super::super::super::read::Strand::Forward,
            mod_5mc: None,
        };
        let cols = visible_columns(&read, BasePair(0), 100, 1);
        assert_eq!(cols, 10..30);
        let cols = visible_columns(&read, BasePair(5), 100, 1);
        assert_eq!(cols, 5..25);
        let cols = visible_columns(&read, BasePair(0), 15, 1);
        assert_eq!(cols, 10..15);
        let cols = visible_columns(&read, BasePair(0), 5, 1);
        assert_eq!(cols, 5..5);
    }

    #[test]
    fn visible_columns_at_two_bp_per_column() {
        let read = super::super::super::read::Read {
            start: BasePair(0),
            length: BasePair::length(20),
            id: super::super::super::read::ReadId::from_name(b"test"),
            cigar: crate::cigar::Cigar::default(),
            seq: None,
            strand: super::super::super::read::Strand::Forward,
            mod_5mc: None,
        };
        let cols = visible_columns(&read, BasePair(0), 100, 2);
        assert_eq!(cols.start, 0);
        assert_eq!(cols.end, 10);
    }
}
