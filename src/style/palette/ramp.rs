//! ANSI256 ramps — the vocabulary every palette colour is authored from.
//!
//! A ramp is a hue with five brightness steps, authored to roughly line up in brightness across
//! hues. That is what makes a palette authorable
//! without an ANSI lookup table — pick the hue for meaning, pick the step for emphasis.
//!
//! Every hue is a function, and so is [`grey`], so all of them read alike at a call site even though
//! grey has 24 steps to a hue's five. Nothing has to remember which: every accessor clamps.
//!
//! Indices 0–15 are excluded deliberately: terminal schemes override them, so they are the one part
//! of the 256-colour space whose appearance we cannot predict.
//!
//! The hue ramps are **accent colours only**. Anything wanting a dark surface or near-white text
//! takes it from [`GREY_SCALE`], the raw ANSI greyscale, which exists so the full range stays
//! reachable.
//!
//! Invariant 8 is unchanged: every entry below is a hand-picked `Color::Indexed` literal, and this
//! is the one file they may live in.

use ratatui::style::Color;

/// Brightness steps per hue ramp.
pub const RAMP_STEPS: usize = 5;

/// A hue ramp: five indices, ordered dark to bright.
///
/// This is the lookup table behind the authoring functions. Palette entries call [`red`], [`cyan`]
/// and friends instead of touching it; the named table remains test-only until a palette parser
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ramp([Color; RAMP_STEPS]);

impl Ramp {
    /// Returns the colour at `step`, clamped to the ramp's ends.
    ///
    /// Clamping rather than panicking: a palette spec is user input, and an out-of-range step
    /// should pin to the end of the ramp rather than take down the app mid-frame.
    pub const fn at(self, step: usize) -> Color {
        self.0[if step >= RAMP_STEPS { RAMP_STEPS - 1 } else { step }]
    }

    /// The ramp's entries, dark to bright.
    pub(crate) const fn steps(self) -> [Color; RAMP_STEPS] {
        self.0
    }
}

const RED_RAMP: Ramp = Ramp([
    Color::Indexed(124),
    Color::Indexed(131),
    Color::Indexed(203),
    Color::Indexed(211),
    Color::Indexed(217),
]);

const ORANGE_RAMP: Ramp = Ramp([
    Color::Indexed(94),
    Color::Indexed(130),
    Color::Indexed(202),
    Color::Indexed(179),
    Color::Indexed(222),
]);

const YELLOW_RAMP: Ramp = Ramp([
    Color::Indexed(58),
    Color::Indexed(64),
    Color::Indexed(136),
    Color::Indexed(178),
    Color::Indexed(185),
]);

const GREEN_RAMP: Ramp = Ramp([
    Color::Indexed(22),
    Color::Indexed(65),
    Color::Indexed(34),
    Color::Indexed(40),
    Color::Indexed(46),
]);

const CYAN_RAMP: Ramp = Ramp([
    Color::Indexed(24),
    Color::Indexed(30),
    Color::Indexed(36),
    Color::Indexed(42),
    Color::Indexed(117),
]);

const BLUE_RAMP: Ramp = Ramp([
    Color::Indexed(25),
    Color::Indexed(63),
    Color::Indexed(105),
    Color::Indexed(147),
    Color::Indexed(153),
]);

const PURPLE_RAMP: Ramp = Ramp([
    Color::Indexed(127),
    Color::Indexed(164),
    Color::Indexed(171),
    Color::Indexed(213),
    Color::Indexed(219),
]);

/// The authoring surface: one function per hue, taking a brightness step.
///
/// `red(4)` and `grey(3)` are the same call shape even though grey has 24 steps and a hue has 5 —
/// every one of them clamps, so the step counts need not be memorised to write a palette safely.
macro_rules! hue_fns {
    ($($name:ident => $ramp:ident,)*) => {
        $(
            /// Returns this hue at `step`, clamped to the ramp's ends.
            pub const fn $name(step: usize) -> Color {
                $ramp.at(step)
            }
        )*

        /// Every hue ramp, paired with its spec name — the vocabulary `:set palette` resolves
        /// against, and the test oracle's table.
        pub(crate) const RAMPS: [(&str, Ramp); 7] = [$((stringify!($name), $ramp),)*];
    };
}

hue_fns! {
    red => RED_RAMP,
    orange => ORANGE_RAMP,
    yellow => YELLOW_RAMP,
    green => GREEN_RAMP,
    cyan => CYAN_RAMP,
    blue => BLUE_RAMP,
    purple => PURPLE_RAMP,
}

