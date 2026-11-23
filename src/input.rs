//! Backend-agnostic keyboard and mouse input types.
//!
//! Keeps terminal and windowing crates out of the library: each runner translates its own
//! backend events into these types at its boundary.

use std::fmt;
use std::ops::{BitOr, BitOrAssign};

use ratatui::layout::Position;

/// A keyboard key recognised by the keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    Tab,
    BackTab,
    Esc,
    Backspace,
    F(u8),
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Left => f.write_str("←"),
            Self::Right => f.write_str("→"),
            Self::Up => f.write_str("↑"),
            Self::Down => f.write_str("↓"),
            Self::PageUp => f.write_str("PgUp"),
            Self::PageDown => f.write_str("PgDn"),
            Self::Enter => f.write_str("⏎"),
            Self::Tab => f.write_str("⇥"),
            Self::BackTab => f.write_str("Back Tab"),
            Self::Esc => f.write_str("Esc"),
            Self::Backspace => f.write_str("Backspace"),
            Self::F(n) => write!(f, "F{n}"),
            Self::Char(' ') => f.write_str("Space"),
            Self::Char(character) => write!(f, "{character}"),
        }
    }
}

/// Bitmask of keyboard modifiers held with a [`Key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);
    pub const HYPER: Self = Self(1 << 4);
    pub const META: Self = Self(1 << 5);

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Supplemental mouse event kinds handled by viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Moved,
    LeftDown,
    LeftDrag,
    LeftUp,
}

/// A mouse event in terminal cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub column: u16,
    pub row: u16,
    pub kind: MouseKind,
}

impl MouseEvent {
    pub(crate) fn position(self) -> Position {
        Position::new(self.column, self.row)
    }
}
