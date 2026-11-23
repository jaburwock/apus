//! Native `winit` window runner over `wgpu::Surface`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use apus::{
    ANIMATION_FRAME_INTERVAL, App, DRAW_ACTIVITY_WINDOW, FileArgs, Key, Modifiers, MouseEvent, MouseKind,
    advance_settle_deadline, build_app, frame_layout, should_poll_rather_than_block,
};
use ratatui::Terminal;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use apus_desktop::{CellMetrics, GridBackend, Renderer};

const CRT_FADE_DURATION: Duration = Duration::from_millis(1000);
const MIN_FONT_SIZE_PX: f32 = 4.0;
const MIN_INTERACTIVE_FONT_SIZE: f32 = 6.0;
const MAX_INTERACTIVE_FONT_SIZE: f32 = 48.0;
const SCROLL_LOGICAL_PIXELS_PER_FONT_POINT: f32 = 40.0;

pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub font_size: f32,
    pub weight_boost: bool,
    pub crt: bool,
    pub files: FileArgs,
}

struct WindowGpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    plain_renderer: Renderer,
    crt_renderer: Option<Renderer>,
}

impl WindowGpu {
    fn crt_renderer(&mut self, weight_boost: bool) -> &mut Renderer {
        self.crt_renderer
            .get_or_insert_with(|| Renderer::new(&self.device, &self.queue, self.config.format, weight_boost, true))
    }
}

struct CrtFade {
    start: Instant,
    from: f32,
}

struct DesktopApp {
    cfg: WindowConfig,
    gpu: Option<WindowGpu>,
    app: Option<App>,
    terminal: Option<Terminal<GridBackend>>,
    cell_metrics: Option<CellMetrics>,
    modifiers: Modifiers,
    cursor_px: (f64, f64),
    left_button_down: bool,
    settle_deadline: Option<Instant>,
    crt_fade: Option<CrtFade>,
    had_input_event: bool,
    scale_factor: f64,
    size_px: (u32, u32),
}

fn grid_dimensions(size_px: (u32, u32), metrics: CellMetrics) -> (u16, u16) {
    let cols = (size_px.0 as f32 / metrics.width_px).floor() as u16;
    let rows = (size_px.1 as f32 / metrics.height_px).floor() as u16;
    (cols, rows)
}

impl DesktopApp {
    fn new(cfg: WindowConfig) -> Self {
        Self {
            cfg,
            gpu: None,
            app: None,
            terminal: None,
            cell_metrics: None,
            modifiers: Modifiers::NONE,
            cursor_px: (0.0, 0.0),
            left_button_down: false,
            settle_deadline: Some(Instant::now() + DRAW_ACTIVITY_WINDOW),
            crt_fade: None,
            had_input_event: true,
            scale_factor: 1.0,
            size_px: (0, 0),
        }
    }

    fn reconfigure(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        if self.size_px.0 == 0 || self.size_px.1 == 0 {
            self.cancel_mouse_gesture();
            return;
        }
        gpu.config.width = self.size_px.0;
        gpu.config.height = self.size_px.1;
        gpu.surface.configure(&gpu.device, &gpu.config);
        self.resize_grid();
    }

    fn resize_grid(&mut self) {
        self.cancel_mouse_gesture();
        let Some(gpu) = &self.gpu else {
            return;
        };
        let size_px = (self.cfg.font_size * self.scale_factor as f32).max(MIN_FONT_SIZE_PX);
        let metrics = gpu.plain_renderer.cell_metrics(size_px);
        self.cell_metrics = Some(metrics);

        let (cols, rows) = grid_dimensions(self.size_px, metrics);
        if cols >= 1 && rows >= 1 {
            let pw = self.size_px.0.min(u32::from(u16::MAX)) as u16;
            let ph = self.size_px.1.min(u32::from(u16::MAX)) as u16;
            if let Some(terminal) = self.terminal.as_mut() {
                terminal.backend_mut().resize(cols, rows, pw, ph);
                let _ = terminal.autoresize();
            } else {
                let backend = GridBackend::new(cols, rows, pw, ph);
                self.terminal = Terminal::new(backend).ok();
            }
        }
    }

    fn cursor_cell(&self) -> Option<(u16, u16)> {
        let metrics = self.cell_metrics?;
        let (cols, rows) = grid_dimensions(self.size_px, metrics);
        let col = (self.cursor_px.0 / f64::from(metrics.width_px)).floor();
        let row = (self.cursor_px.1 / f64::from(metrics.height_px)).floor();
        ((0.0..f64::from(cols)).contains(&col) && (0.0..f64::from(rows)).contains(&row))
            .then_some((col as u16, row as u16))
    }

    fn send_mouse(&mut self, kind: MouseKind) {
        let cell = self.cursor_cell();
        let Some(app) = &mut self.app else {
            return;
        };
        if let Some((column, row)) = cell {
            app.on_mouse(MouseEvent { column, row, kind });
            self.had_input_event = true;
            if let Some(gpu) = &self.gpu {
                gpu.window.request_redraw();
            }
        } else {
            app.cancel_mouse_gesture();
        }
    }

