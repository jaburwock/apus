//! Bounded read-window residency and persistent pileup lane layout.
//!
//! [`ReadWindowCache`] is deliberately independent of a concrete BAM source: the caller supplies
//! a [`ReadSource`] when resolving a viewport, so asynchronous I/O and rendering state cannot
//! become one owner. A ready window carries both reads and the coverage derived from those same
//! reads; a failed window is also a settled cache entry, preventing a per-frame retry storm while
//! still becoming retryable after normal eviction.
//!
//! Lane assignment is cached separately in grid-aligned tiles. Read locators are revalidated when
//! materialised, and neighbouring tiles provide seam constraints, so MRU promotion or a scroll
//! cannot silently change which read a lane refers to. A landed window invalidates layout across
//! its span and adjacent seams, while an eviction invalidates any tile using that window.
//! The cache also exposes resident coverage for the depth readouts after alignment rendering has
//! completed; it never fetches as a side effect of that read-only query.

use std::collections::{HashMap, HashSet};

use crate::basepair::BasePair;

use super::read::{Read, ReadId};
use super::window_counts::{CoverageWindow, WindowCounts, counts_from_reads};

pub(super) const DEFAULT_WINDOW_SIZING: (usize, BasePair) = (8, BasePair(2_000));
pub(super) const LANE_TILE_LEN: BasePair = BasePair(2_000);

/// Source of synchronous or asynchronous read windows.
pub(super) enum WindowState {
    /// The source has not produced a result yet.
    Pending,
    /// The source produced a window, including a genuinely empty one.
    Ready(WindowData),
    /// The source settled the request with an error.
    Failed(String),
}

pub(super) trait ReadSource {
    /// Drops unsettled requests and replies outside the visible windows.
    fn retain_windows(&mut self, _contig: &str, _starts: &[BasePair]) {}

    /// Advances in-flight fetches outside `draw`, returning whether any window settled.
    fn poll(&mut self) -> bool {
        false
    }

    /// Returns this window's state without blocking.
    fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState;
    /// Requests a window fetch.
    ///
    /// Returned reads must overlap `[start, end)`. Coverage belongs to that same span even
    /// when the source uses extra flanking context to cluster insertions.
    fn request_window(&mut self, contig: &str, start: BasePair, end: BasePair);
    /// Returns whether a fetch is outstanding.
    fn is_fetching(&self) -> bool;
}

impl ReadSource for Box<dyn ReadSource> {
    fn retain_windows(&mut self, contig: &str, starts: &[BasePair]) {
        (**self).retain_windows(contig, starts);
    }

    fn poll(&mut self) -> bool {
        (**self).poll()
    }

    fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState {
        (**self).take_window(contig, start, end)
    }

    fn request_window(&mut self, contig: &str, start: BasePair, end: BasePair) {
        (**self).request_window(contig, start, end)
    }

    fn is_fetching(&self) -> bool {
        (**self).is_fetching()
    }
}

/// Fetched reads and their window-clamped coverage.
pub(crate) struct WindowData {
    pub reads: Vec<Read>,
    /// Coverage indexed by offset from the window start.
    pub(in crate::viewer_app::genome) counts: WindowCounts,
}

impl WindowData {
    /// Builds reads and matching coverage together.
    pub(super) fn from_reads(mut reads: Vec<Read>, window_start: BasePair, window_len: BasePair) -> Self {
        let counts = counts_from_reads(&reads, window_start, window_len);
        let window_end = window_start + window_len;
        reads.retain(|read| read.start < window_end && window_start < read.start + read.length);
        Self { reads, counts }
    }
}

pub(super) struct Window {
    contig: String,
    start: BasePair,
    end: BasePair,
    reads: Vec<Read>,
    /// Coverage over `[start, end)`.
    counts: WindowCounts,
}

enum WindowEntry {
    Ready(Window),
    Failed { contig: String, start: BasePair },
}

impl WindowEntry {
    fn matches(&self, contig: &str, start: BasePair) -> bool {
        match self {
            Self::Ready(window) => window.contig == contig && window.start == start,
            Self::Failed {
                contig: failed_contig,
                start: failed_start,
                ..
            } => failed_contig == contig && *failed_start == start,
        }
    }

    fn is_touched(&self, contig: &str, touched: &[BasePair]) -> bool {
        match self {
            Self::Ready(window) => window.contig == contig && touched.contains(&window.start),
            Self::Failed {
                contig: failed_contig,
                start,
                ..
            } => failed_contig == contig && touched.contains(start),
        }
    }
}

/// Ring buffer of fixed-size read windows.
pub(super) struct ReadWindowCache {
    windows: Vec<WindowEntry>,
    capacity: usize,
    window_len: BasePair,
    /// Layouts for the contiguous tile span most recently used by the viewport.
    layout: LaneLayoutCache,
    /// Errors discovered while resolving a visible window, drained by the viewer.
    errors: Vec<String>,
}

#[derive(Clone, Copy)]
struct Candidate {
    window_start: BasePair,
    read_idx: usize,
    start: BasePair,
    length: BasePair,
    id: ReadId,
}

#[derive(Clone, Copy)]
struct ReadLocator {
    window_start: BasePair,
    read_idx: usize,
    start: BasePair,
    length: BasePair,
    id: ReadId,
}

#[derive(Clone, Copy)]
struct TileAssignment {
    locator: ReadLocator,
    lane: usize,
}

struct LaneTile {
    start: BasePair,
    end: BasePair,
    assignments: Vec<TileAssignment>,
}

#[derive(Default)]
struct LaneLayoutCache {
    tiles: HashMap<(String, BasePair), LaneTile>,
    span: Option<(String, BasePair, BasePair)>,
    dirty: bool,
    #[cfg(test)]
    extend_count: usize,
}

impl LaneLayoutCache {
    fn invalidate_for_window(&mut self, contig: &str, start: BasePair, window_len: BasePair) {
        let (first, end) = tile_span(start, start + window_len);
        let first = first - LANE_TILE_LEN;
        let end = end + LANE_TILE_LEN;
        self.dirty |= self.span.as_ref().is_some_and(|(name, cached_start, cached_end)| {
            name == contig && first < *cached_end && *cached_start < end
        });
    }

