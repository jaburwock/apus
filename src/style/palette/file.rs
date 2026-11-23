//! Reading and writing a palette as a file of command lines.
//!
//! The format is the command language itself: one `set palette <entry>:<colour>` line per entry,
//! replayed through the ordinary router at startup. That is deliberate — a palette file needs no
//! parser of its own, and anything expressible in it is something a user could also type. Lines are
//! written bare, without the `:` a user types at the bar: the trigger is the *bar's* shorthand, not
//! part of the language (see `command::parse::strip_trigger`).
//!
//! A sequence of `set` lines is order-dependent, which is this file's intended semantics: later
//! lines override earlier ones.
//!
//! **What is written is a raw ANSI index**, not a ramp name. A ramp name is a lookup into a table
//! this repo retunes, so a file naming one would change meaning underneath the user the next time a
//! ramp moved — a saved theme has to be an artifact, not a query. `parse_color` still accepts the
//! ramp spelling on the way in, so hand-editing works; files from before the palette-role
//! reorganization must be regenerated with the current entry names.
//!
//! Nothing here parses a colour; `apply_spec` is the one enforcement point. Invariant 8 is
//! unaffected: it governs how palette *defaults are authored* in source, which is still ramps-only,
//! not what a runtime value may hold — a file may name any index, 0–15 included.

use std::path::Path;

use super::command_lines;

/// The palette read at startup and written by the pane's `w`.
///
/// Relative to the process working directory, which makes it the repo's own `palettes/` during
/// development. A binary run from elsewhere falls back to [`FALLBACK`] if the file is absent.
pub const ACTIVE_PALETTE: &str = "palettes/default_light.txt";

/// The built-in applied when no `ACTIVE_PALETTE` file is present, named as [`super::builtin`] knows
/// it. A missing file is the ordinary case for a binary run outside the repo, so the fallback is the
/// shipped light theme rather than the authored defaults.
pub const FALLBACK: &str = "default light";

/// Marks a line as a comment. Not part of the command language — stripped here, so the command
/// parser never has to know about it.
const COMMENT: char = '#';

/// Reads `path` into a command batch, or `None` when there is no file to read.
///
/// A missing file is not a failure: the caller applies [`FALLBACK`] instead. Anything else —
/// unreadable, bad permissions, not UTF-8 — is an error the caller surfaces, since the user asked
/// for a palette on disk and did not get one.
pub fn load_lines(path: &Path) -> Option<Result<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(format!("cannot read palette {}: {error}", path.display()))),
    };
    Some(Ok(strip_comments(&text)))
}

