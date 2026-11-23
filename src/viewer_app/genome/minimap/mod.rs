//! The genome viewer's whole-contig position track.
//!
//! The bar owns the track pane's fixed row; its contig name and span belong to the separate
//! divider pane above it. This split keeps the readout as a heading for the thing it describes
//! while leaving each renderer responsible for its own full-width pane.
//!
//! The contig is drawn at a fixed whole-contig scale, so the propagated `TrackView` position is
//! used only to place the indicator. `MinimapTrack` deliberately stores no position or contig:
//! the shell owns both, while the track keeps only its reference-derived scale and animation state.
//! This is the track form of invariant 6 and the one-way position contract in invariant 10.
//!
//! A contig-length change observed during `draw` starts the local count-up; `tick` keeps frames
//! arriving through the animation and requests one final settled redraw. The main trap is treating
//! that animation state as scroll state, or deriving the divider's number separately from the bar:
//! both panes must use the same warped progress or the label and fill drift apart.

mod widget;

use std::time::Duration;

use crate::Instant;

use ratatui::{Frame, layout::Rect, style::Style};

use crate::basepair::BasePair;
use crate::refgenome::IndexedFasta;
use crate::style::Surface;
#[cfg(test)]
use crate::viewer_app::genome::settings::GenomeViewerSettings;
#[cfg(test)]
use crate::viewer_app::viewport::ViewPosition;

use super::divider::{DividerGlyphs, draw_divider, layout_divider};
#[cfg(test)]
use super::track::Contig;
use super::track::TrackView;

/// Fixed height: the bar alone. Its readout is the divider pane above it.
pub const MINIMAP_TRACK_HEIGHT: u16 = 1;

/// One row above the bar, naming the contig and its length.
pub const MINIMAP_DIVIDER_HEIGHT: u16 = 1;

/// Unused cell at each end of the bar.
const MINIMAP_END_GAP: u16 = 1;

pub(super) fn bar_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(MINIMAP_END_GAP),
        width: area.width.saturating_sub(2 * MINIMAP_END_GAP),
        height: area.height.min(MINIMAP_TRACK_HEIGHT),
        ..area
    }
}

/// Returns the approximate base for a column inside the bar.
pub(super) fn position_at_column(column: u16, width: u16, contig_length: BasePair) -> Option<BasePair> {
    let last = contig_length.checked_sub(BasePair(1))?;
    if width == 1 {
        return Some(last / 2);
    }
    let offset = u128::from(last.as_u64()) * u128::from(column) / u128::from(width - 1);
    Some(BasePair(offset as u64))
}

/// How long the end-label count-up (and the bar fill that tracks it) takes to
/// settle after a contig switch.
const LENGTH_COUNTUP_DURATION: Duration = Duration::from_millis(500);

/// Start time for the end-label count-up.
#[derive(Debug, Clone, Copy)]
struct LengthCountup {
    start: Instant,
}

impl LengthCountup {
    fn fraction(&self) -> f64 {
        let elapsed = self.start.elapsed();
        if elapsed >= LENGTH_COUNTUP_DURATION {
            return 1.0;
        }
        elapsed.as_secs_f64() / LENGTH_COUNTUP_DURATION.as_secs_f64()
    }

    fn finished(&self) -> bool {
        self.start.elapsed() >= LENGTH_COUNTUP_DURATION
    }
}

/// Whole-contig position bar without its own scroll state.
pub struct MinimapTrack {
    /// Last length seen, used only to restart the animation.
    last_contig_length: Option<BasePair>,
    /// Longest contig used to scale the fill animation.
    max_contig_length: Option<BasePair>,
    length_countup: Option<LengthCountup>,
}

impl MinimapTrack {
    pub fn new(genome: &IndexedFasta) -> Self {
        Self {
            last_contig_length: None,
            max_contig_length: longest_contig(genome),
            length_countup: None,
        }
    }

    /// Raw count-up progress, or `1.0` when settled.
    fn fill_fraction(&self) -> f64 {
        self.length_countup.map_or(1.0, |c| c.fraction())
    }
}

/// Drops a leading `chr`, so `chr1` and a bare `1` read the same in the label.
///
/// The same tolerance `resolve_contig` applies to incoming names, applied here to outgoing
/// text — not a name-mapping layer, just the prefix this one label does not want to repeat.
fn strip_chr_prefix(name: &str) -> &str {
    name.get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("chr"))
        .and_then(|_| name.get(3..))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(name)
}

fn longest_contig(genome: &IndexedFasta) -> Option<BasePair> {
    genome
        .index
        .contig_names()
        .filter_map(|n| genome.index.length(&n))
        .max()
}

impl MinimapTrack {
    /// Draws the whole contig with the current centre as its indicator.
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, view: TrackView<'_>) {
        let TrackView { position, contig, .. } = view;
        // Before the zero-height return, so a collapsed frame can't swallow a contig switch.
        if self.last_contig_length != Some(contig.length) {
            self.length_countup = Some(LengthCountup { start: Instant::now() });
            self.last_contig_length = Some(contig.length);
        }
        if area.height == 0 {
            return;
        }
        frame
            .buffer_mut()
            .set_style(area, Style::default().bg(Surface::TRACK_BG()));

