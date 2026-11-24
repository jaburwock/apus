# Contributing

## Project overview

Apus is a lightweight terminal viewer for BAM read alignments and VCF variants on HPC Linux
environments, built with Rust, ratatui, and noodles. Jump from a variant or locus to inspect the
per-read pileup with reference-backed CIGAR and mismatch rendering. Annotation browsing is the
domain of tools such as tgv and IGV.

This guide holds the rules that bind every change. Each module's design is in its module docs; the
[wiki](src-wiki/) holds cross-file rules and active design plans.

## Code doctrine — the order is the rule

Every module and every function is evaluated against these three, **in this order**. Later stages
do not compensate for earlier ones: a clever comment does not rescue a confusing function, and a
thorough test suite does not rescue either. When they conflict, the earlier stage wins.

**1. Structure and clarity first.** The shape of the code carries the meaning. Minimal lines, but
never golfed — density that costs a reader a second pass is a loss, not a win. Prefer making an
invariant impossible to violate in the type system over documenting that it must not be violated.
Fewer moving parts, shallower call chains, and one obvious home for each concern; if explaining
where something lives takes a sentence, it is in the wrong place.

**2. Comments second, sharing the work with the code.** Code and comments split the documentation
job between them — the comment covers what the code cannot say for itself: why this approach, what
was rejected, what will break if it changes, the non-obvious constraint. Never narrate what the
line plainly does. Clear and concise beats complete. A comment that has drifted out of sync is
worse than no comment; delete stale ones on sight rather than leaving them for later.

**3. Tests third, few and earned.** A test is code a human has to review. Every extra one adds
build and run time, and a bloated suite hides its own bugs and gaps behind review fatigue. Prefer
one test over a wide input space to several that each pin one example, and give each test one
reason to fail, which its name states.

Place a test in Tier 1 or Tier 2 before writing it; anything else is Tier 3 (Facade) and should not
be written.

1. **Foundational: is the data right?** Ingest and parsing (VCF, BAM, CIGAR, FASTA), coordinate
   conversion, read classification, caches and indexes checked against a rebuild, and placement:
   column N is the same genomic position in every track (invariant 9). These fail silently, since a
   base one column off still looks plausible, so they get the most care:
   - Property tests and round-trips wherever a cheap oracle exists. Don't fuzz `draw` paths just to
     prove they don't panic.
   - At least one assertion on a rendered `TestBackend` frame wherever two halves of a draw meet:
     the defect lives in the seam, below which every helper can be correct.
   - A fixture that can fail: seed known, non-repeating values and derive the expectation from the
     seed. Generated data that satisfies the assertion at any offset is blind.
2. **Usability and input: does it work?** A key reaches its command, a command resolves and
   reaches its recipients, a jump lands on its locus, a failure is reported rather than panicking.
   One test per path, plus a regression test for a bug actually hit.
3. **Facade: does it look right?** Colour, glyph choice, bold, label wording, centring, spinners,
   animation timing, hand-tuned offsets. Don't write these. The exception is the source-scanning
   guards for project-wide rules (`Color::Rgb`, `Reset`, `DarkGray`, `Black`, `White`, ramp
   authoring, palette test locking), which catch what looking can't.

**Foundational vs facade:** for rendering or output checks, would a wrong result be obvious within
seconds of normal use? If yes, it is facade. If it could be wrong while looking plausible, it is
foundational. (Interactive paths — keybinds, commands, jumps, and error reporting — belong to
Tier 2.)

Standing rules:

- **Delete on contact.** When you touch, retitle, or repair a test that is facade, pins cosmetic UI
  wording/labels, or asserts what the types already guarantee, delete it rather than fixing it, and
  say which went. A `contains`/`starts_with` proxy for cosmetic wording is the same test.
  (Behavioral text — error/diagnostic reporting, formatted coordinates, and command spellings — is
  not facade.)
