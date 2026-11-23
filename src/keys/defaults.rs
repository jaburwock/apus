use crate::command::protocol::{
    CommandOutcome::{self, JumpToPeer, List, Scroll},
    Direction,
    GenomeCommand::*,
    HelpContent, LineEdit, LineTrigger, ListMove, PaneCommand,
    ScrollStep::*,
    ShellCommand::{self, *},
    TableCommand::*,
};
use crate::input::{
    Key::{self, *},
    Modifiers,
};

use super::{Binding, Chord, Scope, Scope::*};

/// Collects bindings. A method rather than a closure so `command` can take any group's variant.
#[derive(Default)]
struct Bindings(Vec<Binding>);

impl Bindings {
    fn bind(
        &mut self,
        scope: Scope,
        codes: &[Key],
        command: impl Into<CommandOutcome>,
        description: Option<&'static str>,
    ) {
        self.bind_chords(scope, codes, Modifiers::NONE, command, description);
    }

    fn bind_chords(
        &mut self,
        scope: Scope,
        codes: &[Key],
        modifiers: Modifiers,
        command: impl Into<CommandOutcome>,
        description: Option<&'static str>,
    ) {
        let command = command.into();
        self.0.extend(codes.iter().map(|&code| Binding {
            scope,
            chord: Chord::new(code, modifiers),
            command: command.clone(),
            description,
        }));
    }
}

