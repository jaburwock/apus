//! Owns the genome viewer's coordinate ruler pane: banding, ticks, labels, and the centreline.
//!
//! `GenomeViewer` gives this stateless track the shell-owned `TrackView` on every frame. The track
//! therefore derives its visible coordinate space from the current position, horizontal zoom,
//! settings, and active contig rather than retaining scroll state of its own.
//!
//! The pane is laid out separately below the reference track because coordinate annotations describe
//! the shared genomic axis, not alignment content. Its fixed row ownership is part of the layout
//! contract; it must not become an inset or a computed margin inside another track.
//!
//! `marks` owns the coordinate geometry and painting helpers. The track establishes the coordinate
//! background first, then applies enabled layers in dependency order, with the centreline painted
//! last. Banding, ticks, and group prefixes suppress content past the contig end. The centreline is
//! resolved from the view position independently of contig-length filtering. Leading underflow is
//! represented through the same full-width coordinate space used by the other genome tracks.

pub(in crate::viewer_app::genome) mod marks;

use ratatui::{Frame, layout::Rect};

#[cfg(test)]
use crate::viewer_app::genome::settings::GenomeViewerSettings;
#[cfg(test)]
use crate::viewer_app::viewport::ViewPosition;

use super::geometry::in_bounds_column_range;
#[cfg(test)]
use super::track::Contig;
use super::track::{TrackView, paint_track_background};
use marks::{draw_centreline, draw_group_prefix, draw_ruler_banding, draw_ticklabels, draw_tickmarks};

/// Fixed height: ruler on row 0 and ticks on row 1.
pub const COORDINATE_TRACK_HEIGHT: u16 = 2;

/// Stateless horizontal coordinate ruler.
#[derive(Default)]
pub struct CoordinateTrack;

impl CoordinateTrack {
    pub fn new() -> Self {
        Self
    }
}

impl CoordinateTrack {
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, view: TrackView<'_>) {
        let TrackView {
            position,
            settings,
            contig,
        } = view;
        let bp_per_column = settings.horizontal_zoom.bp_per_column();
        let in_bounds = in_bounds_column_range(position, contig.length, area.width, bp_per_column);
        paint_track_background(frame.buffer_mut(), area, in_bounds);

        if settings.ruler {
            draw_ruler_banding(frame, area, position, contig.length, bp_per_column);
        }
        if settings.rulermarks {
            draw_tickmarks(frame, area, position, bp_per_column, contig.length);
        }
        if settings.rulernumbers {
            draw_ticklabels(frame, area, position, bp_per_column, contig.length);
            draw_group_prefix(frame, area, position, bp_per_column, contig.length);
        }
        if settings.centnumber {
            draw_centreline(frame, area, position, bp_per_column);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basepair::BasePair;
    use crate::viewer_app::genome::zoom::HorizontalZoom;
    use ratatui::{Terminal, backend::TestBackend};

    fn far_contig() -> Contig {
        Contig {
            name: "chr1".to_string(),
            length: BasePair(1_000_000),
        }
    }

    fn settings() -> GenomeViewerSettings {
        GenomeViewerSettings::new()
    }

    #[test]
    fn ruler_digits_and_marks_stop_at_contig_length() {
        use crate::style::Chrome;

        let _palette = crate::style::palette::lock_for_test();
        let (w, h) = (80u16, COORDINATE_TRACK_HEIGHT);
        let area = Rect::new(0, 0, w, h);
        let contig = Contig {
            name: "chr1".to_string(),
            length: BasePair(50),
        };
        for zoom in [HorizontalZoom::Close, HorizontalZoom::Mid] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
            let mut track = CoordinateTrack::new();
            let mut settings = settings();
            settings.horizontal_zoom = zoom;
            let bp_per_column = zoom.bp_per_column();
            let position = ViewPosition::new(BasePair(45), BasePair::from(w) * BasePair(bp_per_column));
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
                    )
                })
                .expect("draw");

            let buf = terminal.backend().buffer().clone();
            let in_bounds = in_bounds_column_range(position, contig.length, w, bp_per_column);
            assert!(
                in_bounds.end < w,
                "{zoom:?} fixture must expose columns past contig end"
            );
            for row in 0..h {
                for col in (0..in_bounds.start).chain(in_bounds.end..w) {
                    let cell = buf.cell((col, row)).unwrap();
                    assert_eq!(
                        cell.symbol(),
                        " ",
                        "{zoom:?} column {col} row {row} is out of bounds and must stay blank, got {:?}",
                        cell.symbol()
                    );
                    assert_eq!(
                        cell.bg,
                        Chrome::OUT_OF_BOUNDS_BG(),
                        "{zoom:?} column {col} row {row} must use OUT_OF_BOUNDS_BG"
                    );
                }
            }
        }
    }

    #[test]
    fn ruler_banding_stops_before_genomic_zero_and_ticks_shift_with_content() {
        let _palette = crate::style::palette::lock_for_test();
        let (w, h) = (80u16, COORDINATE_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
        let area = Rect::new(0, 0, w, h);
        let mut track = CoordinateTrack::new();
        let mut settings = settings();
        settings.horizontal_zoom = HorizontalZoom::Close;
        let position = ViewPosition::new(BasePair(5), BasePair(80));
        assert_eq!(position.underflow, BasePair(35));
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position,
                        settings: &settings,
                        contig: &far_contig(),
                    },
                )
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        // Internal position 19 (displayed coordinate 20) follows 35 underflow columns.
        assert_ne!(buf.cell((54, 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((19, 1)).unwrap().symbol(), " ");
    }
}
