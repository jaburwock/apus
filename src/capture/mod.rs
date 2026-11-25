//! Shared headless capture settle loop, CPU rasterization, and atomic PNG publication.
//!
//! Both the terminal `apus` binary and `apus-desktop` route `--capture <PATH>` through [`run`]
//! before any terminal or window setup. `apus-desktop --llvmcapture <PATH>` shares
//! [`prepare_capture`] and [`write_png`] so both renderers consume the exact same settled
//! [`CellGrid`] and publish PNGs atomically.

pub mod raster;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Terminal;

use crate::app::App;
use crate::cli::{CaptureArgs, FileArgs};
use crate::grid::{CellGrid, CellMetrics, FIRA_CODE_METRICS, GridBackend};
use crate::input::{Key, Modifiers};
use crate::notification::Severity;
use crate::runtime::{advance_settle_deadline, build_app, frame_layout};
use crate::spinner::{DRAW_ACTIVITY_WINDOW, LoopState};

pub use raster::render_grid;

/// Default headless capture grid and font size.
/// `12pt` at `1x` scale yields `7x15` px cells (`980x600` px for `140x40` cells).
pub const DEFAULT_CAPTURE_COLS: u16 = 140;
pub const DEFAULT_CAPTURE_ROWS: u16 = 40;
pub const DEFAULT_CAPTURE_FONT_SIZE: f32 = 12.0;

/// One scripted input step executed in argv order after startup settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptStep {
    Command(String),
    Key(Key, Modifiers),
}

/// Configuration for one headless capture run.
pub struct CaptureConfig {
    pub output: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub font_size: f32,
    pub scale: f32,
    pub timeout: Duration,
    pub weight_boost: bool,
    pub steps: Vec<ScriptStep>,
    pub files: FileArgs,
}

/// A fully settled cell grid and resolved pixel geometry ready for CPU or GPU rendering.
pub struct PreparedCapture {
    pub grid: CellGrid,
    pub cell_metrics: CellMetrics,
    pub width: u32,
    pub height: u32,
}

/// Builds a [`CaptureConfig`] from shared CLI arguments and clap matches (preserving `--cmd`/`--key` order).
pub fn config_from_args(
    args: &CaptureArgs,
    output: PathBuf,
    matches: &clap::ArgMatches,
    files: FileArgs,
) -> Result<CaptureConfig, String> {
    let steps = collect_ordered_steps(matches)?;
    Ok(CaptureConfig {
        output,
        width: args.width,
        height: args.height,
        font_size: args.font_size.unwrap_or(DEFAULT_CAPTURE_FONT_SIZE),
        scale: args.scale,
        timeout: Duration::from_millis(args.timeout_ms),
        weight_boost: args.weight_boost,
        steps,
        files,
    })
}

/// Collects `--cmd` and `--key` arguments in their original interleaved argv order.
pub fn collect_ordered_steps(matches: &clap::ArgMatches) -> Result<Vec<ScriptStep>, String> {
    let mut indexed: Vec<(usize, ScriptStep)> = Vec::new();
    if let (Some(indices), Some(values)) = (matches.indices_of("cmds"), matches.get_many::<String>("cmds")) {
        for (idx, cmd) in indices.zip(values) {
            indexed.push((idx, ScriptStep::Command(cmd.clone())));
        }
    }
    if let (Some(indices), Some(values)) = (matches.indices_of("keys"), matches.get_many::<String>("keys")) {
        for (idx, spec) in indices.zip(values) {
            let (key, mods) = parse_key_spec(spec)?;
            indexed.push((idx, ScriptStep::Key(key, mods)));
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, step)| step).collect())
}

/// Parses a human-readable scripted key specification such as `Enter`, `Down`, `Ctrl+d`, or `g`.
pub fn parse_key_spec(spec: &str) -> Result<(Key, Modifiers), String> {
    let mut mods = Modifiers::NONE;
    let parts: Vec<&str> = spec.split('+').collect();
    let (mod_parts, key_part) = if parts.len() > 1 && !parts.last().unwrap().is_empty() {
        (&parts[..parts.len() - 1], *parts.last().unwrap())
    } else {
        (&[][..], spec)
    };

    for &m in mod_parts {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "alt" => mods |= Modifiers::ALT,
            "shift" => mods |= Modifiers::SHIFT,
            "super" => mods |= Modifiers::SUPER,
            other => return Err(format!("unknown key modifier {other:?} in {spec:?}")),
        }
    }

    let key = match key_part.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Esc,
        "tab" => Key::Tab,
        "backtab" => Key::BackTab,
        "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "space" => Key::Char(' '),
        f if f.starts_with('f') && f.len() >= 2 => {
            let n = f[1..]
                .parse::<u8>()
                .map_err(|_| format!("invalid function key {key_part:?}"))?;
            Key::F(n)
        }
        _ => {
            let mut chars = key_part.chars();
            let ch = chars
                .next()
                .ok_or_else(|| format!("empty key specification {spec:?}"))?;
            if chars.next().is_some() {
                return Err(format!("unrecognised key name {key_part:?} in {spec:?}"));
            }
            Key::Char(ch)
        }
    };

    Ok((key, mods))
}