        // The bar owns the pane's first row; the readout is the divider pane above it.
        widget::draw(
            frame.buffer_mut(),
            bar_area(area),
            Some(position.centre),
            Some(contig.length),
            self.max_contig_length,
            self.fill_fraction(),
        );
    }

    /// Names the contig and its length in the divider pane above the bar.
    ///
    /// The length counts up with the bar's fill using the same warped fraction. The coordinates
    /// name the contig's bounds throughout the animation.
    pub(super) fn draw_divider(&self, frame: &mut Frame, area: Rect, view: TrackView<'_>, reference_name: &str) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let length = view.contig.length;
        let warped = widget::warp_fill_fraction(self.fill_fraction(), Some(length), self.max_contig_length);
        let counted = BasePair((length.as_f64() * warped).round() as u64);
        let label = format!(
            "Chromosome {} of {reference_name}: {}bp",
            strip_chr_prefix(&view.contig.name),
            counted.format_with_commas()
        );
        let row = layout_divider(
            area.width,
            &label,
            None,
            BasePair(0)..length,
            DividerGlyphs::for_settings(view.settings),
        );
        draw_divider(frame.buffer_mut(), area, &row);
    }

    /// Whether the contig-length count-up animation is active.
    pub(super) fn is_animating(&self) -> bool {
        self.length_countup.is_some()
    }

    /// Advances the count-up; completion also returns true for one final settled draw.
    pub(super) fn tick(&mut self) -> bool {
        match self.length_countup {
            Some(countup) if countup.finished() => {
                self.length_countup = None;
                true
            }
            Some(_) => true,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> MinimapTrack {
        MinimapTrack::new(&IndexedFasta::hs1_fallback())
    }

    fn contig(length: u64) -> Contig {
        Contig {
            name: "chr1".to_string(),
            length: BasePair(length),
        }
    }

    fn settings() -> GenomeViewerSettings {
        GenomeViewerSettings::new()
    }

    fn draw_once(track: &mut MinimapTrack, contig: &Contig) {
        use ratatui::{Terminal, backend::TestBackend};
        let settings = settings();
        let area = Rect::new(0, 0, 40, MINIMAP_TRACK_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(40, MINIMAP_TRACK_HEIGHT)).expect("backend");
        terminal
            .draw(|frame| {
                track.draw(
                    frame,
                    area,
                    TrackView {
                        position: ViewPosition::new(BasePair(0), BasePair(80)),
                        settings: &settings,
                        contig,
                    },
                )
            })
            .expect("draw");
    }

    #[test]
    fn countup_starts_only_when_the_contig_length_changes() {
        let mut track = track();
        assert!(!track.tick(), "no animation before anything is drawn");

        draw_once(&mut track, &contig(248_387_328));
        assert!(track.tick(), "a contig switch should start an animation");
        draw_once(&mut track, &contig(1_000));
        let past = Instant::now() - LENGTH_COUNTUP_DURATION - Duration::from_millis(1);
        track.length_countup = Some(LengthCountup { start: past });
        draw_once(&mut track, &contig(1_000));
        assert_eq!(track.length_countup.expect("same length keeps countup").start, past);

        draw_once(&mut track, &contig(2_000));
        let restarted = track.length_countup.expect("new length restarts countup").start;
        assert!(restarted > past);
    }

    #[test]
    fn the_settling_tick_requests_one_final_redraw() {
        let mut track = track();
        draw_once(&mut track, &contig(1_000));
        track.length_countup = Some(LengthCountup {
            start: Instant::now() - LENGTH_COUNTUP_DURATION - Duration::from_millis(1),
        });

        assert!(track.tick(), "the tick that finishes the count-up still needs a redraw");
        assert!(
            !track.tick(),
            "the completed countup should be idle after its final redraw"
        );
    }

    #[test]
    fn the_indicator_moves_but_the_bar_does_not_scroll() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let settings = settings();
        let bar_styles = |centre: u64| {
            let mut track = track();
            let contig = contig(1_000);
            track.last_contig_length = Some(contig.length);
            let mut terminal = Terminal::new(TestBackend::new(40, MINIMAP_TRACK_HEIGHT)).expect("backend");
            let area = Rect::new(0, 0, 40, MINIMAP_TRACK_HEIGHT);
            terminal
                .draw(|frame| {
                    track.draw(
                        frame,
                        area,
                        TrackView {
                            position: ViewPosition::new(BasePair(centre), BasePair(80)),
                            settings: &settings,
                            contig: &contig,
                        },
                    )
                })
                .expect("draw");
            let buf = terminal.backend().buffer().clone();
            (0..40)
                .map(|x| {
                    let cell = buf.cell((x, area.height - 1)).unwrap();
                    (cell.fg, cell.bg)
                })
                .collect::<Vec<_>>()
        };

        let near_start = bar_styles(0);
        let near_end = bar_styles(1_000);
        let changed_cells: Vec<usize> = near_start
            .iter()
            .zip(&near_end)
            .enumerate()
            .filter_map(|(index, (start, end))| (start != end).then_some(index))
            .collect();
        assert_eq!(changed_cells, [1, 38], "only the endpoint indicator cells should move");
    }
}
