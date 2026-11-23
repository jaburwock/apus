# Style

Covers `src/style/`, `src/viewer_app/painter.rs` and `src/viewer_app/framebuffer.rs`. The colour
rules themselves are invariant 8 and the colour conventions in `CONTRIBUTING.md`. The module docs
carry the ramps, the file format, the built-in palettes and the picker. This page holds what
binds their callers.

## The palette is live, process-wide state

Every accessor (`Text::PRIMARY_FG()`, `Surface::TRACK_BG()`) is a function reading `LIVE`, one
atomic index per entry. Only `:set palette`, `:set colorscheme` and the pane write it. That is sound
because the UI loop is single-threaded, a `:set` applies before the next draw, no worker reads the
palette, and drawing is immediate-mode.

Threading a palette value through `TrackView` was rejected (2026-08-10): a cached `Color` goes stale
on a palette edit wherever the palette lives.

**Never cache a resolved colour across frames.** Nothing enforces this. Watch anything built once and
reused: a held `Line`/`Span`/`Paragraph`, or a pixel buffer built at load.

## Entries are roles, not consumers

- **The namespace says why a colour exists.** `Surface`, `Text`, `Selection`, `Status`, `Chrome`,
  `Coverage` and `Indel` are roles shared across viewers. `Coordinate`, `Reference`, `Alignment`,
  `Features`, `Minimap` and `LocalArea` are data encodings with no safe shared role. Add a domain
  entry only when its meaning, channel or contrast job can't be a shared role.
- **Equal defaults don't make entries interchangeable.** `APP_BG`, `PANEL_BG`, `POPOVER_BG` and
  `LABEL_BG` have different compositing jobs, `FOCUS_BG` and `ACTIVE_TAB_BG` are tuned separately,
  and an `*_FG`/`*_BG` pair never merges, because a background must not become a foreground.
  Channel-agnostic data pigments (no `_FG`/`_BG` suffix) are the deliberate exception: they paint
  both pixel planes and filled badges under `Text::ON_DATA_FG`.
- **Every entry is one colour.** Pairs are carried by name: `_A`/`_B` for colours alternated by an
  index, `_TINTED` for a variant authored against its base, declared on the next line. An array in the
  declaration was rejected because it would give the macro, the spec parser and the pane two entry
  shapes. `ColorPair` is only the return type of the selectors, and nothing computes a tint as an
  index offset.
- **Severity is hue at one brightness.** All four `Status` grounds share one brightness band under
  `Text::ON_STATUS_FG`. Solid data fills (`Reference::BASE_*`, `Indel::*`,
  `Alignment::UNKNOWN_MISMATCH`) share `Text::ON_DATA_FG`.
- **Per-base centre lines use `Text::PRIMARY_FG`.** Other marks with no colour tie use
  `Chrome::MARKER_FG`. Marks coloured to match what they point at keep their own colour.
- `Chrome::bar_color` and `Reference::contig_bg` are separate cycles for separate encodings. Don't
  merge them.
- `App::draw` fills the frame with `Surface::APP_BG` first, so no cell falls through to the
  terminal's background.

## Scope

`style::palette` holds constants and lookups, never drawing. A widget's buffer code stays with the
widget. A constant moves into the palette only on a real second caller, and into a shared namespace
only on a shared visual job. `ChromKey` stays in `contig_grid::chrom` for that reason.

`painter.rs` is the exception: cell mechanics generic enough to share, never which colour or where.
Whole-`Rect` fills (`Buffer::set_style`) stay out of it.

`painter::VERTICAL_RULE` is the one `Close`-zoom centline glyph across the coordinate, reference,
depth and alignment tracks. Don't add a local centline glyph. The `|` in `draw_tickmarks` marks a
tick, not the centline.