/// Resolves and validates pixel dimensions and cell grid size from optional CLI overrides.
pub fn resolve_capture_dimensions(
    width: Option<u32>,
    height: Option<u32>,
    cell_metrics: CellMetrics,
) -> Result<(u32, u32, u16, u16), String> {
    let cell_w = cell_metrics.width_px.round().max(1.0) as u32;
    let cell_h = cell_metrics.height_px.round().max(1.0) as u32;
    let width = match width {
        Some(w) => w,
        None => u32::from(DEFAULT_CAPTURE_COLS)
            .checked_mul(cell_w)
            .ok_or_else(|| "default capture width overflows u32".to_string())?,
    };
    let height = match height {
        Some(h) => h,
        None => u32::from(DEFAULT_CAPTURE_ROWS)
            .checked_mul(cell_h)
            .ok_or_else(|| "default capture height overflows u32".to_string())?,
    };
    raster::validate_dimensions(width, height)?;
    let cols = (width / cell_w).min(u32::from(u16::MAX)) as u16;
    let rows = (height / cell_h).min(u32::from(u16::MAX)) as u16;
    if cols < 1 || rows < 1 {
        return Err(format!(
            "capture dimensions {width}x{height} are smaller than one {cell_w}x{cell_h} cell"
        ));
    }
    Ok((width, height, cols, rows))
}

fn draw_frame(terminal: &mut Terminal<GridBackend>, app: &mut App, loop_state: LoopState) -> Result<(), String> {
    terminal
        .draw(|frame| {
            let [tab_bar, content] = frame_layout(frame.area());
            app.draw(frame, tab_bar, content, loop_state);
        })
        .map_err(|e| format!("terminal draw failed: {e}"))?;
    Ok(())
}

