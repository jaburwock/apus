//! Command-line editing state.
//!
//! This module owns only the mutable input, prompt, and provisional mode for
//! the command bar. It does not resolve commands, search viewer state, or
//! dispatch outcomes.
//!
//! Slash input starts in [`CommandLineMode::Goto`] and remains there while the
//! input uses the restricted goto vocabulary. The first other character switches
//! to [`CommandLineMode::Search`], and deleting it can restore goto mode.
//!
//! This mode also chooses the submission path: numeric `/` input stays a goto
//! and never consults a search corpus, while other input enters search mode.
//! Submission still goes through [`crate::command::parse`], so coordinate
//! validation is not duplicated in the keystroke path.

/// Returns whether a character is valid while typing a goto.
fn is_goto_char(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_whitespace() || matches!(c, '.' | ',' | 'k' | 'K' | 'm' | 'M')
}

/// Mode of a command line opened by `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLineMode {
    Goto,
    Search,
}

/// Mutable command-line input and its display mode.
pub struct CommandLineState {
    pub input: String,
    /// Optional display label before the input.
    pub prompt: &'static str,
    /// Current interpretation of the input.
    pub mode: CommandLineMode,
}

impl CommandLineState {
    /// Starts a command line with its trigger character.
    pub fn new(trigger: char) -> Self {
        let prompt = match trigger {
            'c' => "Switch Contig: ",
            _ => "",
        };
        Self {
            input: String::from(trigger),
            prompt,
            mode: CommandLineMode::Goto,
        }
    }

    /// Re-derives the mode after editing.
    fn update_mode(&mut self) {
        if !self.input.starts_with('/') {
            self.mode = CommandLineMode::Goto;
            return;
        }
        self.mode = if self.input[1..].chars().all(is_goto_char) {
            CommandLineMode::Goto
        } else {
            CommandLineMode::Search
        };
    }

    /// Appends a character and updates the mode.
    pub fn push(&mut self, c: char) {
        self.input.push(c);
        self.update_mode();
    }

    /// Removes the last character while keeping the trigger.
    pub fn backspace(&mut self) {
        if self.input.len() > 1 {
            self.input.pop();
        }
        self.update_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_prefers_goto_until_a_search_only_character_appears() {
        for character in "0123456789.,kKmM \t".chars() {
            let mut line = CommandLineState::new('/');
            line.push(character);
            assert_eq!(line.mode, CommandLineMode::Goto, "{character:?} should remain goto");
        }

        let mut line = CommandLineState::new('/');
        for character in "1.25M, 10k".chars() {
            line.push(character);
        }
        assert_eq!(line.mode, CommandLineMode::Goto);

        line.push('R');
        assert_eq!(line.mode, CommandLineMode::Search);
        line.backspace();
        assert_eq!(line.mode, CommandLineMode::Goto);
    }
}
