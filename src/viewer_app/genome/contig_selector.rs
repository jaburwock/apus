//! State and rendering for `GenomeViewer`'s modal contig selector.
//!
//! The selector snapshots contig names and lengths from the loaded reference index, orders them
//! with the shared natural chromosome key, and keeps only the cursor and display geometry.
//! `GenomeViewer` owns the overlay lifetime and applies the selected name through its normal
//! contig-switch path; this module never owns the active contig or its per-contig viewport.
//!
//! The popover is drawn over the completed genome frame, with the active row marked in bold, the
//! cursor highlighted, and a relative length bar.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::refgenome::RefIndex;
use crate::{
    basepair::BasePair,
    list_cursor::ListCursor,
    popover::{ListTitle, Popover},
    style::Chrome,
};

use super::contig_grid::chrom::{ChromKey, chrom_key};

/// Four-dot filled bar glyph.
const BRAILLE_FILLED: char = '\u{2836}';

/// Contig rows and cursor state for the selector.
pub struct ContigSelectorState {
    contigs: Vec<(String, BasePair)>,
    longest: BasePair,
    cursor: ListCursor,
}

impl ContigSelectorState {
    /// Builds a naturally ordered list, selecting `active_contig` when present.
    pub fn new(ref_index: &RefIndex, active_contig: &str) -> Self {
        let mut contigs: Vec<(String, BasePair)> = ref_index
            .contig_names()
            .map(|name| {
                let length = ref_index.length(&name).unwrap_or(BasePair(0));
                (name, BasePair::length(length.as_u64()))
            })
            .collect();
        contigs.sort_by(|(a, _), (b, _)| sort_key(a).cmp(&sort_key(b)));
        let longest = contigs.iter().map(|(_, len)| *len).max().unwrap_or(BasePair::length(0));
        let selected = contigs.iter().position(|(name, _)| name == active_contig).unwrap_or(0);
        Self {
            contigs,
            longest,
            cursor: ListCursor::at(selected),
        }
    }

    pub fn up(&mut self) {
        self.cursor.up();
    }

    pub fn down(&mut self) {
        self.cursor.down(self.contigs.len());
    }

    /// Returns the contig under the cursor.
    pub fn selected_contig(&self) -> &str {
        &self.contigs[self.cursor.selected()].0
    }

    /// Draws the centered selector, highlighting the active and selected rows.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, active_contig: &str) -> Option<Rect> {
        let popover = Popover::centred(area, 90, 90)?;
        let name_width = self
            .contigs
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        let length_width = self
            .contigs
            .iter()
            .map(|(_, len)| len.format_with_commas().chars().count() + 2)
            .max()
            .unwrap_or(0);
        let bar_width = usize::from(popover.inner.width).saturating_sub(name_width + length_width + 2);

        popover.draw_list(
            frame,
            &mut self.cursor,
            self.contigs.len(),
            ListTitle::Plain("Select Contig"),
            "",
            |index, _| {
                let (name, length) = &self.contigs[index];
                let is_active = name == active_contig;
                let row_style = if is_active {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let marker = if is_active { "▸ " } else { "  " };
                let label = format!(
                    "{marker}{name:<name_width$} {length:>length_width$}bp ",
                    length = length.format_with_commas(),
                );
                let bar = render_bar(*length, self.longest, bar_width);
                let bar_color = Chrome::bar_color(index, true);

                Line::from(vec![
                    Span::raw(label),
                    Span::styled(bar, Style::default().fg(bar_color)),
                ])
                .patch_style(row_style)
            },
        );
        Some(popover.bounds())
    }
}

/// Sort key for natural chromosome order.
fn sort_key(name: &str) -> (bool, Option<ChromKey>, &str) {
    match chrom_key(name) {
        Some(key) => (false, Some(key), name),
        None => (true, None, name),
    }
}

/// Renders a proportional bar of `width` columns.
fn render_bar(length: BasePair, longest: BasePair, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = fill_columns(length, longest, width);
    BRAILLE_FILLED.to_string().repeat(filled) + &" ".repeat(width - filled)
}

/// Returns the proportional fill count, with a minimum of one when non-empty.
fn fill_columns(length: BasePair, longest: BasePair, width: usize) -> usize {
    if longest.as_u64() == 0 || width == 0 {
        return 0;
    }
    let ratio = length.as_f64() / longest.as_f64();
    let level = (ratio * width as f64).round() as usize;
    level.clamp(1, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_index() -> RefIndex {
        crate::refgenome::IndexedFasta::hs1_fallback().index
    }

    #[test]
    fn sorts_in_natural_chromosome_order() {
        let index = RefIndex::from_fai_str(
            "chr10\t110\t0\t50\t51\nchrM\t660\t1\t50\t51\nchr2\t220\t2\t50\t51\nchrY\t550\t3\t50\t51\nchrX\t440\t4\t50\t51\nchr1\t100\t5\t50\t51\n",
        )
        .expect("shuffled contigs parse");
        let state = ContigSelectorState::new(&index, "chr1");
        let names: Vec<&str> = state.contigs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["chr1", "chr2", "chr10", "chrX", "chrY", "chrM"]);
    }

    #[test]
    fn cursor_preplaced_on_active_contig() {
        let state = ContigSelectorState::new(&ref_index(), "chr2");
        assert_eq!(state.selected_contig(), "chr2");
    }

    #[test]
    fn cursor_falls_back_to_top_for_unknown_active_contig() {
        let state = ContigSelectorState::new(&ref_index(), "does-not-exist");
        assert_eq!(state.cursor.selected(), 0);
    }
}
