//! Connects `GenomeViewer` to the app-shell contracts.
//!
//! Command outcomes are applied only to genome-owned state; unsupported outcomes return
//! `Applied::NotApplicable`. The `ViewerApp` implementation composes the frame, advertises active scopes,
//! handles supplemental mouse input, polls viewer-owned background work, forwards notifications and
//! search state, and reports status to the shell.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Position as CellPosition, Rect},
};

use crate::Instant;
use crate::basepair::BasePair;
use crate::command::protocol::{
    Applied, CommandOutcome, CommandTarget, Direction, GenomeCommand, ListMove, ScrollStep, TableCommand,
};
use crate::input::{MouseEvent, MouseKind};
use crate::keys::{Keymap, Scope};
use crate::locus::FeatureFilter;
use crate::notification::Notification;
use crate::search::{MatchTarget, ResolvedMatch};
use crate::setting::SettingValue;
use crate::viewer::{BackgroundWork, MatchRow, StatusProvider, ViewerApp};

use super::contig_selector::ContigSelectorState;
use super::depth::ViewportStats;
use super::divider::{DividerGlyphs, draw_pileup_divider};
use super::geometry::AreaSpan;
use super::reference_data::ReferenceLookup;
use super::track_picker::TrackPicker;
use super::viewerjobs::TrackedJob;
use super::{BamSwitcher, GenomePopover, GenomeViewer, legend, local_area, minimap};

const MOUSE_REPEAT_DELAY: Duration = Duration::from_millis(350);
const MOUSE_REPEAT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
pub(super) enum MouseGesture {
    Chromosome,
    LocalArea {
        cell: CellPosition,
        last_step: Instant,
        delay: Duration,
    },
    Pileup(CellPosition),
}

/// Keep the displayed span through a batch of input events since each jump moves the local area.
pub(super) struct NavigationRegions {
    minimap: Rect,
    local_area: Option<(Rect, AreaSpan)>,
    legend: Option<Rect>,
    pileup: Rect,
    read_color: Option<Rect>,
}

impl NavigationRegions {
    fn chromosome_target(&self, cell: CellPosition, contig_length: BasePair) -> Option<BasePair> {
        if !self.minimap.contains(cell) {
            return None;
        }
        minimap::position_at_column(cell.x - self.minimap.x, self.minimap.width, contig_length)
    }

    fn local_target(&self, cell: CellPosition) -> Option<BasePair> {
        let (area, span) = self.local_area?;
        if !area.contains(cell)
            || span.width() == BasePair(0)
            || self.legend.is_some_and(|legend| legend.contains(cell))
        {
            return None;
        }
        let (start, end) = local_area::column_span(span, cell.x - area.x, area.width);
        Some(start + (end - start) / 2)
    }
}

impl GenomeViewer {
    fn local_mouse_repeat(&self) -> Option<(CellPosition, Instant, BasePair)> {
        let MouseGesture::LocalArea { cell, last_step, delay } = self.mouse_gesture? else {
            return None;
        };
        let target = self.navigation.as_ref()?.local_target(cell)?;
        Some((cell, last_step + delay, target))
    }

    pub(super) fn tick_mouse_navigation(&mut self, now: Instant) -> bool {
        let Some((cell, repeat_at, target)) = self.local_mouse_repeat() else {
            return false;
        };
        if now < repeat_at {
            return false;
        }
        self.mouse_gesture = Some(MouseGesture::LocalArea {
            cell,
            last_step: now,
            delay: MOUSE_REPEAT_INTERVAL,
        });
        let before = self.centre.value();
        self.jump_horizontal(target);
        self.centre.value() != before
    }
}

