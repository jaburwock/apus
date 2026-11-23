//! Typed bindings and the scope rules shared by dispatch and key hints.

mod defaults;

use std::{collections::HashSet, fmt};

use crate::command::{HelpEntry, HelpSection, protocol::CommandOutcome};
use crate::input::{Key, Modifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Chord {
    code: Key,
    mods: Modifiers,
}

impl Chord {
    pub(crate) fn new(code: Key, mut mods: Modifiers) -> Self {
        let code = match code {
            Key::Char(character) if mods.contains(Modifiers::SHIFT) && !character.is_control() => {
                // Terminals can report the same printable with or without an explicit Shift.
                mods.remove(Modifiers::SHIFT);
                Key::Char(character.to_ascii_uppercase())
            }
            code => code,
        };
        Self { code, mods }
    }

    #[cfg(test)]
    pub(crate) fn code(self) -> Key {
        self.code
    }

    #[cfg(test)]
    pub(crate) fn mods(self) -> Modifiers {
        self.mods
    }

    fn printable(self) -> Option<char> {
        match self.code {
            Key::Char(character) if self.mods.is_empty() && !character.is_control() => Some(character),
            _ => None,
        }
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, prefix) in [
            (Modifiers::CONTROL, "C-"),
            (Modifiers::ALT, "A-"),
            (Modifiers::SHIFT, "S-"),
            (Modifiers::SUPER, "Super-"),
            (Modifiers::HYPER, "Hyper-"),
            (Modifiers::META, "Meta-"),
        ] {
            if self.mods.contains(modifier) {
                f.write_str(prefix)?;
            }
        }
        write!(f, "{}", self.code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Genome,
    Vcf,
    ContigSelector,
    TrackPicker,
    BamSwitcher,
    ColumnPicker,
    PaletteMenu,
    PaletteSelect,
    PaletteEntries,
    PalettePicker,
    CommandLine,
    HelpOverlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Transparent,
    Capture,
    Text,
}

impl Scope {
    pub(crate) fn policy(self) -> Policy {
        match self {
            Self::Global | Self::Genome | Self::Vcf => Policy::Transparent,
            Self::ContigSelector
            | Self::TrackPicker
            | Self::BamSwitcher
            | Self::ColumnPicker
            | Self::PaletteMenu
            | Self::PaletteSelect
            | Self::PaletteEntries
            | Self::PalettePicker
            | Self::HelpOverlay => Policy::Capture,
            Self::CommandLine => Policy::Text,
        }
    }

    /// The scope's section heading in `:shortcuts`.
    pub(crate) fn heading(self) -> &'static str {
        match self {
            Self::Global => "Everywhere",
            Self::Genome => "Genome view",
            Self::Vcf => "Variant table",
            Self::ContigSelector => "Contig selector",
            Self::TrackPicker => "Track picker",
            Self::BamSwitcher => "BAM switcher",
            Self::ColumnPicker => "Column picker",
            Self::PaletteMenu => "Palette menu",
            Self::PaletteSelect => "Palette select",
            Self::PaletteEntries => "Palette entries",
            Self::PalettePicker => "Colour picker",
            Self::CommandLine => "Command line",
            Self::HelpOverlay => "Help",
        }
    }
}

pub(crate) struct Binding {
    pub(crate) scope: Scope,
    pub(crate) chord: Chord,
    pub(crate) command: CommandOutcome,
    pub(crate) description: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolution<'a> {
    Bound(&'a CommandOutcome),
    Text(char),
    Swallowed,
    Unbound,
}

/// The shell-owned bindings, borrowed by viewers while drawing.
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub(crate) fn defaults() -> Self {
        Self {
            bindings: defaults::bindings(),
        }
    }

    /// Search from the innermost scope out.
    pub(crate) fn resolve(&self, chord: Chord, stack: &[Scope]) -> Resolution<'_> {
        for &scope in stack {
            if let Some(binding) = self
                .bindings
                .iter()
                .find(|binding| binding.scope == scope && binding.chord == chord)
            {
                return Resolution::Bound(&binding.command);
            }
            match scope.policy() {
                Policy::Transparent => {}
                Policy::Capture => return Resolution::Swallowed,
                Policy::Text => return chord.printable().map_or(Resolution::Swallowed, Resolution::Text),
            }
        }
        Resolution::Unbound
    }

    /// Enumerate reachable explicit bindings in scope order, keeping the first binding for each chord.
    pub(crate) fn visible(&self, stack: &[Scope]) -> Vec<&Binding> {
        let mut seen = HashSet::new();
        let mut visible = Vec::new();
        for &scope in stack {
            visible.extend(
                self.bindings
                    .iter()
                    .filter(|binding| binding.scope == scope && seen.insert(binding.chord)),
            );
            if scope.policy() != Policy::Transparent {
                break;
            }
        }
        visible
    }

    pub(crate) fn keys_for(&self, scope: Scope, command: &CommandOutcome) -> Vec<Chord> {
        self.bindings
            .iter()
            .filter(|binding| binding.scope == scope && &binding.command == command)
            .map(|binding| binding.chord)
            .collect()
    }

    /// Compact captions show the first binding in the scope's declaration order.
    pub(crate) fn primary_key(&self, scope: Scope, command: &CommandOutcome) -> Option<String> {
        self.keys_for(scope, command).first().map(ToString::to_string)
    }

    pub(crate) fn hints(
        &self,
        scope: Scope,
        commands: impl IntoIterator<Item = CommandOutcome>,
        separator: &str,
    ) -> String {
        commands
            .into_iter()
            .filter_map(|command| {
                let key = self.primary_key(scope, &command)?;
                Some(format!("{key} {}", self.caption(scope, &command)?))
            })
            .collect::<Vec<_>>()
            .join(separator)
    }

    /// One section per reachable scope, one row per command: all its reachable chords, then its
    /// caption.
    pub(crate) fn help_sections(&self, stack: &[Scope]) -> Vec<HelpSection> {
        let mut rows: Vec<(Scope, &CommandOutcome, Vec<Chord>)> = Vec::new();
        for binding in self.visible(stack) {
            match rows
                .iter_mut()
                .find(|(scope, command, _)| *scope == binding.scope && **command == binding.command)
            {
                Some((_, _, chords)) => chords.push(binding.chord),
                None => rows.push((binding.scope, &binding.command, vec![binding.chord])),
            }
        }
        // `visible` yields scopes in stack order, so each scope's rows are contiguous.
        rows.chunk_by(|a, b| a.0 == b.0)
            .map(|section| HelpSection {
                heading: section[0].0.heading(),
                entries: section
                    .iter()
                    .filter_map(|(scope, command, chords)| {
                        let keys = chords.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ");
                        Some(HelpEntry {
                            key: keys,
                            setting: None,
                            description: self.caption(*scope, command)?,
                        })
                    })
                    .collect(),
            })
            .collect()
    }

    /// A binding's own caption, falling back to its command's description.
    fn caption(&self, scope: Scope, command: &CommandOutcome) -> Option<&'static str> {
        self.bindings
            .iter()
            .find(|binding| binding.scope == scope && &binding.command == command)
            .and_then(|binding| binding.description)
            .or_else(|| command.descriptor().map(|descriptor| descriptor.description))
    }
}

#[cfg(test)]
mod tests;