    fn cancel_mouse_gesture(&mut self) {
        self.left_button_down = false;
        if let Some(app) = &mut self.app {
            app.cancel_mouse_gesture();
        }
    }

    fn adjust_font_size(&mut self, delta: f32) {
        let current = self.cfg.font_size.max(MIN_FONT_SIZE_PX / self.scale_factor as f32);
        // A CLI size outside the interactive range can still step toward it.
        let min = MIN_INTERACTIVE_FONT_SIZE.min(current);
        let max = MAX_INTERACTIVE_FONT_SIZE.max(current);
        let size = (current + delta).clamp(min, max);
        if size.is_finite() && size != current {
            self.cfg.font_size = size;
            self.resize_grid();
            self.had_input_event = true;
        }
    }

    fn crt_mix(&self, now: Instant) -> f32 {
        let Some(fade) = &self.crt_fade else {
            return f32::from(self.cfg.crt);
        };
        let elapsed = now.duration_since(fade.start).as_secs_f32() / CRT_FADE_DURATION.as_secs_f32();
        if self.cfg.crt {
            (fade.from + elapsed).min(1.0)
        } else {
            (fade.from - elapsed).max(0.0)
        }
    }

    fn redraw(&mut self) {
        let now = Instant::now();
        let crt_mix = self.crt_mix(now);
        let Some(app) = self.app.as_mut() else {
            return;
        };
        let viewer_activity = app.tick();
        let real_draw_required = self.had_input_event || viewer_activity || self.crt_fade.is_some();
        self.had_input_event = false;
        if crt_mix == f32::from(self.cfg.crt) {
            self.crt_fade = None;
        }

        let decision = advance_settle_deadline(self.settle_deadline, real_draw_required, now);
        self.settle_deadline = decision.next_deadline;
        if !decision.should_draw {
            return;
        }

        let Some(metrics) = self.cell_metrics else {
            return;
        };
        let (cols, rows) = grid_dimensions(self.size_px, metrics);
        if cols < 1 || rows < 1 || self.size_px.0 == 0 || self.size_px.1 == 0 {
            return;
        }

        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if terminal
            .draw(|frame| {
                let [tab_bar, content] = frame_layout(frame.area());
                app.draw(frame, tab_bar, content, decision.loop_state);
            })
            .is_err()
        {
            return;
        }

        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let renderer = if crt_mix == 0.0 {
            &mut gpu.plain_renderer
        } else {
            let renderer = gpu.crt_renderer.as_mut().expect("CRT renderer initialized");
            renderer.set_crt_mix(crt_mix);
            renderer
        };
        renderer.render_grid(
            &gpu.device,
            &gpu.queue,
            &view,
            (gpu.config.width, gpu.config.height),
            terminal.backend().grid(),
            metrics,
        );
        gpu.queue.present(frame);
    }
}

fn translate_modifiers(state: ModifiersState) -> Modifiers {
    let mut mods = Modifiers::NONE;
    if state.shift_key() {
        mods |= Modifiers::SHIFT;
    }
    if state.control_key() {
        mods |= Modifiers::CONTROL;
    }
    if state.alt_key() {
        mods |= Modifiers::ALT;
    }
    if state.super_key() {
        mods |= Modifiers::SUPER;
    }
    mods
}

fn translate_key_event(event: &KeyEvent, mods: Modifiers) -> Option<(Key, Modifiers)> {
    let mut out_mods = mods;
    let key = match &event.logical_key {
        WinitKey::Named(named) => match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Escape => Key::Esc,
            NamedKey::Tab => {
                if out_mods.contains(Modifiers::SHIFT) {
                    out_mods.remove(Modifiers::SHIFT);
                    Key::BackTab
                } else {
                    Key::Tab
                }
            }
            NamedKey::Backspace => Key::Backspace,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Char(' '),
            NamedKey::F1 => Key::F(1),
            NamedKey::F2 => Key::F(2),
            NamedKey::F3 => Key::F(3),
            NamedKey::F4 => Key::F(4),
            NamedKey::F5 => Key::F(5),
            NamedKey::F6 => Key::F(6),
            NamedKey::F7 => Key::F(7),
            NamedKey::F8 => Key::F(8),
            NamedKey::F9 => Key::F(9),
            NamedKey::F10 => Key::F(10),
            NamedKey::F11 => Key::F(11),
            NamedKey::F12 => Key::F(12),
            _ => return None,
        },
        WinitKey::Character(text) => {
            let mut chars = text.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            // Match terminal behaviour: shifted character input already carries the shifted
            // scalar (e.g. `G`, `:`, `?`), so drop SHIFT unless another modifier is held.
            if out_mods == Modifiers::SHIFT {
                out_mods = Modifiers::NONE;
            }
            Key::Char(ch)
        }
        _ => return None,
    };
    Some((key, out_mods))
}

