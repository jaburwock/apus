//! The command-language spelling of resolved outcomes.

use crate::basepair::BasePair;

use super::protocol::{
    CommandOutcome, Direction, FeatureFilterEdit, GenomeCommand, HelpContent, LineEdit, ListMove, PaneCommand,
    ScrollStep, ShellCommand, TableCommand,
};
use super::{CommandDescriptor, encode_arg, find_command};

impl CommandOutcome {
    pub(crate) fn descriptor(&self) -> Option<&'static CommandDescriptor> {
        find_command(self.name())
    }

    fn name(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Shell(command) => command.name(),
            Self::Genome(command) => command.name(),
            Self::Table(command) => command.name(),
            Self::Goto { .. } => "goto",
            Self::Setting(value) => value.command_name(),
            Self::Scroll { direction, .. } => match direction {
                Direction::Left => "scroll-left",
                Direction::Right => "scroll-right",
                Direction::Up => "scroll-up",
                Direction::Down => "scroll-down",
            },
            Self::List(action) => match action {
                ListMove::Up => "list-up",
                ListMove::Down => "list-down",
                ListMove::Select => "list-select",
                ListMove::Close => "list-close",
            },
            Self::JumpToPeer => "jump-to-peer",
        }
    }

    /// Writes one line of command language. Derived fields, such as a load's cache path,
    /// are reconstructed by the parser.
    pub fn to_line(&self) -> String {
        let name = self.descriptor().map_or_else(|| self.name(), |entry| entry.name);
        let mut line = format!(":{name}");
        let mut arg = |value: &str| {
            line.push(' ');
            line.push_str(&encode_arg(value).expect("all command arguments can be escaped"));
        };

        match self {
            Self::Genome(GenomeCommand::ActiveContig(value))
            | Self::Shell(
                ShellCommand::FocusTab(value) | ShellCommand::SetPalette(value) | ShellCommand::SetColorscheme(value),
            ) => {
                arg(value);
            }
            Self::Goto { line: Some(row), .. } => arg(&row.to_string()),
            Self::Goto {
                horizontal,
                vertical,
                line: None,
            } => {
                // The comma marks a genome-only target, including when just one axis is set.
                let coordinate = |bp: &Option<BasePair>| bp.map(|bp| bp.to_display().to_string()).unwrap_or_default();
                arg(&format!("{},{}", coordinate(horizontal), coordinate(vertical)));
            }
            Self::Table(TableCommand::GotoLocus { contig, position }) => {
                arg(contig);
                arg(&position.to_display().to_string());
            }
            Self::Genome(GenomeCommand::Zoom {
                horizontal_to_mid,
                alignment_v_to_mid,
            }) => {
                let to_mid = horizontal_to_mid.or(*alignment_v_to_mid).expect("zoom names one axis");
                arg(if to_mid { "mid" } else { "close" });
            }
            Self::Genome(GenomeCommand::Load { gtf_path, .. }) => arg(&gtf_path.to_string_lossy()),
            Self::Genome(GenomeCommand::FeatureFilter(edit)) => {
                let (verb, values) = match edit {
                    FeatureFilterEdit::Include(values) => ("include", Some(values)),
                    FeatureFilterEdit::Exclude(values) => ("exclude", Some(values)),
                    FeatureFilterEdit::Clear => ("clear", None),
                };
                arg(verb);
                if let Some(values) = values {
                    arg(&values.join("|"));
                }
            }
            Self::Genome(
                GenomeCommand::ActiveFasta(path) | GenomeCommand::ActiveBam(path) | GenomeCommand::AddBam(path),
            )
            | Self::Table(TableCommand::ActiveVcf(path)) => arg(&path.to_string_lossy()),
            Self::Setting(value) => arg(&value.command_arg()),
            Self::Scroll { step, .. } => arg(match step {
                ScrollStep::Near => "near",
                ScrollStep::NearX10 => "near-x10",
                ScrollStep::Mid => "mid",
                ScrollStep::Far => "far",
            }),
            Self::Shell(ShellCommand::OpenLine(trigger)) => arg(&trigger.character().to_string()),
            _ => {}
        }
        line
    }
}