/// Ticks and draws into the cell grid until background work drains and `advance_settle_deadline`
/// emits its final settled frame.
fn settle(app: &mut App, terminal: &mut Terminal<GridBackend>, deadline: Instant) -> Result<(), String> {
    // Initial draw lets the active viewer learn its viewport and queue viewport-sized I/O.
    draw_frame(terminal, app, LoopState::Active)?;
    let mut settle_deadline = Some(Instant::now() + DRAW_ACTIVITY_WINDOW);

    loop {
        if Instant::now() >= deadline {
            return Err("capture timed out waiting for app to settle".to_string());
        }

        let activity = app.tick();
        let pending = app.has_pending_work();

        if activity || pending {
            let decision = advance_settle_deadline(settle_deadline, true, Instant::now());
            settle_deadline = decision.next_deadline;
            if decision.should_draw {
                draw_frame(terminal, app, decision.loop_state)?;
            }
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        // Background work is done; emit the final settled frame through `advance_settle_deadline`.
        let final_now = settle_deadline.unwrap_or_else(Instant::now);
        let decision = advance_settle_deadline(settle_deadline, false, final_now);
        settle_deadline = decision.next_deadline;
        if decision.should_draw {
            draw_frame(terminal, app, decision.loop_state)?;
        }

        // One more check in case the final draw kicked off new work.
        if app.tick() || app.has_pending_work() {
            settle_deadline = Some(Instant::now() + DRAW_ACTIVITY_WINDOW);
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        if settle_deadline.is_none() {
            return Ok(());
        }
    }
}

fn dump_diagnostics(app: &App) {
    for diag in app.diagnostics() {
        let level = match diag.severity {
            Severity::Error => "ERROR",
            Severity::Warn => "WARN",
            Severity::Info => "INFO",
        };
        eprintln!("[{level}] {}", diag.message);
    }
}

/// Drives startup, replays scripted `--cmd`/`--key` steps, waits for the view to settle, and
/// returns the final [`PreparedCapture`] cell grid and metrics.
pub fn prepare_capture(config: &CaptureConfig) -> Result<PreparedCapture, String> {
    if !config.font_size.is_finite() || config.font_size <= 0.0 {
        return Err(format!(
            "capture font size must be positive and finite, got {}",
            config.font_size
        ));
    }
    if !config.scale.is_finite() || config.scale <= 0.0 {
        return Err(format!(
            "capture scale must be positive and finite, got {}",
            config.scale
        ));
    }
    let size_px = (config.font_size * config.scale).max(4.0);
    if !size_px.is_finite() {
        return Err("capture font size * scale overflowed".to_string());
    }

    let cell_metrics = FIRA_CODE_METRICS.cell_metrics(size_px);
    let (width, height, cols, rows) = resolve_capture_dimensions(config.width, config.height, cell_metrics)?;

    let backend = GridBackend::new(
        cols,
        rows,
        width.min(u32::from(u16::MAX)) as u16,
        height.min(u32::from(u16::MAX)) as u16,
    );
    let mut terminal = Terminal::new(backend).map_err(|e| format!("failed to create ratatui terminal: {e}"))?;

    let mut app = build_app(&config.files);
    let deadline = Instant::now() + config.timeout;

    if let Err(err) = settle(&mut app, &mut terminal, deadline) {
        dump_diagnostics(&app);
        return Err(err);
    }

    for step in &config.steps {
        match step {
            ScriptStep::Command(cmd) => {
                let report = app.run_command(cmd);
                app.report_dispatch(report, Severity::Error);
            }
            ScriptStep::Key(key, mods) => {
                app.on_key(*key, *mods);
            }
        }
        if let Err(err) = settle(&mut app, &mut terminal, deadline) {
            dump_diagnostics(&app);
            return Err(err);
        }
    }

    if app.has_errors() {
        dump_diagnostics(&app);
        return Err("capture aborted due to application error diagnostics".to_string());
    }

    Ok(PreparedCapture {
        grid: terminal.backend().grid().clone(),
        cell_metrics,
        width,
        height,
    })
}

/// Encodes an `Rgba8` buffer into PNG bytes in memory, validating dimensions and buffer length.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expected = raster::validate_dimensions(width, height)?;
    if rgba.len() != expected {
        return Err(format!(
            "PNG buffer length {} does not match {width}x{height} RGBA ({expected} bytes)",
            rgba.len()
        ));
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("failed to write PNG header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("failed to encode PNG image data: {e}"))?;
    }
    Ok(encoded)
}

/// Encodes `rgba` completely in memory, writes it to a sibling temporary file, and atomically
/// publishes it to `path` so a failed job never leaves a partial report image.
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let encoded = encode_png(width, height, rgba)?;

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid output PNG path: {}", path.display()))?;

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let (tmp_path, mut tmp_file) = loop {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".{file_name}.apus-tmp-{pid}-{seq}"));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "failed to create temporary PNG file in {}: {err}",
                    dir.display()
                ));
            }
        }
    };

    let write_res = tmp_file.write_all(&encoded).and_then(|()| tmp_file.flush());
    drop(tmp_file);
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("failed to write {}: {err}", path.display()));
    }

    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("failed to publish {}: {err}", path.display()));
    }

    Ok(())
}

