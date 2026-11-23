use apus::{
    App, Instant, Key, Modifiers, MouseEvent, MouseKind, advance_settle_deadline, build_demo_app, frame_layout,
};
use apus_desktop::{CellMetrics, GridBackend, Renderer};
use ratatui::Terminal;
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, KeyboardEvent};

// Static allocations keep the optimized WASM from embedding duplicates of the data.
static BAM: &[u8] = include_bytes!(concat!(env!("APUS_DEMO_DATA"), "/demo.bam"));
static VCF: &[u8] = include_bytes!(concat!(env!("APUS_DEMO_DATA"), "/demo.vcf.gz"));
static FAI: &str = include_str!(concat!(env!("APUS_DEMO_DATA"), "/chr1.fai"));
static FASTA: &[u8] = include_bytes!(concat!(env!("APUS_DEMO_DATA"), "/windows.fa.gz"));
const FONT_SIZE: f32 = 15.0;

#[wasm_bindgen]
pub struct WebDemo {
    app: App,
    canvas: HtmlCanvasElement,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    terminal: Terminal<GridBackend>,
    metrics: CellMetrics,
    settle_deadline: Option<Instant>,
    dirty: bool,
}

#[wasm_bindgen]
pub enum PointerKind {
    Move,
    Press,
    Drag,
    Release,
}

#[wasm_bindgen]
pub async fn start(canvas: HtmlCanvasElement) -> Result<WebDemo, JsError> {
    console_error_panic_hook::set_once();
    let app = build_demo_app(VCF, BAM, FAI, FASTA).map_err(|error| JsError::new(&error))?;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::GL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))?;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        })
        .await?;

    let caps = surface.get_capabilities(&adapter);
    let mut config = surface
        .get_default_config(&adapter, 0, 0)
        .ok_or_else(|| JsError::new("WebGL2 canvas has no supported surface configuration"))?;
    if let Some(format) = caps.formats.into_iter().find(|format| !format.is_srgb()) {
        config.format = format;
    }
    let errors = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let renderer = Renderer::new(&device, &queue, config.format, false, false);
    if let Some(error) = errors.pop().await {
        return Err(JsError::new(&error.to_string()));
    }
    let metrics = renderer.cell_metrics(FONT_SIZE);
    let terminal = Terminal::new(GridBackend::new(1, 1, 1, 1))?;
    Ok(WebDemo {
        app,
        canvas,
        surface,
        device,
        queue,
        config,
        renderer,
        terminal,
        metrics,
        settle_deadline: None,
        dirty: true,
    })
}

#[wasm_bindgen]
impl WebDemo {
    pub fn key(&mut self, event: &KeyboardEvent) -> bool {
        let Some((key, modifiers)) = translate_key(event) else {
            return false;
        };
        let handled = self.app.on_key(key, modifiers);
        self.dirty |= handled;
        handled
    }

