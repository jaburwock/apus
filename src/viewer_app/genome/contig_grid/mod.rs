//! Contig grid track: every contig in the loaded reference, with relative-size bars.
//!
//! The widest view the app has — it names the whole genome rather than any span of it, which is
//! why it sits at the top of the track stack and carries no divider of its own.
//!

pub(super) mod chrom;
mod layout;
mod sizebar;

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::Span,
};

use crate::basepair::BasePair;
use crate::refgenome::IndexedFasta;
use crate::style::{Chrome, Reference, Surface, Text};
use crate::viewer_app::painter::GlyphSet;

use super::track::TrackView;
use layout::{CONTIG_CELL_WIDTH, contigs_per_row, human_genome_layout, is_human_genome};
use sizebar::{dot_fill_level, draw_bar};

/// Fixed contig-grid height: the name row, two braille size-bar rows, and the active BAM name.
pub const CONTIG_GRID_TRACK_HEIGHT: u16 = 4;

const BAM_NAME_ROW: u16 = 3;

/// Gap (in columns) between the ref genome name label and the grid.
const LABEL_GAP: u16 = 1;

/// Max characters of the ref genome's title shown in the top-left label before truncation kicks in.
const LABEL_NAME_WIDTH: u16 = 20;

/// Unicode ellipsis used when the genome name is truncated.
const LABEL_ELLIPSIS: &str = "…";

/// Draws every contig in the loaded reference as a named cell over a relative-size bar.
pub struct ContigGridTrack {
    genome: IndexedFasta,
    /// Last-drawn contig cells used by supplemental mouse hit testing.
    last_cell_rects: Vec<(String, Rect)>,
}

impl ContigGridTrack {
    pub fn new(genome: IndexedFasta) -> Self {
        Self {
            genome,
            last_cell_rects: Vec::new(),
        }
    }

    /// Replaces the reference genome and clears hit-test rectangles.
    pub fn update_reference(&mut self, genome: IndexedFasta) {
        *self = Self::new(genome);
    }

    /// Returns the longest contig for size-bar scaling.
    fn max_contig_length(&self) -> Option<BasePair> {
        self.genome
            .index
            .contig_names()
            .filter_map(|n| self.genome.index.length(&n))
            .max()
    }
}

/// Display names for references whose filename reads poorly in a screenshot.
const TITLE_ALIASES: &[(&str, &str)] = &[("hs1.fa", "T2T CHM13")];

/// Returns the screenshot title for a reference filename.
fn contig_title(filename: &str) -> String {
    let name = TITLE_ALIASES
        .iter()
        .find_map(|&(file, alias)| (file == filename).then_some(alias))
        .unwrap_or(filename);
    format!("{name} Contigs")
}

/// Draws a bold file label, truncating to the available terminal columns.
fn draw_contig_label(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, name: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let style = Style::default()
        .bg(Surface::TRACK_BG())
        .fg(Text::PRIMARY_FG())
        .add_modifier(Modifier::BOLD);
    let end = if Span::raw(name).width() > usize::from(width) {
        let (end, _) = buf.set_stringn(x, y, name, usize::from(width - 1), style);
        buf.set_stringn(end, y, LABEL_ELLIPSIS, 1, style).0
    } else {
        buf.set_stringn(x, y, name, usize::from(width), style).0
    };
    end - x
}

fn draw_cell(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    text: &str,
    bg: ratatui::style::Color,
    fg: ratatui::style::Color,
) {
    let mut chars = text.chars();
    for col in 0..CONTIG_CELL_WIDTH {
        if let Some(cell) = buf.cell_mut((x + col, y)) {
            cell.set_char(chars.next().unwrap_or(' '));
            cell.set_style(Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD));
        }
    }
}

impl ContigGridTrack {
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, view: TrackView<'_>, bam_name: Option<&str>) {
        let TrackView { settings, contig, .. } = view;
        let bg = Surface::TRACK_BG();
        let buf = frame.buffer_mut();
        buf.set_style(area, Style::default().bg(bg));

