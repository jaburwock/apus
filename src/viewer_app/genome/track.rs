//! Shared per-frame inputs and viewport-width conversion for genome tracks.
//!
//! `GenomeViewer` owns the shell position, active contig, and settings, then pushes them into each
//! concrete renderer as a `TrackView` during `draw`. Tracks are immediate-mode about those inputs:
//! they render the supplied view and do not retain a second copy of position or contig state.
//!
//! `Contig::length` is always a `BasePair`, allowing each track to paint its own out-of-bounds
//! fallback when the shell-owned viewport reaches a contig edge. `visible_bp_span` converts a
//! pane's terminal width into the bp-denominated `ViewPosition::width` using shared horizontal
//! zoom; renderer-specific data and track-owned vertical state stay outside this seam.

use std::ops::Range;

use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use crate::basepair::BasePair;
use crate::style::{Chrome, Surface};
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::viewport::ViewPosition;

use super::zoom::HorizontalZoom;

/// Active contig passed to each track.
pub(in crate::viewer_app::genome) struct Contig {
    pub(in crate::viewer_app::genome) name: String,
    /// Contig length used for out-of-bounds rendering.
    pub(in crate::viewer_app::genome) length: BasePair,
}

/// Per-frame view shared by genome-track renderers.
#[derive(Clone, Copy)]
pub(in crate::viewer_app::genome) struct TrackView<'a> {
    pub(in crate::viewer_app::genome) position: ViewPosition,
    pub(in crate::viewer_app::genome) settings: &'a GenomeViewerSettings,
    pub(in crate::viewer_app::genome) contig: &'a Contig,
}

/// Returns the bp span covered by `columns` at `hzoom`.
pub(in crate::viewer_app::genome) fn visible_bp_span(columns: u16, hzoom: HorizontalZoom) -> BasePair {
    BasePair::from(columns) * BasePair(hzoom.bp_per_column())
}

/// Paints `Surface::TRACK_BG` across `area` and `Chrome::OUT_OF_BOUNDS_BG` outside `in_bounds`.
pub(in crate::viewer_app::genome) fn paint_track_background(buf: &mut Buffer, area: Rect, in_bounds: Range<u16>) {
    buf.set_style(area, Style::default().bg(Surface::TRACK_BG()));
    let oob_style = Style::default().bg(Chrome::OUT_OF_BOUNDS_BG());
    if in_bounds.start > 0 {
        buf.set_style(Rect::new(area.x, area.y, in_bounds.start, area.height), oob_style);
    }
    if in_bounds.end < area.width {
        buf.set_style(
            Rect::new(area.x + in_bounds.end, area.y, area.width - in_bounds.end, area.height),
            oob_style,
        );
    }
}