    fn invalidate_for_eviction(&mut self, window: &Window) {
        self.dirty = self.dirty
            || self.tiles.iter().any(|((contig, _), tile)| {
                *contig == window.contig
                    && tile
                        .assignments
                        .iter()
                        .any(|assignment| assignment.locator.window_start == window.start)
            });
    }

    fn ensure(&mut self, windows: &[WindowEntry], contig: &str, viewport_start: BasePair, viewport_end: BasePair) {
        let requested = tile_span(viewport_start, viewport_end);
        if requested.0 == requested.1 {
            return;
        }

        if !self.dirty
            && self.span.as_ref().is_some_and(|(cached_contig, start, end)| {
                cached_contig == contig && *start <= requested.0 && requested.1 <= *end
            })
        {
            return;
        }

        let (start, end) = self.build_span(contig, requested.0, requested.1);
        let candidates = collect_layout_candidates(windows, contig, start, end);
        if !self.dirty
            && let Some((cached_contig, cached_start, cached_end)) = self.span.as_ref()
            && cached_contig == contig
            && (start == *cached_start || end == *cached_end)
        {
            self.extend(contig, start, end, *cached_start, *cached_end, &candidates);
            self.span = Some((contig.to_string(), start, end));
            return;
        }
        let preferred = self.preferred_lanes(contig);
        self.tiles = build_tiles(contig, start, end, candidates, &preferred);
        self.dirty = false;
        self.span = Some((contig.to_string(), start, end));
    }

    fn extend(
        &mut self,
        contig: &str,
        start: BasePair,
        end: BasePair,
        old_start: BasePair,
        old_end: BasePair,
        candidates: &[Candidate],
    ) {
        #[cfg(test)]
        {
            self.extend_count += 1;
        }
        let mut preferred = self.preferred_lanes(contig);
        let mut new_tiles: Vec<BasePair> = tile_starts(start, end)
            .into_iter()
            .filter(|tile| *tile < old_start || *tile >= old_end)
            .collect();
        if start < old_start {
            new_tiles.sort_unstable_by(|left, right| right.cmp(left));
        }

        for tile_start in new_tiles {
            let tile_candidates = candidates
                .iter()
                .copied()
                .filter(|candidate| {
                    candidate.start + candidate.length > tile_start && candidate.start < tile_start + LANE_TILE_LEN
                })
                .collect::<Vec<_>>();
            let neighbour_start = if tile_start < old_start {
                tile_start + LANE_TILE_LEN
            } else {
                tile_start.checked_sub(LANE_TILE_LEN).unwrap_or(BasePair(0))
            };
            let fixed = self
                .tiles
                .get(&(contig.to_string(), neighbour_start))
                .into_iter()
                .flat_map(|tile| tile.assignments.iter())
                .filter(|assignment| {
                    assignment.locator.start + assignment.locator.length > tile_start
                        && assignment.locator.start < tile_start + LANE_TILE_LEN
                })
                .map(|assignment| (assignment.locator, assignment.lane))
                .collect::<Vec<_>>();
            let tile = build_tile(tile_start, tile_candidates, &mut preferred, &fixed);
            for assignment in &tile.assignments {
                preferred.insert((assignment.locator.id, assignment.locator.start), assignment.lane);
            }
            self.tiles.insert((contig.to_string(), tile_start), tile);
        }

        // A newly added left tile can contain a read that reaches into the old span. Add that
        // assignment to each old tile it covers; the lane was selected against those tiles' fixed
        // intervals, so this does not disturb their existing assignments.
        let additions = self
            .tiles
            .iter()
            .filter(|((tile_contig, tile_start), _)| {
                tile_contig == contig && (*tile_start < old_start || *tile_start >= old_end)
            })
            .flat_map(|(_, tile)| tile.assignments.iter())
            .copied()
            .collect::<Vec<_>>();
        for assignment in additions {
            let first_tile = tile_grid_start(assignment.locator.start);
            let last_tile = tile_grid_start(assignment.locator.start + assignment.locator.length - BasePair(1));
            let mut tile_start = first_tile;
            while tile_start <= last_tile {
                if tile_start >= old_start
                    && tile_start < old_end
                    && let Some(tile) = self.tiles.get_mut(&(contig.to_string(), tile_start))
                    && !tile.assignments.iter().any(|existing| {
                        existing.locator.id == assignment.locator.id
                            && existing.locator.start == assignment.locator.start
                    })
                {
                    tile.assignments.push(assignment);
                }
                tile_start = tile_start + LANE_TILE_LEN;
            }
        }
    }

    fn preferred_lanes(&self, contig: &str) -> HashMap<(ReadId, BasePair), usize> {
        self.tiles
            .iter()
            .filter(|((name, _), _)| name == contig)
            .flat_map(|(_, tile)| {
                tile.assignments
                    .iter()
                    .map(|assignment| ((assignment.locator.id, assignment.locator.start), assignment.lane))
            })
            .collect()
    }

    fn build_span(&self, contig: &str, requested_start: BasePair, requested_end: BasePair) -> (BasePair, BasePair) {
        let mut start = requested_start;
        let mut end = requested_end;

        if let Some((cached_contig, cached_start, cached_end)) = self.span.as_ref()
            && cached_contig == contig
            && *cached_start <= requested_end
            && requested_start <= *cached_end
        {
            start = start.min(*cached_start);
            end = end.max(*cached_end);
        }

        (start, end)
    }
}

impl ReadWindowCache {
    pub(super) fn new(capacity: usize, window_len: BasePair) -> Self {
        Self {
            windows: Vec::with_capacity(capacity),
            capacity,
            window_len,
            layout: LaneLayoutCache::default(),
            errors: Vec::new(),
        }
    }

    pub(super) fn with_defaults() -> Self {
        let (capacity, window_len) = DEFAULT_WINDOW_SIZING;
        Self::new(capacity, window_len)
    }

    /// Resolves visible reads, fetching touched windows and assigning lanes.
    pub(super) fn resolve(
        &mut self,
        source: &mut impl ReadSource,
        contig: &str,
        viewport_start: BasePair,
        viewport_len: BasePair,
    ) -> Vec<LaneRead<'_>> {
        let viewport_end = viewport_start + viewport_len;
        let touched = windows_touching(viewport_start, viewport_end, self.window_len);

