//! Stateless colour legend overlay for the local-area track.
//!
//! Queries live palette entries on every draw so runtime edits take effect immediately. It
//! describes that pane alone, so a hidden local area has no legend, and sits above the reference
//! band so the CpG colours it explains stay visible.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::popover::{Popover, PopoverAnchor};
use crate::setting::ReadColor;
use crate::style::{Indel, LocalArea};
use crate::viewer_app::painter::LEFT_HALF_BLOCK;

use super::local_area;

/// Three light-to-dark samples (one colour repeated for a flat swatch), then a blank half-cell
/// before the label: `▌` paints a cell's left half in its fg and right half in its bg, and the
/// second cell's unset bg keeps the popover surface.
fn swatch_row([light, mid, dark]: [Color; 3], label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw(LEFT_HALF_BLOCK).fg(light).bg(mid),
        Span::raw(LEFT_HALF_BLOCK).fg(dark),
        Span::raw(label),
    ])
}

/// The 5mC chart replaces the indel/breakpoint coverage chart, so each mode lists only its own
/// chart's colours. Scales are sampled through their own mapping functions across their full
/// domain (the CpG ramp saturates at 8%).
pub(super) fn draw(frame: &mut Frame, local_area: Rect, read_color: ReadColor) -> Option<Rect> {
    let lines = match read_color {
        ReadColor::Lane | ReadColor::Direction => vec![
            swatch_row([Indel::INSERTION(); 3], "Insertion"),
            swatch_row([Indel::DELETION(); 3], "Deletion"),
            swatch_row([LocalArea::BREAKPOINT_COVERAGE_FG(); 3], "Breakpoint"),
        ],
        // CpG density last, nearest the reference band it describes.
        ReadColor::FiveMc => vec![
            swatch_row([0.0, 127.5, 255.0].map(LocalArea::mod_5mc_grey), "Mean 5mC"),
            swatch_row([LocalArea::MOD_5MC_NO_DATA(); 3], "No data"),
            swatch_row([0.0, 0.04, 0.08].map(LocalArea::cpg_density_color), "CpG density"),
        ],
    };
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let bounds = local_area::chart_area(local_area);
    let popover = Popover::anchored(bounds, PopoverAnchor::BottomRight, width, lines.len() as u16)?;
    popover.draw_content(frame, None, "", |frame, inner| {
        frame.render_widget(Paragraph::new(lines), inner);
    });
    Some(popover.bounds())
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::super::{AlignmentTrack, GenomeViewer, GenomeViewerSettings};
    use super::*;
    use crate::command::{
        parse::resolve_command,
        protocol::{Applied, CommandTarget},
    };
    use crate::io::IoWorkerHandle;
    use crate::keys::Keymap;
    use crate::refgenome::IndexedFasta;
    use crate::style::{Surface, palette};
    use crate::viewer::ViewerApp;

    fn new_viewer() -> GenomeViewer {
        GenomeViewer::new(
            GenomeViewerSettings::new(),
            IndexedFasta::hs1_fallback(),
            AlignmentTrack::new(),
            None,
            IoWorkerHandle::spawn(),
        )
    }

    fn render_viewer(terminal: &mut Terminal<TestBackend>, viewer: &mut GenomeViewer, area: Rect) -> Buffer {
        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("draw viewer");
        terminal.backend().buffer().clone()
    }

    fn assert_same_cells_outside(before: &Buffer, after: &Buffer, excluded: &[Rect]) {
        for position in before.area.positions() {
            if !excluded.iter().any(|area| area.contains(position)) {
                assert_eq!(before[position], after[position], "cell at {position:?}");
            }
        }
    }

    #[test]
    fn enabling_legend_overlays_the_local_area_chart_corner_at_every_zoom() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareatrack = true;
        let area = Rect::new(2, 1, 120, 50);
        let panes = GenomeViewer::layout(area, &viewer.settings);
        let chart = local_area::chart_area(panes.local_area);
        assert!(
            chart.bottom() < panes.local_area.bottom(),
            "fixture must leave a reference band"
        );
        // The minimap's contig-length count-up advances between draws.
        let animated = panes.minimap_divider.union(panes.minimap);
        let overlay_x = chart.x + chart.width / 2;
        let overlay = Rect::new(overlay_x, chart.y, chart.right() - overlay_x, chart.height);
        let corner = (chart.right() - 1, chart.bottom() - 1);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");

        // Each zoom state once, with both chart modes covered between them.
        for (read_color, horizontal_mid, vertical_mid) in [
            (ReadColor::Direction, false, false),
            (ReadColor::FiveMc, false, true),
            (ReadColor::FiveMc, true, false),
        ] {
            viewer.settings.readcolor = read_color;
            viewer.apply_zoom(Some(horizontal_mid), Some(vertical_mid));
            viewer.settings.colorlegend = false;
            let before = render_viewer(&mut terminal, &mut viewer, area);

            viewer.settings.colorlegend = true;
            let after = render_viewer(&mut terminal, &mut viewer, area);
            assert_eq!(after[corner].bg, Surface::POPOVER_BG(), "{read_color:?}");
            assert_same_cells_outside(&before, &after, &[overlay, animated]);

            viewer.settings.colorlegend = false;
            let disabled = render_viewer(&mut terminal, &mut viewer, area);
            assert_same_cells_outside(&before, &disabled, &[animated]);
        }
    }

    /// Each row's swatch as `[light, mid, dark]`, read bottom-up from the popover's last row.
    fn render_swatches(
        terminal: &mut Terminal<TestBackend>,
        local_area: Rect,
        read_color: ReadColor,
    ) -> Vec<[Color; 3]> {
        terminal
            .draw(|frame| {
                draw(frame, local_area, read_color);
            })
            .expect("draw legend");
        let buffer = terminal.backend().buffer();
        let border_y = local_area::chart_area(local_area).bottom() - 1;
        let left = (local_area.x..local_area.right())
            .find(|&x| buffer[(x, border_y)].bg == Surface::POPOVER_BG())
            .expect("legend bottom border");
        let mut rows: Vec<_> = (local_area.y..border_y)
            .rev()
            .map_while(|y| {
                let [first, second] = [left + 1, left + 2].map(|x| &buffer[(x, y)]);
                (first.symbol() == LEFT_HALF_BLOCK).then(|| {
                    assert_eq!(second.symbol(), LEFT_HALF_BLOCK);
                    assert_eq!(second.bg, Surface::POPOVER_BG(), "spacer half");
                    [first.fg, first.bg, second.fg]
                })
            })
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn legend_swatches_follow_readcolor_and_live_palette_edits() {
        let _palette = palette::lock_for_test();
        let local_area = Rect::new(0, 5, 80, 10);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");

        let flat = |color| [color; 3];
        let events = [
            flat(Indel::INSERTION()),
            flat(Indel::DELETION()),
            flat(LocalArea::BREAKPOINT_COVERAGE_FG()),
        ];
        for read_color in [ReadColor::Lane, ReadColor::Direction] {
            assert_eq!(
                render_swatches(&mut terminal, local_area, read_color),
                events,
                "{read_color:?}"
            );
        }

        let [grey, no_data, cpg] = render_swatches(&mut terminal, local_area, ReadColor::FiveMc)
            .try_into()
            .expect("three 5mC rows");
        assert_eq!(no_data, flat(LocalArea::MOD_5MC_NO_DATA()));
        for (samples, lightest, darkest) in [
            (grey, LocalArea::mod_5mc_grey(0.0), LocalArea::mod_5mc_grey(255.0)),
            (
                cpg,
                LocalArea::cpg_density_color(0.0),
                LocalArea::cpg_density_color(1.0),
            ),
        ] {
            assert_eq!((samples[0], samples[2]), (lightest, darkest));
            assert!(![lightest, darkest].contains(&samples[1]), "mid sample {samples:?}");
        }

        let edited = palette::ramp::purple(3);
        assert_ne!(Indel::INSERTION(), edited);
        palette::apply_spec("Indel_INSERTION:purple(3)").expect("edit insertion colour");
        assert_eq!(
            render_swatches(&mut terminal, local_area, ReadColor::Lane)[0],
            flat(edited)
        );
    }

    #[test]
    fn colorlegend_command_and_aliases_update_the_genome_setting() {
        let mut viewer = new_viewer();
        for name in ["colorlegend", "colourlegend", "legend"] {
            for enabled in [false, true] {
                let outcome = resolve_command(&format!(":set {name} {enabled}")).expect("resolve setting");
                assert_eq!(viewer.apply_outcome(outcome), Applied::Ok);
                assert_eq!(viewer.settings.colorlegend, enabled, "{name}");
            }
        }
    }
}
