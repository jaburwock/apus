//! Owns `GenomeViewer` state and genome-track composition.
//!
//! The viewer keeps the active reference and contig, shell-owned centre position, settings,
//! search, and notifications, then derives per-pane `TrackView`s during each draw. It owns the
//! fixed pane layout, contig switching, navigation, zoom coordination, divider text, and concrete
//! track instances. `viewerapp.rs` supplies the shell traits; `viewerjobs.rs` supplies asynchronous
//! BAM and GTF loading.
//!
//! Note: roughly 70% of this file is its test module; the viewer and layout code is the remainder.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::basepair::BasePair;
use crate::command::protocol::FeatureFilterEdit;
use crate::locus::FeatureFilter;
use crate::notification::NotificationQueue;
use crate::refgenome::IndexedFasta;
use crate::search::locus::LocusIndex;
use crate::spinner::spinner_glyph;
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::viewport::{Position, ViewPosition};

pub(crate) mod alignment;
mod bam_query;
pub(crate) mod contig_grid;
pub(crate) mod contig_selector;
pub(crate) mod coordinate;
mod coverage;
mod depth;
mod depth_axis;
mod divider;
pub(crate) mod features;
mod geometry;
mod legend;
pub(crate) mod local_area;
pub(crate) mod minimap;
mod mod_5mc;
mod packed_seq;
#[cfg(feature = "profiling")]
pub mod profile;
pub(crate) mod reference;
mod reference_data;
mod ring_buffer_cache;
pub(crate) mod settings;
pub(crate) mod track;
mod track_picker;
pub(super) mod viewerapp;
pub(super) mod viewerjobs;
pub(crate) mod zoom;

use super::bam_switcher::BamSwitcher;
use crate::io::IoWorkerHandle;
use crate::search::state::SearchState;
use alignment::AlignmentTrack;
use contig_grid::{CONTIG_GRID_TRACK_HEIGHT, ContigGridTrack};
use contig_selector::ContigSelectorState;
use coordinate::{COORDINATE_TRACK_HEIGHT, CoordinateTrack};
use coverage::{COVERAGE_TRACK_HEIGHT, CoverageTrack};
use features::{FEATURES_TRACK_HEIGHT, FeaturesTrack, LOCAL_AREA_FEATURES_TRACK_HEIGHT};
use local_area::{LOCAL_AREA_DIVIDER_HEIGHT, LOCAL_AREA_TRACK_HEIGHT, LocalAreaTrack};
use minimap::{MINIMAP_DIVIDER_HEIGHT, MINIMAP_TRACK_HEIGHT, MinimapTrack};
use reference::ReferenceTrack;
use reference_data::ReferenceData;
use track::Contig;
use track_picker::TrackPicker;
use viewerapp::{MouseGesture, NavigationRegions};
use viewerjobs::{BamLoad, TrackedJob};
use zoom::{HorizontalZoom, VerticalZoom};

/// Fixed reference-track height in rows: the `Mid`-zoom letter, the sequence row, the centre
/// marker, and the base legend.
const REFERENCE_TRACK_HEIGHT: u16 = 4;

const DIVIDER_HEIGHT: u16 = 1;

enum GenomePopover {
    Contigs(ContigSelectorState),
    Tracks(TrackPicker),
    Bams(BamSwitcher),
}

/// One frame's pane for every track, named so that consumers never encode the vertical order.
///
/// Fields are declared in screen order, matching `layout`'s constraint list and the value it
/// builds, so the three read alike top to bottom. Field-name initialisation means a mismatch would
/// still compile — the agreement is for the reader, and `in_order` is what tests assert against.
///
/// A disabled track gets a zero-height `Rect` rather than being absent, which keeps this a plain
/// value: the pane always exists, and whether to draw into it stays the caller's decision.
pub(super) struct Panes {
    contig_grid: Rect,
    minimap_divider: Rect,
    minimap: Rect,
    local_divider: Rect,
    local_area: Rect,
    local_area_features: Rect,
    coverage_divider: Rect,
    coverage: Rect,
    pileup_divider: Rect,
    alignment: Rect,
    reference: Rect,
    coordinate: Rect,
    features: Rect,
}

#[cfg(test)]
impl Panes {
    /// Every pane in vertical order, named. The order lives here and in `layout`'s constraint
    /// list; nothing else should need to know it.
    fn in_order(&self) -> [(&'static str, Rect); 13] {
        [
            ("contig grid", self.contig_grid),
            ("minimap divider", self.minimap_divider),
            ("minimap", self.minimap),
            ("local divider", self.local_divider),
            ("local area", self.local_area),
            ("local area features", self.local_area_features),
            ("coverage divider", self.coverage_divider),
            ("coverage", self.coverage),
            ("pileup divider", self.pileup_divider),
            ("alignment", self.alignment),
            ("reference", self.reference),
            ("coordinate", self.coordinate),
            ("features", self.features),
        ]
    }

    /// The panes a toggle must leave alone: everything but the ones it controls and the alignment
    /// that pays for them. Filtered by name, since a disabled pane collapses to a zero-height
    /// `Rect` that another collapsed pane would compare equal to.
    fn unaffected_by<'a>(&self, toggled: &'a [&'a str]) -> impl Iterator<Item = (&'static str, Rect)> + 'a {
        self.in_order()
            .into_iter()
            .filter(move |&(name, _)| name != "alignment" && !toggled.contains(&name))
    }

    fn total_height(&self) -> u16 {
        self.in_order().iter().map(|(_, pane)| pane.height).sum()
    }
}

/// Read-alignment viewer state and concrete tracks.
pub struct GenomeViewer {
    pub(crate) track_contig_grid: ContigGridTrack,
    pub(crate) track_ref: ReferenceTrack,
    pub(crate) track_features: FeaturesTrack,
    pub(crate) track_minimap: MinimapTrack,
    pub(crate) track_local_area: LocalAreaTrack,
    pub(crate) track_coord: CoordinateTrack,
    track_coverage: CoverageTrack,
    pub(crate) track_align: AlignmentTrack,
    pub(crate) settings: GenomeViewerSettings,
    popover: Option<GenomePopover>,
    popover_bounds: Option<Rect>,
    bam_paths: Vec<PathBuf>,
    active_bam: Option<PathBuf>,
    pending_bam: Option<BamLoad>,
    /// GTF jobs awaiting completion.
    pub(super) tracked_jobs: Vec<viewerjobs::TrackedJob>,
    /// Records emitted for the shell, drained each frame.
    pub(super) notify: NotificationQueue,
    /// Loaded feature index used by rendering, search, and navigation.
    locus_index: LocusIndex,
    /// Filtered feature count from the latest draw.
    pub(super) last_filtered_count: usize,
    /// Search session state.
    pub(crate) search: SearchState,
    ref_genome: IndexedFasta,
    /// Nonblocking sequence cache shared by the tracks.
    reference_data: ReferenceData,
    /// Name of the current contig.
    active_contig: String,
    /// Single shell-owned scroll position; tracks receive positions at draw time.
    pub(super) centre: Position,
    /// Hit regions from the last draw on the active reference and contig.
    navigation: Option<NavigationRegions>,
    mouse_gesture: Option<MouseGesture>,
    /// Saved centre positions by contig name.
    contig_scroll_positions: HashMap<String, BasePair>,
    /// Worker for GTF loading and BAM fetches.
    io_worker: Option<IoWorkerHandle>,
}

impl GenomeViewer {
    /// Constructs a viewer from its settings, genome, tracks, and workers.
    pub fn new(
        settings: GenomeViewerSettings,
        genome: IndexedFasta,
        track_align: AlignmentTrack,
        load_error: Option<String>,
        io_worker: IoWorkerHandle,
    ) -> Self {
        let reference_data = ReferenceData::new(&genome, crate::refgenome::ReferenceWorkerHandle::spawn());
        Self::with_sources(
            settings,
            genome,
            track_align,
            reference_data,
            load_error,
            Some(io_worker),
        )
    }