        source.retain_windows(contig, &touched);
        // An oversized pane still needs all of its visible windows. Keep the normal cache bound
        // for smaller panes and evict offscreen windows first even when a miss lands out of order.
        let capacity = self.capacity.max(touched.len());
        self.windows.sort_by_key(|entry| !entry.is_touched(contig, &touched));
        self.trim_windows(capacity);
        for &window_start in &touched {
            self.ensure_resident(source, contig, window_start, capacity);
        }

        self.layout.ensure(&self.windows, contig, viewport_start, viewport_end);

        let mut seen = HashSet::new();
        let mut lane_reads = Vec::new();
        let (first_tile, past_last_tile) = tile_span(viewport_start, viewport_end);
        for tile_start in tile_starts(first_tile, past_last_tile).iter().copied() {
            let Some(tile) = self.layout.tiles.get(&(contig.to_string(), tile_start)) else {
                continue;
            };
            debug_assert_eq!(tile.start, tile_start);
            debug_assert_eq!(tile.end, tile_start + LANE_TILE_LEN);
            for assignment in &tile.assignments {
                let Some(read) = self.read_for_locator(contig, assignment.locator) else {
                    continue;
                };
                if read.start + read.length <= viewport_start || read.start >= viewport_end {
                    continue;
                }
                // Keyed on start as well as id, so a source that over-returns cannot draw a
                // read twice, whether or not it bridges a window boundary.
                if !seen.insert((read.id, read.start)) {
                    continue;
                }
                lane_reads.push(LaneRead {
                    read,
                    lane: assignment.lane,
                });
            }
        }
        lane_reads.sort_by_key(|lane_read| (lane_read.read.start, lane_read.read.id));
        lane_reads
    }

    fn read_for_locator(&self, contig: &str, locator: ReadLocator) -> Option<&Read> {
        self.windows.iter().find_map(|entry| match entry {
            WindowEntry::Ready(window) if window.contig == contig && window.start == locator.window_start => window
                .reads
                .get(locator.read_idx)
                .filter(|read| read.id == locator.id && read.start == locator.start && read.length == locator.length),
            _ => None,
        })
    }

    /// Ensures a grid-aligned window is resident.
    fn ensure_resident(&mut self, source: &mut impl ReadSource, contig: &str, window_start: BasePair, capacity: usize) {
        if self.windows.iter().any(|entry| entry.matches(contig, window_start)) {
            return;
        }
        let window_end = window_start + self.window_len;
        match source.take_window(contig, window_start, window_end) {
            WindowState::Ready(data) => self.insert(contig.to_string(), window_start, data, capacity),
            WindowState::Failed(error) => self.insert_failure(contig.to_string(), window_start, error, capacity),
            WindowState::Pending => source.request_window(contig, window_start, window_end),
        }
    }

    /// Returns the resident window containing `pos`, if any.
    pub(super) fn window_coverage(&self, contig: &str, pos: BasePair) -> Option<CoverageWindow<'_>> {
        self.window_containing(contig, pos).map(coverage_window)
    }

    pub(super) fn window_reads(&self, contig: &str, pos: BasePair) -> &[Read] {
        self.window_containing(contig, pos).map_or(&[], |w| w.reads.as_slice())
    }

    /// Returns resident windows immediately adjacent to the containing window.
    pub(super) fn neighbour_windows(
        &self,
        contig: &str,
        pos: BasePair,
    ) -> (Option<CoverageWindow<'_>>, Option<CoverageWindow<'_>>) {
        let Some(centre) = self.window_containing(contig, pos) else {
            return (None, None);
        };
        let resident = |predicate: &dyn Fn(&Window) -> bool| {
            self.windows.iter().find_map(|entry| match entry {
                WindowEntry::Ready(window) if window.contig == contig && predicate(window) => {
                    Some(coverage_window(window))
                }
                _ => None,
            })
        };
        (
            resident(&|w: &Window| w.end == centre.start),
            resident(&|w: &Window| w.start == centre.end),
        )
    }

    fn window_containing(&self, contig: &str, pos: BasePair) -> Option<&Window> {
        self.windows.iter().find_map(|entry| match entry {
            WindowEntry::Ready(window) if window.contig == contig && window.start <= pos && pos < window.end => {
                Some(window)
            }
            _ => None,
        })
    }

    fn trim_windows(&mut self, capacity: usize) {
        let len = self.windows.len();
        for evicted in self.windows.drain(capacity.min(len)..) {
            if let WindowEntry::Ready(window) = evicted {
                self.layout.invalidate_for_eviction(&window);
            }
        }
    }

    fn insert(&mut self, contig: String, start: BasePair, data: WindowData, capacity: usize) {
        let end = start + self.window_len;
        let WindowData { reads, counts } = data;
        self.layout.invalidate_for_window(&contig, start, self.window_len);
        self.trim_windows(capacity - 1);
        self.windows.insert(
            0,
            WindowEntry::Ready(Window {
                contig,
                start,
                end,
                reads,
                counts,
            }),
        );
    }

    /// A failed window becomes retryable on eviction, keeping a transient I/O failure from
    /// causing a request every frame or blanking the window for the rest of the session.
    fn insert_failure(&mut self, contig: String, start: BasePair, error: String, capacity: usize) {
        self.trim_windows(capacity - 1);
        self.errors.push(format!(
            "Failed to fetch BAM window {contig}:{}: {error}",
            start.as_u64()
        ));
        self.windows.insert(0, WindowEntry::Failed { contig, start });
    }

    /// Drains errors discovered during the previous draw's resolution pass.
    pub(super) fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    /// Test helper that derives coverage from a read list.
    #[cfg(test)]
    pub(super) fn insert_reads(&mut self, contig: String, start: BasePair, reads: Vec<Read>) {
        let data = WindowData::from_reads(reads, start, self.window_len);
        self.insert(contig, start, data, self.capacity);
    }
}

fn coverage_window(w: &Window) -> CoverageWindow<'_> {
    CoverageWindow {
        counts: &w.counts.bases,
        calls: &w.counts.calls,
        insertions: &w.counts.insertions,
        window_start: w.start,
    }
}