    pub fn pointer(&mut self, x: f64, y: f64, kind: PointerKind) -> bool {
        if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) || self.config.width == 0 || self.config.height == 0 {
            return false;
        }
        let column = (x * f64::from(self.config.width) / f64::from(self.metrics.width_px)) as u16;
        let row = (y * f64::from(self.config.height) / f64::from(self.metrics.height_px)) as u16;
        let grid = self.terminal.backend().grid();
        if column >= grid.cols() || row >= grid.rows() {
            return false;
        }
        let kind = match kind {
            PointerKind::Move => MouseKind::Moved,
            PointerKind::Press => MouseKind::LeftDown,
            PointerKind::Drag => MouseKind::LeftDrag,
            PointerKind::Release => MouseKind::LeftUp,
        };
        self.app.on_mouse(MouseEvent { column, row, kind });
        self.dirty = true;
        true
    }

    pub fn cancel_pointer(&mut self) {
        self.app.cancel_mouse_gesture();
    }

    pub fn mouse_repeat_delay_ms(&self) -> Option<f64> {
        self.app
            .mouse_repeat_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_secs_f64() * 1000.0)
    }

    /// Returns whether the browser should schedule another animation frame.
    pub fn frame(&mut self, css_width: u32, css_height: u32, pixel_ratio: f32) -> Result<bool, JsError> {
        if css_width == 0 || css_height == 0 {
            self.app.cancel_mouse_gesture();
            return Ok(false);
        }
        let max_dimension = self.device.limits().max_texture_dimension_2d.min(u16::MAX.into());
        let scale = pixel_ratio.min(max_dimension as f32 / css_width.max(css_height) as f32);
        let width = (css_width as f32 * scale).round().max(1.0) as u32;
        let height = (css_height as f32 * scale).round().max(1.0) as u32;
        let resized = width != self.config.width || height != self.config.height;
        let metrics = self.renderer.cell_metrics(FONT_SIZE * scale);
        if resized {
            self.config.width = width;
            self.config.height = height;
            self.canvas.set_width(width);
            self.canvas.set_height(height);
            self.surface.configure(&self.device, &self.config);
        }
        let cols = ((width as f32 / metrics.width_px) as u16).max(1);
        let rows = ((height as f32 / metrics.height_px) as u16).max(1);
        if resized || metrics.size_px != self.metrics.size_px {
            self.app.cancel_mouse_gesture();
            self.metrics = metrics;
            self.terminal
                .backend_mut()
                .resize(cols, rows, width as u16, height as u16);
            self.terminal.autoresize()?;
            self.dirty = true;
        }

        let active = self.app.tick() | std::mem::take(&mut self.dirty);
        let decision = advance_settle_deadline(self.settle_deadline, active, Instant::now());
        self.settle_deadline = decision.next_deadline;
        if decision.should_draw {
            self.terminal.draw(|frame| {
                let [tabs, content] = frame_layout(frame.area());
                self.app.draw(frame, tabs, content, decision.loop_state);
            })?;
            let frame = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                    self.surface.configure(&self.device, &self.config);
                    self.dirty = true;
                    return Ok(true);
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(true),
                wgpu::CurrentSurfaceTexture::Validation => {
                    return Err(JsError::new("WebGL2 surface validation failed"));
                }
            };
            let view = frame.texture.create_view(&Default::default());
            self.renderer.render_grid(
                &self.device,
                &self.queue,
                &view,
                (width, height),
                self.terminal.backend().grid(),
                self.metrics,
            );
            self.queue.present(frame);
        }
        Ok(self.settle_deadline.is_some() || self.app.has_pending_work())
    }

    pub fn should_quit(&self) -> bool {
        self.app.should_quit()
    }

    #[cfg(feature = "smoke")]
    pub fn frame_text(&self) -> String {
        let grid = self.terminal.backend().grid();
        let mut text = String::new();
        for row in grid.cells().chunks(usize::from(grid.cols())) {
            text.extend(row.iter().map(|cell| cell.symbol));
            text.push('\n');
        }
        text
    }
}

fn translate_key(event: &KeyboardEvent) -> Option<(Key, Modifiers)> {
    if event.is_composing() || event.meta_key() {
        return None;
    }
    let mut mods = Modifiers::NONE;
    for (pressed, modifier) in [
        (event.shift_key(), Modifiers::SHIFT),
        (event.ctrl_key(), Modifiers::CONTROL),
        (event.alt_key(), Modifiers::ALT),
    ] {
        if pressed {
            mods |= modifier;
        }
    }
    let name = event.key();
    let key = match name.as_str() {
        "Enter" => Key::Enter,
        "Escape" => Key::Esc,
        "Tab" if event.shift_key() => {
            mods.remove(Modifiers::SHIFT);
            Key::BackTab
        }
        "Tab" => Key::Tab,
        "Backspace" => Key::Backspace,
        "ArrowLeft" => Key::Left,
        "ArrowRight" => Key::Right,
        "ArrowUp" => Key::Up,
        "ArrowDown" => Key::Down,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        name => {
            let mut chars = name.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if mods == Modifiers::SHIFT {
                mods = Modifiers::NONE;
            }
            Key::Char(ch)
        }
    };
    Some((key, mods))
}