- **Never assert a default.** A default exists to be retuned. Assert a relationship instead (a
  bounded default lies inside its own setting's bounds). A test whose arithmetic depends on a
  setting sets that field itself, at the test, not through a shared test-settings constructor.
- **Keep it fast**, under about 50ms. A slow test is a fixture problem first.

Apply the doctrine to code already in scope.

## Project invariants — read before any edit

These rules apply across modules. A violation is a bug even when the affected module looks locally
consistent. Source comments cite these invariants by number, so keep the numbering stable.

1. **Coordinates are 0-based internally, always.** Display is 1-based via `BasePair::to_display`/
   `from_display`; ingest from external 1-based formats is via `BasePair::from_1based` — the only
   sanctioned conversion points, never a hand-rolled `+1`/`-1` at a call site. External formats
   (noodles, htslib — both 1-based) convert at first contact on ingest, so internal code never sees
   a 1-based value. → `src-wiki/data.md`
2. **Every bp-denominated value is a `BasePair`.** Anything that could justifiably end in "bp"
   (position, scroll delta, tick span, contig length) is a `BasePair`; terminal column/row counts,
   grid indices, and durations stay plain. Unwrap via `as_u64()`/`as_f64()`.
   → `src-wiki/data.md`
3. **Tag lengths with `BasePair::length(n)` if they can reach `to_display`.** Otherwise plain
   `BasePair(n)` is fine — don't retro-tag values that never reach display.
   → `src-wiki/data.md`
4. **Mouse is always supplemental.** Every click-reachable behavior must already be keyboard/
   command-line-reachable, and deleting all mouse plumbing must leave the app fully functional.
5. **Cross-viewer coordination goes through the router.** When something in one viewer should
   affect another — a jump, a focus change, a setting several viewers apply — the source emits
   commands (typed follow-ups from `apply_outcome`, or a line the user typed) and the router
   delivers them. Viewers hold no references to each other. App-global state (the keymap, the
   palette, notifications, focus) is owned by `App`, not copied into viewers. A direct
   viewer-to-viewer call is a strongly discouraged exception, acceptable only where routing it
   would cost disproportionately more (tens of lines against hundreds) and the exception reads
   naturally; justify it in a comment at the call site.
   → `src-wiki/architecture.md`
6. **Track rows get fixed absolute positions, not a computed degradation ladder.** A new row gets a
   hardcoded position, independently skipped if height doesn't reach it — never shifted to make room
   for a sibling. Applies to every fixed-height track and fixed-row pane (such as `ReferenceTrack`,
   `CoordinateTrack`, `FeaturesTrack`, `MinimapTrack`, `ContigGridTrack`, and `LocalAreaTrack`).
   → `src-wiki/tracks.md`
7. **`Close` means zoomed in, `Mid` means zoomed out.** Name new code after the level it serves
   (`_close`/`_mid`), never coarse/fine/density. `(Mid, Mid)` is unreachable by construction.
   → `src-wiki/tracks.md`
8. **Never `Color::Rgb`, terminal-resolved colours, or raw indices — author from ramps.** macOS
   terminal mangles raw truecolor (ratatui#1972), and terminal-resolved colours (`Color::Reset`,
   `Color::DarkGray`, `Color::Black`, `Color::White`) or ANSI indices 0–15 depend on the user's
   terminal scheme, so no contrast claim or retheme can hold. Every custom colour is a hand-picked
   `Color::Indexed` literal in `style::palette::ramp` (the one file they may live in), and every
   palette default is authored from a ramp (`ramp::cyan(2)`, `ramp::grey(3)`). Source-scanning
   guards enforce both rules. This governs **authored source**, not runtime values: a palette is
   *stored* as raw indices, so a saved theme keeps its meaning when a ramp is retuned, and a *user*
   may pick any index (including 0–15) to deliberately inherit their terminal's colour.
   → `src-wiki/style.md`
9. **A pane, never a margin.** Every genome track spans its full `Rect` width so terminal column N
   is the same genomic position in all of them, and viewers likewise use their full pane without
   carving out internal margins. An element needing space of its own (such as the palette editor)
   gets a real ratatui pane beside the viewer — never an inset carved out of one. Drawing *over*
   content (the depth indicator, popovers, notifications) is the sanctioned alternative; reserving
   part of a pane's width is not.
10. **Tracks are immediate-mode about position.** A track holds no scroll offset — `draw` receives a
    `ViewPosition` and renders it. The shell owns the one position (`viewport::Position`), which is
    what makes invariant 9 hold. Track-owned caches and non-position animations (a count-up) are
    still fine; it's *scroll* that's centralized. Position itself no longer animates — scroll
    interpolation was removed. → `src/viewer_app/viewport.rs`

## Module index

A module's own constraints are in its module docs. Four standing pages hold the rules that span
files; read the one for the area you're touching. Active design plans (`plan-*.md`, linked from the
relevant page) live alongside them until landed and removed.

