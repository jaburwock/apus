//! Reference-row painting and centreline content for `ReferenceTrack`.
//!
//! These helpers consume the shell's `ViewPosition` rather than retaining scroll state. At contig
//! starts, `underflow` leaves leading columns blank so real bases begin at the column corresponding
//! to genomic position zero, preserving alignment with every other genome track. Near contig ends,
//! bounds checks stop sequence painting at the active contig boundary so the track's out-of-bounds
//! background shows through.
//!
//! Close zoom paints one base per cell. Mid zoom paints two bases through a half-block cell and derives
//! the centred half from the offset between the centre and the viewport's left edge, not from the
//! absolute coordinate's parity. The centre marker is resolved by the shared coordinate-mark
//! mechanism so column and glyph cannot diverge between tracks.
//!
//! The shared cache resolves placeholder and embedded bases synchronously and queues misses for
//! indexed FASTA. Sequence rows render unavailable in-bounds bases as `UNKNOWN_BASE`.
//!
//! Note: roughly 65% of this file is its test module; the rendering code itself is a little over
//! 200 lines.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::basepair::BasePair;
use crate::viewer_app::painter;

use crate::residue::BaseResidue;
use crate::style::{Chrome, Reference, Surface, Text};

use crate::viewer_app::viewport::ViewPosition;

use super::super::coordinate::marks::centline_mark;
use super::super::geometry::{cent_column, leading_columns};
use super::super::reference_data::ReferenceLookup;

/// Glyph used when an in-bounds reference base is unavailable.
pub(super) const UNKNOWN_BASE: char = BaseResidue::Null.as_char();

/// Label shown while the bundled fallback sequence is displayed.
pub(super) const PLACEHOLDER_LABEL: &str = "Placeholder. No sequence supplied.";

/// The one style for a reference base drawn as a letter. Bold everywhere it is
/// legible as a letter, so the sequence reads at a glance against the tracks
/// stacked around it; `Mid`'s half-block row carries no letters and is unaffected.
pub(super) fn base_letter_style(residue: BaseResidue) -> Style {
    Style::default()
        .fg(Reference::base_color(residue))
        .bg(Surface::TRACK_BG())
        .add_modifier(Modifier::BOLD)
}

