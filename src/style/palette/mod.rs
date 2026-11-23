//! The palette: one editable vocabulary for every colour the app draws.
//!
//! Entries are grouped by visual responsibility. Defaults use [`ramp`] functions; runtime values
//! are stored as indexed colours so the picker and saved palettes can retain any terminal colour.
//! Selectors below the declaration choose alternating and zoom-dependent members without exposing
//! tables to renderers.
//!
//! See `src-wiki/style.md` for the rules that bind callers.

pub mod builtin;
pub mod file;
pub mod ramp;

use std::sync::atomic::{AtomicU8, Ordering};

use crate::residue::BaseResidue;
use ratatui::style::Color;

/// The lowest ANSI index the palette *authors* from, and the first with a fixed meaning.
///
/// Indices 0–15 sit below it and are the terminal's own scheme: their RGB is whatever the user's
/// terminal says it is, so nothing here can reason about their contrast or retint them. [`ramp`]
/// therefore never names one, and no default is authored from one.
///
/// They are still *selectable* — see [`crate::style::pane`]'s full-space grid. Choosing one is a
/// deliberate request to inherit the terminal's colour, which is the one way to reach a hue outside
/// the 256-colour cube; that is a different act from an authored value accidentally depending on it.
pub(crate) const FIRST_ADDRESSABLE_INDEX: u8 = 16;

/// Converts an authored palette value to the representation held by [`LIVE`].
const fn indexed_value(color: Color) -> u8 {
    match color {
        Color::Indexed(value) => value,
        _ => panic!("palette entries must be indexed colours"),
    }
}

/// A colour and its tinted variant, returned by the selectors that band by parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorPair(pub Color, pub Color);

impl ColorPair {
    /// Selects the second color when `second` is true.
    pub fn pick(self, second: bool) -> Color {
        if second { self.1 } else { self.0 }
    }
}