| Page | Covers |
|---|---|
| [`architecture.md`](src-wiki/architecture.md) | The stack, command rules, status row and layer boundaries: `main.rs`, `app.rs`, `cli.rs`, `input.rs`, `runtime.rs`, `lib.rs`, `src/command/`, `src/keys/`, `jobs.rs`, `io.rs`, `setting.rs`, `search*`, `list_cursor.rs`, `notification.rs`, `popover.rs`, `spinner.rs`, `viewer.rs`, `src/locus/`, and the viewer shells (`viewer_app/mod.rs`, `vcf/`, `genome/{mod,viewerapp,viewerjobs,contig_selector}.rs`) |
| [`tracks.md`](src-wiki/tracks.md) | Rules every genome track follows, and the alignment pileup's: `viewer_app/genome/` tracks, `reference_data.rs`, `zoom.rs` |
| [`style.md`](src-wiki/style.md) | The live palette and the framebuffer: `src/style/`, `viewer_app/painter.rs`, `viewer_app/framebuffer.rs` |
| [`data.md`](src-wiki/data.md) | Coordinates, reference/sequence types, and fixtures: `src/basepair/`, `src/refgenome/`, `src/residue.rs`, `src/testgen/`, `src/cigar.rs`, `testdata/` |

For any drawing change, read "Every track" in [`tracks.md`](src-wiki/tracks.md) and check the
change at each reachable zoom state.

## Current architecture

`App` owns the tabbed shell, the router delivers resolved `CommandOutcome` values, viewers own
their domain state, and genome tracks draw the current view from the shell's position.

- **Adding a track.** A concrete field on `GenomeViewer`, a fixed layout position and an explicit
  draw call, so order and dependencies stay visible. Not a trait-object list. Pass the per-frame
  `TrackView` and any extra inputs explicitly (see `genome/track.rs`).