impl ApplicationHandler for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("Apus")
            .with_inner_size(LogicalSize::new(self.cfg.width, self.cfg.height));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create desktop window"),
        );

        let desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(event_loop.owned_display_handle()));
        let instance = wgpu::Instance::new(desc);
        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("failed to find suitable GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("apus-window-device"),
            ..Default::default()
        }))
        .expect("failed to create wgpu device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let phys = window.inner_size();
        self.size_px = (phys.width, phys.height);
        self.scale_factor = window.scale_factor();

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: phys.width.max(1),
            height: phys.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        let plain_renderer = Renderer::new(&device, &queue, format, self.cfg.weight_boost, false);

        let mut gpu = WindowGpu {
            window,
            surface,
            config,
            device,
            queue,
            plain_renderer,
            crt_renderer: None,
        };
        if self.cfg.crt {
            gpu.crt_renderer(self.cfg.weight_boost);
        }
        self.gpu = Some(gpu);
        self.app = Some(build_app(&self.cfg.files));
        self.reconfigure();
        if let Some(gpu) = &self.gpu {
            gpu.window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.size_px = (size.width, size.height);
                self.reconfigure();
                self.had_input_event = true;
                if let Some(gpu) = &self.gpu {
                    gpu.window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.reconfigure();
                self.had_input_event = true;
                if let Some(gpu) = &self.gpu {
                    gpu.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = translate_modifiers(new_mods.state());
            }
            WindowEvent::KeyboardInput {
                event, is_synthetic, ..
            } if event.logical_key == WinitKey::Named(NamedKey::F12) && self.modifiers == Modifiers::NONE => {
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && !is_synthetic
                    && let Some(gpu) = &mut self.gpu
                {
                    gpu.crt_renderer(self.cfg.weight_boost);
                    gpu.window.request_redraw();
                    let start = Instant::now();
                    self.crt_fade = Some(CrtFade {
                        start,
                        from: self.crt_mix(start),
                    });
                    self.cfg.crt = !self.cfg.crt;
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && let Some((key, mods)) = translate_key_event(&event, self.modifiers)
                {
                    match (key, mods) {
                        (Key::Char('+' | '='), Modifiers::CONTROL) => self.adjust_font_size(1.0),
                        (Key::Char('+'), mods) if mods == (Modifiers::CONTROL | Modifiers::SHIFT) => {
                            self.adjust_font_size(1.0);
                        }
                        (Key::Char('-'), Modifiers::CONTROL) => self.adjust_font_size(-1.0),
                        _ => {
                            let Some(app) = self.app.as_mut() else {
                                return;
                            };
                            app.on_key(key, mods);
                            self.had_input_event = true;
                            if app.should_quit() {
                                event_loop.exit();
                                return;
                            }
                            if let Some(gpu) = &self.gpu {
                                gpu.window.request_redraw();
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.modifiers == Modifiers::CONTROL => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pixels) => {
                        pixels.to_logical::<f32>(self.scale_factor).y / SCROLL_LOGICAL_PIXELS_PER_FONT_POINT
                    }
                };
                self.adjust_font_size(steps);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let previous_cell = self.cursor_cell();
                self.cursor_px = (position.x, position.y);
                if self.cursor_cell() != previous_cell {
                    self.send_mouse(if self.left_button_down {
                        MouseKind::LeftDrag
                    } else {
                        MouseKind::Moved
                    });
                }
            }
            WindowEvent::CursorLeft { .. } | WindowEvent::Focused(false) => self.cancel_mouse_gesture(),
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.left_button_down = state == ElementState::Pressed;
                self.send_mouse(if self.left_button_down {
                    MouseKind::LeftDown
                } else {
                    MouseKind::LeftUp
                });
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                if self.app.as_ref().is_some_and(App::should_quit) {
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.as_ref().is_some_and(App::should_quit) {
            event_loop.exit();
            return;
        }
        let needs_poll = self.had_input_event
            || self.crt_fade.is_some()
            || should_poll_rather_than_block(self.settle_deadline)
            || self.app.as_ref().is_some_and(App::has_pending_work);
        let repeat_at = self.app.as_ref().and_then(App::mouse_repeat_deadline);
        let now = Instant::now();

        if needs_poll {
            let frame_at = now + ANIMATION_FRAME_INTERVAL;
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                repeat_at.map_or(frame_at, |at| at.min(frame_at)),
            ));
        } else if let Some(at) = repeat_at.filter(|at| *at > now) {
            event_loop.set_control_flow(ControlFlow::WaitUntil(at));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        if (needs_poll || repeat_at.is_some_and(|at| at <= now))
            && let Some(gpu) = &self.gpu
        {
            gpu.window.request_redraw();
        }
    }
}

pub fn run(cfg: WindowConfig) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("failed to create event loop: {e}"))?;
    let mut app = DesktopApp::new(cfg);
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("desktop event loop error: {e}"))
}
