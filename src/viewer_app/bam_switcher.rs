use std::path::{Path, PathBuf};

use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    text::Line,
};

use crate::command::protocol::{CommandOutcome, ListMove};
use crate::io::{IoSchedule, IoWorkerHandle};
use crate::jobs::{JobHandle, JobPoll};
use crate::keys::{Keymap, Scope};
use crate::list_cursor::ListCursor;
use crate::popover::{ListTitle, Popover};
use crate::vcf_file::AlternateSampleLookup;

use super::bam_index::bai_path;

pub(super) struct BamSwitcher {
    pub(super) cursor: ListCursor,
    variant: bool,
    choices: Vec<BamChoice>,
    probe: Option<JobHandle<Result<Vec<BamChoice>, String>>>,
}

struct BamChoice {
    path: PathBuf,
    status: &'static str,
}

impl BamChoice {
    fn check(path: PathBuf) -> Self {
        let status = match (path.is_file(), bai_path(&path).is_file()) {
            (true, true) => "Ready",
            (true, false) => "No BAI",
            (false, true) => "No BAM",
            (false, false) => "No BAM or BAI",
        };
        Self { path, status }
    }
}

impl BamSwitcher {
    pub(super) fn new(paths: &[PathBuf], active: Option<&Path>, worker: Option<&IoWorkerHandle>) -> Self {
        let selected = paths
            .iter()
            .position(|path| Some(path.as_path()) == active)
            .unwrap_or(0);
        let mut switcher = Self {
            cursor: ListCursor::at(selected),
            variant: false,
            choices: Vec::new(),
            probe: None,
        };
        switcher.refresh(paths, worker);
        switcher
    }

    pub(super) fn for_variant(paths: Vec<PathBuf>, worker: &IoWorkerHandle, samples: AlternateSampleLookup) -> Self {
        let probe = worker.submit_cancellable(IoSchedule::Foreground, move |cancel| {
            let carriers = samples.read().map_err(|error| {
                format!(
                    "Failed to read alternate samples from {}: {error}",
                    samples.path.display()
                )
            })?;
            Ok(paths
                .into_iter()
                .take_while(|_| !cancel.is_cancelled())
                .filter(|path| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| carriers.contains(stem))
                })
                .map(BamChoice::check)
                .collect())
        });
        Self {
            cursor: ListCursor::default(),
            variant: true,
            choices: Vec::new(),
            probe: Some(probe),
        }
    }

    pub(super) fn refresh(&mut self, paths: &[PathBuf], worker: Option<&IoWorkerHandle>) {
        let status = if worker.is_some() { "Checking…" } else { "Unknown" };
        self.choices.extend(
            paths[self.choices.len()..]
                .iter()
                .cloned()
                .map(|path| BamChoice { path, status }),
        );
        self.probe = worker.filter(|_| !paths.is_empty()).map(|worker| {
            let paths = paths.to_vec();
            worker.submit_cancellable(IoSchedule::Fifo, move |cancel| {
                Ok(paths
                    .into_iter()
                    .take_while(|_| !cancel.is_cancelled())
                    .map(BamChoice::check)
                    .collect())
            })
        });
    }

    pub(super) fn tick(&mut self, active: Option<&Path>) -> Result<bool, String> {
        let Some(probe) = &mut self.probe else {
            return Ok(false);
        };
        let result = probe.try_recv();
        if matches!(result, JobPoll::Empty) {
            return Ok(true);
        }
        self.probe = None;
        match result {
            JobPoll::Ready(result) => {
                self.choices = result?;
                if self.variant {
                    let selected = self
                        .choices
                        .iter()
                        .position(|choice| Some(choice.path.as_path()) == active);
                    self.cursor = ListCursor::at(selected.unwrap_or(0));
                }
            }
            _ if self.variant => return Err("BAM sample lookup interrupted".to_string()),
            _ => self.choices.iter_mut().for_each(|choice| choice.status = "Unknown"),
        }
        Ok(true)
    }

    pub(super) fn is_checking(&self) -> bool {
        self.probe.is_some()
    }

    pub(super) fn down(&mut self) {
        self.cursor.down(self.choices.len());
    }

    pub(super) fn selected(&self) -> Option<&Path> {
        self.choices
            .get(self.cursor.selected())
            .map(|choice| choice.path.as_path())
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        keymap: &Keymap,
        active: Option<&Path>,
    ) -> Option<Rect> {
        let height = (self.choices.len().max(1) + 2).min(usize::from(area.height)) as u16;
        let outer = area.centered(Constraint::Length(100.min(area.width)), Constraint::Length(height));
        let popover = Popover::from_rect(outer);
        if popover.inner.is_empty() {
            return None;
        }
        let hint = keymap.hints(
            Scope::BamSwitcher,
            [ListMove::Select, ListMove::Close].map(CommandOutcome::List),
            "  ",
        );
        let title = if self.variant { "Variant BAMs" } else { "BAM files" };
        if self.choices.is_empty() {
            let message = if !self.variant {
                " No BAMs added"
            } else if self.is_checking() {
                " Looking up alternate samples…"
            } else {
                " No BAMs match the alternate samples"
            };
            popover.draw_content(frame, Some(title), &hint, |frame, area| {
                frame.render_widget(Line::from(message), area);
            });
            return Some(outer);
        }

        popover.draw_list(
            frame,
            &mut self.cursor,
            self.choices.len(),
            ListTitle::WithRange(title),
            &hint,
            |index, _| {
                let BamChoice { path, status } = &self.choices[index];
                let marker = if active == Some(path.as_path()) { '*' } else { ' ' };
                let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                let mut row = format!(" {marker} {status:<13} {name}");
                if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                    row.push_str(&format!("  ({})", parent.display()));
                }
                Line::from(row)
            },
        );
        Some(outer)
    }
}
