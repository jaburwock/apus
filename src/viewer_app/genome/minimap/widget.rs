//! Rendering leaf for `MinimapTrack`'s fixed-scale whole-contig position bar.
//!
//! `MinimapTrack` owns pane placement and change-driven animation state; this module turns the
//! propagated centre and active contig length into bar cells. The bar never scrolls: position is
//! an indicator over a whole-contig scale, consistent with invariant 10's shell-owned position
//! contract and invariant 9's full-width track panes.
//!
//! Fill progress is length-scaled and eased by a shared helper, which the divider also calls, so
//! its count and the bar cannot drift apart. The indicator changes one foreground or background
//! channel at half-cell resolution, placing it precisely without introducing another glyph
//! vocabulary.
//!
//! Filled cells use the contig-bar colour pair; unfilled cells use the track ground beneath a
//! placeholder. Keep those roles and channels separate: treating the indicator as text, or using
//! the track background as the bar's own palette, breaks raster placement and background contracts.

use ratatui::layout::Rect;

use crate::basepair::BasePair;
use crate::style::{Minimap, Surface, Text};
use crate::viewer_app::painter;

/// Glyph for an unfilled cell.
const EMPTY_GLYPH: &str = "\u{28FF}";

/// Minimum fill duration fraction for short contigs.
const MIN_SPEED_FRACTION: f64 = 0.40;

/// Scales and eases progress by the active contig's length.
///
/// The divider's counted length reads this too, so its number and the bar's fill advance
/// from one warped value rather than two that can drift apart.
pub(super) fn warp_fill_fraction(
    fill_fraction: f64,
    contig_length: Option<BasePair>,
    max_contig_length: Option<BasePair>,
) -> f64 {
    let (Some(contig_length), Some(max_contig_length)) = (contig_length, max_contig_length) else {
        return fill_fraction;
    };
    if max_contig_length.as_u64() == 0 {
        return fill_fraction;
    }
    let speed = (contig_length.as_f64() / max_contig_length.as_f64()).clamp(MIN_SPEED_FRACTION, 1.0);
    ease_in_out_cubic((fill_fraction / speed).min(1.0))
}

/// Cubic ease-in-out over `[0.0, 1.0]`.
fn ease_in_out_cubic(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Draws the bar at the animation's warped fraction.
pub(super) fn draw(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    centline_position: Option<BasePair>,
    settled_length: Option<BasePair>,
    max_contig_length: Option<BasePair>,
    raw_fraction: f64,
) {
    let warped = warp_fill_fraction(raw_fraction, settled_length, max_contig_length);
    draw_minimap(buf, area, centline_position, settled_length, warped);
}

/// Draws the filled bar and highlights the centre position at half-cell
/// resolution.
fn draw_minimap(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    centline_position: Option<BasePair>,
    contig_length: Option<BasePair>,
    fill_fraction: f64,
) {
    let width = area.width;
    let filled_cells = (fill_fraction.clamp(0.0, 1.0) * width as f64).round() as u16;
    for col in 0..width {
        if let Some(cell) = buf.cell_mut((area.x + col, area.y)) {
            if col < filled_cells {
                painter::paint_cell(
                    cell,
                    painter::LEFT_HALF_BLOCK,
                    Minimap::CONTIG_BAR_FG(),
                    Minimap::CONTIG_BAR_BG(),
                );
            } else {
                painter::paint_cell(cell, EMPTY_GLYPH, Text::DIMMED_FG(), Surface::TRACK_BG());
            }
        }
    }

    if let (Some(centline_position), Some(contig_length)) = (centline_position, contig_length)
        && width > 0
        && contig_length > BasePair(0)
    {
        let fraction = (centline_position.as_f64() / contig_length.as_f64()).clamp(0.0, 1.0);
        let slots = width as u64 * 2;
        let raw = fraction * (slots - 1) as f64;
        let nearest = raw.round();
        let offset = raw - nearest;
        let mut indices = vec![nearest as u64];
        if offset > 0.25 && nearest as u64 + 1 < slots {
            indices.push(nearest as u64 + 1);
        } else if offset < -0.25 && nearest > 0.0 {
            indices.push(nearest as u64 - 1);
        }
        for index in indices {
            let cell_offset = (index / 2) as u16;
            let is_fg = index % 2 == 0;
            if let Some(cell) = buf.cell_mut((area.x + cell_offset, area.y)) {
                let cell_style = cell.style();
                cell.set_style(if is_fg {
                    cell_style.fg(Minimap::CONTIG_BAR_HIGHLIGHT_FG())
                } else {
                    cell_style.bg(Minimap::CONTIG_BAR_HIGHLIGHT_BG())
                });
            }
        }
    }
}