        self.last_cell_rects.clear();

        // The layout collapses this pane when the setting is off, so this guard is normally
        // redundant — but a track that paints regardless of its own setting is a trap the moment
        // either side changes.
        if !settings.contigmarkers || area.height == 0 {
            return;
        }
        let glyphs = GlyphSet::from_octants(settings.octants);

        // Fixed rows are skipped independently; a short pane loses bar rows from the bottom
        // rather than shifting the name row (invariant 6).
        let has_braille_row1 = area.height > 1;
        let has_braille_row2 = area.height > 2;

        let contig_row_y = area.y;
        let braille_row1_y = area.y + 1;
        let braille_row2_y = area.y + 2;

        let title = contig_title(&self.genome.name);
        let label_width = draw_contig_label(buf, area.x, contig_row_y, &title, LABEL_NAME_WIDTH.min(area.width));
        if area.height > BAM_NAME_ROW
            && let Some(name) = bam_name
        {
            draw_contig_label(buf, area.x, area.y + BAM_NAME_ROW, name, area.width);
        }

        let grid_x = area.x + label_width + LABEL_GAP;
        let names: Vec<String> = self.genome.index.contig_names().collect();

        let grid_width = (area.x + area.width).saturating_sub(grid_x);
        let per_row = contigs_per_row(grid_width);

        let cells: Vec<(u16, u16, usize)> = if is_human_genome(&names) {
            human_genome_layout(&names, per_row)
        } else {
            (0..names.len())
                .map(|i| ((i / per_row) as u16, (i % per_row) as u16, i))
                .collect()
        };

        let longest = self.max_contig_length().unwrap_or(BasePair(0));

        let rows_y: Vec<u16> = if has_braille_row2 {
            vec![braille_row1_y, braille_row2_y]
        } else {
            vec![braille_row1_y]
        };
        let max_levels = 4 * rows_y.len() as u8;