pub(super) fn bindings() -> Vec<Binding> {
    let mut keys = Bindings::default();
    let scroll = |direction, step| Scroll { direction, step };
    let palette_list = |list_move| ShellCommand::Palette(PaneCommand::List(list_move));
    let palette_move = |direction| ShellCommand::Palette(PaneCommand::Move(direction));

    keys.bind(Global, &[Char(',')], OpenHelp(HelpContent::Settings), None);
    keys.bind(Global, &[Char('?')], OpenHelp(HelpContent::Shortcuts), None);
    keys.bind(Global, &[Char('q')], Quit, None);
    keys.bind(Global, &[Tab], NextTab, None);
    for trigger in [LineTrigger::Command, LineTrigger::Slash, LineTrigger::Contig] {
        keys.bind(Global, &[Char(trigger.character())], OpenLine(trigger), None);
    }

    keys.bind(Genome, &[Char('h'), Left], scroll(Direction::Left, Near), None);
    keys.bind(Genome, &[Char('j'), Down], scroll(Direction::Down, Near), None);
    keys.bind(Genome, &[Char('k'), Up], scroll(Direction::Up, Near), None);
    keys.bind(Genome, &[Char('l'), Right], scroll(Direction::Right, Near), None);
    keys.bind(Genome, &[Char('H')], scroll(Direction::Left, NearX10), None);
    keys.bind(Genome, &[Char('J')], scroll(Direction::Down, NearX10), None);
    keys.bind(Genome, &[Char('K')], scroll(Direction::Up, NearX10), None);
    keys.bind(Genome, &[Char('L')], scroll(Direction::Right, NearX10), None);
    keys.bind(Genome, &[Char('n')], scroll(Direction::Right, Mid), None);
    keys.bind(Genome, &[Char('p')], scroll(Direction::Left, Mid), None);
    keys.bind(Genome, &[Char('N')], scroll(Direction::Right, Far), None);
    keys.bind(Genome, &[Char('P')], scroll(Direction::Left, Far), None);
    keys.bind(Genome, &[Char('+'), Char('=')], StepZoom { into: true }, None);
    keys.bind(Genome, &[Char('-')], StepZoom { into: false }, None);
    keys.bind(Genome, &[Char('C')], OpenContigSelector, None);
    keys.bind(Genome, &[Char('T')], OpenTrackPicker, Some("toggle track visibility"));
    keys.bind(Genome, &[Char('r')], CycleReadColor, None);
    keys.bind(Genome, &[Char('B')], OpenBamSwitcher, Some("switch BAM"));
    keys.bind(Genome, &[Char('w')], JumpToFeature { forward: true }, None);
    keys.bind(Genome, &[Char('b')], JumpToFeature { forward: false }, None);
    keys.bind(Genome, &[Enter], JumpToPeer, None);

    keys.bind(Vcf, &[Char('h'), Left], scroll(Direction::Left, Near), None);
    keys.bind(Vcf, &[Char('j'), Down], scroll(Direction::Down, Near), None);
    keys.bind(Vcf, &[Char('k'), Up], scroll(Direction::Up, Near), None);
    keys.bind(Vcf, &[Char('l'), Right], scroll(Direction::Right, Near), None);
    keys.bind(Vcf, &[Char('H')], ColumnPage { left: true }, Some("page columns"));
    keys.bind(Vcf, &[Char('L')], ColumnPage { left: false }, Some("page columns"));
    keys.bind(Vcf, &[Char('u')], Page { up: true }, None);
    keys.bind(Vcf, &[Char('d')], Page { up: false }, None);
    keys.bind(Vcf, &[Char('C')], OpenColumnPicker, None);
    keys.bind(Vcf, &[Enter], JumpToPeer, None);

    for scope in [ContigSelector, TrackPicker, BamSwitcher, ColumnPicker] {
        keys.bind(scope, &[Char('j'), Down], List(ListMove::Down), None);
        keys.bind(scope, &[Char('k'), Up], List(ListMove::Up), None);
    }
    keys.bind(ContigSelector, &[Esc, Char('q')], List(ListMove::Close), None);
    keys.bind(ContigSelector, &[Enter], List(ListMove::Select), None);
    keys.bind(
        TrackPicker,
        &[Esc, Char('q'), Char('T')],
        List(ListMove::Close),
        Some("close"),
    );
    keys.bind(TrackPicker, &[Enter, Char(' ')], List(ListMove::Select), Some("toggle"));
    keys.bind(
        BamSwitcher,
        &[Esc, Char('q'), Char('B')],
        List(ListMove::Close),
        Some("close"),
    );
    keys.bind(BamSwitcher, &[Enter], List(ListMove::Select), Some("switch"));
    keys.bind(ColumnPicker, &[Char('q'), Esc], List(ListMove::Close), Some("close"));
    keys.bind(ColumnPicker, &[Char(' ')], ToggleColumn, Some("toggle"));
    keys.bind(ColumnPicker, &[Char('a')], ShowAllColumns, Some("all"));
    keys.bind(ColumnPicker, &[Char('n')], ShowOnlyColumn, Some("only"));

    for scope in [PaletteMenu, PaletteSelect] {
        keys.bind(scope, &[Char('j'), Down], palette_list(ListMove::Down), None);
        keys.bind(scope, &[Char('k'), Up], palette_list(ListMove::Up), None);
    }
    for (scope, description) in [
        (PaletteMenu, "open"),
        (PaletteSelect, "apply"),
        (PaletteEntries, "pick"),
        (PalettePicker, "keep"),
    ] {
        keys.bind(scope, &[Enter], palette_list(ListMove::Select), Some(description));
    }
    keys.bind(
        PaletteMenu,
        &[Char('q'), Esc, Char(',')],
        Palette(PaneCommand::Close),
        Some("close"),
    );
    for scope in [PaletteSelect, PaletteEntries] {
        keys.bind(scope, &[Char(',')], Palette(PaneCommand::Close), None);
        keys.bind(scope, &[Esc, Char('q')], palette_list(ListMove::Close), Some("back"));
    }
    keys.bind(PaletteEntries, &[Char('w')], Palette(PaneCommand::Save), Some("save"));
    for scope in [PaletteEntries, PalettePicker] {
        keys.bind(scope, &[Char('h'), Left], palette_move(Direction::Left), None);
        keys.bind(scope, &[Char('j'), Down], palette_move(Direction::Down), None);
        keys.bind(scope, &[Char('k'), Up], palette_move(Direction::Up), None);
        keys.bind(scope, &[Char('l'), Right], palette_move(Direction::Right), None);
    }
    keys.bind(PalettePicker, &[Tab], Palette(PaneCommand::ToggleReach), None);
    keys.bind(PalettePicker, &[Esc], palette_list(ListMove::Close), None);

    keys.bind(HelpOverlay, &[Char('j'), Down], Help(ListMove::Down), None);
    keys.bind(HelpOverlay, &[Char('k'), Up], Help(ListMove::Up), None);
    keys.bind(HelpOverlay, &[Enter], Help(ListMove::Select), None);
    // `q` first: it is the key the popover's border shows.
    keys.bind(
        HelpOverlay,
        &[Char('q'), Esc, Char('?')],
        Help(ListMove::Close),
        Some("close"),
    );
    keys.bind(CommandLine, &[Esc], Line(LineEdit::Cancel), None);
    keys.bind(CommandLine, &[Enter], Line(LineEdit::Submit), None);
    keys.bind(CommandLine, &[Backspace], Line(LineEdit::Backspace), None);
    keys.bind_chords(
        CommandLine,
        &[Char('['), Char('c')],
        Modifiers::CONTROL,
        Line(LineEdit::Cancel),
        None,
    );
    keys.0
}