/// Runs headless CPU capture end-to-end and atomically writes the output PNG.
pub fn run(config: CaptureConfig) -> Result<(), String> {
    let prepared = prepare_capture(&config)?;
    let rgba = raster::render_grid(
        &prepared.grid,
        prepared.cell_metrics,
        prepared.width,
        prepared.height,
        config.weight_boost,
    )?;
    write_png(&config.output, prepared.width, prepared.height, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::{CommandFactory, FromArgMatches};

    #[test]
    fn resolve_capture_dimensions_fits_exact_default_grid_and_validates_overrides() {
        for (cw, ch) in [(7.0, 15.0), (9.0, 18.0), (18.0, 37.0)] {
            let metrics = CellMetrics {
                width_px: cw,
                height_px: ch,
                baseline_px: ch * 0.75,
                size_px: ch,
            };
            let (w, h, cols, rows) = resolve_capture_dimensions(None, None, metrics).unwrap();
            assert_eq!(cols, DEFAULT_CAPTURE_COLS);
            assert_eq!(rows, DEFAULT_CAPTURE_ROWS);
            assert_eq!(w, u32::from(cols) * cw as u32);
            assert_eq!(h, u32::from(rows) * ch as u32);
        }

        let metrics = CellMetrics {
            width_px: 9.0,
            height_px: 18.0,
            baseline_px: 14.0,
            size_px: 15.0,
        };
        assert_eq!(
            resolve_capture_dimensions(Some(1111), Some(680), metrics).unwrap(),
            (1111, 680, 123, 37)
        );
        assert!(resolve_capture_dimensions(Some(0), Some(720), metrics).is_err());
        assert!(resolve_capture_dimensions(Some(8), Some(720), metrics).is_err());
        assert!(resolve_capture_dimensions(Some(100), Some(17), metrics).is_err());
    }

    #[test]
    fn parse_key_spec_handles_named_keys_chars_and_modifiers() {
        assert_eq!(parse_key_spec("Enter").unwrap(), (Key::Enter, Modifiers::NONE));
        assert_eq!(parse_key_spec("Ctrl+d").unwrap(), (Key::Char('d'), Modifiers::CONTROL));
        assert_eq!(parse_key_spec("F2").unwrap(), (Key::F(2), Modifiers::NONE));
        assert_eq!(parse_key_spec("Space").unwrap(), (Key::Char(' '), Modifiers::NONE));
        assert!(parse_key_spec("UnknownKey").is_err());
    }

    #[test]
    fn collect_ordered_steps_preserves_interleaved_cmd_and_key_order() {
        let matches = Cli::command()
            .try_get_matches_from([
                "apus",
                "--capture",
                "out.png",
                "--cmd",
                ":focustab GenomeViewer",
                "--key",
                "Down",
                "--cmd",
                ":goto 100",
                "--key",
                "Ctrl+d",
            ])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();
        let cfg = config_from_args(&cli.capture, PathBuf::from("out.png"), &matches, cli.files).unwrap();
        assert_eq!(
            cfg.steps,
            vec![
                ScriptStep::Command(":focustab GenomeViewer".to_string()),
                ScriptStep::Key(Key::Down, Modifiers::NONE),
                ScriptStep::Command(":goto 100".to_string()),
                ScriptStep::Key(Key::Char('d'), Modifiers::CONTROL),
            ]
        );
    }

    #[test]
    fn write_png_publishes_atomically_and_leaves_no_partial_file_on_error() {
        let dir = std::env::temp_dir().join(format!("apus-capture-png-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let out = dir.join("figure.png");

        // Bad buffer length fails in memory without touching the destination or leaving temp files.
        assert!(write_png(&out, 4, 4, &[0u8; 8]).is_err());
        assert!(!out.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        // Valid write produces a decodable PNG with exact pixel bytes.
        let rgba: Vec<u8> = (0..(4 * 3))
            .flat_map(|i| [(i * 17) as u8, (i * 31) as u8, (i * 47) as u8, 255])
            .collect();
        write_png(&out, 4, 3, &rgba).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let file = std::fs::File::open(&out).unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(file));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((info.width, info.height), (4, 3));
        assert_eq!(&decoded[..info.buffer_size()], rgba.as_slice());

        // Subsequent encoding error must not clobber the previously published PNG.
        let original = std::fs::read(&out).unwrap();
        assert!(write_png(&out, 0, 3, &[]).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), original);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn capture_aborts_without_publishing_png_on_missing_input_or_bad_command() {
        let _palette = crate::style::palette::lock_for_test();
        let dir = std::env::temp_dir().join(format!("apus-capture-err-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();

        // Missing input file.
        let missing_out = dir.join("missing.png");
        let err = run(CaptureConfig {
            output: missing_out.clone(),
            width: Some(280),
            height: Some(150),
            font_size: DEFAULT_CAPTURE_FONT_SIZE,
            scale: 1.0,
            timeout: Duration::from_secs(5),
            weight_boost: false,
            steps: vec![],
            files: FileArgs {
                config: None,
                files: vec![PathBuf::from("testdata/bam/no-such-capture.bam")],
            },
        });
        assert!(err.is_err());
        assert!(!missing_out.exists(), "failed input must not publish a PNG");

        // Invalid scripted command.
        let bad_cmd_out = dir.join("bad-cmd.png");
        let err = run(CaptureConfig {
            output: bad_cmd_out.clone(),
            width: Some(280),
            height: Some(150),
            font_size: DEFAULT_CAPTURE_FONT_SIZE,
            scale: 1.0,
            timeout: Duration::from_secs(5),
            weight_boost: false,
            steps: vec![ScriptStep::Command(":notacommand".to_string())],
            files: FileArgs::default(),
        });
        assert!(err.is_err());
        assert!(!bad_cmd_out.exists(), "failed command must not publish a PNG");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
