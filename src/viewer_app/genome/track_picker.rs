//! The Genome track picker reads and writes the same visibility settings as `:set`.

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::Style,
    text::Line,
};

use crate::command::protocol::{CommandOutcome, ListMove};
use crate::input::{MouseEvent, MouseKind};
use crate::keys::{Keymap, Scope};
use crate::list_cursor::ListCursor;
use crate::popover::{ListTitle, Popover, VisibleRows};
use crate::setting::SettingValue;
use crate::style::Text;

use super::settings::GenomeViewerSettings;

const TRACK_COUNT: usize = 9;

#[derive(Default)]
pub(super) struct TrackPicker {
    cursor: ListCursor,
    visible_rows: Option<VisibleRows>,
}

impl TrackPicker {
    /// Mirrors the pane order in `GenomeViewer::layout`. Pileup has no visibility setting.
    fn rows(settings: &GenomeViewerSettings) -> [(&'static str, Option<SettingValue>); TRACK_COUNT] {
        use SettingValue::*;
        [
            ("Contig grid", Some(Contigmarkers(settings.contigmarkers))),
            ("Chromosome", Some(Chromosometrack(settings.chromosometrack))),
            ("Local area", Some(Localareatrack(settings.localareatrack))),
            (
                "Local-area features",
                Some(Localareafeatures(settings.localareafeatures)),
            ),
            ("Coverage", Some(Coveragetrack(settings.coveragetrack))),
            ("Pileup", None),
            ("Reference", Some(Referencetrack(settings.referencetrack))),
            ("Coordinates", Some(Coordinatetrack(settings.coordinatetrack))),
            ("Features", Some(Featurestrack(settings.featurestrack))),
        ]
    }

    pub(super) fn up(&mut self) {
        self.cursor.up();
    }

    pub(super) fn down(&mut self) {
        self.cursor.down(TRACK_COUNT);
    }

    pub(super) fn toggle(&self, settings: &mut GenomeViewerSettings) {
        if let Some(current) = &Self::rows(settings)[self.cursor.selected()].1
            && let Some(enabled) = current.as_bool()
            && let Some(next) = SettingValue::from_bool(current.key(), !enabled)
        {
            settings.set(next);
        }
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, settings: &mut GenomeViewerSettings) {
        if matches!(mouse.kind, MouseKind::Moved | MouseKind::LeftDown)
            && let Some(index) = self.visible_rows.and_then(|rows| rows.index_at(mouse.position()))
        {
            self.cursor.select(index);
            if mouse.kind == MouseKind::LeftDown {
                self.toggle(settings);
            }
        }
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        keymap: &Keymap,
        settings: &GenomeViewerSettings,
    ) -> Option<Rect> {
        self.visible_rows = None;
        let rows = Self::rows(settings);
        let outer = area.centered(
            Constraint::Length(32.min(area.width)),
            Constraint::Length((rows.len() as u16 + 2).min(area.height)),
        );
        let popover = Popover::from_rect(outer);
        if popover.inner.is_empty() {
            return None;
        }

        let hint = keymap.hints(
            Scope::TrackPicker,
            [ListMove::Select, ListMove::Close].map(CommandOutcome::List),
            "  ",
        );
        self.visible_rows = Some(popover.draw_list(
            frame,
            &mut self.cursor,
            rows.len(),
            ListTitle::Plain("Tracks"),
            &hint,
            |index, _| {
                let (name, setting) = &rows[index];
                let checkbox = if setting.as_ref().and_then(SettingValue::as_bool) == Some(false) {
                    "[ ]"
                } else {
                    "[x]"
                };
                let line = Line::from(format!(" {checkbox} {name}"));
                if setting.is_none() {
                    line.style(Style::default().fg(Text::DIMMED_FG()))
                } else {
                    line
                }
            },
        ));
        Some(popover.bounds())
    }
}
