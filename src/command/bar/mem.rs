//! Process RSS readout used by the global status bar.
//!
//! `MemoryReadout` owns the small amount of OS-facing state needed to display this
//! process's resident memory without exposing `sysinfo` to the bar or app shell.
//! It requests only process memory data; the portability choice keeps this module
//! independent of Linux-specific `/proc` parsing.
//!
//! Construction performs an initial read, while later `refresh` calls are throttled
//! by the module's refresh interval and reuse the cached display text between reads.
//! A process that cannot be found produces a safe placeholder rather than panicking.
//!
//! The bar calls `refresh` from `BarState::tick`, not from every draw, because a
//! redraw is frequent and an OS refresh is comparatively expensive.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// How often to actually re-query OS memory info.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Tracks this process's own resident memory, refreshed at most once per
/// `REFRESH_INTERVAL` regardless of how often `refresh` is called.
pub struct MemoryReadout {
    system: System,
    pid: Pid,
    last_refresh: Option<Instant>,
    text: String,
}

impl MemoryReadout {
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing().with_memory()),
        );
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let text = format_rss(&system, pid);
        Self {
            system,
            pid,
            last_refresh: Some(Instant::now()),
            text,
        }
    }

    /// Re-queries RSS if `REFRESH_INTERVAL` has elapsed since the last query;
    /// otherwise leaves `text` as-is. Cheap to call every draw.
    pub fn refresh(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_refresh
            && now.duration_since(last) < REFRESH_INTERVAL
        {
            return;
        }
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        self.text = format_rss(&self.system, self.pid);
        self.last_refresh = Some(now);
    }

    /// The current display text, e.g. `"128M"`.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Default for MemoryReadout {
    fn default() -> Self {
        Self::new()
    }
}

fn format_rss(system: &System, pid: Pid) -> String {
    let Some(process) = system.process(pid) else {
        return "?".to_string();
    };
    let bytes = process.memory();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1}G", mb / 1024.0)
    } else {
        format!("{:.0}M", mb)
    }
}
