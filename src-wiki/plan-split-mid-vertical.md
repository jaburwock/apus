# Plan: Mid-vertical split-read layout and Braille connectors

## Goal

Keep CIGAR `N` (`CigarKind::Skip`) connectors visible at `(Close, Mid)` (`1bp/col`, `2 lanes/row`) without clobbering sibling reads or introducing horizontal-scroll jitter:

1. Avoid placing aligned read bases and skip spans in the same `(2k, 2k+1)` lane pair in `ReadWindowCache`.
2. Render single- and double-lane skip connectors at `(Close, Mid)` with Braille glyphs on `Surface::TRACK_BG()`, with a safe pixel-preserving fallback if `expandinsertions` shifts visual lane parity.

## Design

### 1. Pair-aware lane packing (`src/viewer_app/genome/alignment/read_cache.rs`)

- Extract each read's `CigarKind::Skip` reference intervals (`skip_spans(read) -> Option<Arc<[(BasePair, BasePair)]>>`, returning `None` with zero heap allocation for unspliced reads).
- Carry `skips` on `Candidate` and `TileAssignment` so tile-seam `fixed` assignments retain their skip spans without re-querying windows.
- Track three interval sets per lane inside `build_tile` (`LaneOccupancy`):
  - `full`: `[start, start + length + 1)` — same-lane exclusion (unchanged).
  - `aligned`: complement of `skips` within `[start, start + length)` — exact bp intervals with aligned bases.
  - `skips`: exact `[skip_start, skip_end)` intervals.
- A candidate fits in `lane` iff:
  1. Its padded full span does not overlap `full[lane]`.
  2. None of its `skips` overlap `aligned[lane ^ 1]`.
  3. None of its `aligned` spans overlap `skips[lane ^ 1]`.
- Search lanes via `(0..).find(|&lane| occupancy.can_place(lane, ...))` rather than falling back to `next_lane`, because an odd `next_lane` still pairs with occupied `next_lane - 1` and can conflict with it.
- Consequence:
  - Reads sharing the same splice junction pack into `(2k, 2k+1)` together with zero wasted lanes (`▀` across exons, double-row Braille across the skip).
  - Unspliced reads pack into their own `(2k, 2k+1)` pairs with zero wasted lanes and zero overhead when a tile contains no skips.
  - Unpaired or offset-junction split reads leave their sibling lane (`lane ^ 1`) open over the skip interval.

### 2. Braille skip connectors at `(Close, Mid)` (`src/viewer_app/genome/alignment/pileup/{mod.rs,paint.rs}`)

- At `lanes_per_row == 2`, `draw_reads_against` records a reusable `top_skips: Vec<bool>` mask (length `bp_width`) after painting `lane_offset == 0` (`DisplayLane::Reads` marks `BaseKind::Skip`; `DisplayLane::Expansion` clears it).
- After painting `lane_offset == 1`, call `paint_mid_vertical_skips` over `0..bounds_bp.min(bp_width)`:
  - Skip `centline_x` so `VERTICAL_RULE` (`│`) stays visible across the split.
  - Determine `(top_skip, bot_skip)` from `top_skips[x]` and lane 1's `LaneCoverage` (false for `DisplayLane::Expansion`).
  - If either `fb.pixel(x, top_y)` or `fb.pixel(x, top_y + 1)` is `Some(_)`, leave the cell untouched (handles visual parity shifts when `expandinsertions` inserts an odd number of expansion sub-lanes above).
  - Otherwise stamp `Glyph { ch, fg: Text::DIMMED_FG(), bg: None, bold: false }` with uniform 2-dot vertical pitch:
    - `(true, false)` → `'⠒'` (dots 2, 5)
    - `(false, true)` → `'⣀'` (dots 7, 8)
    - `(true, true)` → `'⣒'` (dots 2, 5, 7, 8)
- `(Close, Close)` and `(Mid, Close)` stay unchanged (`'-'` connectors via `glyphs.skips`).

### 3. Tests & docs

- `read_cache.rs` (Tier 1):
  - Pair-aware packing separates aligned bases from sibling skips, pairs matching-junction split reads into `(2k, 2k+1)` without extra lanes, handles the odd-`next_lane` fallback, and preserves the invariant across `LANE_TILE_LEN` tile seams and layout extension.
- `pileup/mod.rs` (Tier 1 `TestBackend` seam):
  - Verify `(Close, Mid)` renders `'⠒'`, `'⣀'`, and `'⣒'` on `Surface::TRACK_BG()`, preserves `'│'` at the centreline, separates an overlapping unspliced read into the next lane pair, and falls back without clobbering pixels when `expandinsertions` shifts parity.
- `src-wiki/tracks.md`: update the skip bullet to document pair-aware lane packing and `(Close, Mid)` Braille connectors; delete this plan once landed.