impl ShellCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::OpenHelp(HelpContent::Settings) => "settings",
            Self::OpenHelp(HelpContent::Shortcuts) => "shortcuts",
            Self::Help(action) => match action {
                ListMove::Up => "help-up",
                ListMove::Down => "help-down",
                ListMove::Select => "help-select",
                ListMove::Close => "help-close",
            },
            Self::FocusTab(_) => "focustab",
            Self::OpenPalettePane => "palette",
            Self::SetPalette(_) => "set palette",
            Self::SetColorscheme(_) => "set colorscheme",
            Self::Palette(action) => match action {
                PaneCommand::List(ListMove::Up) => "palette-up",
                PaneCommand::List(ListMove::Down) => "palette-down",
                PaneCommand::List(ListMove::Select) => "palette-select",
                PaneCommand::List(ListMove::Close) => "palette-back",
                PaneCommand::Close => "palette-close",
                PaneCommand::Save => "palette-save",
                PaneCommand::ToggleReach => "palette-toggle-reach",
                PaneCommand::Move(Direction::Left) => "palette-move-left",
                PaneCommand::Move(Direction::Right) => "palette-move-right",
                PaneCommand::Move(Direction::Up) => "palette-move-up",
                PaneCommand::Move(Direction::Down) => "palette-move-down",
            },
            Self::Line(action) => match action {
                LineEdit::Cancel => "line-cancel",
                LineEdit::Submit => "line-submit",
                LineEdit::Backspace => "line-backspace",
            },
            Self::OpenLine(_) => "open-line",
            Self::NextTab => "next-tab",
            Self::Quit => "quit",
        }
    }
}

impl GenomeCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::ActiveContig(_) => "set activecontig",
            Self::OpenContigSelector => "contigselector",
            Self::OpenTrackPicker => "tracks",
            Self::CycleReadColor => "cycle-readcolor",
            Self::OpenBamSwitcher => "bams",
            Self::ChooseVariantBam { .. } => "choose-variant-bam",
            Self::JumpToVariant { .. } => "jump-to-variant",
            Self::Zoom {
                horizontal_to_mid: Some(_),
                ..
            } => "set horizontalzoom",
            Self::Zoom {
                horizontal_to_mid: None,
                ..
            } => "set alignmentverticalzoom",
            Self::StepZoom { into: true } => "zoom-in",
            Self::StepZoom { into: false } => "zoom-out",
            Self::Load {
                build_missing: false, ..
            } => "load",
            Self::Load {
                build_missing: true, ..
            } => "load_index",
            Self::JumpToFeature { forward: true } => "nextfeature",
            Self::JumpToFeature { forward: false } => "prevfeature",
            Self::FeatureFilter(_) => "set featurefilter",
            Self::ActiveFasta(_) => "set activefasta",
            Self::ActiveBam(_) => "set activebam",
            Self::AddBam(_) => "addbam",
        }
    }
}

