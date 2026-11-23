//! App-global command vocabulary and resolution metadata.
//!
//! This module declares the command registry and connects each descriptor to a
//! text resolver in [`crate::command::parse`]. Resolvers receive argument text
//! only: they do not inspect viewers or apply effects.
//!
//! Plain settings come from the shared [`Setting`] vocabulary; this module
//! converts both command descriptors and setting specs into
//! [`protocol::CommandOutcome`] values.
//!
//! [`encode_arg`] escapes argument text in startup lines, command files, and commands
//! displayed for copying. Even a multiline value stays on one physical batch line.
//!
//! The registry is metadata and lookup, not application state. Dispatch crosses
//! the command/`viewer_app` seam only after resolution, through `protocol`.

pub mod bar;
pub mod line;
pub mod parse;
pub mod protocol;
pub mod router;
mod spelling;

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::setting::{Setting, SettingOwner, SettingSpec, SettingValue};
use protocol::{
    CommandOutcome, Direction, GenomeCommand, HelpContent, LineEdit, ListMove, PaneCommand, ShellCommand, TableCommand,
};

pub use line::CommandLineMode;

/// Encodes a value that will be interpolated into a generated command line.
///
/// Newline is the batch separator, so it is represented as the two characters `\\n` inside the
/// quoted argument. Quotes and backslashes use the same small escape grammar; the resolver's
/// [`unquote`] function is the read half of this format.
pub fn encode_arg(value: &str) -> Result<String, String> {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '"' => encoded.push_str("\\\""),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            character => encoded.push(character),
        }
    }
    encoded.push('"');
    Ok(encoded)
}

/// Strips matching quotes and surrounding whitespace.
///
/// The read half of [`encode_arg`], and kept beside it: a resolver reachable from a generated line
/// sees the quotes that encoder adds unconditionally, so the two halves of that format are one
/// concern. Bare input is returned unchanged, since a typed argument and a generated one must mean
/// the same thing.
pub(crate) fn unquote(arg: &str) -> Option<String> {
    let trimmed = arg.trim();
    match trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some("") => None,
        Some(inner) => decode_quoted(inner).ok(),
        None if trimmed.is_empty() => None,
        None => Some(trimmed.to_string()),
    }
}

fn decode_quoted(value: &str) -> Result<String, ()> {
    let mut decoded = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            match character {
                'n' => decoded.push('\n'),
                'r' => decoded.push('\r'),
                '\\' => decoded.push('\\'),
                '"' => decoded.push('"'),
                other => {
                    decoded.push('\\');
                    decoded.push(other);
                }
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            decoded.push(character);
        }
    }
    if escaped {
        return Err(());
    }
    Ok(decoded)
}

/// Expands a leading `~/`.
pub(crate) fn expand_path(arg: &str) -> Result<std::path::PathBuf, String> {
    match arg.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").map_err(|_| "HOME is not set; cannot expand ~".to_string())?;
            Ok(std::path::PathBuf::from(home).join(rest))
        }
        None => Ok(std::path::PathBuf::from(arg)),
    }
}

/// One command name, help description, and resolver.
pub(crate) struct CommandDescriptor {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) typable: bool,
    pub(crate) blocked_while_busy: bool,
    run: fn(&str) -> Result<CommandOutcome, String>,
}

impl CommandDescriptor {
    const fn typed(
        name: &'static str,
        description: &'static str,
        run: fn(&str) -> Result<CommandOutcome, String>,
    ) -> Self {
        Self {
            name,
            description,
            typable: true,
            blocked_while_busy: false,
            run,
        }
    }

    const fn key_only(
        name: &'static str,
        description: &'static str,
        run: fn(&str) -> Result<CommandOutcome, String>,
    ) -> Self {
        Self {
            typable: false,
            ..Self::typed(name, description, run)
        }
    }

    const fn busy(self) -> Self {
        Self {
            blocked_while_busy: true,
            ..self
        }
    }
}