        // Only layout row 0 is drawn. Overflow takes the row's last column, never the last cell
        // in layout order: the human layout starts unmatched contigs on a fresh row, so that cell
        // can sit at column 0, chr1's slot.
        let overflow_col = cells.iter().any(|&(row, ..)| row > 0).then(|| per_row as u16 - 1);
        if let Some(col) = overflow_col {
            draw_cell(
                buf,
                grid_x + col * CONTIG_CELL_WIDTH,
                contig_row_y,
                "*",
                bg,
                Text::DIMMED_FG(),
            );
        }
        for &(row, col, name_index) in &cells {
            if row > 0 || Some(col) == overflow_col {
                continue;
            }
            let x = grid_x + col * CONTIG_CELL_WIDTH;

            let is_highlighted = names[name_index] == contig.name;
            let name = names[name_index].strip_prefix("chr").unwrap_or(&names[name_index]);
            let fg = if is_highlighted {
                Text::PRIMARY_FG()
            } else {
                Text::DIMMED_FG()
            };
            let hue = Reference::contig_bg(col as usize, is_highlighted);
            draw_cell(buf, x, contig_row_y, name, hue, fg);

            let mut cell_height = 1u16;
            if has_braille_row1 {
                let length = self.genome.index.length(&names[name_index]).unwrap_or(BasePair(0));
                let fill_level = dot_fill_level(length, longest, max_levels);
                draw_bar(
                    buf,
                    x,
                    &rows_y,
                    fill_level,
                    bg,
                    Chrome::bar_color(col as usize, is_highlighted),
                    glyphs,
                );
                cell_height += rows_y.len() as u16;
            }

            self.last_cell_rects.push((
                names[name_index].clone(),
                Rect::new(x, contig_row_y, CONTIG_CELL_WIDTH, cell_height),
            ));
        }
    }

    pub(super) fn hit_test(&self, x: u16, y: u16) -> Option<String> {
        self.last_cell_rects
            .iter()
            .find(|(_, rect)| rect.contains(Position { x, y }))
            .map(|(name, _)| name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer_app::genome::settings::GenomeViewerSettings;
    use crate::viewer_app::genome::track::Contig;
    use crate::viewer_app::viewport::ViewPosition;
    use ratatui::{Terminal, backend::TestBackend};

    /// Rasterises the grid over `active`, returning the frame and the drawn cell rects.
    fn drawn(
        genome: IndexedFasta,
        area: Rect,
        active: &str,
        settings: &GenomeViewerSettings,
    ) -> (ratatui::buffer::Buffer, ContigGridTrack) {
        let mut track = ContigGridTrack::new(genome);
        let contig = Contig {
            name: active.to_string(),
            length: BasePair(248_387_328),
        };
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height + area.y)).expect("test terminal");
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position: ViewPosition::new(BasePair(0), BasePair(100)),
                        settings,
                        contig: &contig,
                    },
                    None,
                )
            })
            .expect("contig grid draw");
        (terminal.backend().buffer().clone(), track)
    }

    /// A longer contig fills more of its braille bar than a shorter one. The expected ordering
    /// comes from the index's own lengths, not from `dot_fill_level`.
    #[test]
    fn bar_fill_follows_contig_length() {
        // Stated rather than inherited from the default: this test counts bits above U+2800, which
        // is arithmetic only braille glyphs answer to. Its octant sibling opts in the same way.
        let mut settings = GenomeViewerSettings::new();
        settings.octants = false;
        let area = Rect::new(0, 0, 120, CONTIG_GRID_TRACK_HEIGHT);
        let (buffer, track) = drawn(IndexedFasta::hs1_fallback(), area, "chr1", &settings);

        let filled_dots = |name: &str| {
            let rect = track.last_cell_rects.iter().find(|(n, _)| n == name).expect("cell").1;
            // Braille codepoints encode filled dots as set bits above U+2800.
            (rect.y + 1..rect.bottom())
                .flat_map(|y| (rect.x..rect.x + CONTIG_CELL_WIDTH).map(move |x| (x, y)))
                .map(|(x, y)| {
                    let ch = buffer
                        .cell((x, y))
                        .expect("cell")
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ');
                    (ch as u32).saturating_sub(0x2800).count_ones()
                })
                .sum::<u32>()
        };

        let genome = IndexedFasta::hs1_fallback();
        let longest = genome.index.length("chr1").expect("chr1 length");
        let shortest = genome.index.length("chr21").expect("chr21 length");
        assert!(shortest < longest, "fixture must have chr21 shorter than chr1");
        assert!(
            filled_dots("chr21") < filled_dots("chr1"),
            "the shorter contig must fill fewer braille dots"
        );
    }

    /// The same length ordering must survive the glyph swap. Filled cells are recovered by looking
    /// each drawn glyph back up in ratatui's octant table, so the oracle is the table itself rather
    /// than a second copy of the mask arithmetic under test.
    #[test]
    fn octant_bar_fill_follows_contig_length() {
        let mut settings = GenomeViewerSettings::new();
        settings.octants = true;
        let area = Rect::new(0, 0, 120, CONTIG_GRID_TRACK_HEIGHT);
        let (buffer, track) = drawn(IndexedFasta::hs1_fallback(), area, "chr1", &settings);

        let filled_cells = |name: &str| {
            let rect = track.last_cell_rects.iter().find(|(n, _)| n == name).expect("cell").1;
            (rect.y + 1..rect.bottom())
                .flat_map(|y| (rect.x..rect.x + CONTIG_CELL_WIDTH).map(move |x| (x, y)))
                .map(|(x, y)| {
                    let ch = buffer
                        .cell((x, y))
                        .expect("cell")
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ');
                    let mask = ratatui::symbols::pixel::OCTANTS
                        .iter()
                        .position(|&o| o == ch)
                        .unwrap_or_else(|| panic!("{ch:?} at {x},{y} is not an octant glyph"));
                    (mask as u8).count_ones()
                })
                .sum::<u32>()
        };

        let genome = IndexedFasta::hs1_fallback();
        let longest = genome.index.length("chr1").expect("chr1 length");
        let shortest = genome.index.length("chr21").expect("chr21 length");
        assert!(shortest < longest, "fixture must have chr21 shorter than chr1");
        assert!(
            filled_cells("chr21") < filled_cells("chr1"),
            "the shorter contig must fill fewer octant cells"
        );
    }

    /// hg38 lays out 25 chromosomes and then starts its unplaced contigs on a fresh row, which put
    /// the overflow `*` over chr1's name. Every drawn cell must name its own contig.
    #[test]
    fn overflow_marker_never_covers_a_drawn_contig_name() {
        let fai: String = (1..=22)
            .map(|n| n.to_string())
            .chain(["X", "Y", "M"].map(str::to_string))
            .map(|c| format!("chr{c}"))
            .chain((0..5).map(|n| format!("chrUn_{n}")))
            .map(|name| format!("{name}\t1000\t0\t50\t51\n"))
            .collect();
        let genome = IndexedFasta::new("hg38.fa".to_string(), &fai).expect("fai parses");
        let area = Rect::new(0, 0, 120, CONTIG_GRID_TRACK_HEIGHT);
        let (buffer, track) = drawn(genome, area, "chr1", &GenomeViewerSettings::new());

        let text = |xs: std::ops::Range<u16>| -> String {
            xs.map(|x| buffer.cell((x, area.y)).expect("cell").symbol()).collect()
        };
        let row = text(area.x..area.right());
        assert!(row.contains('*'), "fixture must overflow the row: {row:?}");
        assert!(track.last_cell_rects.iter().any(|(name, _)| name == "chr1"));
        for (name, rect) in &track.last_cell_rects {
            assert_eq!(
                text(rect.x..rect.x + CONTIG_CELL_WIDTH).trim_end(),
                name.strip_prefix("chr").unwrap_or(name)
            );
        }
    }

    /// The pane collapses when the setting is off, but the guard inside `draw` is what keeps a
    /// stale non-zero pane from painting a track the user turned off.
    #[test]
    fn a_disabled_track_leaves_a_stale_nonempty_pane_untouched() {
        let mut settings = GenomeViewerSettings::new();
        settings.contigmarkers = false;
        let area = Rect::new(0, 0, 120, CONTIG_GRID_TRACK_HEIGHT);
        let mut track = ContigGridTrack::new(IndexedFasta::hs1_fallback());
        let contig = Contig {
            name: "chr1".to_string(),
            length: BasePair(248_387_328),
        };
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position: ViewPosition::new(BasePair(0), BasePair(100)),
                        settings: &settings,
                        contig: &contig,
                    },
                    None,
                );
            })
            .expect("contig grid draw");
        assert!(track.last_cell_rects.is_empty());
        assert_eq!(track.hit_test(0, 0), None);
    }

    fn track_with_rects(rects: Vec<(String, Rect)>) -> ContigGridTrack {
        let mut track = ContigGridTrack::new(IndexedFasta::hs1_fallback());
        track.last_cell_rects = rects;
        track
    }

    #[test]
    fn hit_test_finds_contig_inside_its_cell_rect() {
        let track = track_with_rects(vec![
            ("chr1".to_string(), Rect::new(5, 0, 3, 3)),
            ("chr2".to_string(), Rect::new(8, 0, 3, 3)),
        ]);
        assert_eq!(track.hit_test(6, 1), Some("chr1".to_string()));
        assert_eq!(track.hit_test(9, 2), Some("chr2".to_string()));
    }

    #[test]
    fn hit_test_misses_outside_any_cell_rect() {
        let track = track_with_rects(vec![("chr1".to_string(), Rect::new(5, 0, 3, 3))]);
        assert_eq!(track.hit_test(4, 0), None);
        assert_eq!(track.hit_test(8, 0), None);
        assert_eq!(track.hit_test(5, 3), None);
    }

    #[test]
    fn hit_test_returns_none_with_no_drawn_cells() {
        let track = track_with_rects(vec![]);
        assert_eq!(track.hit_test(0, 0), None);
    }
}
