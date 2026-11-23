//! Shared runner bootstrap, frame layout, and redraw-cadence helpers.
//!
//! Entry points (`apus` and any future runner) share app assembly, startup command ordering,
//! chrome layout, and settling-loop decisions through this module while keeping backend event
//! handling and surface lifecycles in their own binaries.
//!
//! CLI paths become one ordered newline-separated command batch and are replayed through the
//! normal parser and dispatcher. Startup therefore shares quoting, routing, focus, and failure
//! behavior with typed commands instead of becoming a second loading path.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Instant;

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::app::App;
use crate::cli::FileArgs;
use crate::command::encode_arg;
use crate::command::protocol::DispatchReport;
use crate::io::IoWorkerHandle;
use crate::notification::Severity;
use crate::refgenome::IndexedFasta;
use crate::spinner::{DRAW_ACTIVITY_WINDOW, LoopState};
use crate::style::palette::file::{ACTIVE_PALETTE, FALLBACK as PALETTE_FALLBACK, load_lines as load_palette_lines};
use crate::viewer_app::genome::GenomeViewer;
use crate::viewer_app::genome::alignment::AlignmentTrack;
use crate::viewer_app::genome::settings::GenomeViewerSettings;
use crate::viewer_app::vcf::VCFViewer;

/// Poll interval while redraw activity is settling.
pub const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Upper bound on events consumed before a draw is forced.
///
/// Held movement keys repeat faster than a full-frame draw, so the queue is drained per frame
/// rather than one event per draw. The bound only exists so an unbounded event source cannot
/// starve rendering; ordinary key-repeat bursts are far below it.
pub const MAX_EVENTS_PER_FRAME: usize = 256;

const CONFIG_FILENAME: &str = ".apusconfig";

/// One redraw-loop iteration's decisions.
pub struct LoopDecision {
    /// The updated `settle_deadline` the caller should carry into its next
    /// iteration.
    pub next_deadline: Option<Instant>,
    /// Whether this iteration should draw a frame at all — `false`
    /// only once fully settled with nothing left to paint.
    pub should_draw: bool,
    /// What to hand [`App::draw`] when `should_draw` is `true`.
    pub loop_state: LoopState,
}

/// Emits the deadline's final settling frame before returning to the idle state.
pub fn advance_settle_deadline(
    settle_deadline: Option<Instant>,
    real_draw_required: bool,
    now: Instant,
) -> LoopDecision {
    if real_draw_required {
        return LoopDecision {
            next_deadline: Some(now + DRAW_ACTIVITY_WINDOW),
            should_draw: true,
            loop_state: LoopState::Active,
        };
    }
    let Some(deadline) = settle_deadline else {
        return LoopDecision {
            next_deadline: None,
            should_draw: false,
            loop_state: LoopState::Active,
        };
    };
    let remaining = deadline.saturating_duration_since(now);
    // The final frame is still drawn even though no follow-up poll is needed.
    let is_final_settle_frame = remaining == Duration::ZERO;
    LoopDecision {
        next_deadline: if is_final_settle_frame { None } else { Some(deadline) },
        should_draw: true,
        loop_state: LoopState::Settling {
            elapsed: DRAW_ACTIVITY_WINDOW.saturating_sub(remaining),
        },
    }
}

/// Whether the redraw loop should poll or block for input.
pub fn should_poll_rather_than_block(settle_deadline: Option<Instant>) -> bool {
    settle_deadline.is_some()
}

/// Whether the input drain should stop and let the frame draw.
///
/// Quit stops it so a pending `:q` is never delayed behind queued input. The last clause is what
/// keeps the drain from ever *waiting* for an event, so an idle loop still blocks in the outer poll
/// rather than spinning here.
pub fn drain_should_stop(should_quit: bool, drained: usize, more_queued: bool) -> bool {
    should_quit || drained >= MAX_EVENTS_PER_FRAME || !more_queued
}

/// Splits a frame area into the single-row top tab bar and the viewer content area below it.
pub fn frame_layout(area: Rect) -> [Rect; 2] {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(area)
}

/// Converts CLI inputs into the startup command sequence.
struct StartupCommands {
    input: String,
    failures: Vec<String>,
}