/// Declares the palette from one grouped list, emitting four things that cannot drift apart: the
/// per-namespace accessors, the default-value table, the live storage, and the ordered [`REGISTRY`].
///
/// The failure being designed out is a colour that reaches one derivation but not another — present
/// in the accessors but missing from the registry, so it looks correct in source while being
/// invisible and uneditable in the pane. Same shape as `ramp.rs`'s `hue_fns!`.
///
/// Accessors are **functions, not constants**, because every one of them reads mutable state: the
/// pane and `:set palette` retint the app at runtime. Each closes over its own index into
/// [`LIVE`], assigned by declaration position — which is also pane and registry order, so one list
/// decides all three.
macro_rules! palette {
    ($(
        $(#[$ns_meta:meta])*
        $ns:ident {
            $(
                $(#[$meta:meta])*
                $name:ident = $value:expr;
            )*
        }
    )*) => {
        /// Every entry's authored value, in declaration order. The palette resets to this.
        pub const DEFAULTS: &[Color] = &[$($($value,)*)*];

        /// Every entry, in declaration order, as `(namespace, name)`.
        ///
        /// Position in this table is the index into [`LIVE`] and [`DEFAULTS`], which is what lets an
        /// accessor, a default, a registry row and a `:set palette` target all name one colour.
        pub const REGISTRY: &[(&str, &str)] = &[
            $($((stringify!($ns), stringify!($name)),)*)*
        ];

        /// The current indexed value for each entry, in declaration order.
        static LIVE: [AtomicU8; DEFAULTS.len()] = [
            $($(AtomicU8::new(indexed_value($value)),)*)*
        ];

        /// Compile-time indices into [`REGISTRY`] and [`LIVE`], grouped by namespace.
        ///
        /// Used by the palette editor's layout table so renaming or removing an entry fails to
        /// compile rather than drifting silently.
        #[allow(non_snake_case)]
        pub(crate) mod idx {
            palette!(@idx_namespaces 0usize; $($ns { $($name)* })*);
        }

        palette!(@namespaces 0usize; $($(#[$ns_meta])* $ns { $($(#[$meta])* $name)* })*);
    };

    (@idx_namespaces $base:expr;) => {};
    (@idx_namespaces $base:expr; $ns:ident { $($name:ident)* } $($rest:tt)*) => {
        pub(crate) mod $ns {
            palette!(@idx_entries $base; 0usize; $($name)*);
        }
        palette!(@idx_namespaces $base + palette!(@count $($name)*); $($rest)*);
    };

    (@idx_entries $base:expr; $offset:expr;) => {};
    (@idx_entries $base:expr; $offset:expr; $name:ident $($rest:tt)*) => {
        pub(crate) const $name: usize = $base + $offset;
        palette!(@idx_entries $base; $offset + 1usize; $($rest)*);
    };

    // Walks the namespaces, threading a running index so each entry's accessor closes over its own
    // slot. `@count` below turns the preceding entries into that offset without a proc macro.
    (@namespaces $base:expr;) => {};
    (@namespaces $base:expr;
        $(#[$ns_meta:meta])* $ns:ident { $($(#[$meta:meta])* $name:ident)* }
        $($rest:tt)*
    ) => {
        $(#[$ns_meta])*
        pub struct $ns;

        impl $ns {
            palette!(@entries $base; 0usize; $($(#[$meta])* $name)*);
        }

        palette!(@namespaces $base + palette!(@count $($name)*); $($rest)*);
    };

    (@entries $base:expr; $offset:expr;) => {};
    (@entries $base:expr; $offset:expr; $(#[$meta:meta])* $name:ident $($rest:tt)*) => {
        $(#[$meta])*
        #[allow(non_snake_case)]
        pub fn $name() -> Color {
            live($base + $offset)
        }

        palette!(@entries $base; $offset + 1usize; $($rest)*);
    };

    (@count) => { 0usize };
    (@count $head:ident $($tail:ident)*) => { 1usize + palette!(@count $($tail)*) };
}

palette! {
    /// Grounds used by the shell, viewers, overlays, labels, and table headers.
    Surface {
        APP_BG = ramp::grey(1);
        TRACK_BG = ramp::grey(2);
        PANEL_BG = ramp::grey(1);
        POPOVER_BG = ramp::grey(8);
        LABEL_BG = ramp::grey(4);
    }

    /// Text grouped by the surface it must remain readable against.
    Text {
        /// Ordinary text stays below the top of the grey ramp so accents can outrank it.
        PRIMARY_FG = ramp::grey(21);
        DIMMED_FG = ramp::grey(10);
        /// Ink drawn over `Status::*_BG` banners and diagnostic cells.
        ON_STATUS_FG = ramp::grey(23);
        /// Ink drawn over solid data pigments (`Reference::BASE_*`, `Indel::*`, `Alignment::UNKNOWN_MISMATCH`).
        ON_DATA_FG = ramp::grey(4);
    }

    /// Grounds for focused, contextual, and persistent selection.
    Selection {
        FOCUS_BG = ramp::grey(6);
        CONTEXT_BG = ramp::grey(4);
        ACTIVE_TAB_BG = ramp::grey(6);
        /// The `:` command line while it captures keystrokes. A hue rather than a grey step, so focus
        /// moving to the bar (as `:help`'s prefill does) is hard to miss.
        INPUT_BG = ramp::cyan(0);
    }

    /// Severity is carried by hue at one brightness level.
    Status {
        ONGOING_BG = ramp::blue(1);
        OK_BG = ramp::green(1);
        WARN_BG = ramp::yellow(1);
        ERROR_BG = ramp::red(1);
    }

    /// Shared rules, bands, markers, and contig bars.
    Chrome {
        DIVIDER_FG = ramp::grey(14);
        RULER_BAND_BG = ramp::grey(6);
        OUT_OF_BOUNDS_BG = ramp::grey(1);
        ACTIVITY_FG = ramp::green(4);
        MARKER_FG = ramp::grey(20);
        CONTIG_BAR_A = ramp::cyan(4);
        CONTIG_BAR_B = ramp::green(4);
        CONTIG_BAR_DIMMED_A = ramp::cyan(0);
        CONTIG_BAR_DIMMED_B = ramp::green(1);
    }

    /// Colours shared by the coverage track and the local-area chart.
    Coverage {
        FG = ramp::cyan(2);
        /// The ground under both charts in 5mC mode, where `Surface::TRACK_BG` would hide pale calls.
        BG = ramp::blue(4);
    }

    /// Indel data pigments, painted on both pixel planes and filled badges under `Text::ON_DATA_FG`.
    Indel {
        DELETION = ramp::red(3);
        INSERTION = ramp::yellow(4);
    }

    /// The genomic ruler's data colour.
    Coordinate {
        NUMBERING_FG = ramp::orange(3);
    }

    /// Reference bases and contig-grid identity colours.
    Reference {
        BASE_A = ramp::green(2);
        BASE_A_TINTED = ramp::green(3);
        BASE_C = ramp::blue(2);
        BASE_C_TINTED = ramp::blue(3);
        BASE_G = ramp::orange(2);
        BASE_G_TINTED = ramp::orange(3);
        BASE_T = ramp::red(2);
        BASE_T_TINTED = ramp::red(3);
        BASE_N = ramp::grey(15);
        BASE_N_TINTED = ramp::grey(17);
        BASE_NULL = ramp::grey(6);
        BASE_NULL_TINTED = ramp::grey(8);
        CONTIG_A_HIGHLIGHT_BG = ramp::purple(1);
        CONTIG_A_BG = ramp::purple(0);
        CONTIG_B_HIGHLIGHT_BG = ramp::green(1);
        CONTIG_B_BG = ramp::green(0);
    }

    /// Colours for reads and their per-base signals in the alignment pileup.
    Alignment {
        PILEUP_LANE_A = ramp::grey(6);
        PILEUP_LANE_A_TINTED = ramp::grey(7);
        PILEUP_LANE_B = ramp::grey(10);
        PILEUP_LANE_B_TINTED = ramp::grey(11);
        READ_FORWARD_BG = ramp::purple(0);
        READ_REVERSE_BG = ramp::orange(0);
        HIGHLIGHT_A = ramp::yellow(4);
        HIGHLIGHT_B = ramp::yellow(3);
        UNKNOWN_MISMATCH = ramp::red(3);
        ARROW_FG = ramp::grey(15);
        MOD_5MC_HYPO = ramp::grey(23);
        /// Must stay clear of the lane greys so uncertain calls remain visible.
        MOD_5MC_UNCERTAIN = ramp::grey(16);
        MOD_5MC_HYPER = ramp::grey(0);
    }

    /// Transcript intron connectors and exon fills.
    Features {
        INTRON_FG = ramp::orange(0);
        EXON_BG = ramp::orange(0);
    }

    /// Whole-contig overview data.
    Minimap {
        CONTIG_BAR_BG = ramp::cyan(0);
        CONTIG_BAR_FG = ramp::cyan(1);
        CONTIG_BAR_HIGHLIGHT_BG = ramp::green(4);
        CONTIG_BAR_HIGHLIGHT_FG = ramp::cyan(4);
    }

    /// Context-track data not shared with the main coverage chart.
    LocalArea {
        BREAKPOINT_COVERAGE_FG = ramp::purple(3);
        VIEWPORT_MARKER_FG = ramp::cyan(3);
        MOD_5MC_NO_DATA = ramp::orange(1);
    }
}

/// The current colour at `index`.
fn live(index: usize) -> Color {
    Color::Indexed(live_index(index))
}

/// The current colour at `index` as its raw ANSI index — the form the palette actually stores.
///
/// Exists so [`command_lines`] need not destructure a `Color` it knows is indexed: [`LIVE`] is
/// `AtomicU8`, so the index is the value and `Color::Indexed` is the wrapper put on for callers that
/// paint with it.
fn live_index(index: usize) -> u8 {
    LIVE[index].load(Ordering::Relaxed)
}

/// Overrides one entry's colour for the rest of the session.
pub(crate) fn set_entry(index: usize, color: Color) {
    let Color::Indexed(value) = color else {
        panic!("palette entries must be indexed colours")
    };
    LIVE[index].store(value, Ordering::Relaxed);
}

#[cfg(test)]
fn restore_defaults() {
    for (slot, &default) in LIVE.iter().zip(DEFAULTS) {
        slot.store(indexed_value(default), Ordering::Relaxed);
    }
}

/// [`restore_defaults`] under the name tests call it by, for the reason they call it: [`LIVE`] is
/// process-wide, so a test that retints without restoring leaks its colours into every test after
/// it. Kept distinct so a production `reset()` call reads as a mistake.
#[cfg(test)]
pub(crate) fn reset() {
    restore_defaults();
}

/// Serialises tests that touch [`LIVE`] and restores defaults when one finishes.
///
/// Poisoning is ignored so a panic still releases the guard and restores the palette for the next
/// test.
#[cfg(test)]
pub(crate) fn lock_for_test() -> impl Drop {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static PALETTE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    /// Restores the palette when the test holding it ends, panic or not.
    struct PaletteGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl Drop for PaletteGuard {
        fn drop(&mut self) {
            restore_defaults();
        }
    }

    let mutex = PALETTE_LOCK.get_or_init(|| Mutex::new(()));
    let guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    restore_defaults();
    PaletteGuard(guard)
}

// ---------------------------------------------------------------------------
// `:set palette` — spec parsing.
//
// Parsing lives here rather than in `command` because ramp names and index bounds are facts about
// the palette. If `command` knew them, adding or renaming a ramp would mean changing two modules.
// `command` carries the raw spec string and this is the only thing that reads it.
// ---------------------------------------------------------------------------

/// Applies an `<entry>:<colour>` spec, e.g. `surface_track_bg:233` or `surface_track_bg:grey(1)`.
///
/// Returns the entry's display name on success. The spec is validated here, at apply time, rather
/// than at parse time, because only `style` knows the vocabulary. A bad ramp, step or index is
/// rejected rather than clamped: stored values can't be invalid, but a user who typed `grey(40)`
/// made a mistake and should be told.
pub(crate) fn apply_spec(spec: &str) -> Result<String, String> {
    let (index, color) = parse_spec(spec)?;
    set_entry(index, color);
    let (ns, name) = REGISTRY[index];
    Ok(format!("{ns}::{name}"))
}

fn parse_spec(spec: &str) -> Result<(usize, Color), String> {
    let (target, colour) = spec
        .split_once(':')
        .ok_or_else(|| format!("expected <entry>:<index|hue(step)>, got `{spec}`"))?;
    let index = entry_index(target.trim())?;
    let color = parse_color(colour.trim())?;
    Ok((index, color))
}

/// Resolves a canonical entry name to its index, accepting a bare name when unambiguous.
/// Matching is case-insensitive and accepts `namespace_name`, `Namespace::NAME`, or `Namespace.NAME`.
fn entry_index(target: &str) -> Result<usize, String> {
    let wanted = target.replace("::", "_").replace('.', "_");
    let matches: Vec<usize> = REGISTRY
        .iter()
        .enumerate()
        .filter(|(_, (ns, name))| {
            let qualified = format!("{ns}_{name}");
            qualified.eq_ignore_ascii_case(&wanted) || name.eq_ignore_ascii_case(&wanted)
        })
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => Err(format!("unknown palette entry: {target}")),
        // A bare name like `bg` exists in several namespaces; say which rather than picking one.
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|&i| format!("{}_{}", REGISTRY[i].0, REGISTRY[i].1))
                .collect();
            Err(format!(
                "ambiguous palette entry `{target}` — did you mean {}?",
                names.join(", ")
            ))
        }
    }
}

/// Parses a colour: either a raw `<index>` or a `<hue>(<step>)` ramp position.
///
/// The bare index is the *storage* form — what [`command_lines`] writes and what a saved palette holds.
/// The ramp spelling remains accepted for hand-edited files, but resolves to an index and is never
/// written back. Ramps are an authoring vocabulary, not a persistence format: retuning a ramp must
/// not change a saved theme.
///
/// Grey is its own arm within the ramp form: it has 24 steps to a hue's 5, so it is absent from the
/// hue table by design.
fn parse_color(spec: &str) -> Result<Color, String> {
    // Ahead of the ramp lookup, since a bare number can never be a ramp spelling.
    //
    // The whole `u8` range is accepted, 0–15 included. Those are the terminal's own scheme rather
    // than fixed colours, so nothing here can check their contrast — but a user asking for one is
    // asking to inherit the terminal's colour on purpose, which is the only way to reach a hue the
    // 256-colour cube does not contain. Parsing rejects what is *malformed*, not what is unwise.
    if let Ok(index) = spec.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }

    let (name, rest) = spec
        .split_once('(')
        .ok_or_else(|| format!("expected <index> or <hue>(<step>), got `{spec}`"))?;
    let step: usize = rest
        .strip_suffix(')')
        .ok_or_else(|| format!("expected a closing paren in `{spec}`"))?
        .trim()
        .parse()
        .map_err(|_| format!("step must be a number, got `{rest}`"))?;
    let name = name.trim();

    // Bounds are rejected rather than clamped: the accessors clamp so no *stored* value can be
    // invalid, but a user who typed `grey(40)` made a mistake and should be told, not silently
    // given grey(23).
    if name.eq_ignore_ascii_case("grey") || name.eq_ignore_ascii_case("gray") {
        let max = ramp::GREY_SCALE.len() - 1;
        return if step <= max {
            Ok(ramp::grey(step))
        } else {
            Err(format!("grey step must be 0–{max}, got {step}"))
        };
    }
    let ramp = ramp::ramp_by_name(name).ok_or_else(|| format!("unknown colour ramp: {name}"))?;
    let max = ramp::RAMP_STEPS - 1;
    if step <= max {
        Ok(ramp.steps()[step])
    } else {
        Err(format!("{name} step must be 0–{max}, got {step}"))
    }
}

/// The current colour of the entry at `index`, for the pane's swatches.
///
/// Takes a registry index rather than a name: the pane walks [`REGISTRY`] in order, and resolving a
/// name it just read back into an index would be a lookup with no possible failure to report.
pub(crate) fn entry_color(index: usize) -> Color {
    live(index)
}

/// The whole palette as `set palette` lines, in registry order.
///
/// Used by [`file::save`]. Every entry is listed, not just edited ones: a partial list would
/// depend on the defaults at the time it is replayed, which is the fragility a saved theme should
/// not have.
///
/// Colours are written as raw indices, never as ramp names, so retuning a ramp does not change a
/// saved theme. [`parse_color`] still accepts ramp spellings on input.
///
/// Written without the `:` a user types: the trigger belongs to the command bar, not to the
/// language, so a file holds bare lines. Both spellings resolve identically.
pub(crate) fn command_lines() -> Vec<String> {
    REGISTRY
        .iter()
        .enumerate()
        .map(|(index, (ns, name))| format!("set palette {ns}_{name}:{}", live_index(index)))
        .collect()
}

// ---------------------------------------------------------------------------
// Selectors. Most pick between named entries above; continuous or ordinal data
// scales (such as CpG density) map directly onto a fixed ramp instead of
// exposing per-step editable slots.
// ---------------------------------------------------------------------------

impl Chrome {
    /// Background for a ruler banding cell, alternating by column index.
    pub fn ruler_band(index: u64) -> Color {
        if index.is_multiple_of(2) {
            Surface::TRACK_BG()
        } else {
            Self::RULER_BAND_BG()
        }
    }

    /// Alternates the selected or dimmed bar colors by drawn-cell index.
    pub fn bar_color(index: usize, highlighted: bool) -> Color {
        match (index.is_multiple_of(2), highlighted) {
            (true, true) => Self::CONTIG_BAR_A(),
            (false, true) => Self::CONTIG_BAR_B(),
            (true, false) => Self::CONTIG_BAR_DIMMED_A(),
            (false, false) => Self::CONTIG_BAR_DIMMED_B(),
        }
    }
}

impl Reference {
    /// Selects a contig-name background by grid position.
    pub fn contig_bg(index: usize, highlighted: bool) -> Color {
        match (index.is_multiple_of(2), highlighted) {
            (true, true) => Self::CONTIG_A_HIGHLIGHT_BG(),
            (true, false) => Self::CONTIG_A_BG(),
            (false, true) => Self::CONTIG_B_HIGHLIGHT_BG(),
            (false, false) => Self::CONTIG_B_BG(),
        }
    }

    /// Maps a base to its display color.
    pub fn base_color(base: BaseResidue) -> Color {
        Self::base_color_pair(base).pick(false)
    }

    /// Returns the base color and its Mid-zoom variant.
    pub fn base_color_pair(base: BaseResidue) -> ColorPair {
        match base {
            BaseResidue::A => ColorPair(Self::BASE_A(), Self::BASE_A_TINTED()),
            BaseResidue::C => ColorPair(Self::BASE_C(), Self::BASE_C_TINTED()),
            BaseResidue::G => ColorPair(Self::BASE_G(), Self::BASE_G_TINTED()),
            BaseResidue::T => ColorPair(Self::BASE_T(), Self::BASE_T_TINTED()),
            BaseResidue::N => ColorPair(Self::BASE_N(), Self::BASE_N_TINTED()),
            BaseResidue::Null => ColorPair(Self::BASE_NULL(), Self::BASE_NULL_TINTED()),
        }
    }
}

impl LocalArea {
    /// Maps the mean SAM ML byte from near-white at 0 to black at 255.
    pub(crate) fn mod_5mc_grey(mean_ml: f64) -> Color {
        let lightest = ramp::GREY_SCALE.len() - 1;
        let step = (mean_ml * lightest as f64 / f64::from(u8::MAX)).round() as usize;
        ramp::grey(lightest.saturating_sub(step))
    }

    /// Maps reference CpG density (`cpg / valid_bases`) across `ramp::cyan` in 2% steps:
    /// `[0%, 2%)` → `cyan(4)` (lightest) through `>= 8%` → `cyan(0)` (darkest), spanning
    /// genome background (~1%) to dense CpG islands (~4–10%).
    pub(crate) fn cpg_density_color(density: f64) -> Color {
        let lightest = ramp::RAMP_STEPS - 1;
        let step = (density * 50.0) as usize;
        ramp::cyan(lightest.saturating_sub(step))
    }
}

impl Alignment {
    /// Returns the plain and tinted lane colors at `idx`.
    pub fn pileup_lane_color_pair(idx: usize) -> ColorPair {
        if idx.is_multiple_of(2) {
            ColorPair(Self::PILEUP_LANE_A(), Self::PILEUP_LANE_A_TINTED())
        } else {
            ColorPair(Self::PILEUP_LANE_B(), Self::PILEUP_LANE_B_TINTED())
        }
    }

    /// The read-background pair for a strand, in the `direction` read-colour mode.
    ///
    /// Carries both facts by splitting the pair: the strand hue takes the untinted member, the
    /// lane's own tint stays as the other. The caller's absolute-position parity then alternates
    /// between them, so strand reads from the hue cells and lane from the greys between. The tint is
    /// taken from `pileup_lane_color_pair` rather than restated, so the two modes cannot drift.
    ///
    /// `is_reverse` rather than the viewer's `Strand` keeps `style` a leaf that no viewer module has
    /// to be compiled before.
    pub fn read_strand_color_pair(is_reverse: bool, lane_idx: usize) -> ColorPair {
        let hue = if is_reverse {
            Self::READ_REVERSE_BG()
        } else {
            Self::READ_FORWARD_BG()
        };
        ColorPair(hue, Self::pileup_lane_color_pair(lane_idx).pick(true))
    }

    /// The highlighted centline colour for lane `idx`.
    ///
    /// Takes only the lane: the centline occupies one column, so it has no horizontal axis to vary
    /// along, and keying anything here to genomic position makes the whole line flip as you scroll.
    pub fn highlight_color(idx: usize) -> Color {
        if idx.is_multiple_of(2) {
            Self::HIGHLIGHT_A()
        } else {
            Self::HIGHLIGHT_B()
        }
    }

    /// Whether `color` is one of the centline highlight colours.
    ///
    /// Exists for the pileup's frame assertions, which ask "did the centline paint this cell" and
    /// otherwise each rebuild the set themselves. Test-only until something in the app needs to ask.
    #[cfg(test)]
    pub fn is_highlight(color: Color) -> bool {
        color == Self::HIGHLIGHT_A() || color == Self::HIGHLIGHT_B()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spec that names a real entry and a real ramp step must apply, and the applied colour
    /// must be exactly what the ramp yields — built from `ramp::` directly, never from the parser's
    /// own lookup, so the expectation is independent of the code under test.
    ///
    /// Mutates the global palette, so it restores the default at the end. Nextest's per-process
    /// isolation covers the rest; no other test asserts on a non-default palette.
    #[test]
    fn every_entry_accepts_a_spec_and_takes_the_ramps_colour() {
        let _palette = lock_for_test();
        for (index, (ns, name)) in REGISTRY.iter().enumerate() {
            let target = format!("{ns}_{name}");
            for (hue, step) in [("red", 0), ("cyan", 4), ("grey", 23), ("green", 2)] {
                let expected = if hue == "grey" {
                    ramp::grey(step)
                } else {
                    ramp::ramp_by_name(hue).expect("test hue must exist").steps()[step]
                };
                let spec = format!("{target}:{hue}({step})");
                let applied = apply_spec(&spec).unwrap_or_else(|e| panic!("`{spec}` must apply: {e}"));
                assert_eq!(applied, format!("{ns}::{name}"));
                assert_eq!(live(index), expected, "`{spec}` must store the ramp's colour");
            }
        }
        reset();
    }

    #[test]
    fn setting_an_entry_is_visible_through_its_accessor_and_reset_restores_its_prior_value() {
        let _palette = lock_for_test();
        reset();
        let index = REGISTRY
            .iter()
            .position(|&(namespace, name)| namespace == "Surface" && name == "APP_BG")
            .expect("the app surface must be registered");
        let before = entry_color(index);

        set_entry(index, ramp::purple(3));
        assert_eq!(Surface::APP_BG(), ramp::purple(3));

        reset();
        assert_eq!(Surface::APP_BG(), before);
    }

    /// Every `u8` must apply, including terminal-owned indices 0–15. Those values are deliberately
    /// accepted because choosing them is an explicit request to inherit the terminal scheme.
    #[test]
    fn every_index_applies() {
        let _palette = lock_for_test();
        let (ns, name) = REGISTRY[0];

        let Color::Indexed(ramp_index) = ramp::purple(3) else {
            unreachable!("ramps hold indexed colors");
        };
        apply_spec(&format!("{ns}_{name}:purple(3)")).expect("a ramp name must apply");
        let by_ramp = live(0);
        apply_spec(&format!("{ns}_{name}:{ramp_index}")).expect("an index must apply");
        assert_eq!(live(0), by_ramp, "the ramp and index spellings must agree");

        let off_ramp = (FIRST_ADDRESSABLE_INDEX..=u8::MAX)
            .find(|&index| ramp::locate_color(Color::Indexed(index)).is_none())
            .expect("the ramps cover less than the addressable palette");
        for index in 0..=u8::MAX {
            apply_spec(&format!("{ns}_{name}:{index}")).expect("every index must apply");
            assert_eq!(live(0), Color::Indexed(index));
        }
        assert_eq!(live(0), Color::Indexed(u8::MAX));
        apply_spec(&format!("{ns}_{name}:{off_ramp}")).expect("an off-ramp index must apply");
        assert_eq!(live(0), Color::Indexed(off_ramp));
    }

    #[test]
    fn out_of_range_steps_and_unknown_names_are_rejected() {
        // The closing assertion reads `live(0)`, so the palette must hold still for the whole test.
        let _palette = lock_for_test();
        let (ns, name) = REGISTRY[0];
        let entry = format!("{ns}_{name}");
        let before = live(0);
        for spec in [
            format!("{entry}:grey(24)"),
            format!("{entry}:red(5)"),
            format!("{entry}:mauve(1)"),
            format!("{entry}:red(-1)"),
            format!("{entry}:red(x)"),
            format!("{entry}:red(1"),
            format!("{entry}:red"),
            "nonesuch:red(1)".to_string(),
            "no-colon-here".to_string(),
            "TRACK:red(1)".to_string(),
            // Past the end of the space. 0–15 are *not* here: they are the terminal's own scheme,
            // selectable on purpose, so the parser has nothing to object to.
            format!("{entry}:256"),
            format!("{entry}:-1"),
        ] {
            assert!(apply_spec(&spec).is_err(), "`{spec}` must be rejected");
        }
        // A rejected spec must not have moved anything.
        assert_eq!(live(0), before, "rejected specs must leave the entry unchanged");
    }

    /// An unambiguous bare name still resolves without typing the namespace.
    #[test]
    fn an_unambiguous_bare_entry_name_resolves() {
        let _palette = lock_for_test();
        assert_eq!(apply_spec("app_bg:grey(4)").as_deref(), Ok("Surface::APP_BG"));
        reset();
    }

    /// The spec parser takes user input, so it must never panic — only ever return `Err`.
    #[test]
    fn arbitrary_specs_never_panic() {
        let _palette = lock_for_test();
        // A small LCG keeps this deterministic and dependency-free.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet: Vec<char> = "abcdefgrey_:()0123456789-. \tRGB[]{}\u{e9}\u{4e2d}".chars().collect();
        for _ in 0..256 {
            let len = (next() % 24) as usize;
            let spec: String = (0..len).map(|_| alphabet[(next() as usize) % alphabet.len()]).collect();
            // The contract is total: any string is either applied or refused, never a panic.
            let _ = apply_spec(&spec);
        }
        reset();
    }

    /// The registry is the editor pane's list, and a `namespace::name` pair is how an entry will be
    /// addressed by `:set palette`. A duplicate would make one of the two unaddressable.
    #[test]
    fn registry_names_are_unique_within_a_namespace() {
        let mut seen = Vec::new();
        for (ns, name) in REGISTRY {
            let key = (*ns, *name);
            assert!(!seen.contains(&key), "duplicate palette entry {ns}::{name}");
            seen.push(key);
        }
    }

    /// This file's own source, which both guards below read.
    fn palette_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/style/palette/mod.rs");
        std::fs::read_to_string(path).expect("palette source must be readable")
    }

    /// Whether `line` declares a palette entry inside `palette! { … }`.
    fn declared_entry_name(line: &str) -> Option<&str> {
        let line = line.trim();
        let name = line.strip_suffix(';')?.split(" = ").next()?;
        let named = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit());
        (named && line.contains(" = ")).then_some(name)
    }

    /// Guards the derivation the macro exists for: the registry must carry *every* declared entry,
    /// not a hand-maintained subset. Counted from the source declaration rather than from the
    /// registry, so the two are independent — reading the count off `REGISTRY` would compare it
    /// with itself.
    #[test]
    fn registry_holds_every_declared_entry() {
        let source = palette_source();
        let declared: Vec<&str> = source.lines().filter_map(declared_entry_name).collect();
        assert!(
            !declared.is_empty(),
            "the declaration must be found, not missed by the scan"
        );
        assert_eq!(
            REGISTRY.len(),
            declared.len(),
            "every declared palette entry must reach the registry"
        );
    }

    /// Palette entries are authored from ramps, never from raw indices — a bare `Color::Indexed`
    /// here is a colour no ramp retune can reach.
    ///
    /// This reads source rather than values because a *newly added* constant is what it needs to
    /// catch, and a test over today's constants cannot see tomorrow's.
    #[test]
    fn palette_entries_are_authored_from_ramps() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/style/palette");
        let mut offenders = Vec::new();
        let mut declarations = 0usize;
        for entry in std::fs::read_dir(&dir).expect("palette dir must be readable") {
            let path = entry.expect("dir entry must be readable").path();
            // ramp.rs is where the literals are supposed to live.
            if path.extension().is_none_or(|e| e != "rs") || path.file_name().is_some_and(|n| n == "ramp.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("palette source must be readable");
            // Only declarations: a mention in prose or in this test's own body is not an offence.
            for (n, line) in source.lines().enumerate() {
                let code = line.trim();
                if declared_entry_name(code).is_none() {
                    continue;
                }
                declarations += 1;
                if code.contains("Color::Indexed") {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    offenders.push(format!("{name}:{} {code}", n + 1));
                }
            }
        }
        // Counting what was examined, not just files opened: with the palette consolidated behind a
        // macro, a predicate that stops matching would make this pass by inspecting nothing.
        assert_eq!(
            declarations,
            REGISTRY.len(),
            "every palette entry must be examined, not skipped by the scan"
        );
        assert!(
            offenders.is_empty(),
            "raw indices outside ramp.rs — author these from a ramp instead:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Collects every `.rs` file under `dir`.
    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("source dir must be readable") {
            let path = entry.expect("dir entry must be readable").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Terminal-resolved colours cannot support a palette contrast claim, and raw truecolor is
    /// mangled by the macOS terminal (invariant 8). Source-scanning catches their reintroduction in
    /// any production path.
    #[test]
    fn no_terminal_resolved_colours_outside_prose() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths = Vec::new();
        rust_sources(&src, &mut paths);

        // Build the spellings so this scanner does not match its own source.
        let banned: [String; 5] = [
            format!("Color::{}", "Rgb"),
            format!("Color::{}", "Reset"),
            format!("Color::{}", "DarkGray"),
            format!("Color::{}", "Black"),
            format!("Color::{}", "White"),
        ];

        let mut offenders = Vec::new();
        for path in paths {
            let source = std::fs::read_to_string(&path).expect("source must be readable");
            for (n, line) in source.lines().enumerate() {
                let code = line.trim();
                // Prose may name them — explaining why they are banned is the reason this file
                // and `framebuffer.rs` mention them at all.
                if code.starts_with("//") {
                    continue;
                }
                if banned.iter().any(|needle| code.contains(needle.as_str())) {
                    let rel = path.strip_prefix(&src).unwrap_or(&path).to_string_lossy().into_owned();
                    offenders.push(format!("{rel}:{} {code}", n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "terminal-resolved colours must go through the palette instead:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Every test that reads or writes [`LIVE`] must hold [`lock_for_test`] while it does.
    ///
    /// `LIVE` is process-wide, so source-scanning catches direct palette use without first taking
    /// the lock. A helper-based raster test still needs to take it explicitly.
    #[test]
    fn palette_mutating_tests_hold_the_lock() {
        let _palette = lock_for_test();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths = Vec::new();
        rust_sources(&src, &mut paths);

        // `reset` is included: it writes the whole palette, so it races exactly like the others.
        let mutators = [
            "apply_spec(",
            "apply_batch(",
            "apply_named(",
            "restore_defaults(",
            "reset()",
        ];
        let mut offenders = Vec::new();

        for path in paths {
            let source = std::fs::read_to_string(&path).expect("source must be readable");
            let lines: Vec<&str> = source.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if line.trim() != "#[test]" {
                    continue;
                }
                // The test body runs from its `fn` to the closing brace at the same indent, found by
                // matching that `fn`'s leading whitespace — every test in this repo is rustfmt'd.
                let Some(start) = (n..lines.len()).find(|&i| lines[i].trim_start().starts_with("fn ")) else {
                    continue;
                };
                let indent = lines[start].len() - lines[start].trim_start().len();
                let closing = format!("{}}}", " ".repeat(indent));
                let end = (start + 1..lines.len())
                    .find(|&i| lines[i] == closing)
                    .unwrap_or(lines.len());

                let body = lines[start..end].join("\n");
                // A `coverage.reset(...)` and friends are method calls on other types, not the
                // palette — only an unqualified or `palette::`-qualified call is a palette write.
                let writes = mutators
                    .iter()
                    .any(|needle| body.contains(&format!(" {needle}")) || body.contains(&format!("::{needle}")));
                let reads_live = [
                    "Surface::",
                    "Text::",
                    "Selection::",
                    "Status::",
                    "Chrome::",
                    "Coverage::",
                    "Indel::",
                    "Coordinate::",
                    "Reference::",
                    "Alignment::",
                    "Features::",
                    "Minimap::",
                    "LocalArea::",
                    "entry_color",
                ]
                .iter()
                .any(|needle| {
                    body.match_indices(needle).any(|(index, _)| {
                        body[..index]
                            .chars()
                            .next_back()
                            .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
                    })
                });
                if (writes || reads_live) && !body.contains("lock_for_test(") {
                    let rel = path.strip_prefix(&src).unwrap_or(&path).to_string_lossy().into_owned();
                    offenders.push(format!("{rel}:{} {}", start + 1, lines[start].trim()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these tests use the process-wide palette without `lock_for_test()`, so they race \
             under `cargo test`:\n  {}",
            offenders.join("\n  ")
        );
    }
}
