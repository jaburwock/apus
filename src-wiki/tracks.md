# Tracks

Covers the genome tracks under `src/viewer_app/genome/`. Each track's module docs carry its own
design. This page holds the rules that span files or tracks.

## Every track

- **Pane heights.** Optional tracks have fixed row budgets and their dividers follow their
  visibility. Pileup takes the remaining height and keeps at least one row in a nonempty pane.
- **Fixed rows (invariant 6).** Each row has a named offset from its pane's top and is skipped when
  the pane doesn't reach it. A ladder that computes later rows from the space earlier ones used was
  rejected: track heights are fixed in practice, and a collision shows at once. A computed offset
  also broke for real: the reference sequence row was once `area.height / 2 + 1`, which landed
  right only at height 7.
- **Zoom (invariant 7).** `HorizontalZoom` lives on `GenomeViewerSettings` and every full-width
  renderer reads it each draw. `VerticalZoom` belongs to `AlignmentTrack`, the only track with
  lanes. Check a drawing change at all three reachable states: `(Close, Close)`, `(Close, Mid)` and
  `(Mid, Close)`. They are the pileup framebuffer's reduction scales (1×1, 1×2, 2×1), so a reduced
  cell holds at most two colours and maps exactly onto fg and bg.
- **Both contig edges.** `ViewPosition::left` saturates at 0, and `left + width` can run past the
  contig's end. A renderer turns `underflow` into leading columns (`geometry::leading_columns`) and
  bounds its right side by the contig length. At `Mid`, a final column can contain one in-bounds
  base. Coverage and depth stats count the bases left after reserving whole underflow columns.
- **One reference cache.** `GenomeViewer::reference_data` serves the reference, alignment,
  coverage and local-area renderers. Pass it in; don't add a cache beside it.
  Reference availability comes from this cache. Embedded slices retain chromosome coordinates
  and have no FASTA path.
- **Resident Local Area coverage.** Each embedded reference slice is also the complete BAM
  query interval. A bin needs to fit entirely inside one interval to show depth. Reads that
  overhang the slice still reach the pileup.

## Alignment

- **Paint order is the only precedence.** Both framebuffer planes are positional, so the order in
  `paint_one_lane_against` decides every overlap in this order. It draws the centline, read background,
  arrows, highlight, 5mC, CIGAR signals, then letters. Only multi-cell text
  (the depth indicator, `readlength`, `indellength`) draws after reduction, over the finished
  cells, which is the draw-over that invariant 9 allows.
- **Don't hide a glyph by painting it in its background colour.** The framebuffer clears every
  frame, so an absent glyph is representable: branch before the stamp. Read-base digits were once
  hidden this way, and became legible when the highlight changed the cell behind them.
- **Colour carries the signal at every zoom. Letters are an addition**, drawn only at
  `(Close, Close)`. A new op signal needs a colour that works without its letter.
- **One centline anchor.** `coordinate::marks::centline_mark` sets the column and half-cell for
  every track that marks the centre. `alignment::centline::centline_px` converts it to
  a pixel, and the highlight uses that same pixel. Never derive the parity locally: two
  independent rules let the mark and the pileup drift apart (2026-08-13).
  The local-area coverage highlight uses all reads spanning that base in the resident BAM
  window. Neighbouring windows can show bridging reads before the full cohort is available.
- **Banding uses absolute genomic parity in every read-colour mode.** A mode only chooses the
  pair. Screen-column parity would shimmer under scroll.
- **`readcolor` is the one dial for what a read's background encodes.** A new encoding, such as
  mapping quality or base modification, is a new `ReadColor` variant, not a new setting.
- **5mC bars follow the pileup's CpG calls.** In an aligned query CpG, forward calls land at the C
  and reverse calls at the G. Each bar includes all reads covering its base, with plain pigment for
  depth without a call. When both bases share a column at `Mid`, the base with the higher
  fraction of called reads supplies the bar, with ties left.
- **Depth comes from the alignment fetch.** A BAM window's coverage is computed with its reads
  (`alignment/window_counts.rs`), so a source cannot return reads it hasn't counted. Depth sums
  the allele counts, including deletions and unknown bases. CIGAR reference skips contribute no
  depth and render as `-` connectors on the plain track background at single-lane zoom rather than
  filled read bases. The coverage track and depth readouts
  read it through `AlignmentTrack::coverage_at`, which never fetches, after `AlignmentTrack::draw`
  has requested the windows. Insertion clustering uses five extra bases on either side of the
  BAM query window. Each window returns only its own depth, spanning reads, and cluster medians.
- **Pileup lanes use a fixed tile grid.** BAM fetch windows vary with read span. Reads can cross
  both grids and must keep one lane as they pass between tiles and cache windows.

## Active plans

- [Mid-vertical split-read layout and Braille connectors](plan-split-mid-vertical.md)
