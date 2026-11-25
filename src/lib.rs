//! The library crate behind the `apus` binary and the utility binaries in `src/bin/`.
//!
//! Everything is private by default; the `pub use` list below is the whole surface, kept
//! deliberately minimal. It exists so `main` can assemble the app and so the testgen binaries can
//! reach the generators — not as a general-purpose API. Adding an export is a real decision: the
//! smaller this list stays, the freer every module is to change shape.
//!
//! The module list is ordered by layer rather than alphabetised by accident. The first block is the
//! shared root leaves — `basepair`, `jobs`, `residue` and `cigar` depend on nothing, and the rest
//! sit above them without cycling. `command` and `viewer_app` do not reference each other in either
//! direction; they meet only at the protocol seam `command::protocol` defines and `viewer` imports.
//! The second block is composition: `app` owns the shell, `cli` parses the invocation, and
//! `runtime` owns shared startup assembly, frame layout, and redraw-loop cadence.

mod basepair;
mod cigar;
mod command;
mod input;
mod io;
mod jobs;
mod keys;
mod list_cursor;
mod locus;
mod notification;
mod popover;
mod refgenome;
mod residue;
mod search;
#[macro_use]
mod setting;
mod spinner;
mod style;
#[cfg(any(test, feature = "testgen"))]
mod testgen;
mod vcf_file;
mod viewer;
mod viewer_app;

mod app;
#[cfg(not(target_arch = "wasm32"))]
pub mod capture;
mod cli;
#[cfg(feature = "resident-demo")]
mod demo;
pub mod grid;
mod runtime;

// wasm32-unknown-unknown has no standard-library clock.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

#[cfg(feature = "resident-demo")]
pub use demo::build_demo_app;

pub use app::App;
pub use cli::{CaptureArgs, Cli, FileArgs};
pub use input::{Key, Modifiers, MouseEvent, MouseKind};
pub use notification::{Diagnostic, Severity};
#[cfg(any(test, feature = "testgen"))]
pub use refgenome::RefIndex;
pub use runtime::{
    ANIMATION_FRAME_INTERVAL, LoopDecision, MAX_EVENTS_PER_FRAME, advance_settle_deadline, build_app,
    drain_should_stop, frame_layout, should_poll_rather_than_block,
};
pub use spinner::{DRAW_ACTIVITY_WINDOW, LoopState};
#[cfg(any(test, feature = "testgen"))]
pub use testgen::{RefSeq, generate_sam_line, header_from_ref_index};
#[cfg(feature = "profiling")]
pub use viewer_app::genome::profile as profiling;