    #[cfg(feature = "resident-demo")]
    pub(crate) fn resident(
        genome: IndexedFasta,
        windows: Vec<crate::refgenome::resident::ReferenceWindow>,
        bam: &[u8],
    ) -> std::io::Result<Self> {
        let mut settings = GenomeViewerSettings::new();
        settings.localareatrack = true;
        settings.featurestrack = false;
        let source = alignment::ResidentBamSource::read(bam)?;
        let track_align = AlignmentTrack::resident(source.clone());
        let reference_data = ReferenceData::resident(windows);
        let mut viewer = Self::with_sources(settings, genome, track_align, reference_data, None, None);
        viewer.track_local_area.set_resident_source(source);
        viewer.jump_horizontal(BasePair(0));
        Ok(viewer)
    }

    fn with_sources(
        settings: GenomeViewerSettings,
        genome: IndexedFasta,
        track_align: AlignmentTrack,
        reference_data: ReferenceData,
        load_error: Option<String>,
        io_worker: Option<IoWorkerHandle>,
    ) -> Self {
        let mut notify = NotificationQueue::default();
        if let Some(error) = load_error {
            notify.error(error);
        }
        let active_contig = genome.index.contig_name(0);
        Self {
            track_contig_grid: ContigGridTrack::new(genome.clone()),
            track_ref: ReferenceTrack,
            track_features: FeaturesTrack::new(),
            track_minimap: MinimapTrack::new(&genome),
            track_local_area: LocalAreaTrack::new(),
            track_coord: CoordinateTrack::new(),
            track_coverage: CoverageTrack::new(),
            track_align,
            centre: Position::default(),
            navigation: None,
            mouse_gesture: None,
            settings,
            popover: None,
            popover_bounds: None,
            bam_paths: Vec::new(),
            active_bam: None,
            pending_bam: None,
            tracked_jobs: Vec::new(),
            notify,
            locus_index: LocusIndex::new(),
            last_filtered_count: 0,
            search: SearchState::new(),
            ref_genome: genome,
            reference_data,
            active_contig,
            contig_scroll_positions: HashMap::new(),
            io_worker,
        }
    }

    /// Builds the current contig descriptor for tracks.
    fn contig(&self) -> Contig {
        Contig {
            name: self.active_contig.clone(),
            length: self.contig_length(),
        }
    }