/// Commands that require parsing or viewer-side application.
pub(crate) fn registry() -> &'static [CommandDescriptor] {
    use CommandOutcome::{JumpToPeer, List};
    use GenomeCommand::*;
    use ShellCommand::*;
    use TableCommand::*;

    macro_rules! key_only {
        ($name:literal, $description:literal, $value:expr) => {
            CommandDescriptor::key_only($name, $description, |args| parse::no_args(args, $value.into()))
        };
    }

    static COMMANDS: &[CommandDescriptor] = &[
        CommandDescriptor::typed(
            "goto",
            "jump to <h>[,<v>] (absolute position, or a bare record number in the VCF table)",
            parse::run_goto,
        ),
        CommandDescriptor::typed(
            "gotolocus",
            "jump the VCF table to the nearest variant at <contig> <position>",
            parse::run_goto_locus,
        ),
        CommandDescriptor::typed("contigselector", "open the contig selector popover", |_| {
            Ok(OpenContigSelector.into())
        })
        .busy(),
        CommandDescriptor::typed("columnpicker", "open the variant table column picker", |_| {
            Ok(OpenColumnPicker.into())
        }),
        CommandDescriptor::typed("tracks", "open the Genome track picker", |args| {
            parse::no_args(args, OpenTrackPicker.into())
        }),
        CommandDescriptor::typed("bams", "open the Genome BAM switcher", |args| {
            parse::no_args(args, OpenBamSwitcher.into())
        }),
        CommandDescriptor::typed("settings", "open settings and the command reference", |_| {
            Ok(OpenHelp(HelpContent::Settings).into())
        }),
        CommandDescriptor::typed("shortcuts", "show the keys reachable from the current view", |_| {
            Ok(OpenHelp(HelpContent::Shortcuts).into())
        }),
        CommandDescriptor::typed(
            "focustab",
            "switch to a tab by name (e.g. GenomeViewer, VCFViewer)",
            parse::run_focus_tab,
        ),
        CommandDescriptor::typed(
            "load",
            "load a gzipped GTF file's already-built locus-jump index",
            parse::run_load,
        ),
        CommandDescriptor::typed(
            "load_index",
            "load a gzipped GTF file's locus-jump index, building it first if absent",
            parse::run_load_index,
        ),
        CommandDescriptor::typed(
            "nextfeature",
            "jump to the next feature start on the active contig",
            |_| Ok(JumpToFeature { forward: true }.into()),
        )
        .busy(),
        CommandDescriptor::typed(
            "prevfeature",
            "jump to the previous feature start on the active contig",
            |_| Ok(JumpToFeature { forward: false }.into()),
        )
        .busy(),
        CommandDescriptor::typed(
            "set activecontig",
            "switch the active contig by name or bare number/letter (e.g. 1, X)",
            parse::run_active_contig,
        ),
        CommandDescriptor::typed(
            "set activefasta",
            "load an indexed FASTA (needs .fai and, for BGZF, .gzi)",
            parse::run_active_fasta,
        ),
        CommandDescriptor::typed(
            "set activebam",
            "load an alignment pileup from a .bam path (needs a sibling .bai)",
            parse::run_active_bam,
        ),
        CommandDescriptor::typed("addbam", "add a .bam path to the BAM switcher", parse::run_add_bam),
        CommandDescriptor::typed(
            "set activevcf",
            "load a variant table from a .vcf/.vcf.gz path (indexed retrieval needs a .tbi)",
            parse::run_active_vcf,
        ),
        CommandDescriptor::typed(
            "palette",
            "open the palette menu (also `,`): edit entries, or select a built-in palette",
            |_| Ok(OpenPalettePane.into()),
        )
        .busy(),
        CommandDescriptor::typed(
            "set palette",
            "retint one palette entry: <entry>:<index|hue(step)>, e.g. surface_track_bg:grey(1) or :173",
            |args| {
                unquote(args)
                    .map(|spec| SetPalette(spec).into())
                    .ok_or_else(|| "set palette requires <entry>:<index|hue(step)>".to_string())
            },
        ),
        CommandDescriptor::typed(
            "set colorscheme",
            "select a whole built-in palette by name (also `,` > select palette)",
            |args| {
                unquote(args)
                    .filter(|name| !name.trim().is_empty())
                    .map(|name| SetColorscheme(name).into())
                    .ok_or_else(|| "set colorscheme requires a palette name".to_string())
            },
        ),
        CommandDescriptor::typed(
            "set featurefilter",
            "restrict drawn/jumpable features: include/exclude \"<substrings>\" or clear",
            parse::run_feature_filter,
        ),
        CommandDescriptor::typed(
            "set horizontalzoom",
            "step horizontal zoom: close or mid",
            parse::run_horizontal_zoom,
        ),
        CommandDescriptor::typed(
            "set alignmentverticalzoom",
            "step alignment vertical zoom: close or mid",
            parse::run_alignment_vertical_zoom,
        ),
        CommandDescriptor::key_only("scroll-left", "scroll left", |args| {
            parse::run_scroll(args, Direction::Left)
        }),
        CommandDescriptor::key_only("scroll-right", "scroll right", |args| {
            parse::run_scroll(args, Direction::Right)
        }),
        CommandDescriptor::key_only("scroll-up", "scroll up", |args| parse::run_scroll(args, Direction::Up)),
        CommandDescriptor::key_only("scroll-down", "scroll down", |args| {
            parse::run_scroll(args, Direction::Down)
        }),
        key_only!("page-up", "page up", Page { up: true }),
        key_only!("page-down", "page down", Page { up: false }),
        key_only!("zoom-in", "zoom in", StepZoom { into: true }),
        key_only!("zoom-out", "zoom out", StepZoom { into: false }),
        key_only!("cycle-readcolor", "cycle read colours", CycleReadColor),
        key_only!("column-page-left", "page columns left", ColumnPage { left: true }),
        key_only!("column-page-right", "page columns right", ColumnPage { left: false }),
        key_only!("list-up", "move selection up", List(ListMove::Up)),
        key_only!("list-down", "move selection down", List(ListMove::Down)),
        key_only!("list-select", "select an item", List(ListMove::Select)),
        key_only!("list-close", "close the list", List(ListMove::Close)),
        key_only!("toggle-column", "toggle the selected column", ToggleColumn),
        key_only!("show-all-columns", "show all columns", ShowAllColumns),
        key_only!("show-only-column", "show only the selected column", ShowOnlyColumn),
        key_only!(
            "palette-up",
            "move selection up",
            Palette(PaneCommand::List(ListMove::Up))
        ),
        key_only!(
            "palette-down",
            "move selection down",
            Palette(PaneCommand::List(ListMove::Down))
        ),
        key_only!(
            "palette-select",
            "select the current item",
            Palette(PaneCommand::List(ListMove::Select))
        ),
        key_only!(
            "palette-back",
            "return to the previous palette view",
            Palette(PaneCommand::List(ListMove::Close))
        ),
        key_only!("palette-close", "close the palette pane", Palette(PaneCommand::Close)),
        key_only!("palette-save", "write palette to file", Palette(PaneCommand::Save)),
        key_only!(
            "palette-toggle-reach",
            "switch between named ramps and all colours",
            Palette(PaneCommand::ToggleReach)
        ),
        key_only!(
            "palette-move-left",
            "move left",
            Palette(PaneCommand::Move(Direction::Left))
        ),
        key_only!(
            "palette-move-right",
            "move right",
            Palette(PaneCommand::Move(Direction::Right))
        ),
        key_only!("palette-move-up", "move up", Palette(PaneCommand::Move(Direction::Up))),
        key_only!(
            "palette-move-down",
            "move down",
            Palette(PaneCommand::Move(Direction::Down))
        ),
        key_only!("line-cancel", "cancel the command line", Line(LineEdit::Cancel)),
        key_only!("line-submit", "submit the command line", Line(LineEdit::Submit)),
        key_only!(
            "line-backspace",
            "delete the preceding character",
            Line(LineEdit::Backspace)
        ),
        CommandDescriptor::key_only("open-line", "open the command line", parse::run_open_line),
        key_only!("help-up", "scroll help up", Help(ListMove::Up)),
        key_only!("help-down", "scroll help down", Help(ListMove::Down)),
        key_only!("help-select", "select", Help(ListMove::Select)),
        key_only!("help-close", "close help", Help(ListMove::Close)),
        key_only!("next-tab", "focus the next tab", NextTab).busy(),
        key_only!("quit", "quit Apus", Quit),
        key_only!("jump-to-peer", "jump to this locus in the other viewer", JumpToPeer).busy(),
    ];
    COMMANDS
}