/// Drops comments and blank lines, leaving a batch the router can resolve.
///
/// Blank lines are already ignored by `resolve_batch`; they are dropped here anyway so the batch
/// handed over is exactly the commands, and a failure's reported line content is a real command.
///
/// Shared with [`super::builtin`], whose palettes are hand-edited files that happen to be compiled
/// in: one comment convention for a palette however it reaches the app.
pub(super) fn strip_comments(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with(COMMENT))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Writes the current palette to `path`, creating the parent directory if needed.
///
/// Every entry is written, not just edited ones: a partial file would depend on the defaults at the
/// time it is replayed, which is exactly the fragility a saved theme should not have.
pub fn save(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut text = format!("{COMMENT} apus palette — edit by hand, or overwrite from the palette pane\n");
    for line in command_lines() {
        text.push_str(&line);
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|error| format!("cannot write palette {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::protocol::{CommandOutcome, ShellCommand};
    use crate::style::palette::{DEFAULTS, REGISTRY, apply_spec, entry_color, lock_for_test, ramp, reset};

    /// A scratch path under the repo's own `target/`, so tests never touch anything outside it.
    fn scratch(name: &str) -> std::path::PathBuf {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let dir = DIR.get_or_init(|| {
            let unique = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after the Unix epoch")
                    .as_nanos()
            );
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("palette-file-tests")
                .join(unique)
        });
        std::fs::create_dir_all(dir).expect("scratch dir must be creatable");
        dir.join(name)
    }

    /// Applies a batch the way startup does, so the test exercises the real resolution path rather
    /// than a reimplementation of it.
    fn apply_batch(batch: &str) {
        let resolved = crate::command::parse::resolve_batch(batch).expect("a saved palette must resolve");
        for line in resolved {
            let CommandOutcome::Shell(ShellCommand::SetPalette(spec)) = line.outcome else {
                panic!("a palette file must hold only palette settings");
            };
            apply_spec(&spec).expect("a saved spec must apply");
        }
    }

    /// The property the format exists for: what is saved reproduces the palette that was saved.
    ///
    /// Round-tripped against a *mutated* palette, not the defaults — writing the authored value
    /// regardless of what was edited would pass a defaults-only check while losing every edit,
    /// which is the whole content of a saved theme.
    ///
    /// One edit is an **off-ramp index**, which is the case the old ramp-named format could not
    /// express at all: it is the whole reason the file holds indices, so a round-trip that only
    /// covered ramp members would not exercise the format's own reason for existing.
    #[test]
    fn a_saved_palette_reloads_to_the_palette_it_described() {
        let _palette = lock_for_test();
        // A cube index no ramp contains, so it can only survive as an index.
        const OFF_RAMP: &str = "173";
        let edits = [
            (0, "purple(3)"),
            (REGISTRY.len() / 3, OFF_RAMP),
            (REGISTRY.len() / 2, "grey(17)"),
            (REGISTRY.len() - 1, "cyan(1)"),
        ];
        for (index, colour) in edits {
            let (ns, name) = REGISTRY[index];
            apply_spec(&format!("{ns}_{name}:{colour}")).expect("a valid spec must apply");
        }
        let expected: Vec<_> = (0..REGISTRY.len()).map(entry_color).collect();

        let path = scratch("roundtrip.txt");
        save(&path).expect("the palette must save");
        reset();

        let batch = load_lines(&path).expect("the file exists").expect("and must read");
        apply_batch(&batch);
        let replayed: Vec<_> = (0..REGISTRY.len()).map(entry_color).collect();

        assert_eq!(replayed, expected, "a reloaded palette must match the one saved");
        // The edits must actually have moved something, or the assertion above is vacuous.
        assert_ne!(
            expected,
            DEFAULTS.to_vec(),
            "the test must edit the palette it round-trips"
        );
    }

    /// A hand-edited file carries comments and spacing, and a line copied from the command bar
    /// still has the `:` a user types. All three must load.
    #[test]
    fn comments_blank_lines_and_a_typed_trigger_all_load() {
        let _palette = lock_for_test();
        let (ns, name) = REGISTRY[0];
        let path = scratch("handwritten.txt");
        std::fs::write(
            &path,
            format!("# a header\n\n  set palette {ns}_{name}:purple(3)\n\n# trailing note\n"),
        )
        .expect("the fixture must write");

        let batch = load_lines(&path).expect("the file exists").expect("and must read");
        assert_eq!(batch, format!("set palette {ns}_{name}:purple(3)"));

        apply_batch(&batch);
        let applied = entry_color(0);
        reset();
        assert_eq!(applied, ramp::purple(3));

        // The `:` spelling resolves identically, so an old clipboard dump pastes back unchanged.
        std::fs::write(&path, format!(":set palette {ns}_{name}:cyan(1)\n")).expect("the fixture must write");
        let batch = load_lines(&path).expect("the file exists").expect("and must read");
        apply_batch(&batch);
        let applied = entry_color(0);
        reset();
        assert_eq!(applied, ramp::cyan(1));
    }

    /// A checkout with no palette file reports nothing to read, so the caller falls back to
    /// [`FALLBACK`] — which must name a palette that actually exists, or startup silently keeps the
    /// defaults on every machine without a palette file.
    #[test]
    fn a_missing_file_reads_as_absent_and_the_fallback_names_a_real_palette() {
        let _palette = lock_for_test();
        assert!(
            load_lines(&scratch("definitely_not_written.txt")).is_none(),
            "a missing palette file is not an error"
        );
        super::super::builtin::apply_named(FALLBACK).expect("the fallback must name a built-in");
    }

    /// Saving into a directory that does not exist yet must work — the first save of a fresh
    /// checkout creates `palettes/`.
    #[test]
    fn saving_creates_the_directory() {
        let _palette = lock_for_test();
        let path = scratch("fresh").join("nested").join("palette.txt");
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));

        save(&path).expect("the palette must save into a new directory");
        assert!(path.is_file(), "the saved file must exist at the requested path");
        assert!(
            load_lines(&path)
                .expect("the file exists")
                .expect("and must read")
                .contains("set palette"),
            "the saved file must read back as palette commands"
        );
    }
}
