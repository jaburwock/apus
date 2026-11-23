//! Immediate-mode reference track orchestration.
//!
//! Sequence data and cache state live in the genome-level `ReferenceData` service shared with
//! other renderers.
//!
//! Each draw receives the shell-owned view position and paints the reference pane directly. Rows use
//! fixed absolute offsets and are independently omitted when the pane is too short; they are never
//! compacted around one another. The shared centre marker is painted first, allowing sequence,
//! labels, placeholder text, and the legend to overwrite their own cells.
//!
//! Horizontal zoom changes the sequence representation: close zoom draws one base per column, while
//! mid zoom packs bases into half-block cells and adds a centred base label. The sequence helpers
//! receive the position's underflow so contig-start padding remains aligned with the shared centre
//! geometry.

pub(in crate::viewer_app::genome) mod sequence_row;

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use crate::residue::BaseResidue;
use crate::setting::ReadColor;
use crate::style::{Indel, Reference, Surface, Text};
use crate::viewer_app::painter;

use super::alignment::mod_5mc_color;
use super::geometry::{cent_column, in_bounds_column_range};
use super::mod_5mc::Call;
use super::reference_data::{ReferenceData, ReferenceLookup};
use super::track::{TrackView, paint_track_background};
use super::zoom::HorizontalZoom;
use sequence_row::{
    draw_centline_label, draw_centline_markers, draw_centred_sequence_letter, draw_placeholder_label,
    draw_sequence_row_close, draw_sequence_row_mid,
};

/// Draws the reference sequence at the current position.
pub struct ReferenceTrack;

/// Row offsets within the track, each drawn only if the pane reaches it and never shifted to make
/// room for a sibling (invariant 6). At `Mid`, the centred letter sits above the sequence and the
/// centre-coordinate label sits below it. The depth label shares the legend's row, under the
/// centre-coordinate label; the legend stops short of it.
const SEQUENCE_ROW: u16 = 1;
const MARKER_ROW: u16 = 0;
const CENTRED_LETTER_ROW: u16 = 0;
const CENTRE_LABEL_ROW: u16 = 2;
const CONTROLS_ROW: u16 = 3;

/// Symbols shown in the base color legend, in display order — the four
/// nucleotides, `N` (unknown-but-real), and `Null` (no sequence data here).
const LEGEND_SYMBOLS: [BaseResidue; 6] = [
    BaseResidue::A,
    BaseResidue::T,
    BaseResidue::C,
    BaseResidue::G,
    BaseResidue::N,
    BaseResidue::Null,
];

/// A legend swatch drawn in `▌` cells, which paint their left half in fg and right half in bg.
enum Swatch {
    /// One cell holding a plain and a tinted half.
    Pair(Color, Color),
    /// Three light-to-dark samples over two cells; the last half keeps the background underneath.
    Scale([Color; 3]),
}

impl Swatch {
    fn width(&self) -> u16 {
        match self {
            Swatch::Pair(..) => 1,
            Swatch::Scale(_) => 2,
        }
    }
}

/// The legend's entries, left to right: the pileup's 5mC call colours when reads are coloured by
/// them, its indel colours, then the bases. Only the 5mC entry comes and goes, so it sits leftmost
/// and the rest keep their columns.
fn legend_entries(read_color: ReadColor) -> Vec<(Swatch, String)> {
    let five_mc = (read_color == ReadColor::FiveMc).then(|| {
        (
            Swatch::Scale([Call::Hypo, Call::Uncertain, Call::Hyper].map(mod_5mc_color)),
            "5mC".to_string(),
        )
    });
    let indels = [(Indel::INSERTION(), 'I'), (Indel::DELETION(), 'D')]
        .map(|(color, label)| (Swatch::Pair(color, color), label.to_string()));
    let bases = LEGEND_SYMBOLS.map(|base| {
        let pair = Reference::base_color_pair(base);
        (
            Swatch::Pair(pair.pick(false), pair.pick(true)),
            base.as_char().to_string(),
        )
    });
    five_mc.into_iter().chain(indels).chain(bases).collect()
}