    /// Uses the shell-owned centre with this pane's width; settings and contig
    /// remain explicit track inputs.
    fn track_view<'a>(
        &self,
        pane: Rect,
        contig: &'a Contig,
        settings: &'a GenomeViewerSettings,
    ) -> track::TrackView<'a> {
        track::TrackView {
            position: self.view_position(pane.width),
            settings,
            contig,
        }
    }

    /// Resolves an exact name case-insensitively, then tries a `chr` prefix.
    pub(super) fn resolve_contig(&self, arg: &str) -> Result<String, String> {
        let arg = arg.trim();
        if arg.is_empty() {
            return Err("activecontig requires a value".to_string());
        }
        let index = &self.ref_genome.index;
        if let Some(name) = index.contig_names().find(|name| name.eq_ignore_ascii_case(arg)) {
            return Ok(name);
        }
        let prefixed = format!("chr{arg}");
        if let Some(name) = index.contig_names().find(|name| name.eq_ignore_ascii_case(&prefixed)) {
            return Ok(name);
        }
        Err(format!("unknown contig: {arg:?}"))
    }

    /// Saves the old position and restores the new contig's position.
    fn switch_active_contig(&mut self, new_contig: String) {
        if new_contig != self.active_contig {
            self.navigation = None;
            self.mouse_gesture = None;
        }
        self.contig_scroll_positions
            .insert(self.active_contig.clone(), self.centre.value());

        self.active_contig = new_contig;

        let restored = self
            .contig_scroll_positions
            .get(&self.active_contig)
            .copied()
            .unwrap_or(BasePair(0));
        self.jump_horizontal(restored);
        self.track_local_area.clear_contig();
    }

    fn jump_to_locus(&mut self, contig: &str, position: BasePair) -> Result<(), String> {
        let contig = self.resolve_contig(contig)?;
        self.switch_active_contig(contig);
        self.jump_horizontal(position);
        Ok(())
    }

    /// Jumps to the next or previous matching feature start.
    fn jump_to_feature(&mut self, forward: bool) {
        let target = {
            let idx = &self.locus_index;
            let feature_index = match idx.active() {
                Some(fi) => fi,
                None => {
                    self.notify.warn("No loci loaded — use :load first");
                    return;
                }
            };
            let current = self.centre.value();
            let filter = &self.settings.feature_filter;
            if forward {
                feature_index.next_start(&self.active_contig, current, filter)
            } else {
                feature_index.prev_start(&self.active_contig, current, filter)
            }
        };
        let Some(target) = target else {
            self.notify.warn(format!(
                "No {} feature on this contig",
                if forward { "next" } else { "previous" }
            ));
            return;
        };
        self.jump_horizontal(target);
    }

    /// Applies a feature-filter edit.
    fn apply_feature_filter_edit(&mut self, edit: FeatureFilterEdit) {
        match edit {
            FeatureFilterEdit::Include(substrings) => self.settings.feature_filter.include = substrings,
            FeatureFilterEdit::Exclude(substrings) => self.settings.feature_filter.exclude = substrings,
            FeatureFilterEdit::Clear => self.settings.feature_filter = FeatureFilter::none(),
        }
    }

    /// Replaces the reference genome and resets derived viewer state.
    pub(crate) fn set_reference(&mut self, path: &std::path::Path) -> Result<(), String> {
        if self.io_worker.is_none() {
            return Err("Loading FASTA files is unavailable in this demo".to_string());
        }
        let genome = IndexedFasta::from_file(path).map_err(|err| format!("Failed to load reference genome: {err}"))?;
        self.navigation = None;
        self.mouse_gesture = None;
        self.track_contig_grid.update_reference(genome.clone());
        self.track_minimap = MinimapTrack::new(&genome);
        self.active_contig = genome.index.contig_name(0);
        self.ref_genome = genome;
        self.reference_data.replace(&self.ref_genome);
        self.track_local_area.clear_contig();
        self.contig_scroll_positions.clear();
        self.centre.jump_to(BasePair(0), self.contig_length());
        Ok(())
    }

    /// Returns the active contig length, or zero for an inconsistent state.
    fn contig_length(&self) -> BasePair {
        self.ref_genome.index.length(&self.active_contig).unwrap_or(BasePair(0))
    }

    /// Moves the centre by whole terminal columns. Mid zoom doubles the bp delta;
    /// the `move*` settings therefore read as columns per keypress, not bp.
    ///
    /// Doubling keeps the centre's parity, so an odd centre from `:goto` or a
    /// clamp stays odd. There is deliberately no parity snap: it would fight
    /// the centline's half-step and land `:goto` somewhere other than asked.
    fn nudge_horizontal(&mut self, step: BasePair, forward: bool) {
        let delta = step.as_u64() as i64;
        let delta = if self.settings.horizontal_zoom == HorizontalZoom::Mid {
            delta + delta
        } else {
            delta
        };
        let signed = if forward { delta } else { -delta };
        #[cfg(feature = "resident-demo")]
        if let Some(span) = self
            .reference_data
            .resident_span(&self.active_contig, self.centre.value())
        {
            let last = span.end - BasePair(1);
            self.centre.nudge(signed, last);
            self.centre.jump_to(self.centre.value().max(span.start), last);
            return;
        }
        self.centre.nudge(signed, self.contig_length());
    }

    fn jump_horizontal(&mut self, target: BasePair) {
        #[cfg(feature = "resident-demo")]
        if let Some(span) = self
            .reference_data
            .resident_span(&self.active_contig, target)
            .or_else(|| {
                self.reference_data
                    .resident_span(&self.active_contig, self.centre.value())
            })
            .or_else(|| self.reference_data.first_resident_span(&self.active_contig))
        {
            self.centre.jump_to(target.max(span.start), span.end - BasePair(1));
            return;
        }
        self.centre.jump_to(target, self.contig_length());
    }

    /// Steps the combined zoom state and applies only changed axes.
    fn step_lod(&mut self, out: bool) {
        let h = self.settings.horizontal_zoom;
        let v = self.track_align.vertical_zoom();
        let (next_h, next_v) = zoom::step_lod(h, v, out);
        if (next_h, next_v) == (h, v) {
            return;
        }
        self.apply_zoom(
            (next_h != h).then_some(next_h == HorizontalZoom::Mid),
            (next_v != v).then_some(next_v == VerticalZoom::Mid),
        );
    }

    /// Builds this frame's viewport for a pane width.
    fn view_position(&self, columns: u16) -> ViewPosition {
        ViewPosition::new(
            self.centre.value(),
            track::visible_bp_span(columns, self.settings.horizontal_zoom),
        )
    }

    /// Applies horizontal and vertical zoom changes through their constraint paths.
    fn apply_zoom(&mut self, horizontal_to_mid: Option<bool>, alignment_v_to_mid: Option<bool>) {
        if let Some(to_mid) = horizontal_to_mid {
            self.settings.horizontal_zoom = match (self.settings.horizontal_zoom, to_mid) {
                (HorizontalZoom::Close, true) => HorizontalZoom::Mid,
                (HorizontalZoom::Mid, false) => HorizontalZoom::Close,
                (current, _) => current,
            };
        }
        if let Some(to_mid) = alignment_v_to_mid {
            self.track_align
                .step_vertical_zoom(to_mid, self.settings.horizontal_zoom);
        } else {
            self.track_align.resync_vertical_zoom(self.settings.horizontal_zoom);
        }
    }

    /// Keeps a row for the pileup when space is tight and gives it all spare rows.
    fn layout(area: Rect, settings: &GenomeViewerSettings) -> Panes {
        let fixed = |visible, height| Constraint::Length(if visible { height } else { 0 });
        let [
            contig_grid,
            minimap_divider,
            minimap,
            local_divider,
            local_area,
            local_area_features,
            coverage_divider,
            coverage,
            pileup_divider,
            alignment,
            reference,
            coordinate,
            features,
        ] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                fixed(settings.contigmarkers, CONTIG_GRID_TRACK_HEIGHT),
                fixed(settings.chromosometrack, MINIMAP_DIVIDER_HEIGHT),
                fixed(settings.chromosometrack, MINIMAP_TRACK_HEIGHT),
                fixed(settings.localareatrack, LOCAL_AREA_DIVIDER_HEIGHT),
                fixed(settings.localareatrack, LOCAL_AREA_TRACK_HEIGHT),
                fixed(settings.localareafeatures, LOCAL_AREA_FEATURES_TRACK_HEIGHT),
                fixed(settings.coveragetrack, DIVIDER_HEIGHT),
                fixed(settings.coveragetrack, COVERAGE_TRACK_HEIGHT),
                Constraint::Length(DIVIDER_HEIGHT),
                Constraint::Min(1),
                fixed(settings.referencetrack, REFERENCE_TRACK_HEIGHT),
                fixed(settings.coordinatetrack, COORDINATE_TRACK_HEIGHT),
                fixed(settings.featurestrack, FEATURES_TRACK_HEIGHT),
            ])
            .areas(area);
        Panes {
            contig_grid,
            minimap_divider,
            minimap,
            local_divider,
            local_area,
            local_area_features,
            coverage_divider,
            coverage,
            pileup_divider,
            alignment,
            reference,
            coordinate,
            features,
        }
    }

    /// Returns the status override for active file loads or filtered features.
    fn status_text(&self) -> Option<String> {
        if let Some(load) = &self.pending_bam {
            Some(format!("{} Loading BAM", spinner_glyph(load.started_at)))
        } else if let Some(job) = self
            .tracked_jobs
            .iter()
            .find(|j| matches!(j, TrackedJob::GtfLoad { .. }))
        {
            Some(format!(
                "{} {label}",
                spinner_glyph(job.started_at()),
                label = job.label()
            ))
        } else if !self.settings.feature_filter.is_empty() && self.last_filtered_count > 0 {
            Some(format!(
                "{} features filtered from current region",
                self.last_filtered_count
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::protocol::{
        Applied, CommandOutcome, CommandTarget, Direction as ScrollDirection, GenomeCommand, ListMove, ScrollStep,
    };
    use crate::input::{MouseEvent, MouseKind};
    use crate::keys::Keymap;
    use crate::locus::{self, FeatureIndex, Locus};
    use crate::search::{MatchTarget, ResolvedMatch};
    use crate::style::palette;
    use crate::viewer::ViewerApp;
    use geometry::cent_column;
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    pub(super) fn new_viewer() -> GenomeViewer {
        let settings = GenomeViewerSettings::new();
        GenomeViewer::new(
            settings,
            IndexedFasta::hs1_fallback(),
            AlignmentTrack::for_test(),
            None,
            IoWorkerHandle::spawn(),
        )
    }

    fn draw_for_clicks(terminal: &mut Terminal<TestBackend>, viewer: &mut GenomeViewer, area: Rect) -> Panes {
        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("draw genome");
        GenomeViewer::layout(area, &viewer.settings)
    }

    fn click(viewer: &mut GenomeViewer, column: u16, row: u16) {
        mouse(viewer, MouseKind::LeftDown, column, row);
    }

    fn mouse(viewer: &mut GenomeViewer, kind: MouseKind, column: u16, row: u16) {
        viewer.on_mouse(MouseEvent { column, row, kind });
    }

    #[test]
    fn chromosome_clicks_use_the_whole_bar_during_countup() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.contigmarkers = false;
        let length = viewer.contig_length();
        let area = Rect::new(7, 2, 82, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let bar = draw_for_clicks(&mut terminal, &mut viewer, area).minimap;
        assert!(viewer.track_minimap.is_animating());

        for (x, target) in [(bar.right() - 2, length - BasePair(1)), (bar.x + 1, BasePair(0))] {
            assert_ne!(terminal.backend().buffer()[(x, bar.y)].symbol(), " ");
            click(&mut viewer, x, bar.y);
            assert_eq!(viewer.centre.value(), target);
        }
        viewer.centre.jump_to(length / 3, length);
        for (x, y) in [(bar.x, bar.y), (bar.right() - 1, bar.y), (bar.x + 1, bar.y - 1)] {
            click(&mut viewer, x, y);
            assert_eq!(viewer.centre.value(), length / 3);
        }

        let narrow = draw_for_clicks(&mut terminal, &mut viewer, Rect { width: 3, ..area }).minimap;
        click(&mut viewer, narrow.x + 1, narrow.y);
        let midpoint = (length - BasePair(1)) / 2;
        assert_eq!(viewer.centre.value(), midpoint);
        draw_for_clicks(&mut terminal, &mut viewer, Rect { width: 2, ..area });
        click(&mut viewer, narrow.x + 1, narrow.y);
        assert_eq!(viewer.centre.value(), midpoint);
    }

    #[test]
    fn chromosome_drag_stays_on_the_pressed_track_until_release() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareatrack = true;
        let area = Rect::new(7, 2, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let panes = draw_for_clicks(&mut terminal, &mut viewer, area);
        let left = panes.minimap.x + 1;
        let right = panes.minimap.right() - 2;
        let row = panes.minimap.y;
        click(&mut viewer, right, row);
        mouse(&mut viewer, MouseKind::LeftDrag, left, row);
        assert_eq!(viewer.centre.value(), BasePair(0));
        draw_for_clicks(&mut terminal, &mut viewer, area);

        let local = panes.local_area;
        mouse(&mut viewer, MouseKind::LeftDrag, right, local.bottom() - 1);
        assert_eq!(viewer.centre.value(), BasePair(0));
        mouse(&mut viewer, MouseKind::LeftDrag, right, row);
        let last = viewer.contig_length() - BasePair(1);
        assert_eq!(viewer.centre.value(), last);

        mouse(&mut viewer, MouseKind::LeftUp, right, local.y);
        mouse(&mut viewer, MouseKind::LeftDrag, left, row);
        assert_eq!(viewer.centre.value(), last);
    }

    #[test]
    fn pileup_drag_pans_both_axes_from_press_to_release_at_each_zoom() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        let area = Rect::new(7, 2, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let centre = BasePair(10_000);

        for (horizontal_mid, vertical_mid, bp_per_column) in [(false, false, 1), (false, true, 1), (true, false, 2)] {
            viewer.apply_zoom(Some(horizontal_mid), Some(vertical_mid));
            viewer.jump_horizontal(centre);
            viewer.track_align.jump_vertical(BasePair(0));
            let pileup = draw_for_clicks(&mut terminal, &mut viewer, area).alignment;
            let (x, y) = (pileup.x + 8, pileup.y + 1);
            let offset = |viewer: &GenomeViewer| viewer.track_align.vertical_view().row_offset();

            click(&mut viewer, x - 4, y - 2);
            mouse(&mut viewer, MouseKind::LeftDrag, x, y);
            assert_eq!((viewer.centre.value(), offset(&viewer)), (centre, BasePair(0)));

            click(&mut viewer, x, y);
            assert_eq!((viewer.centre.value(), offset(&viewer)), (centre, BasePair(0)));
            mouse(&mut viewer, MouseKind::LeftDrag, x - 4, y - 2);
            assert_eq!(viewer.centre.value(), centre + BasePair(4 * bp_per_column));
            assert_eq!(offset(&viewer), BasePair(2));

            draw_for_clicks(&mut terminal, &mut viewer, area);
            mouse(&mut viewer, MouseKind::LeftDrag, x, y);
            mouse(&mut viewer, MouseKind::LeftUp, x, y);
            mouse(&mut viewer, MouseKind::LeftDrag, x - 4, y - 2);
            assert_eq!((viewer.centre.value(), offset(&viewer)), (centre, BasePair(0)));
        }
    }

    #[test]
    fn local_area_clicks_use_the_displayed_span_until_the_next_draw() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareatrack = true;
        viewer.settings.localareascale = 5;
        viewer.settings.colorlegend = false;
        let area = Rect::new(7, 2, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let length = viewer.contig_length();

        for (horizontal_mid, vertical_mid, bucket_width) in [(false, false, 5), (false, true, 5), (true, false, 10)] {
            viewer.apply_zoom(Some(horizontal_mid), Some(vertical_mid));
            let zoom = lod_state(&viewer);
            let last_base = length - BasePair(1);
            let right_start = length - BasePair(u64::from(area.width) * bucket_width);
            for (centre, start, first_column) in [(BasePair(0), BasePair(0), 60), (last_base, right_start, 20)] {
                viewer.centre.jump_to(centre, length);
                let local = draw_for_clicks(&mut terminal, &mut viewer, area).local_area;
                for (column, row) in [(first_column, local.y), (13, local.bottom() - 1)] {
                    click(&mut viewer, local.x + column, row);
                    assert_eq!(
                        viewer.centre.value(),
                        start + BasePair(u64::from(column) * bucket_width + bucket_width / 2)
                    );
                    assert_eq!(lod_state(&viewer), zoom);
                }
            }
        }

        let local = draw_for_clicks(&mut terminal, &mut viewer, area).local_area;
        viewer.settings.localareatrack = false;
        draw_for_clicks(&mut terminal, &mut viewer, area);
        let centre = viewer.centre.value();
        click(&mut viewer, local.x, local.y);
        assert_eq!(viewer.centre.value(), centre);
    }

    #[test]
    fn local_area_hold_repeats_against_the_redrawn_span_and_follows_the_pointer() {
        let _palette = palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareatrack = true;
        viewer.settings.localareascale = 5;
        viewer.settings.colorlegend = false;
        viewer.apply_zoom(Some(false), Some(false));
        let start = BasePair(10_000);
        viewer.jump_horizontal(start);
        let area = Rect::new(7, 2, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let local = draw_for_clicks(&mut terminal, &mut viewer, area).local_area;
        let (right, left) = (local.x + 60, local.x + 20);
        click(&mut viewer, right, local.y);
        assert_eq!(viewer.centre.value(), start + BasePair(102));
        draw_for_clicks(&mut terminal, &mut viewer, area);

        let first_repeat = viewer
            .mouse_repeat_deadline()
            .expect("stationary hold schedules a repeat");
        assert!(!viewer.tick_mouse_navigation(first_repeat - Duration::from_millis(1)));
        let late = first_repeat + Duration::from_secs(1);
        assert!(viewer.tick_mouse_navigation(late));
        assert_eq!(viewer.centre.value(), start + BasePair(204));
        assert!(!viewer.tick_mouse_navigation(late), "a slow frame only takes one step");
        draw_for_clicks(&mut terminal, &mut viewer, area);

        mouse(&mut viewer, MouseKind::LeftDrag, left, local.bottom());
        assert!(viewer.mouse_repeat_deadline().is_none());
        mouse(&mut viewer, MouseKind::LeftDrag, left, local.y);
        let next = viewer.mouse_repeat_deadline().expect("dragging back resumes the hold");
        assert!(viewer.tick_mouse_navigation(next));
        assert_eq!(viewer.centre.value(), start + BasePair(106));

        mouse(&mut viewer, MouseKind::LeftUp, left, local.bottom());
        assert!(viewer.mouse_repeat_deadline().is_none());

        click(&mut viewer, right, local.y);
        let hold = viewer.mouse_repeat_deadline().unwrap();
        mouse(&mut viewer, MouseKind::LeftDrag, left, local.y);
        assert!(viewer.mouse_repeat_deadline().unwrap() < hold);
    }

    #[test]
    fn local_area_clicks_respect_overlays_and_contig_changes() {
        let _palette = palette::lock_for_test();
        let genome = IndexedFasta::new("test".to_string(), "chr1\t7\t6\t7\t8\nchr2\t7\t20\t7\t8\n").unwrap();
        let mut viewer = GenomeViewer::new(
            GenomeViewerSettings::new(),
            genome,
            AlignmentTrack::for_test(),
            None,
            IoWorkerHandle::spawn(),
        );
        viewer.settings.colorlegend = true;
        viewer.settings.localareatrack = true;
        let area = Rect::new(7, 2, 80, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.right(), area.bottom())).expect("test terminal");
        let local = draw_for_clicks(&mut terminal, &mut viewer, area).local_area;
        let x = local.right() - 1;
        let band = local.bottom() - 1;
        click(&mut viewer, x, band - 1);
        assert_eq!(viewer.centre.value(), BasePair(0), "legend covers the chart corner");
        click(&mut viewer, x, band);
        assert_eq!(
            viewer.centre.value(),
            BasePair(6),
            "the short contig still ends in bounds"
        );

        viewer.apply_outcome(GenomeCommand::ActiveContig("chr2".to_string()).into());
        click(&mut viewer, x, band);
        assert_eq!(viewer.centre.value(), BasePair(0), "the old chromosome frame is stale");
        draw_for_clicks(&mut terminal, &mut viewer, area);
        click(&mut viewer, x, band);
        assert_eq!(viewer.centre.value(), BasePair(6));

        viewer.apply_outcome(GenomeCommand::OpenContigSelector.into());
        click(&mut viewer, local.x, band);
        assert_eq!(viewer.centre.value(), BasePair(6));
        draw_for_clicks(&mut terminal, &mut viewer, area);
        viewer.apply_outcome(CommandOutcome::List(ListMove::Close));
        click(&mut viewer, local.x, band);
        assert_eq!(viewer.centre.value(), BasePair(6), "the selector is still on screen");
    }

    fn lod_state(viewer: &GenomeViewer) -> (HorizontalZoom, VerticalZoom) {
        (viewer.settings.horizontal_zoom, viewer.track_align.vertical_zoom())
    }

    #[test]
    fn centline_mark_agrees_across_track_areas_at_both_zooms_and_parities() {
        let reference_area = Rect::new(0, 0, 80, 7);
        let alignment_area = Rect::new(0, 9, 80, 20);

        for bp_per_column in [1u64, 2] {
            for centre in [BasePair(1_000), BasePair(1_001)] {
                let position =
                    ViewPosition::new(centre, BasePair::from(reference_area.width) * BasePair(bp_per_column));
                assert_eq!(
                    coordinate::marks::centline_mark(reference_area, bp_per_column, position),
                    coordinate::marks::centline_mark(alignment_area, bp_per_column, position),
                    "centre {centre:?} at {bp_per_column}bp/column must resolve to the same \
                     (column, glyph) for both tracks"
                );
            }
        }
    }

    /// A palette edit must reach the *rasterised frame*, not merely the stored value.
    ///
    /// This is the assertion that proves the whole feature: an accessor returning the new colour
    /// says nothing about whether the track that draws the coordinate ruler actually calls it each
    /// frame. A renderer that cached a resolved `Color` would pass every test one layer down and
    /// fail here — which is exactly the standing risk the mutable palette carries.
    #[test]
    fn a_palette_edit_changes_what_the_next_frame_draws() {
        use crate::style::palette;
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = palette::lock_for_test();

        let mut viewer = new_viewer();
        let area = Rect::new(0, 0, 120, 40);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        let coordinate_area = GenomeViewer::layout(area, &viewer.settings).coordinate;
        let ruler_row = coordinate_area.y;
        let sample = |terminal: &Terminal<TestBackend>| {
            let buffer = terminal.backend().buffer();
            (0..area.width)
                .filter_map(|x| buffer.cell((x, ruler_row)).map(|c| c.bg))
                .collect::<Vec<_>>()
        };

        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("first draw");
        let before = sample(&terminal);

        // Expected value comes from `ramp::` directly, never from the palette's own accessor.
        let expected = crate::style::palette::ramp::purple(3);
        palette::apply_spec("chrome_ruler_band_bg:purple(3)").expect("spec must apply");

        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("redraw after the edit");
        let after = sample(&terminal);

        palette::reset();

        assert_ne!(before, after, "the ruler row must repaint after a palette edit");
        assert!(
            after.contains(&expected),
            "the edited colour must appear in the drawn ruler row"
        );
    }

    /// Invariant 9: the composed tracks put the centred locus in one terminal column. The
    /// expected column comes from the layout geometry, never from one of the renderers under test.
    #[test]
    fn every_track_puts_the_centred_locus_in_the_same_column() {
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        for width in [120u16, 121] {
            for scale in [2, 100, 10_000] {
                let mut viewer = new_viewer();
                viewer.settings.localareatrack = true;
                viewer.settings.localareascale = scale;
                viewer.settings.coveragetrack = true;
                viewer.settings.centline = true;
                // Keep the centre outside tiny.bam's reads so every pileup row exposes its marker.
                let contig_length = viewer.contig_length();
                viewer.centre.jump_to(BasePair(500), contig_length);
                let area = Rect::new(0, 0, width, 50);
                let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

                terminal
                    .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
                    .expect("composed genome draw");

                let centre = cent_column(area).expect("non-empty test area must have a centre column");
                let panes = GenomeViewer::layout(area, &viewer.settings);
                let (reference, local_area, local_divider, coordinate, alignment) = (
                    panes.reference,
                    panes.local_area,
                    panes.local_divider,
                    panes.coordinate,
                    panes.alignment,
                );
                let buffer = terminal.backend().buffer();
                let marked_rows = |track: Rect| {
                    (track.y..track.y + track.height)
                        .filter(|&row| {
                            buffer
                                .cell((centre, row))
                                .is_some_and(|cell| matches!(cell.symbol(), "│" | "|"))
                        })
                        .collect::<Vec<_>>()
                };

                let reference_rows = marked_rows(reference);
                assert!(
                    !reference_rows.is_empty(),
                    "width {width}: reference lost the centre marker"
                );
                assert!(
                    !marked_rows(panes.coverage).is_empty(),
                    "width {width}: coverage lost the centre marker"
                );

                for (name, track) in [("coordinate", coordinate), ("alignment", alignment)] {
                    let rows = marked_rows(track);
                    assert_eq!(
                        rows.len(),
                        usize::from(track.height),
                        "width {width}: {name} did not mark every row at column {centre}"
                    );
                }

                // The marker is drawn over the chart's bottom row, so its column is derived
                // against the track pane's own width.
                let view = viewer.view_position(local_area.width);
                let span = geometry::AreaSpan::for_viewport(view, scale, contig_length);
                let offset = u128::from((view.centre - span.start()).as_u64());
                let expected = (offset * u128::from(local_area.width) / u128::from(span.width().as_u64()))
                    .min(u128::from(local_area.width - 1)) as u16;
                let marker_row = local_area.bottom() - 2;
                let marker = buffer.cell((local_area.x + expected, marker_row)).expect("marker cell");
                assert_eq!(
                    marker.symbol(),
                    "█",
                    "width {width}, scale {scale}: local-area marker missed its independently derived column"
                );
                assert!(
                    (local_divider.y..local_divider.bottom()).all(|row| buffer
                        .cell((local_area.x + expected, row))
                        .expect("cell")
                        .symbol()
                        != "█"),
                    "width {width}, scale {scale}: the marker must not also paint inside the divider pane"
                );
            }
        }
    }

    #[test]
    fn local_area_feature_context_uses_the_local_area_span_projection() {
        use crate::style::{Features, Surface};
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareafeatures = true;
        viewer.settings.localareascale = 2;
        viewer.centre.jump_to(BasePair(100), viewer.contig_length());
        viewer.locus_index.set(FeatureIndex::new(
            vec![Locus {
                start: BasePair(103),
                end: BasePair(104),
                key: "x".into(),
                strand: crate::locus::FeatureStrand::Unknown,
            }],
            HashMap::from([("chr1".to_string(), 0..1)]),
        ));

        let area = Rect::new(0, 0, 40, 50);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
        for (horizontal_mid, vertical_mid) in [(false, false), (false, true), (true, false)] {
            viewer.apply_zoom(Some(horizontal_mid), Some(vertical_mid));
            for chart_visible in [true, false] {
                viewer.settings.localareatrack = chart_visible;
                let panes = GenomeViewer::layout(area, &viewer.settings);
                assert_eq!(panes.local_area_features.y, panes.local_area.bottom());
                assert_eq!(panes.local_area_features.height, LOCAL_AREA_FEATURES_TRACK_HEIGHT);

                terminal
                    .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
                    .expect("composed draw");

                let view = viewer.view_position(area.width);
                let span =
                    geometry::AreaSpan::for_viewport(view, viewer.settings.localareascale, viewer.contig_length());
                let span_width = u128::from(span.width().as_u64());
                let columns = u128::from(panes.local_area_features.width);
                let start_offset = u128::from((BasePair(103) - span.start()).as_u64());
                let end_offset = u128::from((BasePair(104) - span.start()).as_u64());
                let expected_start = (start_offset * columns / span_width).min(columns - 1) as u16;
                let expected_end = (end_offset * columns)
                    .div_ceil(span_width)
                    .max(u128::from(expected_start) + 1)
                    .min(columns) as u16;

                let buffer = terminal.backend().buffer();
                assert_eq!(
                    expected_end,
                    expected_start + 1,
                    "fixture must exercise minimum-width projection"
                );
                assert_eq!(
                    buffer
                        .cell((
                            panes.local_area_features.x + expected_start,
                            panes.local_area_features.y + 1
                        ))
                        .expect("feature cell")
                        .bg,
                    Features::EXON_BG()
                );
                assert_eq!(
                    buffer
                        .cell((
                            panes.local_area_features.x + expected_end,
                            panes.local_area_features.y + 1
                        ))
                        .expect("following cell")
                        .bg,
                    Surface::TRACK_BG()
                );
            }
        }
    }
    /// Asserts the shape every optional track shares: the panes it names collapse to nothing when
    /// off, the alignment alone absorbs the difference, no other pane moves height, and the panes
    /// still tile the area in both states. `cost` comes from the tracks' own height constants, not
    /// from measuring the layout under test.
    fn assert_toggle_costs_only_the_alignment(area: Rect, with: &Panes, without: &Panes, toggled: &[&str], cost: u16) {
        for &name in toggled {
            let (enabled, disabled) = (pane_named(with, name), pane_named(without, name));
            assert_ne!(enabled.height, 0, "{name} must reserve rows when enabled");
            assert_eq!(disabled.height, 0, "{name} must reserve no rows when disabled");
        }
        assert_eq!(
            with.alignment.height + cost,
            without.alignment.height,
            "the toggled tracks must be paid for out of the alignment"
        );
        for ((name, enabled), (_, disabled)) in with.unaffected_by(toggled).zip(without.unaffected_by(toggled)) {
            assert_eq!(
                enabled.height, disabled.height,
                "{name} must keep its height across the toggle"
            );
        }
        assert_eq!(with.total_height(), area.height, "enabled panes must tile the area");
        assert_eq!(without.total_height(), area.height, "disabled panes must tile the area");
    }

    fn pane_named(panes: &Panes, name: &str) -> Rect {
        panes
            .in_order()
            .into_iter()
            .find(|&(n, _)| n == name)
            .unwrap_or_else(|| panic!("no pane named {name}"))
            .1
    }

    #[test]
    fn picker_rows_toggle_only_their_panes_in_screen_order() {
        use crate::setting::{Setting::*, SettingValue};

        let area = Rect::new(3, 5, 120, 60);
        let cases = [
            Some((Contigmarkers, &["contig grid"][..], CONTIG_GRID_TRACK_HEIGHT)),
            Some((
                Chromosometrack,
                &["minimap divider", "minimap"][..],
                MINIMAP_DIVIDER_HEIGHT + MINIMAP_TRACK_HEIGHT,
            )),
            Some((
                Localareatrack,
                &["local divider", "local area"][..],
                LOCAL_AREA_DIVIDER_HEIGHT + LOCAL_AREA_TRACK_HEIGHT,
            )),
            Some((
                Localareafeatures,
                &["local area features"][..],
                LOCAL_AREA_FEATURES_TRACK_HEIGHT,
            )),
            Some((
                Coveragetrack,
                &["coverage divider", "coverage"][..],
                DIVIDER_HEIGHT + COVERAGE_TRACK_HEIGHT,
            )),
            None,
            Some((Referencetrack, &["reference"][..], REFERENCE_TRACK_HEIGHT)),
            Some((Coordinatetrack, &["coordinate"][..], COORDINATE_TRACK_HEIGHT)),
            Some((Featurestrack, &["features"][..], FEATURES_TRACK_HEIGHT)),
        ];
        let mut viewer = new_viewer();
        for (setting, _, _) in cases.into_iter().flatten() {
            viewer.settings.set(SettingValue::from_bool(setting, true).unwrap());
        }
        let all = GenomeViewer::layout(area, &viewer.settings);
        viewer.apply_outcome(GenomeCommand::OpenTrackPicker.into());
        for case in cases {
            viewer.apply_outcome(CommandOutcome::List(ListMove::Select));
            let toggled = GenomeViewer::layout(area, &viewer.settings);
            if let Some((_, panes, cost)) = case {
                assert_toggle_costs_only_the_alignment(area, &all, &toggled, panes, cost);
                viewer.apply_outcome(CommandOutcome::List(ListMove::Select));
            } else {
                assert_toggle_costs_only_the_alignment(area, &all, &toggled, &[], 0);
            }
            viewer.apply_outcome(CommandOutcome::List(ListMove::Down));
        }

        let short_area = Rect { height: 1, ..area };
        assert_eq!(GenomeViewer::layout(short_area, &viewer.settings).alignment, short_area);
        for (setting, _, _) in cases.into_iter().flatten() {
            viewer.settings.set(SettingValue::from_bool(setting, false).unwrap());
        }
        let pileup = GenomeViewer::layout(area, &viewer.settings);
        assert_eq!(pileup.alignment.height, area.height - DIVIDER_HEIGHT);
    }
    /// Invariant 6: the marker owns one absolute row of the local-area pane and is skipped
    /// whole rather than shifted when a short terminal cannot give that pane the rows.
    #[test]
    fn short_layouts_never_move_the_local_area_marker_into_another_row() {
        use crate::style::LocalArea;
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let mut viewer = new_viewer();
        viewer.settings.localareatrack = true;
        for height in [10u16, 27] {
            let area = Rect::new(0, 0, 24, height);
            let local_area = GenomeViewer::layout(area, &viewer.settings).local_area;
            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");
            terminal
                .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
                .expect("short draw");

            let marked_rows = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .enumerate()
                .filter(|(_, cell)| cell.symbol() == "█" && cell.fg == LocalArea::VIEWPORT_MARKER_FG())
                .map(|(index, _)| index as u16 / area.width)
                .collect::<Vec<_>>();

            // The marker owns its fixed absolute row; a pane shorter than that row draws no
            // marker, even when the reference band below it cannot fit.
            if local_area.height <= LOCAL_AREA_TRACK_HEIGHT - 2 {
                assert!(
                    marked_rows.is_empty(),
                    "height {height}: marker must skip when its row is unreachable"
                );
            } else {
                assert!(
                    marked_rows
                        .iter()
                        .all(|&row| row == local_area.y + LOCAL_AREA_TRACK_HEIGHT - 2),
                    "height {height}: marker escaped its fixed row: {marked_rows:?}"
                );
            }
        }
    }
    #[test]
    fn horizontal_scroll_uses_the_configured_step_and_zoom() {
        let mut viewer = new_viewer();
        viewer.settings.movemin = BasePair(7);
        viewer.settings.movemid = BasePair(23);
        viewer.settings.movefar = BasePair(91);

        for (zoom, bp_per_column) in [(HorizontalZoom::Close, 1), (HorizontalZoom::Mid, 2)] {
            viewer.settings.horizontal_zoom = zoom;
            for (step, distance) in [
                (ScrollStep::Near, 7),
                (ScrollStep::NearX10, 70),
                (ScrollStep::Mid, 23),
                (ScrollStep::Far, 91),
            ] {
                // Both parities: at Mid zoom an odd start must keep its parity, not snap to even.
                for start in [1_000, 1_001] {
                    viewer.centre.jump_to(BasePair(start), viewer.contig_length());
                    let delta = distance * bp_per_column;
                    for (direction, expected) in
                        [(ScrollDirection::Right, start + delta), (ScrollDirection::Left, start)]
                    {
                        assert_eq!(
                            viewer.apply_outcome(CommandOutcome::Scroll { direction, step }),
                            Applied::Ok
                        );
                        assert_eq!(
                            viewer.centre.value(),
                            BasePair(expected),
                            "{zoom:?} {direction:?} {step:?} from {start}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn zoom_changes_the_visible_span_without_moving_the_centre() {
        let mut viewer = new_viewer();
        viewer.settings.horizontal_zoom = HorizontalZoom::Close;
        viewer.centre.jump_to(BasePair(5_000), BasePair(1_000_000));
        let before = viewer.view_position(80);

        viewer.apply_zoom(Some(true), Some(true));
        let after = viewer.view_position(80);

        assert_eq!(after.centre, before.centre);
        assert_ne!(after.width, before.width);
    }
    #[test]
    fn zoom_step_moves_in_the_commanded_direction() {
        let mut viewer = new_viewer();
        viewer.settings.horizontal_zoom = HorizontalZoom::Close;
        viewer.track_align.set_vzoom(HorizontalZoom::Close, VerticalZoom::Mid);

        assert_eq!(
            viewer.apply_outcome(GenomeCommand::StepZoom { into: false }.into()),
            Applied::Ok
        );
        assert_eq!(lod_state(&viewer), (HorizontalZoom::Mid, VerticalZoom::Close));

        assert_eq!(
            viewer.apply_outcome(GenomeCommand::StepZoom { into: true }.into()),
            Applied::Ok
        );
        assert_eq!(lod_state(&viewer), (HorizontalZoom::Close, VerticalZoom::Mid));
    }

    #[test]
    fn selecting_a_contig_applies_the_selector_choice() {
        let mut viewer = new_viewer();
        for outcome in [
            CommandOutcome::Genome(GenomeCommand::ActiveContig("chr1".to_string())),
            CommandOutcome::Genome(GenomeCommand::OpenContigSelector),
            CommandOutcome::List(ListMove::Down),
            CommandOutcome::List(ListMove::Select),
        ] {
            assert_eq!(viewer.apply_outcome(outcome), Applied::Ok);
        }
        assert_eq!(viewer.active_contig, "chr2");
        assert!(viewer.popover.is_none());
    }

    fn type_command(viewer: &mut GenomeViewer, input: &str) -> Result<(), String> {
        let outcome = crate::command::parse::resolve_command(&format!(":{input}"))?;
        viewer.apply_outcome(outcome).into_target_result()
    }

    #[test]
    fn local_area_5mc_threshold_applies_within_its_bounds() {
        use crate::setting::{Setting, SettingKind};

        let mut viewer = new_viewer();
        let SettingKind::BoundedInteger { min, max } = Setting::Localareamin5mc.kind() else {
            panic!("localareamin5mc must be bounded");
        };
        assert!((min..=max).contains(&viewer.settings.localareamin5mc));
        for value in [min, max] {
            type_command(&mut viewer, &format!("set localareamin5mc {value}")).unwrap();
            assert_eq!(viewer.settings.localareamin5mc, value);
        }
        for value in [min - 1, max + 1] {
            assert!(type_command(&mut viewer, &format!("set localareamin5mc {value}")).is_err());
            assert_eq!(viewer.settings.localareamin5mc, max);
        }
    }

    pub(super) fn scratch_gtf_copy(id: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apus_gtf_test_{id}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("gtf_subset.gtf.gz");
        std::fs::copy("testdata/gtf/gtf_subset.gtf.gz", &dest).unwrap();
        dest
    }

    #[test]
    fn setting_a_reference_replaces_the_index_not_just_the_name() {
        let mut viewer = new_viewer();
        let fallback_length = viewer.contig_length();
        viewer
            .set_reference(std::path::Path::new("testdata/reference/hs1_subset.fa"))
            .expect("the committed subset fixture must load");

        assert_ne!(
            viewer.contig_length(),
            fallback_length,
            "the swapped-in index must supply the active contig's length"
        );
        assert_eq!(viewer.contig_length(), BasePair(248_400), "chr1 in the subset");
    }

    #[test]
    fn a_reference_that_does_not_load_leaves_the_previous_one_in_place() {
        let mut viewer = new_viewer();
        let before = viewer.contig_length();
        let err = viewer
            .set_reference(std::path::Path::new("testdata/no_such_reference.fa"))
            .expect_err("a missing fasta must not load");

        assert!(err.contains("Failed to load reference genome"), "got {err:?}");
        assert_eq!(
            viewer.contig_length(),
            before,
            "the old genome must survive the failure"
        );
        assert!(
            viewer.ref_genome.path.is_none(),
            "the old reference must survive the failure"
        );
    }

    #[test]
    fn loading_a_bam_installs_a_real_source() {
        let mut viewer = new_viewer();
        viewer
            .load_active_bam("testdata/bam/tiny.bam".into())
            .expect("a valid fixture must load");
        assert!(!viewer.notify.has_pending(), "a valid fixture must not report an error");
        assert!(
            viewer.track_local_area.has_source(),
            "the summary source installs atomically"
        );

        let contig = "chr1";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if viewer.track_align.has_reads_in(contig, BasePair(0), BasePair(50)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "on-disk BAM fetch did not complete in time"
            );
            viewer.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn failed_bam_replacement_preserves_the_active_bam_and_locus() {
        let mut viewer = new_viewer();
        let previous = PathBuf::from("testdata/bam/tiny.bam");
        let missing = "testdata/bam/no-such.bam";
        viewer.switch_active_contig("chr2".to_string());
        viewer.jump_horizontal(BasePair(637));
        viewer.load_active_bam(previous.clone()).unwrap();
        viewer.add_bam(missing.into());
        viewer
            .switch_bam(missing.into(), Some(("chr1".into(), BasePair(123))))
            .unwrap();
        await_bam(&mut viewer);

        assert!(viewer.track_local_area.has_source());
        assert_eq!(viewer.active_bam, Some(previous));
        assert_eq!(viewer.active_contig, "chr2");
        assert_eq!(viewer.centre.value(), BasePair(637));
        assert!(
            viewer.notify.drain().iter().any(|notice| notice.text.contains(missing)),
            "a failed switch must name the requested file"
        );
    }

    fn await_bam(viewer: &mut GenomeViewer) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while viewer.pending_bam.is_some() {
            assert!(std::time::Instant::now() < deadline, "BAM load did not settle");
            viewer.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn tick_keeps_animating_while_a_record_is_queued() {
        let mut viewer = new_viewer();
        viewer.notify.info("test message");

        assert!(
            viewer.tick(),
            "tick must report animating so App gets a chance to drain the queue"
        );
    }

    fn type_slash(viewer: &mut GenomeViewer, input: &str) {
        viewer.search.tick(input, &viewer.locus_index);
    }

    pub(super) fn await_load(viewer: &mut GenomeViewer) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while viewer.gtf_load_in_flight() {
            assert!(
                std::time::Instant::now() < deadline,
                "background GTF load did not finish in time"
            );
            viewer.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn viewer_with_loaded_entries(id: &str) -> GenomeViewer {
        let mut viewer = new_viewer();
        let gtf_path = scratch_gtf_copy(id);
        let cache_path = locus::cache_path_for(&gtf_path);
        viewer.start_load(gtf_path, cache_path, true).unwrap();
        await_load(&mut viewer);
        assert!(!viewer.locus_index.active().is_none(), "fixture must load successfully");
        viewer
    }

    #[test]
    fn a_query_builds_the_matcher_once_entries_are_loaded() {
        let mut viewer = viewer_with_loaded_entries("search_builds_matcher");
        type_slash(&mut viewer, "IL9R");
        assert!(viewer.search.has_matcher());
    }
    #[test]
    fn a_query_with_nothing_loaded_builds_no_matcher() {
        let mut viewer = new_viewer();
        type_slash(&mut viewer, "IL9R");
        assert!(
            !viewer.search.has_matcher(),
            "no locus_index means nothing to build yet"
        );
    }

    pub(super) fn settle_search(viewer: &mut GenomeViewer, query: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !viewer.search.tick(query, &viewer.locus_index) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "locus search did not converge in time"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn search_matches_retain_the_locus_key_allocation() {
        let mut viewer = viewer_with_loaded_entries("search_shared_arc");
        let idx = &viewer.locus_index;
        let feature_index = idx.active().unwrap();
        let key_in_map = feature_index.keys().next().expect("fixture has at least one entry");
        let (_, locus) = feature_index.locus_by_key(&key_in_map).expect("key exists");
        assert!(
            Arc::ptr_eq(&key_in_map, &locus.key),
            "loci_by_key and loci must share the same Arc allocation"
        );
        settle_search(&mut viewer, &key_in_map);

        let key_in_search = viewer
            .search
            .latest_matches()
            .iter()
            .map(|m| &m.key)
            .find(|k| Arc::ptr_eq(k, &key_in_map))
            .expect("key must be retained by the search worker");
        assert!(
            Arc::ptr_eq(key_in_search, &key_in_map),
            "search must share the same Arc, not a copy"
        );
    }

    #[test]
    fn search_enter_jumps_to_the_top_match_and_does_not_flash() {
        let mut viewer = viewer_with_loaded_entries("search_enter_match");
        settle_search(&mut viewer, "IL9R");

        let top_key = viewer
            .search
            .latest_matches()
            .first()
            .expect("a match for IL9R")
            .key
            .clone();
        let idx = &viewer.locus_index;
        let feature_index = idx.active().unwrap();
        let (expected_index, expected_locus) = feature_index.locus_by_key(top_key.as_ref()).unwrap();
        let expected_start = expected_locus.start;
        let expected_transcript = expected_locus.transcript_id().to_string();
        let expected_contig = feature_index.contig_for_index(expected_index).unwrap().to_string();
        let selected = viewer.search.resolved().first().unwrap().clone();
        assert!(viewer.search_enter(&selected).is_none());

        assert_eq!(viewer.active_contig, expected_contig);
        assert_eq!(viewer.centre.value(), expected_start);
        assert_eq!(viewer.settings.feature_filter.include, vec![expected_transcript]);
    }

    #[test]
    fn search_enter_with_a_missing_reference_contig_flashes_instead_of_panicking() {
        let mut viewer = new_viewer();
        let selected = ResolvedMatch {
            key: Arc::from("GENE:NM_000000.1:ex1"),
            bold_char_indices: Vec::new(),
            destination: MatchTarget::Locus {
                contig: "missing_contig".to_string(),
                start: BasePair(10),
                end: BasePair(20),
                transcript_id: "NM_000000.1".to_string(),
            },
        };

        assert_eq!(
            viewer.search_enter(&selected).as_deref(),
            Some("missing_contig is not in the loaded reference")
        );
        assert_ne!(viewer.active_contig, "missing_contig");
    }

    /// This viewer reads only its own corpus's destination. A row match cannot
    /// reach it in practice — matches are resolved by the index that produced
    /// them — but the arm must refuse rather than jump somewhere invented.
    #[test]
    fn search_enter_on_a_destination_this_viewer_does_not_own_refuses() {
        let mut viewer = new_viewer();
        let before = viewer.centre.value();
        let selected = ResolvedMatch {
            key: Arc::from("TP53"),
            bold_char_indices: Vec::new(),
            destination: MatchTarget::Row(41_882),
        };

        assert!(viewer.search_enter(&selected).is_some(), "a refusal must be reported");
        assert_eq!(viewer.centre.value(), before, "and must not move the viewport");
    }

    #[test]
    fn search_enter_with_no_matches_has_no_resolved_entry() {
        let mut viewer = viewer_with_loaded_entries("search_enter_nomatch");
        settle_search(&mut viewer, "zzz_no_such_gene");

        assert!(viewer.search.resolved().is_empty());
    }

    fn switch_to_fixture_contig(viewer: &mut GenomeViewer) -> String {
        let contig = viewer
            .locus_index
            .active()
            .unwrap()
            .contig_for_index(0)
            .unwrap()
            .to_string();
        viewer.switch_active_contig(contig.clone());
        contig
    }

    #[test]
    fn nextfeature_jumps_to_the_next_feature_start_on_the_active_contig() {
        let mut viewer = viewer_with_loaded_entries("nextfeature");
        let contig = switch_to_fixture_contig(&mut viewer);
        viewer.centre.jump_to(BasePair(0), BasePair(u64::MAX));
        let expected = viewer
            .locus_index
            .active()
            .unwrap()
            .next_start(&contig, BasePair(0), &FeatureFilter::none())
            .expect("fixture has a feature after 0");

        type_command(&mut viewer, "nextfeature").expect("nextfeature applies");

        assert_eq!(viewer.centre.value(), expected);
    }

    #[test]
    fn prevfeature_jumps_to_the_previous_feature_start_on_the_active_contig() {
        let mut viewer = viewer_with_loaded_entries("prevfeature");
        let contig = switch_to_fixture_contig(&mut viewer);
        let far = viewer.contig_length();
        viewer.centre.jump_to(far, BasePair(u64::MAX));
        let expected = viewer
            .locus_index
            .active()
            .unwrap()
            .prev_start(&contig, far, &FeatureFilter::none())
            .expect("fixture has a feature before this position");

        type_command(&mut viewer, "prevfeature").expect("prevfeature applies");

        assert_eq!(viewer.centre.value(), expected);
    }

    #[test]
    fn jump_to_feature_with_nothing_loaded_flashes_and_does_not_move() {
        let mut viewer = new_viewer();
        let before = viewer.centre.value();

        type_command(&mut viewer, "nextfeature").expect("nextfeature applies");

        assert_eq!(viewer.centre.value(), before);
        assert!(viewer.notify.has_pending());
    }

    fn other_contig(viewer: &GenomeViewer) -> String {
        viewer
            .ref_genome
            .index
            .contig_names()
            .find(|name| *name != viewer.active_contig)
            .expect("hs1 fallback has more than one contig")
    }

    #[test]
    fn contig_position_starts_at_zero_then_restores_the_previous_centre() {
        let mut viewer = new_viewer();
        viewer.centre.jump_to(BasePair(500), BasePair(1_000_000));
        let original = viewer.active_contig.clone();
        let other = other_contig(&viewer);

        viewer.switch_active_contig(other.clone());
        assert_eq!(viewer.active_contig, other);
        assert_eq!(viewer.centre.value(), BasePair(0));
        viewer.switch_active_contig(original);
        assert_eq!(viewer.centre.value(), BasePair(500));
    }
    #[test]
    fn switching_to_a_shorter_contig_clamps_the_restored_offset() {
        let mut viewer = new_viewer();
        let index = viewer.ref_genome.index.clone();
        let (short_contig, short_length) = index
            .contig_names()
            .filter_map(|name| index.length(&name).map(|len| (name, len)))
            .filter(|(name, _)| *name != viewer.active_contig)
            .min_by_key(|(_, len)| *len)
            .expect("hs1 fallback has more than one contig");

        viewer
            .centre
            .jump_to(short_length + BasePair(1_000_000), BasePair(u64::MAX));
        viewer.switch_active_contig(short_contig);

        assert!(viewer.centre.value() <= short_length);
    }

    #[test]
    fn centre_can_land_exactly_on_either_edge_of_the_contig() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut viewer = new_viewer();
        let contig_length = viewer.contig_length();
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("test terminal");

        viewer.apply_zoom(Some(true), None);

        viewer.centre.jump_to(BasePair(0), viewer.contig_length());
        assert_eq!(
            viewer.centre.value(),
            BasePair(0),
            "centre must land exactly on the first base"
        );
        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("draw at the start edge must not panic");

        let last_base = contig_length - BasePair(1);
        viewer.centre.jump_to(last_base, viewer.contig_length());
        assert_eq!(
            viewer.centre.value(),
            last_base,
            "centre must land exactly on the last base"
        );
        terminal
            .draw(|frame| viewer.draw(frame, area, &Keymap::defaults()))
            .expect("draw at the end edge must not panic");
    }
}