fn startup_commands(files: &FileArgs) -> StartupCommands {
    let mut failures = Vec::new();
    let mut lines = Vec::new();
    let mut push_path = |prefix: &str, path: &Path| match encode_arg(&path.display().to_string()) {
        Ok(argument) => lines.push(format!("{prefix} {argument}")),
        Err(error) => failures.push(format!("cannot emit {prefix} {path:?}: {error}")),
    };
    if let Some(path) = files.fasta_arg() {
        push_path(":set activefasta", path);
    }
    for (index, path) in files.bam_args().enumerate() {
        push_path(if index == 0 { ":set activebam" } else { ":addbam" }, path);
    }
    if let Some(path) = files.vcf_arg() {
        push_path(":set activevcf", path);
    }
    if let Some(path) = files.gtf_arg() {
        // `:load_index`, not `:load`: the user named a GTF on the command line, so building its
        // index is the thing they asked for. `:load` is for a config that would rather be told.
        push_path(":load_index", path);
    }
    // Emitted in argv order, unlike the `:set` lines above which are ordered by
    // kind: the tab holding the last-named file is the one left focused.
    lines.extend(files.focus_targets().map(|tab| format!(":focustab {tab}")));
    StartupCommands {
        input: lines.join("\n"),
        failures,
    }
}

