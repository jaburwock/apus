//! The named palettes compiled into the binary, and applying one by name.
//!
//! A built-in is a whole theme rather than a single entry, which is what makes it worth naming: a
//! file of `set palette` lines shipped in the binary. Selecting one is `:set colorscheme <name>`;
//! retinting one entry stays `:set palette <entry>:<colour>`.
//!
//! **Shipped palettes are pinned index snapshots**, including `default dark`. That reverses an
//! earlier decision worth stating: `default dark` used to carry no text, on the grounds that
//! [`super::DEFAULTS`] *was* that palette and a text copy could only drift from it.
//!
//! Drift is now the point. `DEFAULTS` is authored from [`super::ramp`], and the ramps are expected
//! to be retuned — so a named palette defined *as* the defaults would silently change every time one
//! moved, which is the failure the index format exists to prevent. A snapshot means one set of
//! colours forever.

use std::sync::OnceLock;

use ratatui::style::Color;

use super::{DEFAULTS, file::strip_comments, live, parse_spec, set_entry};

/// One selectable palette: a display name, and the lines that produce it.
pub struct BuiltIn {
    pub name: &'static str,
    /// The palette as `set palette` lines — a snapshot of indices, not ramp names, so the theme
    /// cannot move when a ramp is retuned.
    lines: &'static str,
}

/// Every palette selectable by name, in menu order.
pub const BUILT_INS: &[BuiltIn] = &[
    BuiltIn {
        name: "default dark",
        lines: include_str!("default_dark_palette.txt"),
    },
    BuiltIn {
        name: "default light",
        lines: include_str!("default_light_palette.txt"),
    },
    BuiltIn {
        name: "red-green dark",
        lines: include_str!("red_green_dark_palette.txt"),
    },
];

type Snapshot = [Color; DEFAULTS.len()];

fn snapshots() -> &'static [Result<Snapshot, String>] {
    static SNAPSHOTS: OnceLock<Vec<Result<Snapshot, String>>> = OnceLock::new();
    SNAPSHOTS.get_or_init(|| {
        BUILT_INS
            .iter()
            .map(|built_in| {
                let mut colors = std::array::from_fn(|index| DEFAULTS[index]);
                for line in strip_comments(built_in.lines).lines() {
                    let spec = line
                        .strip_prefix("set palette ")
                        .ok_or_else(|| format!("`{}` holds a non-palette line: {line}", built_in.name))?;
                    let (index, color) = parse_spec(spec)?;
                    colors[index] = color;
                }
                Ok(colors)
            })
            .collect()
    })
}

/// Identifies the active colours even when they came from a palette file or a picker edit.
pub(crate) fn current() -> Option<&'static str> {
    BUILT_INS
        .iter()
        .zip(snapshots())
        .find(|(_, snapshot)| {
            snapshot
                .as_ref()
                .is_ok_and(|colors| colors.iter().enumerate().all(|(index, &color)| live(index) == color))
        })
        .map(|(built_in, _)| built_in.name)
}

/// Applies the built-in named `name`, returning its display name.
///
/// Entries missing from a snapshot use their authored defaults, keeping selection independent
/// of the previous scheme.
pub fn apply_named(name: &str) -> Result<&'static str, String> {
    let index = BUILT_INS
        .iter()
        .position(|built_in| built_in.name.eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| {
            let names: Vec<&str> = BUILT_INS.iter().map(|built_in| built_in.name).collect();
            format!("unknown colorscheme `{name}` — try {}", names.join(", "))
        })?;

    let colors = snapshots()[index].as_ref().map_err(Clone::clone)?;
    for (entry, &color) in colors.iter().enumerate() {
        set_entry(entry, color);
    }
    Ok(BUILT_INS[index].name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::palette::{REGISTRY, apply_spec, entry_color, lock_for_test, ramp};

    fn snapshot() -> Vec<ratatui::style::Color> {
        (0..REGISTRY.len()).map(entry_color).collect()
    }

    /// Every built-in must apply cleanly. A palette that fails to resolve would otherwise surface
    /// only as an error on the machine someone selected it on.
    #[test]
    fn every_built_in_applies() {
        let _palette = lock_for_test();
        for built_in in BUILT_INS {
            apply_named(built_in.name).unwrap_or_else(|error| panic!("{}: {error}", built_in.name));
        }
    }

    /// Every shipped snapshot must name every registry entry.
    ///
    /// A snapshot is a *complete* theme; an entry it omits silently falls back to that entry's
    /// authored default, which is a colour from the other theme's world. That is invisible until
    /// someone looks at the one wrong cell, and it is exactly what happens when an entry is added to
    /// the declaration and the snapshots are not regenerated — so this is the test that says
    /// "regenerate them": select the built-in, press `w` in the entry list, and copy the written
    /// `palettes/` file over the committed snapshot.
    ///
    /// Expectations come from `REGISTRY` rather than from the files, so a snapshot cannot satisfy
    /// this by being compared against itself.
    #[test]
    fn every_snapshot_covers_every_entry() {
        for built_in in BUILT_INS {
            let text = strip_comments(built_in.lines);
            let named: Vec<&str> = text
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("set palette ")?
                        .split_once(':')
                        .map(|(entry, _)| entry)
                })
                .collect();
            for (ns, name) in REGISTRY {
                let entry = format!("{ns}_{name}");
                assert!(
                    named.iter().any(|n| n.eq_ignore_ascii_case(&entry)),
                    "`{}` does not set {entry} — regenerate the snapshot",
                    built_in.name
                );
            }
        }
    }

    /// Selection is idempotent and independent of what preceded it: the restore-first rule. Without
    /// it a palette that omits an entry would keep the previous selection's colour there.
    #[test]
    fn selecting_a_palette_discards_the_previous_one() {
        let _palette = lock_for_test();
        let (ns, name) = REGISTRY[0];
        apply_spec(&format!("{ns}_{name}:purple(3)")).expect("a valid spec applies");

        apply_named("default light").expect("the light palette must apply");
        let once = snapshot();
        apply_named("default dark").expect("the dark palette must apply");
        apply_named("default light").expect("the light palette must apply again");
        let again = snapshot();

        assert_eq!(once, again, "a selection must not depend on what preceded it");
        assert_ne!(
            once[0],
            ramp::purple(3),
            "the hand edit must not survive a palette selection"
        );
    }

    #[test]
    fn an_unknown_name_is_rejected() {
        let _palette = lock_for_test();
        apply_named("solarised").expect_err("an unknown name must fail");
    }
}