/// A read paired with its pileup lane.
pub(super) struct LaneRead<'a> {
    pub read: &'a Read,
    pub lane: usize,
}

fn tile_grid_start(pos: BasePair) -> BasePair {
    pos - (pos % LANE_TILE_LEN)
}

fn tile_span(start: BasePair, end: BasePair) -> (BasePair, BasePair) {
    if end <= start {
        return (start, start);
    }
    let first = tile_grid_start(start);
    let last = tile_grid_start(end - BasePair(1)) + LANE_TILE_LEN;
    (first, last)
}

fn tile_starts(start: BasePair, end: BasePair) -> Vec<BasePair> {
    let mut starts = Vec::new();
    let mut tile = start;
    while tile < end {
        starts.push(tile);
        tile = tile + LANE_TILE_LEN;
    }
    starts
}

fn collect_layout_candidates(
    windows: &[WindowEntry],
    contig: &str,
    span_start: BasePair,
    span_end: BasePair,
) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    windows
        .iter()
        .filter_map(|entry| match entry {
            WindowEntry::Ready(window) if window.contig == contig => Some(window),
            WindowEntry::Ready(_) => None,
            WindowEntry::Failed { .. } => None,
        })
        .flat_map(|window| {
            window.reads.iter().enumerate().map(move |(read_idx, read)| Candidate {
                window_start: window.start,
                read_idx,
                start: read.start,
                length: read.length,
                id: read.id,
            })
        })
        .filter(|candidate| candidate.start + candidate.length > span_start && candidate.start < span_end)
        .filter(|candidate| seen.insert((candidate.id, candidate.start)))
        .collect()
}

fn intervals_overlap(
    intervals: &HashMap<usize, Vec<(BasePair, BasePair)>>,
    lane: usize,
    start: BasePair,
    end: BasePair,
) -> bool {
    intervals
        .get(&lane)
        .is_some_and(|lane_intervals| lane_intervals.iter().any(|&(left, right)| left < end && start < right))
}

fn build_tile(
    start: BasePair,
    mut candidates: Vec<Candidate>,
    preferred: &mut HashMap<(ReadId, BasePair), usize>,
    fixed: &[(ReadLocator, usize)],
) -> LaneTile {
    // Retain existing lanes before assigning new reads introduced by a newly landed window.
    candidates.sort_by_key(|candidate| {
        let key = (candidate.id, candidate.start);
        (!preferred.contains_key(&key), candidate.start, candidate.id)
    });
    let mut intervals: HashMap<usize, Vec<(BasePair, BasePair)>> = HashMap::new();
    let mut assignments = Vec::with_capacity(candidates.len());
    let mut fixed_ids = HashSet::new();
    for &(locator, lane) in fixed {
        if !fixed_ids.insert((locator.id, locator.start)) {
            continue;
        }
        let end = locator.start + locator.length + BasePair(1);
        intervals.entry(lane).or_default().push((locator.start, end));
        assignments.push(TileAssignment { locator, lane });
    }

    let mut next_lane = intervals.keys().copied().max().map_or(0, |lane| lane + 1);
    for candidate in candidates {
        let key = (candidate.id, candidate.start);
        if fixed_ids.contains(&key) {
            continue;
        }
        let end = candidate.start + candidate.length + BasePair(1);
        let lane = preferred
            .get(&key)
            .copied()
            .filter(|&lane| !intervals_overlap(&intervals, lane, candidate.start, end))
            .unwrap_or_else(|| {
                (0..next_lane)
                    .find(|&lane| !intervals_overlap(&intervals, lane, candidate.start, end))
                    .unwrap_or(next_lane)
            });
        next_lane = next_lane.max(lane + 1);
        intervals.entry(lane).or_default().push((candidate.start, end));
        assignments.push(TileAssignment {
            locator: ReadLocator {
                window_start: candidate.window_start,
                read_idx: candidate.read_idx,
                start: candidate.start,
                length: candidate.length,
                id: candidate.id,
            },
            lane,
        });
        preferred.insert(key, lane);
    }

    LaneTile {
        start,
        end: start + LANE_TILE_LEN,
        assignments,
    }
}

fn build_tiles(
    contig: &str,
    span_start: BasePair,
    span_end: BasePair,
    candidates: Vec<Candidate>,
    preferred: &HashMap<(ReadId, BasePair), usize>,
) -> HashMap<(String, BasePair), LaneTile> {
    let mut preferred = preferred.clone();
    let mut tiles = HashMap::new();
    for start in tile_starts(span_start, span_end) {
        let tile_candidates = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.start + candidate.length > start && candidate.start < start + LANE_TILE_LEN)
            .collect::<Vec<_>>();
        let fixed = if start == span_start {
            Vec::new()
        } else {
            tiles
                .get(&(contig.to_string(), start - LANE_TILE_LEN))
                .into_iter()
                .flat_map(|tile: &LaneTile| tile.assignments.iter())
                .filter(|assignment| {
                    assignment.locator.start + assignment.locator.length > start
                        && assignment.locator.start < start + LANE_TILE_LEN
                })
                .map(|assignment| (assignment.locator, assignment.lane))
                .collect()
        };
        let tile = build_tile(start, tile_candidates, &mut preferred, &fixed);
        tiles.insert((contig.to_string(), start), tile);
    }
    tiles
}