/// One visibly headed group in the help popover.
pub(crate) struct HelpSection {
    pub(crate) heading: &'static str,
    pub(crate) entries: Vec<HelpEntry>,
}

pub(crate) struct HelpEntry {
    /// A command name, or a binding's chords.
    pub(crate) key: String,
    pub(crate) setting: Option<HelpSetting>,
    pub(crate) description: &'static str,
}

pub(crate) struct HelpSetting {
    pub(crate) spec: &'static SettingSpec,
    pub(crate) value: Option<SettingValue>,
}

pub(crate) fn help_sections(values: &[SettingValue]) -> Vec<HelpSection> {
    let mut sections = Vec::new();
    for owner in SettingOwner::ALL {
        let entries: Vec<_> = Setting::specs()
            .iter()
            .filter(|spec| spec.owner == owner)
            .map(|spec| HelpEntry {
                key: spec.set_name.to_string(),
                setting: Some(HelpSetting {
                    spec,
                    value: values.iter().find(|value| value.key() == spec.key).cloned(),
                }),
                description: spec.description,
            })
            .collect();
        if !entries.is_empty() {
            sections.push(HelpSection {
                heading: owner.help_heading(),
                entries,
            });
        }
    }
    sections.push(HelpSection {
        heading: "Commands",
        entries: registry()
            .iter()
            .filter(|entry| entry.typable)
            .map(|entry| HelpEntry {
                key: entry.name.to_string(),
                setting: None,
                description: entry.description,
            })
            .collect(),
    });
    sections
}