- **Adding a command.** A descriptor in `command::registry()` (`src/command/mod.rs`) and a resolver
  in `command::parse`; a variant in the recipient's group in `command/protocol.rs` (`ShellCommand`,
  `GenomeCommand`, `TableCommand`) with an arm in its owner's match; and its spelling in
  `command/spelling.rs`. A command several viewers read on their own axis is a shared
  `CommandOutcome` variant with an arm in every viewer. A setting is declared in `src/setting.rs`,
  with its field and behaviour in the owning viewer. The rules are in
  [architecture.md](src-wiki/architecture.md#commands).
- **Ownership.** `App` owns the viewers, the command bar, notifications, the palette pane, the
  keymap and focus ([invariant 5](#project-invariants--read-before-any-edit)). Viewers own their
  settings, domain state, overlays and tracks, and poll their background jobs from `tick()`. A type
  used by one viewer lives under it; shared root leaves (`basepair`, `cigar`, `residue`,
  `refgenome`, `viewer`, `jobs`, `io`, `setting`, `search`, `list_cursor`, `notification`,
  `popover`, `spinner`, `style`) hold what several areas use.
- **Outside tests, `command` and `viewer_app` don't import each other.** They meet only through
  `command/protocol.rs` and `src/viewer.rs`.

## Conventions

- **Use semantic palette roles, never `Color::Reset`, `DarkGray`, `Black`, or `White`.** Use
  `Text::PRIMARY_FG()`, `Text::DIMMED_FG()`, `Surface::APP_BG()`, or `Surface::LABEL_BG()` for an
  opaque label backing ([invariant 8](#project-invariants--read-before-any-edit)).
- **Never cross-assign UI fg/bg.** A `*_BG` surface is never used as a foreground, or a `*_FG`
  role as a background. Channel-agnostic data pigments (no `_FG`/`_BG` suffix: `Reference::BASE_*`,
  `Indel::*`, `Alignment::PILEUP_LANE_*`, `Alignment::HIGHLIGHT_*`, `Alignment::UNKNOWN_MISMATCH`)
  paint both half-block pixel planes and filled badges (always under `Text::ON_DATA_FG()`).
- **Text drawn over a track keeps the background underneath it.** Read the cell (`let bg =
  cell.bg;`) where banding or content varies; use the flat `Surface::TRACK_BG()` only where the
  label must be opaque. → `src/viewer_app/painter.rs`
- Respect the crate layer boundaries (no noodles types in UI/rendering, no `unwrap()` on
  user-facing file I/O). → [Layer boundaries](src-wiki/architecture.md#layer-boundaries)
- Document the public source and reproducible aggregate counts for empirical testgen parameters.
  Exercise columns together through `testgen::generate_sam_line`.
  → [`data.md`](src-wiki/data.md#fixtures)
- **Encode arguments at the command text boundary.** Generated command text (startup lines, command
  files) passes each value through `command::encode_arg`, and omits the line if a value fails to
  encode. Resolvers for such text accept quoted arguments via `unquote`; test that generated and
  typed spellings resolve to the same outcome. Follow-ups carry typed arguments and need neither.
- **Matching references are the user's responsibility.** Don't validate that the fasta, BAM and VCF
  describe the same assembly. Every jump goes through `GenomeViewer::resolve_contig`, which matches
  case-insensitively and then retries with a `chr` prefix, so ClinVar's bare `1` finds `chr1`.
  Widen that function if a real case needs it rather than adding a name-mapping layer. Making a
  mismatch visible is the sanctioned direction; rejecting the load is not.

## Key project files

- The [test data manifest](testdata/README.md) lists every fixture, how to rebuild it, and the
  optional downloads in `testdata/large/`. Read contig lists from the reference index.
- The full ClinVar VCF has its own INFO fields (`CLNSIG`, `CLNDN`, `CLNREVSTAT`, `GENEINFO`, `MC`)
  and no VEP `CSQ`/`ANN`, so don't write a consequence parser against it.
- `src/bin/` holds the fixture generators, `bam_to_sam`, and the `prof_genome` profiling runner
  (behind the `profiling` feature). They consume `src/lib.rs`, whose `pub` surface is deliberately
  minimal.

## Commands

The aliases' rationale is in `.cargo/config.toml`.

```
cargo fmt            # rustfmt with max_width=120 (rustfmt.toml) — run before commit
cargo clippy         # the lint pass; treat warnings as the bar for clean
cargo build --release
cargo nextest run    # the test runner — needs `cargo install cargo-nextest` on a fresh machine
cargo run
cargo ex-tiny        # rendering example: the app against testdata/bam/tiny.bam
cargo ex-medium      # rendering example: the app against testdata/bam/medium.bam
cargo ex-fullgtf     # rendering example: the full RefSeq GTF alone, needs fetch.sh DATA_DIR gtf
cargo ex-full        # rendering example: full reference + full GTF + medium.bam, needs fetch.sh DATA_DIR ref gtf
cargo ex-vcf-clinvar # rendering example: the variant table over the committed ClinVar subset
cargo ex-vcf-clinvar-full # the same at full scale, needs fetch.sh DATA_DIR clinvar
cargo ex-hg002       # rendering example: two HG002 SVs with bundled hg38 reference windows
cargo ex-hg002-full  # the full 133 GiB HG002 BAM with the sampled-indel VCF, needs fetch.sh --bam
cargo ex-hg002-full-gtf # the full HG002 BAM with the depth-annotated CDS-overlap VCF and hg38 RefSeq GTF
cargo prof-hg002     # headless profiling runner against the full HG002 BAM
cargo build-linux    # cross-compile a standalone Linux binary for HPC
```

Run the `ex-*` aliases as `cargo ex-full`, never `cargo run ex-full`. The latter passes `ex-full` to
the binary as a file name, and the app starts with nothing loaded and no error.

**Both `cargo nextest run` and `cargo test` must pass.** Nextest runs each test in its own process,
while `cargo test` runs tests as threads in one process, so unsynchronised global state can pass
only under nextest. Any test that reads or mutates the live palette must take
`palette::lock_for_test()`; the `palette_mutating_tests_hold_the_lock` guard enforces this for
direct palette access. Protect new global mutable state the same way.

**If builds start hanging, count the files in `target/debug/deps`.** Every rebuild under a new
crate hash leaves a fresh set of `*.rcgu.o` objects, so the count climbs with ordinary edit-test
cycles (it once reached 748k files). The objects are hardlinked into `incremental/`, so deleting
them by hand is unsafe: `cargo clean` is the only sweep. `codegen-units = 16` in `Cargo.toml` slows
the growth; its comment has the measurements.