/// Draws the legend over the right edge of the given row, on the popover ground so it reads as the
/// same kind of overlay as the popover legends. Each entry is its swatch, its label and one column
/// of padding, and one more column leads the backing. Entries are placed from the right and stop at
/// the first that no longer fits, so a narrow pane drops the leftmost rather than drawing past its
/// edge or into the columns left of `floor`.
fn draw_legend(buf: &mut Buffer, area: Rect, row_y: u16, read_color: ReadColor, floor: u16) {
    let backing = Style::default().fg(Text::PRIMARY_FG()).bg(Surface::POPOVER_BG());
    let fill = |buf: &mut Buffer, columns: std::ops::Range<u16>| {
        for x in columns {
            buf[(x, row_y)].set_char(' ').set_style(backing);
        }
    };
    let label_style = backing.add_modifier(Modifier::BOLD);
    let mut right = area.right();
    for (swatch, label) in legend_entries(read_color).iter().rev() {
        let width = swatch.width() + label.len() as u16 + 1;
        if right.saturating_sub(floor) < width {
            break;
        }
        let x = right - width;
        fill(buf, x..right);
        right = x;
        match *swatch {
            Swatch::Pair(fg, bg) => painter::paint_cell(&mut buf[(x, row_y)], painter::LEFT_HALF_BLOCK, fg, bg),
            Swatch::Scale([light, mid, dark]) => {
                painter::paint_cell(&mut buf[(x, row_y)], painter::LEFT_HALF_BLOCK, light, mid);
                let spacer = Surface::POPOVER_BG();
                painter::paint_cell(&mut buf[(x + 1, row_y)], painter::LEFT_HALF_BLOCK, dark, spacer);
            }
        }
        let label_x = i32::from(x + swatch.width());
        painter::paint_text(buf, area, label_x, row_y, label, label_style);
    }
    fill(buf, right.saturating_sub(1).max(floor)..right);
}

/// Draws the centre base's depth beside the shared mark, aligned under the centre-coordinate label,
/// and returns the first column right of it.
fn draw_depth_label(buf: &mut Buffer, area: Rect, row_y: u16, depth: u32) -> u16 {
    let Some(centre_x) = cent_column(area) else {
        return area.x;
    };
    let text = depth_label_text(depth);
    let x = centre_x + 1;
    painter::paint_text(
        buf,
        area,
        i32::from(x),
        row_y,
        &text,
        Style::default().fg(Text::PRIMARY_FG()),
    );
    (x + text.chars().count() as u16).min(area.right())
}

fn depth_label_text(depth: u32) -> String {
    format!("Depth: {depth}X")
}

fn draw_controls(buf: &mut Buffer, area: Rect, read_color: ReadColor, point_depth: Option<u32>) -> Option<Rect> {
    if area.height <= CONTROLS_ROW {
        return None;
    }
    let centre_x = cent_column(area)?;
    let row_y = area.y + CONTROLS_ROW;
    let mut legend_floor = point_depth.map_or(area.x, |depth| {
        draw_depth_label(buf, area, row_y, depth)
            .saturating_add(1)
            .min(area.right())
    });
    const MAX_LABEL_WIDTH: u16 = "Reads: Direction".len() as u16;
    let width = (MAX_LABEL_WIDTH + 2).min(centre_x - area.x);
    let button = (width >= MAX_LABEL_WIDTH).then(|| Rect::new(area.x, row_y, width, 1));
    if let Some(button) = button {
        let label = match read_color {
            ReadColor::Lane => "Reads: Lane",
            ReadColor::Direction => "Reads: Direction",
            ReadColor::FiveMc => "Reads: 5mC",
        };
        let style = Style::default()
            .fg(Text::PRIMARY_FG())
            .bg(Surface::LABEL_BG())
            .add_modifier(Modifier::BOLD);
        buf.set_style(button, style);
        buf.set_string(button.x + u16::from(width > MAX_LABEL_WIDTH), button.y, label, style);
        legend_floor = legend_floor.max(button.right() + 1);
    }
    draw_legend(buf, area, row_y, read_color, legend_floor);
    button
}