/// Looks up a command by name.
fn find_command(name: &str) -> Option<&'static CommandDescriptor> {
    static BY_NAME: OnceLock<HashMap<&'static str, &'static CommandDescriptor>> = OnceLock::new();
    let name = if name == "help" { "settings" } else { name };
    BY_NAME
        .get_or_init(|| registry().iter().map(|command| (command.name, command)).collect())
        .get(name)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::encode_arg;
    use crate::command::protocol::{CommandOutcome, ShellCommand};

    #[test]
    fn generated_arguments_are_quoted_and_escape_command_delimiters() {
        assert_eq!(encode_arg("my reads.bam").unwrap(), "\"my reads.bam\"");
        assert_eq!(encode_arg("a\\b\"c\nd\r").unwrap(), "\"a\\\\b\\\"c\\nd\\r\"");
    }

    /// The property that makes a generated line and a typed one mean the same thing.
    #[test]
    fn unquote_recovers_multiline_and_quoted_values() {
        for value in [
            "plain.bam",
            "my reads.bam",
            "  leading and trailing  ",
            "say \"hi\".bam",
            "chr1",
            "\\server\\reads.bam",
            "line one\nline two\r\n",
            "~/reads.bam",
            "1,234",
        ] {
            let encoded = super::encode_arg(value).expect("these values must encode");
            assert_eq!(
                super::unquote(&encoded),
                Some(value.to_string()),
                "{value:?} must survive the encode/unquote round trip"
            );
        }
    }

    /// A bare argument is what a user types, and must mean what the quoted form means.
    #[test]
    fn unquote_passes_bare_input_through_and_refuses_empty_input() {
        assert_eq!(super::unquote("chr1"), Some("chr1".to_string()));
        assert_eq!(super::unquote("  chr1  "), Some("chr1".to_string()));
        for empty in ["", "   ", "\"\""] {
            assert_eq!(super::unquote(empty), None, "{empty:?} names no value");
        }
    }

    #[test]
    fn expand_path_expands_a_leading_tilde_and_leaves_other_paths_alone() {
        let home = std::env::var("HOME").expect("HOME must be set for this test");
        assert_eq!(
            super::expand_path("~/reads.bam").unwrap(),
            std::path::PathBuf::from(home).join("reads.bam")
        );
        for untouched in ["reads.bam", "/abs/reads.bam", "./reads.bam", "a~/b.bam"] {
            assert_eq!(
                super::expand_path(untouched).unwrap(),
                std::path::PathBuf::from(untouched),
                "{untouched:?} has no leading ~/ to expand"
            );
        }
    }

    /// Startup uses a quoted built-in name for the fallback, and every shipped name contains a
    /// space. Compare that spelling with what a user would type at the bar.
    #[test]
    fn the_typed_and_generated_spellings_of_a_colorscheme_resolve_alike() {
        for built_in in crate::style::palette::builtin::BUILT_INS {
            let encoded = encode_arg(built_in.name).expect("a palette name must encode");
            let typed = crate::command::parse::resolve_command(&format!("set colorscheme {}", built_in.name));
            let generated = crate::command::parse::resolve_command(&format!("set colorscheme {encoded}"));
            assert_eq!(
                typed, generated,
                "the two spellings of `{}` must resolve alike",
                built_in.name
            );
            assert!(
                matches!(typed, Ok(CommandOutcome::Shell(ShellCommand::SetColorscheme(ref name))) if name == built_in.name),
                "`{}` must resolve to its own name, got {typed:?}",
                built_in.name
            );
        }
    }
}