impl CommandTarget for GenomeViewer {
    /// Applies a resolved command outcome.
    fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
        match outcome {
            CommandOutcome::Genome(command) => self.apply_genome(command),
            CommandOutcome::List(movement) => {
                match (&mut self.popover, movement) {
                    (None, _) => return Applied::NotApplicable,
                    (Some(_), ListMove::Close) => self.popover = None,
                    (Some(GenomePopover::Contigs(selector)), ListMove::Up) => selector.up(),
                    (Some(GenomePopover::Contigs(selector)), ListMove::Down) => selector.down(),
                    (Some(GenomePopover::Contigs(selector)), ListMove::Select) => {
                        let selected = selector.selected_contig().to_string();
                        self.switch_active_contig(selected);
                        self.popover = None;
                    }
                    (Some(GenomePopover::Tracks(picker)), ListMove::Up) => picker.up(),
                    (Some(GenomePopover::Tracks(picker)), ListMove::Down) => picker.down(),
                    (Some(GenomePopover::Tracks(picker)), ListMove::Select) => picker.toggle(&mut self.settings),
                    (Some(GenomePopover::Bams(switcher)), ListMove::Up) => switcher.cursor.up(),
                    (Some(GenomePopover::Bams(switcher)), ListMove::Down) => switcher.down(),
                    (Some(GenomePopover::Bams(switcher)), ListMove::Select) => {
                        if let Some(path) = switcher.selected().map(std::path::Path::to_path_buf) {
                            self.popover = None;
                            return match self.switch_bam(path, None) {
                                Ok(()) => Applied::Ok,
                                Err(message) => Applied::Err(message),
                            };
                        }
                    }
                }
                Applied::Ok
            }
            CommandOutcome::Scroll { direction, step } => {
                let distance = match step {
                    ScrollStep::Near => self.settings.movemin,
                    ScrollStep::NearX10 => self.settings.movemin * BasePair(10),
                    ScrollStep::Mid => self.settings.movemid,
                    ScrollStep::Far => self.settings.movefar,
                };
                match direction {
                    Direction::Left => self.nudge_horizontal(distance, false),
                    Direction::Right => self.nudge_horizontal(distance, true),
                    Direction::Up => {
                        self.track_align.scroll_vertical(distance, false);
                    }
                    Direction::Down => {
                        if self.track_align.scroll_vertical(distance, true) {
                            self.notify.warn("Reached maximum pileup scroll depth");
                        }
                    }
                }
                Applied::Ok
            }
            // `line` is the VCF table's reading of the same text; a bp axis has
            // no use for it.
            CommandOutcome::Goto {
                horizontal, vertical, ..
            } => {
                if let Some(h) = horizontal {
                    self.jump_horizontal(h);
                }
                if let Some(v) = vertical
                    && self.track_align.jump_vertical(v)
                {
                    self.notify.warn("Reached maximum pileup scroll depth");
                }
                Applied::Ok
            }
            CommandOutcome::JumpToPeer => Applied::in_tab(
                "VCFViewer",
                [TableCommand::GotoLocus {
                    contig: self.active_contig.clone(),
                    position: self.centre.value(),
                }
                .into()],
            ),
            CommandOutcome::Setting(value) => {
                if self.settings.set(value) {
                    Applied::Ok
                } else {
                    Applied::NotApplicable
                }
            }
            CommandOutcome::Shell(_) | CommandOutcome::Table(_) => Applied::NotApplicable,
            // The router consumes this before dispatch, but direct callers still get a silent no-op.
            CommandOutcome::None => Applied::Ok,
        }
    }
}

impl GenomeViewer {
    fn open_popover(&mut self, popover: GenomePopover) {
        self.cancel_mouse_gesture();
        self.navigation = None;
        self.popover = Some(popover);
        self.popover_bounds = None;
    }