impl ReferenceTrack {
    /// `point_depth` is the centre base's read depth, absent until its coverage is resident.
    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        view: TrackView<'_>,
        reference: &mut ReferenceData,
        point_depth: Option<u32>,
    ) -> Option<Rect> {
        let TrackView {
            position,
            settings,
            contig,
        } = view;
        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let in_bounds = in_bounds_column_range(position, contig.length, area.width, bp_per_column);
        let buf = frame.buffer_mut();
        paint_track_background(buf, area, in_bounds);

        if settings.sequencerow {
            draw_centline_markers(buf, area, position, bp_per_column);
        }

        if settings.sequencerow && area.height > SEQUENCE_ROW {
            let sequence_row_y = area.y + SEQUENCE_ROW;
            let is_placeholder = !reference.has_real_reference();
            match settings.horizontal_zoom {
                HorizontalZoom::Close => draw_sequence_row_close(
                    buf,
                    area,
                    sequence_row_y,
                    position.left,
                    position.underflow,
                    ReferenceLookup::new(reference, contig),
                ),
                HorizontalZoom::Mid => {
                    draw_sequence_row_mid(
                        buf,
                        area,
                        sequence_row_y,
                        position.left,
                        position.underflow,
                        ReferenceLookup::new(reference, contig),
                        settings.horizontal_zoom.bp_per_column(),
                    );
                    if area.height > CENTRED_LETTER_ROW {
                        draw_centred_sequence_letter(
                            buf,
                            area,
                            area.y + CENTRED_LETTER_ROW,
                            position.centre,
                            ReferenceLookup::new(reference, contig),
                        );
                    }
                }
            }
            if area.height > CENTRE_LABEL_ROW {
                draw_centline_label(buf, area, area.y + CENTRE_LABEL_ROW, position);
            }
            if is_placeholder {
                draw_placeholder_label(buf, area, area.y + MARKER_ROW);
            }
        }
        draw_controls(buf, area, settings.readcolor, point_depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basepair::BasePair;
    use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
    use crate::viewer_app::genome::coordinate::marks::centline_mark;
    use crate::viewer_app::genome::settings::GenomeViewerSettings;
    use crate::viewer_app::genome::track::Contig;
    use crate::viewer_app::viewport::ViewPosition;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn placeholder_keeps_its_visible_suffix_anchored_left_of_the_centre() {
        let _palette = crate::style::palette::lock_for_test();
        let genome = IndexedFasta::hs1_fallback();
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut track = ReferenceTrack;
        let bases = b"ACGTTGCAAGAT";
        let contig = Contig {
            name: "chr1".to_string(),
            length: BasePair::length(bases.len() as u64),
        };
        reference.seed_chunk(&contig.name, BasePair(0), bases);
        let sentinels = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let mut settings = GenomeViewerSettings::new();
        settings.sequencerow = true;

        for (offset_x, zoom, centre) in [
            (0, HorizontalZoom::Close, BasePair(10)),
            (9, HorizontalZoom::Close, BasePair(10)),
            (9, HorizontalZoom::Mid, BasePair(10)),
            // Odd underflow at Mid puts the marker one cell left of the centred base.
            (9, HorizontalZoom::Mid, BasePair(9)),
        ] {
            settings.horizontal_zoom = zoom;
            let area = Rect::new(offset_x, 1, 24, super::super::REFERENCE_TRACK_HEIGHT);
            let position = ViewPosition::new(centre, BasePair::from(area.width) * BasePair(zoom.bp_per_column()));
            let centre_x = area.x + area.width / 2;
            let row_y = area.y + MARKER_ROW;
            let mut terminal = Terminal::new(TestBackend::new(sentinels.len() as u16, area.bottom())).unwrap();
            terminal
                .draw(|frame| {
                    for (x, &sentinel) in sentinels.iter().enumerate() {
                        frame.buffer_mut()[(x as u16, row_y)].set_char(char::from(sentinel));
                    }
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position,
                            settings: &settings,
                            contig: &contig,
                        },
                        &mut reference,
                        None,
                    );
                })
                .unwrap();

            let buffer = terminal.backend().buffer();
            let (mark_x, mark) = centline_mark(area, zoom.bp_per_column(), position).unwrap();
            let mut expected: Vec<_> = sentinels.iter().map(|&byte| char::from(byte).to_string()).collect();
            expected[usize::from(mark_x)] = mark.to_owned();
            expected[usize::from(centre_x)] = match zoom {
                HorizontalZoom::Close => mark.to_string(),
                HorizontalZoom::Mid => char::from(bases[centre.as_u64() as usize]).to_string(),
            };
            for (x, ch) in (area.x..centre_x - 1)
                .rev()
                .zip(sequence_row::PLACEHOLDER_LABEL.chars().rev())
            {
                expected[usize::from(x)] = ch.to_string();
            }
            for (x, expected) in expected.iter().enumerate() {
                assert_eq!(buffer[(x as u16, row_y)].symbol(), expected, "pane {area:?}, x={x}");
            }
        }
    }

    #[test]
    fn mid_zoom_puts_the_centred_letter_above_the_sequence_and_label_below() {
        let _palette = crate::style::palette::lock_for_test();
        let mut settings = GenomeViewerSettings::new();
        settings.horizontal_zoom = HorizontalZoom::Mid;
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").expect("reference fixture");
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut track = ReferenceTrack;
        let contig = Contig {
            name: "chr1".to_string(),
            length: BasePair(248_387_328),
        };
        let area = Rect::new(0, 4, 80, super::super::REFERENCE_TRACK_HEIGHT);
        let position = ViewPosition::new(BasePair(7), BasePair(160));
        let seeded = b"ACGTTGCAAGGT";
        reference.seed_chunk("chr1", BasePair(0), seeded);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.bottom() + 2)).expect("test terminal");
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &contig,
                    },
                    &mut reference,
                    None,
                );
            })
            .expect("reference draw");
        let buffer = terminal.backend().buffer();
        let (marker_x, marker) = centline_mark(area, 2, position).expect("marker");
        let letter = buffer
            .cell((area.x + area.width / 2, area.y))
            .expect("centred letter cell")
            .symbol();
        assert_eq!(
            letter,
            char::from(seeded[position.centre.as_u64() as usize]).to_string()
        );

        assert_eq!(
            buffer
                .cell((marker_x, area.y + CENTRE_LABEL_ROW))
                .expect("marker cell")
                .symbol(),
            marker.to_string(),
            "the marker must remain under the sequence beside the label"
        );
        let label_lead = sequence_row::centline_label_text(position)
            .chars()
            .next()
            .expect("centre label");
        assert_eq!(
            buffer
                .cell((area.x + area.width / 2 + 1, area.y + CENTRE_LABEL_ROW))
                .expect("label cell")
                .symbol(),
            label_lead.to_string(),
            "the reference label must sit below the sequence row"
        );
    }

    #[test]
    fn out_of_bounds_columns_use_oob_background_and_stay_blank_on_sequence_row() {
        use crate::style::{Chrome, Surface};

        let _palette = crate::style::palette::lock_for_test();
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").expect("reference fixture");
        let mut reference = ReferenceData::new(&genome, ReferenceWorkerHandle::spawn());
        let mut track = ReferenceTrack;
        // Odd length (11) exercises both contig edges at once and a half-occupied boundary cell at Mid.
        let bases = b"ACGTTGCAAGC";
        let contig = Contig {
            name: "chr1".to_string(),
            length: BasePair::length(bases.len() as u64),
        };
        reference.seed_chunk(&contig.name, BasePair(0), bases);
        let area = Rect::new(0, 0, 40, super::super::REFERENCE_TRACK_HEIGHT);

        for zoom in [HorizontalZoom::Close, HorizontalZoom::Mid] {
            let mut settings = GenomeViewerSettings::new();
            settings.horizontal_zoom = zoom;
            let bp_per_column = zoom.bp_per_column();
            let position = ViewPosition::new(
                BasePair(bases.len() as u64 - 1),
                BasePair::from(area.width) * BasePair(bp_per_column),
            );
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position,
                            settings: &settings,
                            contig: &contig,
                        },
                        &mut reference,
                        None,
                    );
                })
                .expect("reference draw");

            let buf = terminal.backend().buffer();
            let in_bounds = in_bounds_column_range(position, contig.length, area.width, bp_per_column);
            assert!(in_bounds.start > 0 && in_bounds.end < area.width);

            for col in (0..in_bounds.start).chain(in_bounds.end..area.width) {
                let seq_cell = buf.cell((col, SEQUENCE_ROW)).unwrap();
                assert_eq!(seq_cell.symbol(), " ", "{zoom:?} OOB col {col} must stay blank");
                assert_eq!(seq_cell.bg, Chrome::OUT_OF_BOUNDS_BG(), "{zoom:?} OOB col {col} bg");
                assert_eq!(
                    buf.cell((col, MARKER_ROW)).unwrap().bg,
                    Chrome::OUT_OF_BOUNDS_BG(),
                    "{zoom:?} OOB col {col} row 0 bg"
                );
            }

            // The centre coordinate label over OOB columns must preserve the background under it.
            assert_eq!(
                buf.cell((area.width / 2 + 1, CENTRE_LABEL_ROW)).unwrap().bg,
                Chrome::OUT_OF_BOUNDS_BG(),
                "{zoom:?} centre label over OOB must keep OOB bg"
            );

            // At Mid zoom, the last base (index 10, 'C') occupies the left half of the last
            // in-bounds column while its out-of-bounds right half uses Chrome::OUT_OF_BOUNDS_BG.
            if zoom == HorizontalZoom::Mid {
                let last_col = in_bounds.end - 1;
                let cell = buf.cell((last_col, SEQUENCE_ROW)).unwrap();
                assert_eq!(cell.fg, Reference::base_color_pair(BaseResidue::C).pick(false));
                assert_eq!(cell.bg, Chrome::OUT_OF_BOUNDS_BG());
            } else {
                assert_eq!(
                    buf.cell((in_bounds.start, SEQUENCE_ROW)).unwrap().bg,
                    Surface::TRACK_BG()
                );
            }
        }
    }
}