fn read_startup_config(
    explicit: Option<&Path>,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<(PathBuf, io::Result<String>)> {
    if let Some(path) = explicit {
        return Some((path.to_path_buf(), std::fs::read_to_string(path)));
    }
    let candidates = std::iter::once(cwd.join(CONFIG_FILENAME)).chain(home.map(|home| home.join(CONFIG_FILENAME)));
    for path in candidates {
        let contents = std::fs::read_to_string(&path);
        if !matches!(&contents, Err(error) if error.kind() == io::ErrorKind::NotFound) {
            return Some((path, contents));
        }
    }
    None
}

/// Assembles [`App`] and replays the palette, startup config, and CLI file arguments in that order.
pub fn build_app(files: &FileArgs) -> App {
    let genome_viewer_settings = GenomeViewerSettings::new();
    let io_worker = IoWorkerHandle::spawn();

    let genome_viewer = GenomeViewer::new(
        genome_viewer_settings,
        IndexedFasta::hs1_fallback(),
        AlignmentTrack::new(),
        None,
        io_worker.clone(),
    );
    let mut app = App::new(
        vec![Box::new(genome_viewer), Box::new(VCFViewer::new(io_worker.clone()))],
        io_worker.health(),
    );

    // The palette runs as its own batch, ahead of the file batch, so the app is already tinted
    // before any viewer draws. Kept out of `startup_commands` because it is not a file-loading
    // concern: folding dozens of `set palette` lines in would bury that function's ordering — reference
    // before reads, focus last — under them.
    //
    // Palette failures are warnings so startup still reaches the input files.
    //
    // With no palette file — the ordinary case for a binary run outside the repo — the shipped
    // fallback is selected instead, as the same `:set colorscheme` line a user would type.
    let palette_batch = match load_palette_lines(Path::new(ACTIVE_PALETTE)) {
        Some(Ok(batch)) => Some(batch),
        // Reported here rather than carried to the batch below: a palette that could not be read is
        // this step's failure, and the two reports are the same severity anyway.
        Some(Err(message)) => {
            app.report_dispatch(DispatchReport::failure(message), Severity::Warn);
            None
        }
        None => encode_arg(PALETTE_FALLBACK)
            .map(|name| format!("set colorscheme {name}"))
            .ok(),
    };
    if let Some(batch) = palette_batch {
        let report = app.run_command(&batch);
        app.report_dispatch(report, Severity::Warn);
    }

    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    if let Some((path, contents)) = read_startup_config(files.config.as_deref(), Path::new("."), home.as_deref()) {
        let mut report = match contents {
            Ok(batch) => app.run_command(&batch),
            Err(error) => DispatchReport::failure(format!("cannot read startup config: {error}")),
        };
        for failure in &mut report.failures {
            *failure = format!("{}: {failure}", path.display());
        }
        app.report_dispatch(report, Severity::Error);

        // A config can leave another tab focused; positional Genome files still need their owner.
        if files.focus_targets().any(|tab| tab == "GenomeViewer") {
            let report = app.run_command(":focustab GenomeViewer");
            app.report_dispatch(report, Severity::Error);
        }
    }

    // Errors, unlike the palette's warnings: the user named these files, so a runner without a
    // screen (headless capture) must be able to fail on them through `App::has_errors`.
    let startup = startup_commands(files);
    let mut report = app.run_command(&startup.input);
    report.failures.extend(startup.failures);
    app.report_dispatch(report, Severity::Error);

    app
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A burst of queued input collapses into one frame instead of one frame per event.
    #[test]
    fn the_drain_consumes_a_whole_burst_before_drawing() {
        let mut consumed = 0usize;
        let queued = 40usize;
        loop {
            consumed += 1;
            if drain_should_stop(false, consumed, consumed < queued) {
                break;
            }
        }
        assert_eq!(
            consumed, queued,
            "every queued event must be consumed before the frame draws"
        );
    }

    /// The drain never waits for input, so an idle loop still blocks in the outer poll.
    #[test]
    fn the_drain_stops_as_soon_as_nothing_is_queued() {
        assert!(drain_should_stop(false, 1, false));
    }

    /// A pending quit is not delayed behind queued input.
    #[test]
    fn the_drain_stops_immediately_on_quit_even_with_input_waiting() {
        assert!(drain_should_stop(true, 1, true));
    }

    /// An unbounded event source cannot starve rendering.
    #[test]
    fn the_drain_yields_to_the_frame_at_its_bound() {
        assert!(!drain_should_stop(false, MAX_EVENTS_PER_FRAME - 1, true));
        assert!(drain_should_stop(false, MAX_EVENTS_PER_FRAME, true));
    }

    /// The CLI as a user would type it, with the kinds inferred exactly as `build_app` infers them.
    fn commands_for(files: &[&str]) -> Vec<String> {
        let args = FileArgs {
            files: files.iter().map(PathBuf::from).collect(),
            ..FileArgs::default()
        };
        startup_commands(&args).input.lines().map(str::to_string).collect()
    }

    #[test]
    fn the_reference_is_applied_before_the_bam_that_needs_it() {
        let lines = commands_for(&["reads.bam", "genome.fa"]);
        let fasta = lines.iter().position(|l| l.starts_with(":set activefasta"));
        let bam = lines.iter().position(|l| l.starts_with(":set activebam"));
        assert!(
            fasta < bam,
            "fasta must precede the bam regardless of CLI order, got {lines:?}"
        );
    }

    #[test]
    fn a_vcf_argument_sets_the_active_vcf_whatever_else_is_named() {
        for files in [
            vec!["variants.vcf"],
            vec!["variants.vcf.gz"],
            vec!["reads.bam", "variants.vcf.gz", "genome.fa"],
        ] {
            let lines = commands_for(&files);
            assert!(
                lines.iter().any(|l| l.starts_with(":set activevcf")),
                "{files:?} should set an active vcf, got {lines:?}"
            );
        }
    }

    /// The tab left focused is the one holding the last file named, whatever
    /// its kind — so a VCF alone opens the variant table, but a BAM after it
    /// wins the pileup back.
    #[test]
    fn the_last_named_file_decides_the_focused_tab() {
        // Expected values come from the argv list, not from `focus_targets`.
        let cases = [
            (vec!["variants.vcf.gz"], Some("VCFViewer")),
            (vec!["variants.vcf.gz", "reads.bam"], Some("GenomeViewer")),
            (vec!["reads.bam", "variants.vcf.gz"], Some("VCFViewer")),
            (vec!["reads.bam"], Some("GenomeViewer")),
            (vec!["genome.fa", "variants.vcf.gz"], Some("VCFViewer")),
            (vec!["variants.vcf.gz", "notes.txt"], Some("VCFViewer")),
            (vec!["notes.txt"], None),
            (vec![], None),
        ];
        for (files, expected) in cases {
            let lines = commands_for(&files);
            let focused = lines
                .iter()
                .filter_map(|l| l.strip_prefix(":focustab "))
                .next_back()
                .map(str::to_string);
            assert_eq!(
                focused.as_deref(),
                expected,
                "{files:?} should leave {expected:?} focused, got {lines:?}"
            );
            if expected.is_some() {
                let last_load = lines.iter().rposition(|line| !line.starts_with(":focustab ")).unwrap();
                let first_focus = lines.iter().position(|line| line.starts_with(":focustab ")).unwrap();
                assert!(last_load < first_focus, "focus must follow all loads: {lines:?}");
            }
        }
    }

    #[test]
    fn a_gtf_alone_loads_without_a_reference_or_a_bam() {
        let lines = commands_for(&["hs1.ncbiRefSeq.gtf.gz"]);
        assert!(
            lines.contains(&":load_index \"hs1.ncbiRefSeq.gtf.gz\"".to_string()),
            "got {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with(":set active")),
            "a GTF swaps no active data source, got {lines:?}"
        );
    }

    /// The oracle for `encode_arg` at startup: a generated line must reach a viewer as exactly the
    /// outcome the hand-typed spelling does, and must not split the batch.
    #[test]
    fn generated_paths_preserve_argument_boundaries_and_resolve_like_typed_lines() {
        for (name, typed) in [
            ("my reads.bam", ":set activebam \"my reads.bam\""),
            ("bad\nname.bam", ":set activebam \"bad\\nname.bam\""),
        ] {
            let lines = commands_for(&[name]);
            assert_eq!(lines.len(), 2, "{name:?} split the batch: {lines:?}");
            let generated = lines
                .iter()
                .find(|line| line.starts_with(":set activebam "))
                .expect("a named BAM must produce an activebam line");
            assert_eq!(received(generated), received(typed), "{name:?}: {generated}");
        }
    }

    /// The outcomes a viewer receives when `line` is run as startup runs it.
    fn received(line: &str) -> Vec<crate::command::protocol::CommandOutcome> {
        use crate::command::protocol::{Applied, CommandOutcome, CommandTarget};
        use crate::keys::Keymap;
        use crate::viewer::{StatusProvider, ViewerApp};
        use std::{cell::RefCell, rc::Rc};

        struct Recorder(Rc<RefCell<Vec<CommandOutcome>>>);
        impl CommandTarget for Recorder {
            fn apply_outcome(&mut self, outcome: CommandOutcome) -> Applied {
                self.0.borrow_mut().push(outcome);
                Applied::Ok
            }
        }
        impl StatusProvider for Recorder {}
        impl ViewerApp for Recorder {
            fn title(&self) -> &'static str {
                "Recorder"
            }
            fn draw(&mut self, _frame: &mut ratatui::Frame, _area: Rect, _keymap: &Keymap) {}
            fn tick(&mut self) -> bool {
                false
            }
            fn has_pending_work(&self) -> bool {
                false
            }
        }

        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(vec![Box::new(Recorder(log.clone()))], IoWorkerHandle::spawn().health());
        let report = app.run_command(line);
        assert!(report.is_ok(), "{line}: {:?}", report.failures);
        log.take()
    }

    #[test]
    fn a_bare_run_loads_nothing() {
        assert!(commands_for(&[]).is_empty());
    }

    /// A headless runner exits non-zero through `has_errors` when a named file fails to load.
    #[test]
    fn a_named_file_that_fails_to_load_is_an_error() {
        let _palette = crate::style::palette::lock_for_test();
        let app = build_app(&FileArgs {
            files: vec![PathBuf::from("testdata/bam/no-such.bam")],
            ..FileArgs::default()
        });

        assert!(app.has_errors(), "got {:?}", app.diagnostics());
        assert!(
            app.bar.flash_message.is_none(),
            "startup failures must not use the command bar"
        );
    }

    fn settle_startup(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.has_pending_work() {
            assert!(Instant::now() < deadline, "startup work did not settle");
            app.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn startup_config_reads_only_the_highest_priority_file() {
        let root = std::env::temp_dir().join(format!("apus-config-{}", uuid::Uuid::new_v4()));
        let cwd = root.join("cwd");
        let home = root.join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let explicit = root.join("explicit.conf");
        let local = cwd.join(CONFIG_FILENAME);
        let global = home.join(CONFIG_FILENAME);
        for (path, text) in [(&explicit, "explicit"), (&local, ""), (&global, "home")] {
            std::fs::write(path, text).unwrap();
        }
        let read = |explicit| read_startup_config(explicit, &cwd, Some(&home)).map(|(_, contents)| contents);
        assert_eq!(read(Some(&explicit)).unwrap().unwrap(), "explicit");
        assert_eq!(read(None).unwrap().unwrap(), "", "an empty local config still wins");
        std::fs::remove_file(&explicit).unwrap();
        assert_eq!(
            read(Some(&explicit)).unwrap().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_file(&local).unwrap();
        assert_eq!(read(None).unwrap().unwrap(), "home");
        std::fs::remove_file(&global).unwrap();
        assert!(read(None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_bam_switcher_uses_config_and_cli_paths_and_keeps_the_locus() {
        use crate::input::{Key, Modifiers};
        use ratatui::{Terminal, backend::TestBackend};

        let _palette = crate::style::palette::lock_for_test();
        let root = std::env::temp_dir().join(format!("apus-bams-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let no_bai = root.join("no-index.bam");
        std::fs::copy("testdata/bam/tiny.bam", &no_bai).unwrap();
        let config = root.join("startup.conf");
        std::fs::write(
            &config,
            format!(
                "  # Register a secondary alignment\naddbam {}\nset activecontig chr2\ngoto 637\n\
                 set localareatrack false\nset contigmarkers true\nset chromosometrack false\n\
                 set referencetrack true\nset centline true\nset horizontalzoom close\n\
                 set alignmentverticalzoom close\nfocustab VCFViewer",
                encode_arg(no_bai.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        let mut app = build_app(&FileArgs {
            config: Some(config),
            files: [
                "testdata/bam/tiny.bam",
                "testdata/bam/medium.bam",
                "testdata/bam/missing.bam",
            ]
            .map(PathBuf::from)
            .into(),
        });
        settle_startup(&mut app);
        assert!(!app.has_errors(), "{:?}", app.diagnostics());
        let mut terminal = Terminal::new(TestBackend::new(110, 22)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, app: &mut App| {
            terminal
                .draw(|frame| {
                    let [tabs, view] = frame_layout(frame.area());
                    app.draw(frame, tabs, view, LoopState::Active);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        let first = draw(&mut terminal, &mut app);
        assert!(first[4].starts_with("tiny.bam"));
        app.on_key(Key::Char('B'), Modifiers::NONE);
        settle_startup(&mut app);
        let menu = draw(&mut terminal, &mut app);
        for (file, status) in [
            ("no-index.bam", "No BAI"),
            ("tiny.bam", "Ready"),
            ("medium.bam", "Ready"),
            ("missing.bam", "No BAM or BAI"),
        ] {
            assert!(
                menu.iter().any(|row| row.contains(file) && row.contains(status)),
                "{menu:?}"
            );
        }
        app.on_key(Key::Down, Modifiers::NONE);
        app.on_key(Key::Enter, Modifiers::NONE);
        settle_startup(&mut app);
        let switched = draw(&mut terminal, &mut app);
        assert!(switched[4].starts_with("medium.bam"), "{switched:?}");
        for rows in [&first, &switched] {
            assert!(rows.iter().any(|row| row.contains("g.637")), "{rows:?}");
        }
        for zoom in ["set alignmentverticalzoom mid", "set horizontalzoom mid"] {
            assert!(app.run_command(zoom).is_ok());
            let rows = draw(&mut terminal, &mut app);
            assert!(rows[4].starts_with("medium.bam"), "{rows:?}");
        }
        assert!(!app.has_errors(), "{:?}", app.diagnostics());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settling_requests_a_final_redraw_before_idle_blocking() {
        let start = Instant::now();
        let active = advance_settle_deadline(None, true, start);
        let deadline = active.next_deadline.expect("activity schedules settling");
        assert!(active.should_draw);
        assert!(should_poll_rather_than_block(Some(deadline)));

        for now in [deadline, deadline + Duration::from_millis(1)] {
            let final_frame = advance_settle_deadline(Some(deadline), false, now);
            assert!(final_frame.should_draw, "pending settle must draw its final frame");
            assert_eq!(final_frame.next_deadline, None);
            assert!(!should_poll_rather_than_block(final_frame.next_deadline));
            let idle = advance_settle_deadline(final_frame.next_deadline, false, now);
            assert!(!idle.should_draw);
        }
    }
}