/// Looks up a hue ramp by spec name, case-insensitively.
///
/// Grey is deliberately absent: its step range is 0–23 rather than 0–4, so a spec parser has to
/// treat it as its own arm regardless.
pub(crate) fn ramp_by_name(name: &str) -> Option<Ramp> {
    RAMPS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, r)| *r)
}

/// Locates a colour on the axes, as `(axis, step)`.
///
/// Lets a picker open *on* an entry's current value rather than at an arbitrary origin, so moving an
/// axis is a change from where the user already is. `None` for a colour authored outside the ramps,
/// which invariant 8 forbids but a future palette import could still produce.
pub(crate) fn locate_color(color: Color) -> Option<(usize, usize)> {
    axes()
        .iter()
        .enumerate()
        .find_map(|(index, axis)| (0..axis.len()).find(|&step| axis.at(step) == color).map(|s| (index, s)))
}

/// One axis of the picker: a named colour family that can be stepped through.
///
/// Exists so a picker traverses "every colour family" without hardcoding "the seven hues, plus grey
/// as a special case". Grey genuinely is a special case — 24 steps to a hue's 5 — but that difference
/// is a `len`, which a caller can handle uniformly.
/// Adding a hue to [`hue_fns!`] extends this automatically; a picker that built its own list would
/// silently keep showing seven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Axis {
    pub(crate) name: &'static str,
    /// `None` for grey, which is not a [`Ramp`].
    ramp: Option<Ramp>,
}

impl Axis {
    /// Steps available on this axis.
    pub(crate) const fn len(self) -> usize {
        match self.ramp {
            Some(_) => RAMP_STEPS,
            None => GREY_SCALE.len(),
        }
    }

    /// The colour at `step`, clamped — so no reachable position is an invalid value.
    pub(crate) const fn at(self, step: usize) -> Color {
        match self.ramp {
            Some(ramp) => ramp.at(step),
            None => grey(step),
        }
    }
}

/// Every colour family, in picker order: the hue ramps as declared, then grey.
pub(crate) fn axes() -> Vec<Axis> {
    RAMPS
        .iter()
        .map(|(name, ramp)| Axis {
            name,
            ramp: Some(*ramp),
        })
        .chain(std::iter::once(Axis {
            name: "grey",
            ramp: None,
        }))
        .collect()
}

/// The raw ANSI greyscale (indices 232–255), dark to bright.
///
/// Not a [`Ramp`]: this exists so the full greyscale stays reachable for surfaces, banding, and
/// text, where fine gradations matter more than matching a hue's brightness.
pub const GREY_SCALE: [Color; 24] = {
    let mut scale = [Color::Indexed(232); 24];
    let mut k = 0;
    while k < 24 {
        scale[k] = Color::Indexed(232 + k as u8);
        k += 1;
    }
    scale
};

/// Returns a greyscale entry, clamped to the scale's ends.
pub const fn grey(step: usize) -> Color {
    GREY_SCALE[if step >= GREY_SCALE.len() {
        GREY_SCALE.len() - 1
    } else {
        step
    }]
}

// ---------------------------------------------------------------------------
// ANSI256 colour facts.
//
// What an index *is*, independent of any ramp: its sRGB triple, its perceptual
// lightness, and its hue. The ramps are one curated selection out of this space;
// the palette pane's full-space grid orders the whole of it. Both need the same
// arithmetic, and the ramp tests use it as their oracle, so it lives here once.
// ---------------------------------------------------------------------------

/// sRGB channel levels of the 6×6×6 colour cube (indices 16–231).
const CUBE_LEVELS: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];

/// First index of the ANSI greyscale ramp, where the colour cube ends.
pub(crate) const GREYSCALE_START: u8 = 232;

/// The sRGB triple an ANSI256 index renders as, for the cube and greyscale regions.
///
/// Indices below 16 are not addressable by the palette (terminal schemes override them), so this is
/// defined only from 16 up; it treats anything lower as the cube's origin rather than pretending to
/// know a colour the terminal owns.
pub(crate) fn srgb(index: u8) -> (f64, f64, f64) {
    if index >= GREYSCALE_START {
        let value = 8.0 + (f64::from(index) - f64::from(GREYSCALE_START)) * 10.0;
        return (value, value, value);
    }
    let offset = usize::from(index.saturating_sub(16));
    (
        CUBE_LEVELS[offset / 36],
        CUBE_LEVELS[(offset / 6) % 6],
        CUBE_LEVELS[offset % 6],
    )
}

