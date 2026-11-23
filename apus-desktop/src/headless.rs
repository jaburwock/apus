//! Headless offscreen PNG capture under Mesa `llvmpipe`.
//!
//! Runs without creating any `winit` event loop or window. Drives the shared `App` settle loop
//! purely on Ratatui's cell grid and submits one GPU render pass on the final settled frame,
//! reading back `Rgba8Unorm` pixels with 256-byte row alignment stripped before PNG encoding.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use apus::{
    App, DRAW_ACTIVITY_WINDOW, FileArgs, Key, LoopState, Modifiers, Severity, advance_settle_deadline, build_app,
    frame_layout,
};
use ratatui::Terminal;

use apus_desktop::{CellMetrics, GridBackend, Renderer};

/// Default headless capture grid and font size, independent of windowed mode's `1280x720` / `15pt`
/// defaults. `12pt` at `1x` scale yields `7x15` px cells (`980x600` px for `140x40` cells): crisp
/// for LLM vision inspection while staying under 1024px wide and minimising `llvmpipe` pixel work.
pub const DEFAULT_CAPTURE_COLS: u16 = 140;
pub const DEFAULT_CAPTURE_ROWS: u16 = 40;
pub const DEFAULT_CAPTURE_FONT_SIZE: f32 = 12.0;

/// One scripted input step executed in argv order after startup settles.
#[derive(Debug, Clone)]
pub enum ScriptStep {
    Command(String),
    Key(Key, Modifiers),
}

/// Configuration for one headless `--capture` run.
pub struct CaptureConfig {
    pub output: PathBuf,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub font_size: f32,
    pub scale: f32,
    pub timeout: Duration,
    pub weight_boost: bool,
    pub crt: bool,
    pub steps: Vec<ScriptStep>,
    pub files: FileArgs,
}

fn resolve_capture_dimensions(
    width: Option<u32>,
    height: Option<u32>,
    cell_metrics: CellMetrics,
) -> Result<(u32, u32, u16, u16), String> {
    let cell_w = cell_metrics.width_px.round().max(1.0) as u32;
    let cell_h = cell_metrics.height_px.round().max(1.0) as u32;
    let width = width.unwrap_or(u32::from(DEFAULT_CAPTURE_COLS) * cell_w);
    let height = height.unwrap_or(u32::from(DEFAULT_CAPTURE_ROWS) * cell_h);
    if width == 0 || height == 0 {
        return Err("capture width and height must be non-zero".to_string());
    }
    let cols = (width / cell_w).min(u32::from(u16::MAX)) as u16;
    let rows = (height / cell_h).min(u32::from(u16::MAX)) as u16;
    if cols < 1 || rows < 1 {
        return Err(format!(
            "capture dimensions {width}x{height} are smaller than one {cell_w}x{cell_h} cell"
        ));
    }
    Ok((width, height, cols, rows))
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

fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

fn strip_row_padding(padded: &[u8], width: u32, height: u32, padded_row_bytes: u32) -> Vec<u8> {
    let unpadded_row = (width * 4) as usize;
    let stride = padded_row_bytes as usize;
    let mut out = Vec::with_capacity(unpadded_row * height as usize);
    for row in 0..height as usize {
        let start = row * stride;
        out.extend_from_slice(&padded[start..start + unpadded_row]);
    }
    out
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

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder
        .write_header()
        .map_err(|e| format!("failed to write PNG header for {}: {e}", path.display()))?;
    png_writer
        .write_image_data(rgba)
        .map_err(|e| format!("failed to write PNG data for {}: {e}", path.display()))?;
    Ok(())
}

/// Runs headless capture end-to-end, returning `Err` with a message for `stderr` on failure.
pub fn run(config: CaptureConfig) -> Result<(), String> {
    if config.width == Some(0) || config.height == Some(0) {
        return Err("capture width and height must be non-zero".to_string());
    }

    // GL-only headless instance verified against Mesa llvmpipe.
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::GL;
    let instance = wgpu::Instance::new(desc);

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|e| format!("failed to find GL wgpu adapter for headless capture: {e}"))?;

    let info = adapter.get_info();
    if !info.name.to_ascii_lowercase().contains("llvmpipe") {
        return Err(format!(
            "headless capture requires Mesa llvmpipe adapter, got {:?} ({:?})",
            info.name, info.backend
        ));
    }

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("apus-headless-device"),
        ..Default::default()
    }))
    .map_err(|e| format!("failed to create wgpu device: {e}"))?;

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut renderer = Renderer::new(&device, &queue, format, config.weight_boost, config.crt);

    let size_px = (config.font_size * config.scale).max(4.0);
    let cell_metrics = renderer.cell_metrics(size_px);
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

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("apus-headless-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    renderer.render_grid(
        &device,
        &queue,
        &view,
        (width, height),
        terminal.backend().grid(),
        cell_metrics,
    );

    let padded_row = padded_bytes_per_row(width);
    let buffer_size = u64::from(padded_row) * u64::from(height);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("apus-headless-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("apus-headless-copy"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = staging.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed during readback: {e}"))?;
    rx.recv()
        .map_err(|_| "readback channel dropped".to_string())?
        .map_err(|e| format!("staging buffer map failed: {e}"))?;

    let pixels = {
        let mapped = slice
            .get_mapped_range()
            .map_err(|e| format!("failed to map readback buffer range: {e}"))?;
        strip_row_padding(&mapped, width, height, padded_row)
    };
    staging.unmap();

    write_png(&config.output, width, height, &pixels)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_padding_alignment_and_stripping_preserve_exact_pixels() {
        // Width 333 * 4 = 1332 bytes -> pads to 1536 (6 * 256).
        let width = 333u32;
        let height = 5u32;
        let padded_row = padded_bytes_per_row(width);
        assert_eq!(padded_row, 1536);

        let mut padded = vec![0xEEu8; (padded_row * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let off = (y * padded_row + x * 4) as usize;
                padded[off] = (x & 0xff) as u8;
                padded[off + 1] = (y & 0xff) as u8;
                padded[off + 2] = ((x + y) & 0xff) as u8;
                padded[off + 3] = 255;
            }
        }

        let stripped = strip_row_padding(&padded, width, height, padded_row);
        assert_eq!(stripped.len(), (width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let off = ((y * width + x) * 4) as usize;
                assert_eq!(
                    &stripped[off..off + 4],
                    &[(x & 0xff) as u8, (y & 0xff) as u8, ((x + y) & 0xff) as u8, 255]
                );
            }
        }
    }

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
}