    fn apply_genome(&mut self, command: GenomeCommand) -> Applied {
        match command {
            GenomeCommand::ActiveContig(new_contig) => {
                let resolved = match self.resolve_contig(&new_contig) {
                    Ok(resolved) => resolved,
                    Err(message) => return Applied::Err(message),
                };
                self.switch_active_contig(resolved);
                Applied::Ok
            }
            GenomeCommand::OpenContigSelector => {
                let selector = ContigSelectorState::new(&self.ref_genome.index, &self.active_contig);
                self.open_popover(GenomePopover::Contigs(selector));
                Applied::Ok
            }
            GenomeCommand::OpenTrackPicker => {
                self.open_popover(GenomePopover::Tracks(TrackPicker::default()));
                Applied::Ok
            }
            GenomeCommand::CycleReadColor => {
                self.settings
                    .set(SettingValue::Readcolor(self.settings.readcolor.next()));
                Applied::Ok
            }
            GenomeCommand::OpenBamSwitcher => {
                let switcher = BamSwitcher::new(&self.bam_paths, self.active_bam.as_deref(), self.io_worker.as_ref());
                self.open_popover(GenomePopover::Bams(switcher));
                Applied::Ok
            }
            GenomeCommand::ChooseVariantBam {
                contig,
                position,
                samples,
            } => {
                if self.bam_paths.len() > 1 && self.io_worker.is_some() {
                    Applied::FollowUps(vec![
                        TableCommand::OpenVariantBams {
                            contig,
                            position,
                            samples,
                            paths: self.bam_paths.clone(),
                            active: self.active_bam.clone(),
                        }
                        .into(),
                    ])
                } else {
                    Applied::in_tab(
                        "GenomeViewer",
                        [GenomeCommand::JumpToVariant {
                            contig,
                            position,
                            bam: None,
                        }
                        .into()],
                    )
                }
            }
            GenomeCommand::JumpToVariant { contig, position, bam } => {
                let result = match bam {
                    Some(path) => self.switch_bam(path, Some((contig, position))),
                    None => self.jump_to_locus(&contig, position),
                };
                match result {
                    Ok(()) => Applied::Ok,
                    Err(message) => Applied::Err(message),
                }
            }
            GenomeCommand::StepZoom { into } => {
                self.step_lod(!into);
                Applied::Ok
            }
            GenomeCommand::Zoom {
                horizontal_to_mid,
                alignment_v_to_mid,
            } => {
                self.apply_zoom(horizontal_to_mid, alignment_v_to_mid);
                Applied::Ok
            }
            GenomeCommand::Load {
                gtf_path,
                cache_path,
                build_missing,
            } => match self.start_load(gtf_path, cache_path, build_missing) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            GenomeCommand::ActiveFasta(path) => match self.set_reference(&path) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            GenomeCommand::ActiveBam(path) => match self.load_active_bam(path) {
                Ok(()) => Applied::Ok,
                Err(message) => Applied::Err(message),
            },
            GenomeCommand::AddBam(path) => {
                self.add_bam(path);
                Applied::Ok
            }
            GenomeCommand::JumpToFeature { forward } => {
                self.jump_to_feature(forward);
                Applied::Ok
            }
            GenomeCommand::FeatureFilter(edit) => {
                self.apply_feature_filter_edit(edit);
                Applied::Ok
            }
        }
    }
}

impl StatusProvider for GenomeViewer {
    fn status_override(&self) -> Option<String> {
        self.status_text()
    }

    fn drain_notifications(&mut self) -> Vec<Notification> {
        self.notify.drain()
    }

    fn background_work(&self) -> Option<BackgroundWork> {
        let started_at = self.tracked_jobs.iter().map(TrackedJob::started_at).min()?;
        let tasks = self.tracked_jobs.iter().map(|job| job.label().to_string()).collect();
        Some(BackgroundWork { tasks, started_at })
    }
}