/// Returns aligned windows intersecting `[start, end)`.
pub(in crate::viewer_app::genome) fn windows_touching(
    start: BasePair,
    end: BasePair,
    window_len: BasePair,
) -> Vec<BasePair> {
    if end <= start {
        return Vec::new();
    }
    let first = start - (start % window_len);
    first.step_to(end, window_len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cigar::Cigar;

    use super::super::read::Strand;

    fn test_cache(capacity: usize, window_len: BasePair) -> ReadWindowCache {
        ReadWindowCache::new(capacity, window_len)
    }

    fn id(name: &str) -> ReadId {
        ReadId::from_name(name.as_bytes())
    }

    fn read(start: u64, length: u64, name: &str) -> Read {
        Read {
            start: BasePair(start),
            length: BasePair::length(length),
            id: id(name),
            cigar: Cigar::default(),
            seq: None,
            strand: Strand::Forward,
            mod_5mc: None,
        }
    }

    /// Deterministic source for resolve/lane tests.
    struct DenseTestSource {
        reads_per_window: usize,
        min_len: u64,
        max_len: u64,
    }

    impl DenseTestSource {
        fn new(reads_per_window: usize, min_len: u64, max_len: u64) -> Self {
            Self {
                reads_per_window,
                min_len,
                max_len,
            }
        }

        /// Returns the deterministic read at a slot.
        fn slot_read(&self, contig: &str, slot: u64) -> Option<Read> {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            contig.hash(&mut hasher);
            slot.hash(&mut hasher);
            let seed = hasher.finish();

            let spawn = seed % 100 < (self.reads_per_window as u64).min(100);
            if !spawn {
                return None;
            }
            let span = self.max_len - self.min_len + 1;
            let length = self.min_len + (seed / 100) % span;
            Some(Read {
                start: BasePair(slot),
                length: BasePair::length(length),
                id: id(&format!("{contig}:{slot}")),
                cigar: Cigar::default(),
                seq: None,
                strand: Strand::Forward,
                mod_5mc: None,
            })
        }
    }

    impl ReadSource for DenseTestSource {
        fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState {
            let scan_start = start.checked_sub(BasePair(self.max_len)).unwrap_or(BasePair(0));
            let reads: Vec<Read> = (scan_start.as_u64()..end.as_u64())
                .filter_map(|slot| self.slot_read(contig, slot))
                .filter(|r| r.start + r.length > start && r.start < end)
                .collect();
            WindowState::Ready(WindowData::from_reads(reads, start, end - start))
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    struct FixtureSource {
        reads: Vec<Read>,
        taken: Vec<(String, BasePair, BasePair)>,
    }

    impl FixtureSource {
        fn new(reads: Vec<Read>) -> Self {
            Self {
                reads,
                taken: Vec::new(),
            }
        }
    }

    impl ReadSource for FixtureSource {
        fn take_window(&mut self, contig: &str, start: BasePair, end: BasePair) -> WindowState {
            self.taken.push((contig.to_string(), start, end));
            let reads = self
                .reads
                .iter()
                .filter(|read| read.start + read.length > start && read.start < end)
                .cloned()
                .collect();
            WindowState::Ready(WindowData::from_reads(reads, start, end - start))
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    struct UnreachableSource;
    impl ReadSource for UnreachableSource {
        fn take_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) -> WindowState {
            panic!("ReadSource::take_window must not be called when every touched window is already resident");
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    struct FailingSource {
        take_calls: usize,
    }

    impl ReadSource for FailingSource {
        fn take_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) -> WindowState {
            self.take_calls += 1;
            WindowState::Failed("test BAM failure".to_string())
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            false
        }
    }

    struct LandingSource {
        landed: bool,
        read: Read,
    }

    impl ReadSource for LandingSource {
        fn take_window(&mut self, _contig: &str, start: BasePair, end: BasePair) -> WindowState {
            if self.landed {
                WindowState::Ready(WindowData::from_reads(vec![self.read.clone()], start, end - start))
            } else {
                WindowState::Pending
            }
        }

        fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

        fn is_fetching(&self) -> bool {
            !self.landed
        }
    }

    #[test]
    fn windows_touching_uses_the_half_open_grid_span() {
        let cases = [
            (10, 20, vec![0]),
            (50, 150, vec![0, 100]),
            (50, 320, vec![0, 100, 200, 300]),
            (0, 100, vec![0]),
            (50, 50, vec![]),
            (150, 160, vec![100]),
        ];

        for (start, end, expected) in cases {
            assert_eq!(
                windows_touching(BasePair(start), BasePair(end), BasePair(100)),
                expected.into_iter().map(BasePair).collect::<Vec<_>>(),
                "window span for [{start}, {end})"
            );
        }
    }

    #[test]
    fn resolve_fetches_a_known_read_on_a_cache_miss() {
        let mut cache = test_cache(2, BasePair(100));
        let expected = read(12, 7, "known");
        let mut source = FixtureSource::new(vec![expected.clone()]);

        let reads = cache.resolve(&mut source, "chrM", BasePair(0), BasePair(30));

        assert_eq!(source.taken, vec![("chrM".to_string(), BasePair(0), BasePair(100))]);
        assert_eq!(
            reads.iter().map(|lane_read| lane_read.read.id).collect::<Vec<_>>(),
            vec![expected.id]
        );
    }

    #[test]
    fn a_failed_window_is_settled_and_not_requested_again() {
        let mut cache = test_cache(4, BasePair(100));
        let mut source = FailingSource { take_calls: 0 };

        assert!(cache.resolve(&mut source, "chr1", BasePair(0), BasePair(50)).is_empty());
        assert_eq!(source.take_calls, 1);
        assert_eq!(
            cache.take_errors(),
            vec!["Failed to fetch BAM window chr1:0: test BAM failure"]
        );

        assert!(cache.resolve(&mut source, "chr1", BasePair(0), BasePair(50)).is_empty());
        assert_eq!(source.take_calls, 1);
        assert!(cache.take_errors().is_empty());
    }

    #[test]
    fn resolve_multi_window_leaves_both_windows_resident() {
        let mut cache = test_cache(4, BasePair(100));
        let mut source = DenseTestSource::new(50, 10, 50);
        cache.resolve(&mut source, "chrM", BasePair(50), BasePair(100));
        assert_eq!(cache.windows.len(), 2);
        assert!(
            cache
                .windows
                .iter()
                .any(|entry| matches!(entry, WindowEntry::Ready(window) if window.start == BasePair(0)))
        );
        assert!(
            cache
                .windows
                .iter()
                .any(|entry| matches!(entry, WindowEntry::Ready(window) if window.start == BasePair(100)))
        );
    }

    #[test]
    fn resolve_filters_out_reads_not_overlapping_the_viewport() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(5, 10, "a")]);
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(50), BasePair(10));
        assert!(
            reads.is_empty(),
            "a read entirely outside the viewport should be filtered out"
        );
    }

    #[test]
    fn resolve_includes_a_read_starting_before_the_viewport_but_overlapping_it() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(10, 20, "a")]);
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(25), BasePair(10));
        assert_eq!(
            reads.len(),
            1,
            "read spans [10, 30), viewport is [25, 35) — should overlap"
        );
    }

    #[test]
    fn resolve_dedups_a_bridging_read_sharing_an_id_across_two_windows() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(90, 20, "bridge")]);
        cache.insert_reads("chr1".to_string(), BasePair(100), vec![read(90, 20, "bridge")]);
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(200));
        assert_eq!(
            reads.iter().filter(|lr| lr.read.id == id("bridge")).count(),
            1,
            "a read straddling the window boundary must not be double-counted"
        );
    }

    #[test]
    fn resolve_dedups_a_read_that_was_over_returned_by_its_source() {
        struct OverReturningSource {
            over_return: Vec<Read>,
        }

        impl ReadSource for OverReturningSource {
            fn take_window(&mut self, _contig: &str, start: BasePair, end: BasePair) -> WindowState {
                let reads = self.over_return.clone();
                let counts = counts_from_reads(&reads, start, end - start);
                WindowState::Ready(WindowData { reads, counts })
            }

            fn request_window(&mut self, _contig: &str, _start: BasePair, _end: BasePair) {}

            fn is_fetching(&self) -> bool {
                false
            }
        }

        let mut cache = test_cache(4, BasePair(1_000));
        let mut source = OverReturningSource {
            over_return: vec![read(500, 20, "over")],
        };
        cache.resolve(&mut source, "chr1", BasePair(0), BasePair(1_000));
        let reads = cache.resolve(&mut source, "chr1", BasePair(0), BasePair(2_000));
        let count = reads.iter().filter(|lr| lr.read.id == id("over")).count();
        assert_eq!(
            count, 1,
            "a source that over-returns the same read across two adjacent windows \
             must not result in a duplicate read from resolve"
        );
    }

    #[test]
    fn resolve_does_not_dedup_reads_fully_inside_their_own_window_even_if_ids_collide() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(10, 5, "dup")]);
        cache.insert_reads("chr1".to_string(), BasePair(100), vec![read(110, 5, "dup")]);
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(200));
        assert_eq!(
            reads.iter().filter(|lr| lr.read.id == id("dup")).count(),
            2,
            "two distinct non-bridging reads with the same source name must both survive"
        );
    }

    #[test]
    fn resolve_returns_reads_sorted_by_start_then_id() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![read(30, 5, "c"), read(10, 5, "a"), read(10, 5, "b")],
        );
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(100));
        let ids: Vec<ReadId> = reads.iter().map(|lr| lr.read.id).collect();
        let mut expected = vec![
            (BasePair(30), id("c")),
            (BasePair(10), id("a")),
            (BasePair(10), id("b")),
        ];
        expected.sort_unstable();
        assert_eq!(ids, expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>());
    }

    #[test]
    fn resolve_assigns_the_earliest_lane_that_respects_read_spacing() {
        let mut cache = test_cache(2, BasePair(100));
        cache.insert_reads(
            "chr1".to_string(),
            BasePair(0),
            vec![
                read(0, 10, "first"),
                read(10, 10, "adjacent"),
                read(5, 20, "overlap"),
                read(11, 5, "one-base-gap"),
                read(16, 5, "adjacent-to-gap"),
                read(30, 5, "reuses-lowest"),
            ],
        );
        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(100));
        let lane_of = |name: &str| reads.iter().find(|lr| lr.read.id == id(name)).unwrap().lane;

        assert_ne!(lane_of("first"), lane_of("adjacent"));
        assert_ne!(lane_of("first"), lane_of("overlap"));
        assert_eq!(lane_of("one-base-gap"), lane_of("first"));
        assert_ne!(lane_of("adjacent-to-gap"), lane_of("one-base-gap"));
        assert_eq!(lane_of("reuses-lowest"), lane_of("first"));
    }

    #[test]
    fn resolve_persists_a_reads_lane_across_a_later_query_even_as_other_reads_scroll_in_and_out() {
        let mut cache = test_cache(4, BasePair(100));
        let mut source = FixtureSource::new(vec![
            read(90, 40, "persistent"),
            read(105, 12, "competitor"),
            read(80, 8, "departing"),
            read(150, 20, "entering"),
        ]);
        let contig = "chr1";
        let initial = cache.resolve(&mut source, contig, BasePair(80), BasePair(60));
        assert!(initial.iter().any(|lane_read| lane_read.read.id == id("departing")));
        let first = cache.resolve(&mut source, contig, BasePair(90), BasePair(60));
        assert!(first.iter().all(|lane_read| lane_read.read.id != id("entering")));
        let persistent_lane = first
            .iter()
            .find(|lane_read| lane_read.read.id == id("persistent"))
            .unwrap()
            .lane;
        let second = cache.resolve(&mut source, contig, BasePair(100), BasePair(60));
        let persistent = second.iter().find(|lane_read| lane_read.read.id == id("persistent"));

        assert_eq!(persistent.map(|lane_read| lane_read.lane), Some(persistent_lane));
        assert!(second.iter().any(|lane_read| lane_read.read.id == id("entering")));
    }

    #[test]
    fn resolve_reuses_cached_windows_without_refetching() {
        let mut cache = test_cache(4, BasePair(2_000));
        let mut source = FixtureSource::new(vec![read(10, 20, "cached")]);
        cache.resolve(&mut source, "chr1", BasePair(0), BasePair(50));
        let window_count_before = cache.windows.len();

        let reads = cache.resolve(&mut UnreachableSource, "chr1", BasePair(0), BasePair(50));
        assert_eq!(
            reads.iter().map(|lane_read| lane_read.read.id).collect::<Vec<_>>(),
            vec![id("cached")]
        );
        assert_eq!(cache.windows.len(), window_count_before);
    }

    #[test]
    fn resolve_keeps_touched_windows_resident_across_an_intervening_query() {
        let mut cache = test_cache(4, BasePair(100));
        let mut source = DenseTestSource::new(50, 10, 50);
        cache.resolve(&mut source, "chr1", BasePair(0), BasePair(300)); // touches 0, 100, 200
        cache.resolve(&mut source, "chr1", BasePair(1_000), BasePair(50));

        assert!(
            cache
                .windows
                .iter()
                .any(|entry| matches!(entry, WindowEntry::Ready(window) if window.start == BasePair(0)))
        );
        assert!(
            cache
                .windows
                .iter()
                .any(|entry| matches!(entry, WindowEntry::Ready(window) if window.start == BasePair(100)))
        );
        assert!(
            cache
                .windows
                .iter()
                .any(|entry| matches!(entry, WindowEntry::Ready(window) if window.start == BasePair(200)))
        );
    }

    #[test]
    fn resolve_does_not_leak_reads_from_another_contig() {
        let mut cache = test_cache(4, BasePair(100));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(0, 10, "marker")]);
        let reads = cache.resolve(
            &mut DenseTestSource::new(50, 10, 50),
            "chr2",
            BasePair(0),
            BasePair(100),
        );
        assert!(
            reads.iter().all(|r| r.read.id != id("marker")),
            "chr1's hand-inserted marker read must not leak into a chr2 resolve"
        );
    }

    #[test]
    fn visible_reads_keep_their_lanes_during_a_bidirectional_scroll() {
        let reads = vec![
            read(70, 60, "left-bridge"),
            read(120, 80, "middle"),
            read(180, 40, "right-bridge"),
            read(230, 40, "entering-right"),
            read(145, 8, "competitor"),
            read(260, 30, "far-right"),
        ];
        let mut cache = test_cache(4, BasePair(100));
        let mut source = FixtureSource::new(reads);
        let width = BasePair(60);
        let mut previous = HashMap::new();

        for offset in [80, 110, 150, 190, 220, 190, 150, 110] {
            let start = BasePair(offset);
            let end = start + width;
            let current: HashMap<ReadId, (BasePair, BasePair, usize)> = cache
                .resolve(&mut source, "chrM", start, width)
                .iter()
                .map(|lane_read| {
                    (
                        lane_read.read.id,
                        (lane_read.read.start, lane_read.read.length, lane_read.lane),
                    )
                })
                .collect();

            for (read_id, (read_start, read_len, lane)) in &previous {
                let visible = *read_start + *read_len > start && *read_start < end;
                if visible {
                    let (_, _, current_lane) = current.get(read_id).expect("still-visible read remains present");
                    assert_eq!(
                        current_lane, lane,
                        "read {read_id:?} changed lane at viewport {start:?}"
                    );
                }
            }
            previous = current;
        }
    }

    #[test]
    fn window_and_tile_grids_keep_reads_and_depth_aligned_across_scroll_and_eviction() {
        let tile = LANE_TILE_LEN.as_u64();
        let contig = "chr1";
        for (capacity, len) in [(8, 2_000), (8, 8_000), (6, 16_000), (4, 32_000)] {
            let window_len = BasePair(len);
            let mut cache = test_cache(capacity, window_len);
            let mut reads = vec![read(len - tile / 2, len * capacity as u64 + tile, "long-bridge")];
            let mut offsets = Vec::new();
            for k in 1..=capacity as u64 + 2 {
                let seam = len * k;
                for (j, (start, span)) in [
                    (seam - tile, tile / 2 + k),
                    (seam - 217, 401 + k),
                    (seam + 11, 83 + k),
                    (seam + tile / 2, tile / 2 + 127 + k),
                ]
                .into_iter()
                .enumerate()
                {
                    reads.push(read(start, span, &format!("r{k}-{j}")));
                }
                offsets.extend([seam - tile, seam - 130, seam + tile / 2]);
            }
            let mut source = FixtureSource::new(reads.clone());
            let width = BasePair(tile + 233);
            let mut previous = HashMap::new();
            for &offset in offsets.iter().chain(offsets.iter().rev().skip(1)) {
                let start = BasePair(offset);
                let end = start + width;
                let lanes = cache.resolve(&mut source, contig, start, width);
                let expected: HashSet<_> = reads
                    .iter()
                    .filter(|read| read.start < end && read.start + read.length > start)
                    .map(|read| read.id)
                    .collect();
                let current: HashMap<_, _> = lanes.iter().map(|entry| (entry.read.id, entry.lane)).collect();
                assert_eq!(
                    lanes.len(),
                    current.len(),
                    "duplicate read at {offset}, {len} bp windows"
                );
                assert_eq!(
                    current.keys().copied().collect::<HashSet<_>>(),
                    expected,
                    "{offset}, {len} bp windows"
                );
                let mut lane_ends = HashMap::new();
                for entry in lanes {
                    if let Some(end) = lane_ends.insert(entry.lane, entry.read.start + entry.read.length) {
                        assert!(
                            end <= entry.read.start,
                            "overlap in lane {} at {offset}, {len} bp windows",
                            entry.lane
                        );
                    }
                }
                for (id, lane) in &current {
                    if let Some(previous_lane) = previous.get(id) {
                        assert_eq!(lane, previous_lane, "{id:?} moved at {offset}, {len} bp windows");
                    }
                }
                previous = current;

                assert!(cache.windows.len() <= capacity);
                let resident = |start| cache.windows.iter().any(|entry| entry.matches(contig, start));
                for pos in [start, start + width / 2, end - BasePair(1)] {
                    let window_start = BasePair(pos.as_u64() / len * len);
                    let coverage = cache
                        .window_coverage(contig, pos)
                        .expect("visible window stays resident");
                    assert_eq!(coverage.window_start, window_start);
                    assert_eq!(coverage.counts.len(), len as usize);
                    let expected_depth = reads
                        .iter()
                        .filter(|read| read.start <= pos && pos < read.start + read.length)
                        .count();
                    assert_eq!(
                        coverage.counts[(pos - window_start).as_u64() as usize].depth(),
                        expected_depth as u32
                    );
                    let (left, right) = cache.neighbour_windows(contig, pos);
                    assert_eq!(
                        left.map(|window| window.window_start),
                        window_start.checked_sub(window_len).filter(|&p| resident(p))
                    );
                    assert_eq!(
                        right.map(|window| window.window_start),
                        Some(window_start + window_len).filter(|&p| resident(p))
                    );
                }
            }
        }
    }

    #[test]
    fn a_view_wider_than_the_cache_keeps_its_windows_and_releases_them_when_narrowed() {
        let len = BasePair(8_000);
        let mut cache = test_cache(8, len);
        let mut source = FixtureSource::new(Vec::new());
        let first = BasePair(7_999);
        let wide = len * BasePair(8);
        cache.resolve(&mut source, "chr1", first, wide);
        assert_eq!(cache.windows.len(), 9);
        for i in 0..9 {
            assert!(cache.window_coverage("chr1", len * BasePair(i)).is_some());
        }
        cache.resolve(&mut UnreachableSource, "chr1", first, wide);
        cache.resolve(&mut UnreachableSource, "chr1", first, len);
        assert_eq!(cache.windows.len(), 8);
    }

    #[test]
    fn a_read_landing_in_a_stationary_viewport_invalidates_its_tile() {
        let mut cache = test_cache(4, BasePair(2_000));
        let read = read(10, 20, "landed");
        let mut source = LandingSource {
            landed: false,
            read: read.clone(),
        };

        assert!(
            cache
                .resolve(&mut source, "chr1", BasePair(0), BasePair(100))
                .is_empty()
        );
        source.landed = true;
        let reads = cache.resolve(&mut source, "chr1", BasePair(0), BasePair(100));

        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].read.id, read.id);
    }

    #[test]
    fn tiled_layout_matches_an_independent_rebuild_while_extending_both_sides() {
        let mut incremental_cache = test_cache(4, BasePair(2_000));
        let reads = [
            read(1_500, 20, "left-only"),
            read(1_990, 30, "left-seam"),
            read(2_010, 40, "left-competitor"),
            read(3_990, 30, "right-seam"),
            read(4_010, 40, "right-competitor"),
            read(4_500, 20, "right-only"),
        ];
        for window_start in [BasePair(0), BasePair(2_000), BasePair(4_000)] {
            let window_reads = reads
                .iter()
                .filter(|read| read.start + read.length > window_start && read.start < window_start + BasePair(2_000))
                .cloned()
                .collect();
            incremental_cache.insert_reads("chr1".to_string(), window_start, window_reads);
        }
        let positions = [BasePair(2_100), BasePair(4_100), BasePair(1_900), BasePair(2_100)];

        for viewport_start in positions {
            let viewport_end = viewport_start + BasePair(200);
            let cached = incremental_cache
                .resolve(&mut UnreachableSource, "chr1", viewport_start, BasePair(200))
                .into_iter()
                .map(|lr| (lr.read.id, lr.lane))
                .collect::<Vec<_>>();
            let (rebuild_start, rebuild_end) = incremental_cache
                .layout
                .span
                .as_ref()
                .map(|(_, start, end)| (*start, *end))
                .expect("the incremental resolve must establish a layout span");

            let mut direct_cache = test_cache(4, BasePair(2_000));
            for window_start in [BasePair(0), BasePair(2_000), BasePair(4_000)] {
                let window_reads = reads
                    .iter()
                    .filter(|read| {
                        read.start + read.length > window_start && read.start < window_start + BasePair(2_000)
                    })
                    .cloned()
                    .collect();
                direct_cache.insert_reads("chr1".to_string(), window_start, window_reads);
            }
            let rebuilt = direct_cache
                .resolve(
                    &mut UnreachableSource,
                    "chr1",
                    rebuild_start,
                    rebuild_end - rebuild_start,
                )
                .into_iter()
                .filter(|lr| lr.read.start < viewport_end && lr.read.start + lr.read.length > viewport_start)
                .map(|lr| (lr.read.id, lr.lane))
                .collect::<Vec<_>>();

            let mut expected_ids = reads
                .iter()
                .filter(|read| read.start < viewport_end && read.start + read.length > viewport_start)
                .map(|read| (read.start, read.id))
                .collect::<Vec<_>>();
            expected_ids.sort_unstable();
            assert_eq!(
                cached.iter().map(|(read_id, _)| *read_id).collect::<Vec<_>>(),
                expected_ids.into_iter().map(|(_, read_id)| read_id).collect::<Vec<_>>(),
                "all explicit visible reads must remain present at {viewport_start:?}"
            );
            assert_eq!(cached, rebuilt, "cached layout diverged at {viewport_start:?}");
        }

        assert!(
            incremental_cache.layout.extend_count > 0,
            "the scroll sequence must exercise incremental tile extension"
        );
    }

    #[test]
    fn a_read_crossing_a_tile_boundary_keeps_one_lane_in_both_tiles() {
        let mut cache = test_cache(4, BasePair(2_000));
        cache.insert_reads("chr1".to_string(), BasePair(0), vec![read(1_990, 30, "bridge")]);
        cache.insert_reads("chr1".to_string(), BasePair(2_000), vec![read(1_990, 30, "bridge")]);

        cache.resolve(&mut UnreachableSource, "chr1", BasePair(1_900), BasePair(200));

        let lanes: Vec<usize> = cache
            .layout
            .tiles
            .values()
            .flat_map(|tile| {
                tile.assignments
                    .iter()
                    .filter(|assignment| assignment.locator.id == id("bridge"))
                    .map(|assignment| assignment.lane)
            })
            .collect();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0], lanes[1]);
    }

    #[test]
    fn extending_left_adds_a_new_crossing_read_to_the_existing_tile() {
        let existing = Candidate {
            window_start: BasePair(2_000),
            read_idx: 0,
            start: BasePair(2_100),
            length: BasePair::length(20),
            id: id("existing"),
        };
        let crossing = Candidate {
            window_start: BasePair(0),
            read_idx: 0,
            start: BasePair(1_990),
            length: BasePair::length(30),
            id: id("crossing"),
        };
        let mut layout = LaneLayoutCache {
            tiles: build_tiles(
                "chr1",
                BasePair(2_000),
                BasePair(4_000),
                vec![existing],
                &HashMap::new(),
            ),
            span: Some(("chr1".to_string(), BasePair(2_000), BasePair(4_000))),
            ..Default::default()
        };

        layout.extend(
            "chr1",
            BasePair(0),
            BasePair(4_000),
            BasePair(2_000),
            BasePair(4_000),
            &[existing, crossing],
        );

        assert!(
            layout
                .tiles
                .get(&("chr1".to_string(), BasePair(2_000)))
                .unwrap()
                .assignments
                .iter()
                .any(|assignment| assignment.locator.id == id("crossing")),
            "a read introduced by the left tile must also be materialised in the old tile"
        );
    }
}