/// Draws one base per column across the in-bounds span of the track. Leading underflow columns
/// and columns at or past `contig_length` are left untouched so the track's out-of-bounds
/// background shows through; in-bounds misses render as `UNKNOWN_BASE`.
pub(super) fn draw_sequence_row_close(
    buf: &mut Buffer,
    area: Rect,
    row_y: u16,
    offset: BasePair,
    underflow: BasePair,
    mut reference: ReferenceLookup<'_>,
) {
    let width = area.width;
    let unknown_style = base_letter_style(BaseResidue::Null);
    let leading = leading_columns(underflow, width, 1);
    let contig_length = reference.contig_length();

    for col in leading..width {
        let pos = offset + BasePair::from(col - leading);
        if pos >= contig_length {
            break;
        }
        let residue = reference.base_at(pos).and_then(|base| BaseResidue::try_from(base).ok());
        let (ch, style) = match residue {
            Some(residue) => (residue.as_char(), base_letter_style(residue)),
            None => (UNKNOWN_BASE, unknown_style),
        };
        if let Some(cell) = buf.cell_mut((area.x + col, row_y)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

/// Draws two bases per column at `HorizontalZoom::Mid`. The left half uses the plain base color
/// and the right half the tinted color. Out-of-bounds columns are left untouched; a boundary
/// column whose left base is in bounds and right base is past `contig_length` paints the right
/// half with `Chrome::OUT_OF_BOUNDS_BG`.
pub(super) fn draw_sequence_row_mid(
    buf: &mut Buffer,
    area: Rect,
    row_y: u16,
    offset: BasePair,
    underflow: BasePair,
    mut reference: ReferenceLookup<'_>,
    bp_per_column: u64,
) {
    let width = area.width;
    let leading = leading_columns(underflow, width, bp_per_column);
    let contig_length = reference.contig_length();
    let oob_bg = Chrome::OUT_OF_BOUNDS_BG();

    let mut resolve = |pos: BasePair| {
        reference
            .base_at(pos)
            .and_then(|base| BaseResidue::try_from(base).ok())
            .unwrap_or(BaseResidue::Null)
    };

    for col in leading..width {
        let left_pos = offset + BasePair::from(col - leading) * BasePair::length(bp_per_column);
        if left_pos >= contig_length {
            break;
        }
        let right_pos = left_pos + BasePair(1);
        let fg = Reference::base_color_pair(resolve(left_pos)).pick(false);
        let bg = if right_pos < contig_length {
            Reference::base_color_pair(resolve(right_pos)).pick(true)
        } else {
            oob_bg
        };
        if let Some(cell) = buf.cell_mut((area.x + col, row_y)) {
            painter::paint_cell(cell, painter::LEFT_HALF_BLOCK, fg, bg);
        }
    }
}

/// Draws the centre base on the fixed row above the `Mid`-zoom sequence row.
pub(super) fn draw_centred_sequence_letter(
    buf: &mut Buffer,
    area: Rect,
    row_y: u16,
    centre: BasePair,
    mut reference: ReferenceLookup<'_>,
) {
    let Some(x) = cent_column(area) else { return };
    let Some(residue) = reference
        .base_at(centre)
        .and_then(|base| BaseResidue::try_from(base).ok())
    else {
        return;
    };
    if let Some(cell) = buf.cell_mut((x, row_y)) {
        cell.set_symbol(&residue.as_char().to_string());
        cell.set_style(base_letter_style(residue));
    }
}

/// Draws the centre marker on one reference row, preserving the cell's background.
pub(super) fn draw_centline_marker(
    buf: &mut Buffer,
    area: Rect,
    row_y: u16,
    position: ViewPosition,
    bp_per_column: u64,
) {
    if let Some((mark_x, glyph)) = centline_mark(area, bp_per_column, position)
        && let Some(cell) = buf.cell_mut((mark_x, row_y))
    {
        let bg = cell.bg;
        painter::paint_cell(cell, glyph, Text::PRIMARY_FG(), bg);
    }
}

pub(super) fn centline_label_text(position: ViewPosition) -> String {
    format!("g.{}", BasePair(position.centre.to_display()).format_with_commas())
}

/// Draws the reference track's centre coordinate label beside the shared mark.
pub(super) fn draw_centline_label(buf: &mut Buffer, area: Rect, row_y: u16, position: ViewPosition) {
    let Some(centre_x) = cent_column(area) else { return };
    painter::paint_text(
        buf,
        area,
        i32::from(centre_x) + 1,
        row_y,
        &centline_label_text(position),
        Style::default().fg(Text::PRIMARY_FG()),
    );
}

/// Draws the centre marker through the whole reference pane before its content.
pub(super) fn draw_centline_markers(buf: &mut Buffer, area: Rect, position: ViewPosition, bp_per_column: u64) {
    for row_y in area.y..area.bottom() {
        draw_centline_marker(buf, area, row_y, position, bp_per_column);
    }
}

/// Leaves one column between the placeholder and the centre, even when the left edge clips it.
pub(super) fn draw_placeholder_label(buf: &mut Buffer, area: Rect, row_y: u16) {
    let Some(centre_x) = cent_column(area) else { return };
    let label_x = i32::from(centre_x) - 1 - PLACEHOLDER_LABEL.chars().count() as i32;
    painter::paint_text(
        buf,
        area,
        label_x,
        row_y,
        PLACEHOLDER_LABEL,
        Style::default().fg(Text::PRIMARY_FG()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refgenome::{IndexedFasta, ReferenceWorkerHandle};
    use crate::viewer_app::genome::reference_data::ReferenceData;
    use crate::viewer_app::genome::track::Contig;

    fn fallback_reference() -> ReferenceData {
        ReferenceData::new(&IndexedFasta::hs1_fallback(), ReferenceWorkerHandle::spawn())
    }

    fn real_reference() -> ReferenceData {
        let genome = IndexedFasta::from_file("testdata/reference/hs1_subset.fa").unwrap();
        ReferenceData::new(&genome, ReferenceWorkerHandle::spawn())
    }

    /// The contig these rows are drawn against, named once so each test states only the length it
    /// actually depends on.
    fn contig(length: BasePair) -> Contig {
        Contig {
            name: "chr1".to_string(),
            length,
        }
    }

    const FAR: BasePair = BasePair(1_000_000);

    #[test]
    fn draw_sequence_row_close_leaves_underflow_blank_and_shifts_content_right() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let underflow = BasePair(5);
        draw_sequence_row_close(
            &mut buf,
            area,
            1,
            BasePair(0),
            underflow,
            ReferenceLookup::new(&mut reference, &contig(FAR)),
        );

        for col in 0..5u16 {
            assert_eq!(buf.cell((col, 1)).unwrap().symbol(), " ");
        }
        let base_at_5 = buf.cell((5, 1)).unwrap().symbol().chars().next().unwrap();
        assert!(
            matches!(base_at_5, 'A' | 'C' | 'G' | 'T'),
            "genomic position 0 must land at screen column `underflow` (5), got {base_at_5:?}"
        );
    }

    /// Which base each column holds, not merely that it holds one.
    ///
    /// The sibling test above reads a fallback-generated row, where every position yields some
    /// A/C/G/T — so an offset error there still satisfies it. Seeding a known non-repeating chunk
    /// is what makes the column-to-position mapping observable: shift it by one and the letters
    /// move.
    #[test]
    fn draw_sequence_row_close_puts_each_seeded_base_in_its_own_column() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 12, 3);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let seeded = b"ACGTTGCAAGGT";
        reference.seed_chunk("chr1", BasePair(0), seeded);
        let underflow = BasePair(4);
        draw_sequence_row_close(
            &mut buf,
            area,
            1,
            BasePair(0),
            underflow,
            ReferenceLookup::new(&mut reference, &contig(FAR)),
        );

        // Content starts at `underflow`, so column `underflow + i` holds genomic position i.
        let leading = usize::from(u16::try_from(underflow.as_u64()).unwrap());
        for (index, base) in seeded.iter().take(usize::from(area.width) - leading).enumerate() {
            let column = u16::try_from(leading + index).unwrap();
            assert_eq!(
                buf.cell((column, 1)).unwrap().symbol(),
                char::from(*base).to_string(),
                "column {column} must hold genomic position {index}"
            );
        }
    }

    /// A viewport centred on `centre` across a pane `columns` wide.
    fn at(centre: BasePair, columns: u16, bp_per_column: u64) -> ViewPosition {
        ViewPosition::new(centre, BasePair::from(columns) * BasePair(bp_per_column))
    }

    #[test]
    fn centline_marker_is_drawn_at_the_centre_column() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 80, 9);
        let mut buf = Buffer::empty(area);
        draw_centline_marker(&mut buf, area, 5, at(BasePair(1_000), area.width, 1), 1);

        let x = cent_column(area).unwrap();
        assert_ne!(buf.cell((x, 5)).unwrap().symbol(), " ");
    }

    #[test]
    fn close_zoom_centline_stays_in_the_centre_column_at_both_parities() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 80, 9);
        let x = cent_column(area).unwrap();

        for centre in [BasePair(1_000), BasePair(1_001)] {
            let mut buf = Buffer::empty(area);
            draw_centline_marker(&mut buf, area, 5, at(centre, area.width, 1), 1);
            assert_ne!(
                buf.cell((x, 5)).unwrap().symbol(),
                " ",
                "centre {centre:?} must mark the centre column"
            );
        }
    }

    /// The mark must land on the half-cell the centre base actually renders in.
    /// The sequence row is the oracle: it paints the left half as fg and the
    /// right half as bg, so a uniquely-coloured base at `centre` says which half
    /// is the real one. Which half that is falls out of `centre - left`, not of
    /// `centre`'s own parity — see `centline_mark`.
    #[test]
    fn centline_mark_lands_on_the_half_cell_holding_the_centre_base() {
        let _palette = crate::style::palette::lock_for_test();
        for width in [20u16, 21] {
            for centre in [BasePair(1_000), BasePair(1_001)] {
                let area = Rect::new(0, 0, width, 3);
                let position = ViewPosition::new(centre, BasePair::from(width) * BasePair(2));

                // A lone `A` at `centre`, `T` everywhere else, so exactly one
                // half-cell in the row carries A's colour.
                let span = usize::from(width) * 2;
                let mut bases = vec![b'T'; span];
                let idx = (centre - position.left).as_u64() as usize;
                bases[idx] = b'A';
                let mut reference = fallback_reference();
                reference.seed_chunk("chr1", position.left, &bases);

                let mut buf = Buffer::empty(area);
                draw_sequence_row_mid(
                    &mut buf,
                    area,
                    1,
                    position.left,
                    position.underflow,
                    ReferenceLookup::new(&mut reference, &contig(FAR)),
                    2,
                );

                let a_fg = Reference::base_color_pair(BaseResidue::A).pick(false);
                let a_bg = Reference::base_color_pair(BaseResidue::A).pick(true);
                let expected = (0..width)
                    .find_map(|col| {
                        let cell = buf.cell((col, 1)).unwrap();
                        if cell.fg == a_fg {
                            Some((col, painter::LEFT_HALF_BLOCK))
                        } else if cell.bg == a_bg {
                            Some((col, painter::RIGHT_HALF_BLOCK))
                        } else {
                            None
                        }
                    })
                    .expect("the centre base must render somewhere in the row");

                assert_eq!(
                    centline_mark(area, 2, position),
                    Some(expected),
                    "width {width}, centre {centre:?}: the mark must sit on the half-cell drawing the centre base"
                );
            }
        }
    }

    #[test]
    fn draw_sequence_row_mid_fg_and_bg_are_independently_colored_bases() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 1, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), b"AC");
        draw_sequence_row_mid(
            &mut buf,
            area,
            1,
            BasePair(0),
            BasePair(0),
            ReferenceLookup::new(&mut reference, &contig(FAR)),
            2,
        );

        let cell = buf.cell((0, 1)).unwrap();
        assert_eq!(cell.fg, Reference::base_color_pair(BaseResidue::A).pick(false));
        assert_eq!(cell.bg, Reference::base_color_pair(BaseResidue::C).pick(true));
    }

    #[test]
    fn draw_sequence_row_mid_falls_back_to_unknown_base_color_on_a_miss() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 1, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = real_reference();
        draw_sequence_row_mid(
            &mut buf,
            area,
            1,
            BasePair(0),
            BasePair(0),
            ReferenceLookup::new(
                &mut reference,
                &Contig {
                    name: "not_a_real_contig".to_string(),
                    length: FAR,
                },
            ),
            2,
        );

        let cell = buf.cell((0, 1)).unwrap();
        assert_eq!(cell.fg, Reference::base_color_pair(BaseResidue::Null).pick(false));
        assert_eq!(cell.bg, Reference::base_color_pair(BaseResidue::Null).pick(true));
    }

    #[test]
    fn draw_sequence_row_mid_stride_is_driven_by_bp_per_column_parameter() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        reference.seed_chunk("chr1", BasePair(0), b"ACGTNA");
        draw_sequence_row_mid(
            &mut buf,
            area,
            1,
            BasePair(0),
            BasePair(0),
            ReferenceLookup::new(&mut reference, &contig(FAR)),
            3,
        );

        let col1 = buf.cell((1, 1)).unwrap();
        assert_eq!(
            col1.fg,
            Reference::base_color_pair(BaseResidue::T).pick(false),
            "column 1's fg should read position 3 (offset + 1*3)"
        );
        assert_eq!(
            col1.bg,
            Reference::base_color_pair(BaseResidue::N).pick(true),
            "column 1's bg should read position 4"
        );
    }

    #[test]
    fn draw_centred_sequence_letter_shows_the_seeded_base_at_centre() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let centre = BasePair(10);
        let seeded = b"ACGTTGCAAGGT";
        reference.seed_chunk("chr1", BasePair(0), seeded);
        draw_centred_sequence_letter(
            &mut buf,
            area,
            1,
            centre,
            ReferenceLookup::new(&mut reference, &contig(FAR)),
        );

        let x = cent_column(area).unwrap();
        let cell = buf.cell((x, 1)).unwrap();
        let base = seeded[centre.as_u64() as usize];
        assert_eq!(cell.symbol(), char::from(base).to_string());
    }

    #[test]
    fn draw_centred_sequence_letter_draws_nothing_past_contig_length() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let centre = BasePair(10);
        let contig_length = BasePair(5);
        draw_centred_sequence_letter(
            &mut buf,
            area,
            1,
            centre,
            ReferenceLookup::new(&mut reference, &contig(contig_length)),
        );

        let x = cent_column(area).unwrap();
        assert!(!matches!(
            buf.cell((x, 1)).unwrap().symbol(),
            "A" | "C" | "G" | "T" | "N"
        ));
    }

    #[test]
    fn draw_centred_sequence_letter_only_touches_the_centre_column() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let centre = BasePair(10);
        reference.seed_chunk("chr1", BasePair(0), &[b'A'; 50]);
        draw_centred_sequence_letter(
            &mut buf,
            area,
            1,
            centre,
            ReferenceLookup::new(&mut reference, &contig(FAR)),
        );

        let x = cent_column(area).unwrap();
        for col in 0..area.width {
            if col == x {
                continue;
            }
            assert_eq!(
                buf.cell((col, 1)).unwrap().symbol(),
                " ",
                "column {col} outside centre should stay untouched"
            );
        }
    }

    #[test]
    fn draw_centred_sequence_letter_aligns_with_centline_markers_column() {
        let _palette = crate::style::palette::lock_for_test();
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        let mut reference = fallback_reference();
        let centre = BasePair(100);
        reference.seed_chunk("chr1", BasePair(0), &[b'T'; 200]);

        draw_centred_sequence_letter(
            &mut buf,
            area,
            0,
            centre,
            ReferenceLookup::new(&mut reference, &contig(FAR)),
        );
        draw_centline_marker(&mut buf, area, 1, at(centre, area.width, 2), 2);

        let x = cent_column(area).unwrap();
        assert_ne!(buf.cell((x, 0)).unwrap().symbol(), " ");
        assert_eq!(
            buf.cell((x, 1)).unwrap().symbol(),
            painter::LEFT_HALF_BLOCK,
            "same column the centline marker occupies"
        );
    }
}
