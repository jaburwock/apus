//! Timing values and lightweight indicators shared by the redraw loop and command bar.
//!
//! The draw-activity indicator answers one question: whether the frame loop is actively redrawing
//! or settling toward idle. The ASCII spinner is separate and marks an in-flight progress line.
//! Neither indicator carries notification severity; user-facing severity belongs to the
//! notification system.
//!
//! `FlashMessage` is short-lived feedback for command-line input, while redraw-loop timing is
//! supplied by `main` through `LoopState`. Keep these signals independent so activity, progress,
//! and user messages cannot be mistaken for one another.

use std::time::Duration;

use crate::Instant;

use ratatui::style::Color;

use crate::style::Chrome;

/// Flash-message lifetime.
pub(crate) const FLASH_MESSAGE_DURATION: Duration = Duration::from_secs(4);

/// Busy spinner frames.
const SPINNER_FRAMES: [char; 4] = ['-', '\\', '|', '/'];
const SPINNER_FRAME_DURATION: Duration = Duration::from_millis(120);

/// Two-cell frames for the status-row radial sweep.
const BRAILLE_SPINNER_FRAMES: [(char, char); 24] = [
    ('\u{2800}', '\u{2803}'),
    ('\u{2800}', '\u{2803}'),
    ('\u{2800}', '\u{280b}'),
    ('\u{2800}', '\u{280a}'),
    ('\u{2800}', '\u{281a}'),
    ('\u{2800}', '\u{2812}'),
    ('\u{2800}', '\u{2834}'),
    ('\u{2800}', '\u{2824}'),
    ('\u{2800}', '\u{28a4}'),
    ('\u{2800}', '\u{2884}'),
    ('\u{2800}', '\u{28c4}'),
    ('\u{2800}', '\u{2844}'),
    ('\u{2800}', '\u{2844}'),
    ('\u{28a0}', '\u{2800}'),
    ('\u{28e0}', '\u{2800}'),
    ('\u{2860}', '\u{2800}'),
    ('\u{2864}', '\u{2800}'),
    ('\u{2824}', '\u{2800}'),
    ('\u{2824}', '\u{2800}'),
    ('\u{2812}', '\u{2800}'),
    ('\u{2813}', '\u{2800}'),
    ('\u{2811}', '\u{2800}'),
    ('\u{2819}', '\u{2800}'),
    ('\u{2818}', '\u{2800}'),
];
const BRAILLE_SPINNER_FRAME_DURATION: Duration = Duration::from_millis(40);

/// The settled glyph: a full pair, meaning the app is asleep.
const BRAILLE_SPINNER_IDLE: (char, char) = ('\u{28FF}', '\u{28FF}'); // ⣿⣿

/// Duration of the post-activity settling animation.
pub const DRAW_ACTIVITY_WINDOW: Duration = Duration::from_millis(1000);

/// Returns a busy spinner glyph.
pub fn spinner_glyph(started_at: Instant) -> char {
    let frame = (started_at.elapsed().as_millis() / SPINNER_FRAME_DURATION.as_millis()) as usize;
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

/// Whether a draw is active or settling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopState {
    /// A real event drove this draw.
    Active,
    /// No event this frame; `elapsed` is time since the last event.
    Settling { elapsed: Duration },
}

/// A command-line message and its start time.
///
/// The status row shows one kind of message — feedback on what was just typed — so this carries
/// no severity. A failure worth ranking is a [`crate::notification::Notification`].
pub struct FlashMessage {
    pub(crate) text: String,
    pub(crate) set_at: Instant,
}

impl FlashMessage {
    pub fn new(text: String) -> Self {
        Self {
            text,
            set_at: Instant::now(),
        }
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.set_at.elapsed() >= FLASH_MESSAGE_DURATION
    }
}

/// The draw-activity indicator: two cells saying whether the app is redrawing or asleep.
///
/// **Do not overload this.** It reports one thing — whether the frame loop is doing work right
/// now — and it deliberately knows nothing about jobs, errors, or messages. Anything a *user*
/// needs told belongs in the notification system ([`crate::notification`]), which has severity,
/// lifetimes, and room for words.
pub fn draw_activity_glyphs(loop_state: LoopState) -> (char, char, Color) {
    let (left, right) = match loop_state {
        LoopState::Active => BRAILLE_SPINNER_FRAMES[0],
        LoopState::Settling { elapsed } if elapsed < DRAW_ACTIVITY_WINDOW => {
            let frame = (elapsed.as_millis() / BRAILLE_SPINNER_FRAME_DURATION.as_millis()) as usize;
            BRAILLE_SPINNER_FRAMES[frame % BRAILLE_SPINNER_FRAMES.len()]
        }
        LoopState::Settling { .. } => BRAILLE_SPINNER_IDLE,
    };
    (left, right, Chrome::ACTIVITY_FG())
}