/// CIE L\* — perceptual lightness, which the picker grid sorts by.
pub(crate) fn lstar(index: u8) -> f64 {
    fn linearize(channel: f64) -> f64 {
        let channel = channel / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = srgb(index);
    let y = 0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b);
    if y > 0.008856 {
        116.0 * y.cbrt() - 16.0
    } else {
        903.3 * y
    }
}

/// The index's HSV saturation, 0 for a pure grey and 1 for a fully saturated colour.
///
/// How much colour there is, as distinct from how light it is — the question that decides whether an
/// index's hue angle means anything. A near-grey has a nominal hue that is numerically real and
/// visually meaningless.
pub(crate) fn saturation(index: u8) -> f64 {
    let (r, g, b) = srgb(index);
    let max = r.max(g).max(b);
    if max == 0.0 {
        return 0.0;
    }
    (max - r.min(g).min(b)) / max
}

/// The index's hue angle in degrees, or `None` when it is achromatic and has no hue to speak of.
pub(crate) fn hue_degrees(index: u8) -> Option<f64> {
    let (r, g, b) = srgb(index);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let chroma = max - min;
    if chroma == 0.0 {
        return None;
    }
    let degrees = if max == r {
        60.0 * (((g - b) / chroma) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / chroma + 2.0)
    } else {
        60.0 * ((r - g) / chroma + 4.0)
    };
    Some(degrees.rem_euclid(360.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(color: Color) -> u8 {
        match color {
            Color::Indexed(i) => i,
            other => panic!("ramps hold only indexed colours, found {other:?}"),
        }
    }

    #[test]
    fn every_ramp_colour_is_distinct() {
        let mut seen: Vec<(u8, &str, usize)> = Vec::new();
        for (name, ramp) in RAMPS {
            for (level, color) in ramp.steps().into_iter().enumerate() {
                let i = index_of(color);
                assert!(i > 15, "{name}[{level}] uses terminal-overridden index {i}");
                if let Some((_, other, other_level)) = seen.iter().find(|(s, _, _)| *s == i) {
                    panic!("index {i} is both {other}[{other_level}] and {name}[{level}]");
                }
                seen.push((i, name, level));
            }
        }
    }

    #[test]
    fn every_step_accessor_saturates_rather_than_panicking() {
        let picker_axes = axes();
        assert_eq!(picker_axes.len(), RAMPS.len() + 1, "one axis per ramp plus grey");
        for (name, ramp) in RAMPS {
            let brightest = ramp.at(RAMP_STEPS - 1);
            assert_eq!(ramp.at(RAMP_STEPS), brightest, "{name} must clamp one past the end");
            assert_eq!(ramp.at(usize::MAX), brightest, "{name} must clamp at usize::MAX");
        }
        for (axis, (name, ramp)) in picker_axes.iter().zip(RAMPS) {
            assert_eq!(axis.name, name);
            assert_eq!(axis.len(), RAMP_STEPS);
            for step in 0..RAMP_STEPS {
                assert_eq!(axis.at(step), ramp.at(step));
            }
        }
        let grey_axis = picker_axes.last().expect("grey axis");
        assert_eq!(grey_axis.name, "grey");
        assert_eq!(grey_axis.len(), GREY_SCALE.len());
        for (step, expected) in GREY_SCALE.into_iter().enumerate() {
            assert_eq!(grey_axis.at(step), expected);
        }
        assert_eq!(grey(GREY_SCALE.len()), grey(GREY_SCALE.len() - 1));
        assert_eq!(grey(usize::MAX), grey(GREY_SCALE.len() - 1));
        for axis in picker_axes {
            let last = axis.at(axis.len() - 1);
            assert_eq!(axis.at(axis.len()), last, "{} must clamp past its end", axis.name);
            assert_eq!(axis.at(usize::MAX), last, "{} must clamp at usize::MAX", axis.name);
        }
    }

    #[test]
    fn grey_scale_covers_the_ansi_greyscale_in_order() {
        for (index, color) in GREY_SCALE.into_iter().enumerate() {
            assert_eq!(index_of(color), 232 + index as u8);
        }
    }

    #[test]
    fn ramp_names_resolve_case_insensitively() {
        assert_eq!(ramp_by_name("blue"), Some(BLUE_RAMP));
        assert_eq!(ramp_by_name("BLUE"), Some(BLUE_RAMP));
        assert_eq!(ramp_by_name("Purple"), Some(PURPLE_RAMP));
        assert_eq!(ramp_by_name("chartreuse"), None);
        assert_eq!(ramp_by_name(""), None);
        // Grey resolves through `grey()`, not the hue table — its range and rules differ.
        assert_eq!(ramp_by_name("grey"), None);
    }
}
