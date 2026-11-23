use super::*;
use crate::command::{parse::resolve_command, protocol::ShellCommand};

const CAPTURE_SCOPES: &[Scope] = &[
    Scope::ContigSelector,
    Scope::TrackPicker,
    Scope::ColumnPicker,
    Scope::PaletteMenu,
    Scope::PaletteSelect,
    Scope::PaletteEntries,
    Scope::PalettePicker,
    Scope::HelpOverlay,
];
const TRANSPARENT_SCOPES: &[Scope] = &[Scope::Global, Scope::Genome, Scope::Vcf];

#[test]
fn no_chord_is_bound_twice_in_a_scope() {
    let keymap = Keymap::defaults();
    let mut seen = HashSet::new();
    for binding in &keymap.bindings {
        assert!(
            seen.insert((binding.scope, binding.chord)),
            "{:?} {}",
            binding.scope,
            binding.chord
        );
    }
}

#[test]
fn every_bound_command_resolves_its_spelling() {
    let keymap = Keymap::defaults();
    for binding in &keymap.bindings {
        let line = binding.command.to_line();
        assert_eq!(resolve_command(&line).as_ref(), Ok(&binding.command), "{line}");
    }
}

#[test]
fn a_capture_scope_never_leaks_an_unbound_chord() {
    let keymap = Keymap::defaults();
    for &scope in CAPTURE_SCOPES {
        let inner = keymap.visible(&[scope]);
        for outer in keymap.bindings.iter().filter(|binding| binding.scope != scope) {
            if inner.iter().any(|binding| binding.chord == outer.chord) {
                continue;
            }
            let stack = [scope, outer.scope];
            assert_eq!(
                keymap.resolve(outer.chord, &stack),
                Resolution::Swallowed,
                "{scope:?} {}",
                outer.chord
            );
            assert!(keymap.visible(&stack).iter().all(|binding| binding.scope == scope));
        }
    }
}

#[test]
fn a_transparent_scope_passes_unbound_chords_outward() {
    let keymap = Keymap::defaults();
    for &scope in TRANSPARENT_SCOPES {
        let inner = keymap.visible(&[scope]);
        for outer in keymap.bindings.iter().filter(|binding| binding.scope != scope) {
            if inner.iter().any(|binding| binding.chord == outer.chord) {
                continue;
            }
            let stack = [scope, outer.scope];
            assert_eq!(
                keymap.resolve(outer.chord, &stack),
                Resolution::Bound(&outer.command),
                "{scope:?} {}",
                outer.chord
            );
            assert!(
                keymap
                    .visible(&stack)
                    .iter()
                    .any(|&binding| std::ptr::eq(binding, outer))
            );
        }
    }
}

#[test]
fn a_text_scope_types_unbound_printables() {
    let keymap = Keymap::defaults();
    let scope = Scope::CommandLine;
    let inner = keymap.visible(&[scope]);
    for outer in keymap.bindings.iter().filter(|binding| binding.scope != scope) {
        if inner.iter().any(|binding| binding.chord == outer.chord) {
            continue;
        }

        let expected = match outer.chord.code() {
            Key::Char(character) if outer.chord.mods().is_empty() => Resolution::Text(character),
            _ => Resolution::Swallowed,
        };
        let stack = [scope, outer.scope];
        assert_eq!(keymap.resolve(outer.chord, &stack), expected, "{}", outer.chord);
        assert!(keymap.visible(&stack).iter().all(|binding| binding.scope == scope));
    }
}

#[test]
fn an_inner_binding_shadows_an_outer_one() {
    let chord = Chord::new(Key::F(1), Modifiers::NONE);
    let keymap = Keymap {
        bindings: vec![
            Binding {
                scope: Scope::Global,
                chord,
                command: CommandOutcome::Shell(ShellCommand::OpenPalettePane),
                description: None,
            },
            Binding {
                scope: Scope::Genome,
                chord,
                command: CommandOutcome::Shell(ShellCommand::NextTab),
                description: None,
            },
        ],
    };
    let stack = [Scope::Genome, Scope::Global];
    assert_eq!(
        keymap.resolve(chord, &stack),
        Resolution::Bound(&CommandOutcome::Shell(ShellCommand::NextTab))
    );
    let visible = keymap.visible(&stack);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].command, CommandOutcome::Shell(ShellCommand::NextTab));
}

#[test]
fn keys_for_returns_every_chord_for_the_command_in_its_scope() {
    let keymap = Keymap::defaults();
    for binding in &keymap.bindings {
        let expected: Vec<_> = keymap
            .visible(&[binding.scope])
            .into_iter()
            .filter(|candidate| candidate.command == binding.command)
            .map(|candidate| candidate.chord)
            .collect();
        assert_eq!(keymap.keys_for(binding.scope, &binding.command), expected);
    }
}

#[test]
fn shift_normalizes_to_the_printable_glyph() {
    let keymap = Keymap::defaults();
    for binding in &keymap.bindings {
        let Key::Char(character) = binding.chord.code() else {
            continue;
        };
        if character.is_ascii_lowercase() || !binding.chord.mods().is_empty() {
            continue;
        }

        let chord = Chord::new(Key::Char(character.to_ascii_lowercase()), Modifiers::SHIFT);
        assert_eq!(chord, binding.chord, "{}", binding.chord);
    }
}
