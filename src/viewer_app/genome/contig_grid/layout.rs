//! Pure placement logic for the contig row, separate from painting.
//!
//! This module converts available grid width into a bounded column count and, for a human-style
//! reference, assigns row-major slots from natural chromosome keys. It returns indices into the
//! caller's name slice rather than owning contig data; the track decides which layout path to use
//! and how the resulting cells are drawn or truncated.

use super::chrom::{ChromKey, chrom_key};

/// Width of one contig cell.
pub(super) const CONTIG_CELL_WIDTH: u16 = 3;

/// Maximum contigs in one row.
const MAX_CONTIGS_PER_ROW: usize = 26;

/// Returns the number of contig columns that fit.
pub(super) fn contigs_per_row(grid_width: u16) -> usize {
    let fits = (grid_width / CONTIG_CELL_WIDTH) as usize;
    fits.clamp(1, MAX_CONTIGS_PER_ROW)
}

/// Whether the names include chromosomes 1 through 22.
pub(super) fn is_human_genome(names: &[String]) -> bool {
    (1..=22).all(|n| names.iter().any(|name| chrom_key(name) == Some(ChromKey::Numbered(n))))
}

/// Builds row-major layout entries, with unmatched contigs after the
/// recognized chromosomes.
pub(super) fn human_genome_layout(names: &[String], per_row: usize) -> Vec<(u16, u16, usize)> {
    let per_row = per_row.max(1);
    let mut matched: Vec<(ChromKey, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, name)| chrom_key(name).map(|k| (k, i)))
        .collect();
    matched.sort_by(|a, b| a.0.cmp(&b.0));

    let mut layout = Vec::with_capacity(names.len());
    let mut next_slot = 0usize; // flat index, consumed by everything but M
    for (key, idx) in &matched {
        if *key == ChromKey::M {
            continue;
        }
        let row = (next_slot / per_row) as u16;
        let col = (next_slot % per_row) as u16;
        layout.push((row, col, *idx));
        next_slot += 1;
    }
    if let Some((_, m_idx)) = matched.iter().find(|(k, _)| *k == ChromKey::M) {
        let row = (next_slot / per_row) as u16;
        let col = (next_slot % per_row) as u16;
        layout.push((row, col, *m_idx));
        next_slot += 1;
    }

    let unmatched = names.iter().enumerate().filter(|(_, name)| chrom_key(name).is_none());
    let extra_start_row = (next_slot.div_ceil(per_row)) as u16;
    for (i, (idx, _)) in unmatched.enumerate() {
        let slot = extra_start_row as usize * per_row + i;
        let row = (slot / per_row) as u16;
        let col = (slot % per_row) as u16;
        layout.push((row, col, idx));
    }

    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_contig_detection_and_layout_follow_chromosome_order() {
        let complete: Vec<String> = (1..=22).map(|n| format!("CHR{n}")).collect();
        let prefixless: Vec<String> = (1..=22).map(|n| n.to_string()).collect();
        assert!(is_human_genome(&complete));
        assert!(is_human_genome(&prefixless));
        assert!(!is_human_genome(&complete[..21]));
        assert!(!is_human_genome(&["scaffold_1".to_string()]));

        let names = ["chr10", "X", "chr2", "M", "chr1", "Y", "scaffold_17"].map(str::to_string);
        let expected_indices = [4, 2, 0, 1, 5, 3, 6];
        let expected = expected_indices
            .into_iter()
            .enumerate()
            .map(|(slot, index)| ((slot / 3) as u16, (slot % 3) as u16, index))
            .collect::<Vec<_>>();
        assert_eq!(human_genome_layout(&names, 3), expected);

        let one_row = human_genome_layout(&names[..6], 6);
        assert_eq!(one_row.last(), Some(&(0, 5, 3)));
    }

    #[test]
    fn contigs_per_row_clamps_to_bounds() {
        assert_eq!(contigs_per_row(0), 1);
        assert_eq!(contigs_per_row(CONTIG_CELL_WIDTH - 1), 1);
        assert_eq!(contigs_per_row(CONTIG_CELL_WIDTH * 10), 10);
        assert_eq!(
            contigs_per_row(CONTIG_CELL_WIDTH * (MAX_CONTIGS_PER_ROW as u16 + 5)),
            MAX_CONTIGS_PER_ROW
        );
    }
}
