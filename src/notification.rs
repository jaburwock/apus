//! User-facing messages and the shell's append-only diagnostic log.
//!
//! A [`Notification`] is an *instant* — it records that something happened. In-flight work is a
//! *duration* and is not a notification: a job emits one record when it starts and the shell
//! derives its progress line from [`crate::viewer::BackgroundWork`] every frame. Keep the two
//! separate.
//!
//! Producers push through [`NotificationQueue`]'s methods rather than assigning a field, so the
//! call stays the one seam every message crosses. The shell appends every record it receives to
//! [`DiagnosticLog`] and projects it into [`NotificationStack`] for timed display.

use std::time::Duration;

use crate::Instant;

/// Lifetime of an [`Severity::Info`] record.
pub const INFO_DURATION: Duration = Duration::from_secs(4);

/// Lifetime of a [`Severity::Warn`] record — long enough to read a thing that went wrong.
pub const WARN_DURATION: Duration = Duration::from_secs(10);

/// How much attention a record deserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something finished, and the user may want to know.
    Info,
    /// The app declined an action or degraded, but carried on.
    Warn,
    /// Work the user asked for cannot happen. Never expires; see [`Lifetime::Sticky`].
    Error,
}

impl Severity {
    /// The lifetime a record of this severity gets unless a caller overrides it.
    pub fn default_lifetime(self) -> Lifetime {
        match self {
            Self::Info => Lifetime::Timed(INFO_DURATION),
            Self::Warn => Lifetime::Timed(WARN_DURATION),
            Self::Error => Lifetime::Sticky,
        }
    }
}

/// How long a record stays on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    Timed(Duration),
    /// Shown until the process exits. There is deliberately no dismissal: an error must be
    /// impossible to miss, and a corner filling with stale failures is the signal working — it
    /// says the error sources need resolving, not that the display needs a hide key.
    Sticky,
}

/// One thing that happened, ready to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub text: String,
    pub severity: Severity,
    pub lifetime: Lifetime,
}

impl Notification {
    /// A record with its severity's default lifetime.
    pub fn new(severity: Severity, text: String) -> Self {
        Self {
            text,
            severity,
            lifetime: severity.default_lifetime(),
        }
    }

    /// Overrides the default lifetime, for a record that should linger or pass quickly.
    pub fn with_lifetime(mut self, lifetime: Lifetime) -> Self {
        self.lifetime = lifetime;
        self
    }
}

/// A producer's outbox. Viewers own one and push into it; the shell drains it each frame.
///
/// Draining rather than sharing keeps ownership plain: a viewer never reaches into the shell's
/// display, and the shell never reaches into a viewer's state.
#[derive(Debug, Default)]
pub(crate) struct NotificationQueue {
    pending: Vec<Notification>,
}

impl NotificationQueue {
    pub(crate) fn info(&mut self, text: impl Into<String>) {
        self.push(Severity::Info, text);
    }

    pub(crate) fn warn(&mut self, text: impl Into<String>) {
        self.push(Severity::Warn, text);
    }

    pub(crate) fn error(&mut self, text: impl Into<String>) {
        self.push(Severity::Error, text);
    }

    pub(crate) fn push(&mut self, severity: Severity, text: impl Into<String>) {
        self.pending.push(Notification::new(severity, text.into()));
    }

    /// Whether anything is waiting. Lets a viewer report that it needs a frame.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Takes everything queued since the last drain.
    pub(crate) fn drain(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.pending)
    }
}

/// One recorded diagnostic event in the shell's append-only log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
}

/// Append-only diagnostic log owned by the shell.
///
/// [`NotificationStack`] is the timed visual projection over this log: entries expire from the
/// corner display, while the log retains every record emitted during the session.
#[derive(Debug, Default)]
pub(crate) struct DiagnosticLog {
    entries: Vec<Diagnostic>,
}

impl DiagnosticLog {
    pub(crate) fn push(&mut self, severity: Severity, message: impl Into<String>) {
        self.entries.push(Diagnostic {
            severity,
            message: message.into(),
        });
    }

    pub(crate) fn entries(&self) -> &[Diagnostic] {
        &self.entries
    }

    pub(crate) fn has_errors(&self) -> bool {
        self.entries.iter().any(|entry| entry.severity == Severity::Error)
    }
}

/// A record the shell is displaying, with the moment it arrived.
#[derive(Debug)]
struct Entry {
    notification: Notification,
    received_at: Instant,
}

impl Entry {
    fn is_expired(&self, now: Instant) -> bool {
        match self.notification.lifetime {
            Lifetime::Timed(duration) => now.duration_since(self.received_at) >= duration,
            Lifetime::Sticky => false,
        }
    }
}

/// The shell's live display list, oldest first.
#[derive(Debug, Default)]
pub(crate) struct NotificationStack {
    entries: Vec<Entry>,
}

impl NotificationStack {
    pub(crate) fn push(&mut self, notification: Notification) {
        self.entries.push(Entry {
            notification,
            received_at: Instant::now(),
        });
    }

    /// Drops expired records. Returns whether anything is still displayed, so the shell knows
    /// whether it must keep redrawing to expire the rest.
    pub(crate) fn tick(&mut self) -> bool {
        let now = Instant::now();
        self.entries.retain(|entry| !entry.is_expired(now));
        !self.entries.is_empty()
    }

    /// The records currently displayed, oldest first.
    pub(crate) fn visible(&self) -> impl ExactSizeIterator<Item = &Notification> {
        self.entries.iter().map(|entry| &entry.notification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_a_queue_yields_each_record_once() {
        let mut queue = NotificationQueue::default();
        queue.info("indexed");
        queue.error("failed");

        let drained = queue.drain();
        assert_eq!(
            drained.iter().map(|n| n.severity).collect::<Vec<_>>(),
            vec![Severity::Info, Severity::Error],
            "records drain in emission order"
        );
        assert!(!queue.has_pending(), "a drained queue is empty");
        assert!(queue.drain().is_empty(), "a second drain finds nothing left");
    }

    #[test]
    fn tick_expires_timed_records_and_keeps_sticky_errors() {
        let mut stack = NotificationStack::default();
        stack.push(
            Notification::new(Severity::Info, "indexed".into())
                .with_lifetime(Lifetime::Timed(Duration::from_millis(1))),
        );
        stack.push(Notification::new(Severity::Error, "worker stopped".into()));
        // Advance records directly so the test checks persistence without waiting on policy time.
        for entry in &mut stack.entries {
            entry.received_at -= Duration::from_secs(86_400);
        }

        assert!(stack.tick(), "the error keeps the stack alive");
        let visible: Vec<_> = stack.visible().collect();
        assert_eq!(visible.len(), 1, "the timed record must expire");
        assert_eq!(visible[0].severity, Severity::Error);
    }

    #[test]
    fn an_explicit_lifetime_overrides_the_severity_default() {
        let sticky_info = Notification::new(Severity::Info, "held".into()).with_lifetime(Lifetime::Sticky);
        let entry = Entry {
            notification: sticky_info,
            received_at: Instant::now() - Duration::from_secs(86_400),
        };
        assert!(
            !entry.is_expired(Instant::now()),
            "an explicit lifetime wins over the default"
        );
    }
}