impl ViewerApp for GenomeViewer {
    fn title(&self) -> &'static str {
        "GenomeViewer"
    }

    fn display_name(&self) -> &'static str {
        "Genome"
    }

    fn setting_values(&self) -> Vec<SettingValue> {
        self.settings.values()
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, keymap: &Keymap) {
        let panes = Self::layout(area, &self.settings);
        let contig = self.contig();
        let bam_name = self
            .active_bam
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy());
        self.track_contig_grid.draw(
            frame,
            panes.contig_grid,
            self.track_view(panes.contig_grid, &contig, &self.settings),
            bam_name.as_deref(),
        );
        self.last_filtered_count = if self.settings.featurestrack {
            self.track_features.draw_features(
                frame,
                panes.features,
                self.track_view(panes.features, &contig, &self.settings),
                self.locus_index.active(),
                &self.settings.feature_filter,
            )
        } else {
            0
        };
        if self.settings.chromosometrack {
            self.track_minimap.draw(
                frame,
                panes.minimap,
                self.track_view(panes.minimap, &contig, &self.settings),
            );
            self.track_minimap.draw_divider(
                frame,
                panes.minimap_divider,
                self.track_view(panes.minimap_divider, &contig, &self.settings),
                &self.ref_genome.name,
            );
        }

        // The alignment draw requests the windows shared by the coverage bars and depth readouts.
        let pileup_view = self.track_view(panes.alignment, &contig, &self.settings);
        let centreline_read_spans =
            self.track_align
                .draw(frame, panes.alignment, pileup_view, &mut self.reference_data);
        let coverage = self.track_align.coverage_at(&contig.name, pileup_view.position.centre);
        let bp_per_column = self.settings.horizontal_zoom.bp_per_column();
        let stats =
            coverage.map(|coverage| ViewportStats::over(pileup_view.position, contig.length, bp_per_column, coverage));
        let mean = stats.map(|s| s.mean);
        let glyphs = DividerGlyphs::for_settings(&self.settings);
        if self.settings.coveragetrack {
            self.track_coverage.draw(
                frame,
                panes.coverage,
                pileup_view,
                coverage,
                ReferenceLookup::new(&mut self.reference_data, &contig),
                stats.map_or(0, |s| s.max),
            );
            draw_pileup_divider(
                frame.buffer_mut(),
                panes.coverage_divider,
                "Pileup coverage",
                pileup_view.position,
                mean,
                glyphs,
            );
        }

        draw_pileup_divider(
            frame.buffer_mut(),
            panes.pileup_divider,
            "Pileup span",
            pileup_view.position,
            mean,
            glyphs,
        );
        let read_color = if panes.reference.is_empty() {
            None
        } else {
            self.track_ref.draw(
                frame,
                panes.reference,
                self.track_view(panes.reference, &contig, &self.settings),
                &mut self.reference_data,
                stats.and_then(|s| s.point),
            )
        };

        let local_area = if self.settings.localareatrack {
            let span = self.track_local_area.draw(
                frame,
                panes.local_area,
                self.track_view(panes.local_area, &contig, &self.settings),
                &mut self.reference_data,
                &centreline_read_spans,
            );
            self.track_local_area
                .draw_divider(frame, panes.local_divider, span, &self.settings);
            Some((panes.local_area, span))
        } else {
            None
        };
        if !panes.local_area_features.is_empty() {
            self.track_features.draw_local_area_features(
                frame,
                panes.local_area_features,
                self.track_view(panes.local_area_features, &contig, &self.settings),
                self.locus_index.active(),
                &self.settings.feature_filter,
            );
        }
        if !panes.coordinate.is_empty() {
            self.track_coord.draw(
                frame,
                panes.coordinate,
                self.track_view(panes.coordinate, &contig, &self.settings),
            );
        }

        let legend = if self.settings.colorlegend {
            legend::draw(frame, panes.local_area, self.settings.readcolor)
        } else {
            None
        };

        self.navigation = self.popover.is_none().then_some(NavigationRegions {
            minimap: minimap::bar_area(panes.minimap),
            local_area,
            legend,
            pileup: panes.alignment,
            read_color,
        });
        self.popover_bounds = match &mut self.popover {
            Some(GenomePopover::Contigs(selector)) => selector.draw(frame, area, &self.active_contig),
            Some(GenomePopover::Tracks(picker)) => picker.draw(frame, area, keymap, &self.settings),
            Some(GenomePopover::Bams(switcher)) => switcher.draw(frame, area, keymap, self.active_bam.as_deref()),
            None => None,
        };
    }

    fn scopes(&self) -> Vec<Scope> {
        match self.popover {
            Some(GenomePopover::Contigs(_)) => vec![Scope::ContigSelector, Scope::Genome],
            Some(GenomePopover::Tracks(_)) => vec![Scope::TrackPicker, Scope::Genome],
            Some(GenomePopover::Bams(_)) => vec![Scope::BamSwitcher, Scope::Genome],
            None => vec![Scope::Genome],
        }
    }

    /// Handles track picker input and mouse navigation.
    fn on_mouse(&mut self, mouse: MouseEvent) -> Option<CommandOutcome> {
        if mouse.kind != MouseKind::LeftDrag {
            self.cancel_mouse_gesture();
        }
        let cell = mouse.position();
        if let Some(popover) = &mut self.popover {
            if mouse.kind == MouseKind::LeftDown && self.popover_bounds.is_none_or(|bounds| !bounds.contains(cell)) {
                return Some(CommandOutcome::List(ListMove::Close));
            }
            if let GenomePopover::Tracks(picker) = popover {
                picker.on_mouse(mouse, &mut self.settings);
            }
            return None;
        }
        if matches!(mouse.kind, MouseKind::Moved | MouseKind::LeftUp) {
            return None;
        }
        let navigation = self.navigation.as_ref()?;
        if mouse.kind == MouseKind::LeftDrag {
            match &mut self.mouse_gesture {
                Some(MouseGesture::Chromosome) => {
                    if let Some(target) = navigation.chromosome_target(cell, self.contig_length()) {
                        self.jump_horizontal(target);
                    }
                }
                Some(MouseGesture::LocalArea {
                    cell: previous, delay, ..
                }) => {
                    if cell.x != previous.x {
                        *delay = MOUSE_REPEAT_INTERVAL;
                    }
                    *previous = cell;
                }
                Some(MouseGesture::Pileup(previous)) => {
                    let last = std::mem::replace(previous, cell);
                    if cell.y != last.y {
                        let rows = BasePair::from(cell.y.abs_diff(last.y));
                        self.track_align.scroll_vertical(rows, cell.y < last.y);
                    }
                    if cell.x != last.x {
                        let columns = BasePair::from(cell.x.abs_diff(last.x));
                        self.nudge_horizontal(columns, cell.x < last.x);
                    }
                }
                None => {}
            }
            return None;
        }
        if navigation.read_color.is_some_and(|button| button.contains(cell)) {
            return Some(GenomeCommand::CycleReadColor.into());
        }
        if let Some(contig) = self.track_contig_grid.hit_test(cell.x, cell.y) {
            self.switch_active_contig(contig);
        } else if let Some(target) = navigation.chromosome_target(cell, self.contig_length()) {
            self.mouse_gesture = Some(MouseGesture::Chromosome);
            self.jump_horizontal(target);
        } else if let Some(target) = navigation.local_target(cell) {
            self.mouse_gesture = Some(MouseGesture::LocalArea {
                cell,
                last_step: Instant::now(),
                delay: MOUSE_REPEAT_DELAY,
            });
            self.jump_horizontal(target);
        } else if navigation.pileup.contains(cell) {
            self.mouse_gesture = Some(MouseGesture::Pileup(cell));
        }
        None
    }

    fn cancel_mouse_gesture(&mut self) {
        self.mouse_gesture = None;
    }

    fn mouse_repeat_deadline(&self) -> Option<Instant> {
        self.local_mouse_repeat().map(|(_, repeat_at, _)| repeat_at)
    }

    /// Advances background work, returning whether to redraw.
    fn tick(&mut self) -> bool {
        let mut animating = self.tick_mouse_navigation(Instant::now());
        animating |= self.poll_bam_load();
        if let Some(GenomePopover::Bams(switcher)) = &mut self.popover {
            match switcher.tick(self.active_bam.as_deref()) {
                Ok(checking) => animating |= checking,
                Err(message) => {
                    self.notify.error(message);
                    self.popover = None;
                }
            }
        }
        animating |= self.track_minimap.tick();

        animating |= self.track_local_area.tick(self.settings.localareatrack);
        animating |= self.track_align.tick();

        for error in self.track_align.take_errors() {
            self.notify.error(error);
        }
        for error in self.track_local_area.take_errors() {
            self.notify.error(error);
        }
        if let Some(error) = self.search.take_error() {
            self.notify.error(error);
        }

        self.reference_data.poll();
        if self.track_local_area.is_waiting_for_reference() && !self.reference_data.is_fetching() {
            // A waiting grid must resume when the FASTA settles even on a tab that is not drawing.
            let contig = self.contig();
            self.track_local_area
                .resume_after_reference(ReferenceLookup::new(&mut self.reference_data, &contig));
        }
        if let Some(error) = self.reference_data.take_error() {
            self.notify.error(error);
        }

        if self.notify.has_pending() {
            animating = true;
        }

        if self.search.is_converging() {
            animating = true;
        }

        if self.track_align.is_fetching() {
            animating = true;
        }

        if self.track_local_area.is_fetching() {
            animating = true;
        }

        if self.reference_data.is_fetching() {
            animating = true;
        }

        animating |= self.drain_tracked_jobs();

        animating
    }

    fn has_pending_work(&self) -> bool {
        !self.tracked_jobs.is_empty()
            || self.pending_bam.is_some()
            || matches!(&self.popover, Some(GenomePopover::Bams(switcher)) if switcher.is_checking())
            || self.reference_data.is_fetching()
            || self.track_align.is_fetching()
            || self.track_local_area.is_fetching()
            || self.search.is_converging()
            || self.track_minimap.is_animating()
    }

    fn search_tick(&mut self, query: &str) {
        self.search.tick(query, &self.locus_index);
    }

    fn search_matches(&self) -> &[ResolvedMatch] {
        self.search.resolved()
    }

    /// Formats resolved matches, including the empty-index state.
    ///
    /// Only [`MatchTarget::Locus`] can appear: matches are resolved by the corpus
    /// that produced them, and this viewer's corpus is its own `locus_index`.
    fn format_matches(&self, resolved: &[ResolvedMatch]) -> Vec<MatchRow> {
        if self.locus_index.active().is_none() {
            return vec![MatchRow {
                text: "No loci loaded — use :load first".to_string(),
                bold_char_indices: Vec::new(),
            }];
        }
        resolved
            .iter()
            .filter_map(|r| {
                let MatchTarget::Locus { contig, start, end, .. } = &r.destination else {
                    return None;
                };
                Some(MatchRow {
                    text: format!(
                        "{}  {}:{}-{}",
                        r.key,
                        contig,
                        start.format_with_commas(),
                        end.format_with_commas()
                    ),
                    bold_char_indices: r.bold_char_indices.clone(),
                })
            })
            .collect()
    }

    /// Jumps by narrowing `feature_filter` to the matched transcript rather than through a separate
    /// single-transcript field, so the filter stays sticky until the next search or `:set
    /// featurefilter`. A real jump returns `None`: the moved view is confirmation enough.
    fn search_enter(&mut self, selected: &ResolvedMatch) -> Option<String> {
        let MatchTarget::Locus {
            contig,
            start,
            transcript_id,
            ..
        } = &selected.destination
        else {
            return Some("that match does not name a locus".to_string());
        };
        if self.ref_genome.index.length(contig).is_none() {
            return Some(format!("{contig} is not in the loaded reference"));
        }
        let (start, transcript_id) = (*start, transcript_id.clone());
        if *contig != self.active_contig {
            self.switch_active_contig(contig.clone());
        }
        self.settings.feature_filter = FeatureFilter {
            include: vec![transcript_id],
            exclude: vec![],
        };
        self.jump_horizontal(start);
        None
    }
}