impl TableCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::ActiveVcf(_) => "set activevcf",
            Self::GotoLocus { .. } => "gotolocus",
            Self::OpenVariantBams { .. } => "open-variant-bams",
            Self::Page { up: true } => "page-up",
            Self::Page { up: false } => "page-down",
            Self::ColumnPage { left: true } => "column-page-left",
            Self::ColumnPage { left: false } => "column-page-right",
            Self::OpenColumnPicker => "columnpicker",
            Self::ToggleColumn => "toggle-column",
            Self::ShowAllColumns => "show-all-columns",
            Self::ShowOnlyColumn => "show-only-column",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse::resolve_command;
    use crate::command::protocol::LineTrigger;
    use crate::setting::SettingValue;

    #[test]
    fn help_resolves_to_the_canonical_settings_command() {
        let outcome = resolve_command(":help").unwrap();
        assert_eq!(outcome, ShellCommand::OpenHelp(HelpContent::Settings).into());
        assert_eq!(resolve_command(":settings"), Ok(outcome.clone()));
        assert_eq!(outcome.to_line(), ":settings");
    }

    #[test]
    fn every_command_round_trips_through_its_spelling() {
        use CommandOutcome::*;
        use GenomeCommand::*;
        use ShellCommand::*;
        use TableCommand::*;
        use std::path::PathBuf;

        // The exhaustive match requires an entry for every outcome. Typed-only follow-ups have
        // no command-language examples.
        macro_rules! round_trip {
            ($($pattern:pat => [$($example:expr),* $(,)?]),+ $(,)?) => {
                let variant = |outcome: &CommandOutcome| match outcome {
                    $($pattern => stringify!($pattern),)+
                };
                $(
                    let examples: &[CommandOutcome] = &[$(CommandOutcome::from($example)),*];
                    for outcome in examples {
                        assert_eq!(variant(outcome), stringify!($pattern));
                        let line = outcome.to_line();
                        assert_eq!(line.lines().count(), 1, "{outcome:?} must occupy one line");
                        assert_eq!(resolve_command(&line), Ok(outcome.clone()), "spelling: {line}");
                    }
                )+
            };
        }

        let path = PathBuf::from("white space\\with \"quotes\"\nfile.gtf.gz");
        let contig = "chrX\n\"not a command\"\\name";
        let focus = "Tab name with spaces";
        let load = |build_missing| Load {
            cache_path: crate::locus::cache_path_for(&path),
            gtf_path: path.clone(),
            build_missing,
        };
        round_trip! {
            None => [None],
            Genome(ActiveContig(_)) => [ActiveContig(contig.into()), ActiveContig(String::new())],
            Genome(OpenContigSelector) => [OpenContigSelector],
            Genome(OpenTrackPicker) => [OpenTrackPicker],
            Genome(CycleReadColor) => [CycleReadColor],
            Genome(OpenBamSwitcher) => [OpenBamSwitcher],
            Genome(ChooseVariantBam { .. }) => [], // Typed follow-up only.
            Genome(JumpToVariant { .. }) => [], // Typed follow-up only.
            Table(OpenVariantBams { .. }) => [], // Typed follow-up only.
            Table(OpenColumnPicker) => [OpenColumnPicker],
            Shell(OpenHelp(_)) => [OpenHelp(HelpContent::Settings), OpenHelp(HelpContent::Shortcuts)],
            Shell(Help(_)) => [
                Help(ListMove::Up),
                Help(ListMove::Down),
                Help(ListMove::Select),
                Help(ListMove::Close),
            ],
            Shell(FocusTab(_)) => [FocusTab(focus.into())],
            Shell(OpenPalettePane) => [OpenPalettePane],
            Goto { .. } => [
                resolve_command(":goto 0").unwrap(),
                resolve_command(":goto 500k").unwrap(),
                resolve_command(":goto 500,").unwrap(),
                resolve_command(":goto ,50").unwrap(),
                resolve_command(":goto 500,50").unwrap(),
            ],
            Table(GotoLocus { .. }) => [GotoLocus { contig: contig.into(), position: BasePair::from_display(13) }],
            Genome(Zoom { .. }) => [
                resolve_command(":set horizontalzoom mid").unwrap(),
                resolve_command(":set alignmentverticalzoom close").unwrap(),
            ],
            Genome(Load { .. }) => [load(false), load(true)],
            Genome(JumpToFeature { .. }) => [JumpToFeature { forward: true }, JumpToFeature { forward: false }],
            Genome(FeatureFilter(_)) => [
                FeatureFilter(FeatureFilterEdit::Include(vec!["locus\\n".into(), contig.into()])),
                FeatureFilter(FeatureFilterEdit::Exclude(vec!["prefix".into(), String::new()])),
                FeatureFilter(FeatureFilterEdit::Clear),
            ],
            Shell(SetPalette(_)) => [SetPalette("surface_track_bg:grey(1)".into())],
            Shell(SetColorscheme(_)) => [SetColorscheme("default dark".into())],
            Genome(ActiveFasta(_)) => [ActiveFasta(path.clone())],
            Genome(ActiveBam(_)) => [ActiveBam(path.clone())],
            Genome(AddBam(_)) => [AddBam(path.clone())],
            Table(ActiveVcf(_)) => [ActiveVcf(path)],
            Setting(_) => [
                Setting(SettingValue::Ruler(false)),
                resolve_command(":set movemin 3").unwrap(),
                resolve_command(":set readcolor direction").unwrap(),
            ],
            Scroll { .. } => [
                Scroll { direction: Direction::Left, step: ScrollStep::Near },
                Scroll { direction: Direction::Right, step: ScrollStep::NearX10 },
                Scroll { direction: Direction::Up, step: ScrollStep::Mid },
                Scroll { direction: Direction::Down, step: ScrollStep::Far },
            ],
            Table(Page { .. }) => [Page { up: true }, Page { up: false }],
            Genome(StepZoom { .. }) => [StepZoom { into: true }, StepZoom { into: false }],
            Table(ColumnPage { .. }) => [ColumnPage { left: true }, ColumnPage { left: false }],
            List(_) => [List(ListMove::Up), List(ListMove::Down), List(ListMove::Select), List(ListMove::Close)],
            Table(ToggleColumn) => [ToggleColumn],
            Table(ShowAllColumns) => [ShowAllColumns],
            Table(ShowOnlyColumn) => [ShowOnlyColumn],
            Shell(Palette(_)) => [
                Palette(PaneCommand::List(ListMove::Up)),
                Palette(PaneCommand::List(ListMove::Down)),
                Palette(PaneCommand::List(ListMove::Select)),
                Palette(PaneCommand::List(ListMove::Close)),
                Palette(PaneCommand::Close),
                Palette(PaneCommand::Save),
                Palette(PaneCommand::ToggleReach),
                Palette(PaneCommand::Move(Direction::Left)),
                Palette(PaneCommand::Move(Direction::Right)),
                Palette(PaneCommand::Move(Direction::Up)),
                Palette(PaneCommand::Move(Direction::Down)),
            ],
            Shell(Line(_)) => [Line(LineEdit::Cancel), Line(LineEdit::Submit), Line(LineEdit::Backspace)],
            Shell(OpenLine(_)) => [OpenLine(LineTrigger::Command), OpenLine(LineTrigger::Slash), OpenLine(LineTrigger::Contig)],
            Shell(NextTab) => [NextTab],
            Shell(Quit) => [Quit],
            JumpToPeer => [JumpToPeer],
        }
    }
}
