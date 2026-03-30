/// glazeid — a minimal GlazeWM workspace bar.
///
/// One bar window is created per monitor.  A background tokio task maintains a
/// persistent WebSocket connection to GlazeWM and publishes workspace state
/// through a `watch` channel.  The winit event loop polls the channel on every
/// `AboutToWait` wake-up (driven by a `UserEvent` fired from the IPC task) and
/// redraws only when state has changed.
mod client;
mod config;
mod ipc;
mod renderer;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Result;
use client::{BarState, MonitorWorkspaces};
use config::{BarPosition, Config};
use renderer::Renderer;
use softbuffer::{Context as SbContext, Surface};
use tokio::sync::watch;
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    monitor::MonitorHandle,
    window::{Window, WindowId, WindowLevel},
};

// ---------------------------------------------------------------------------
// User events (IPC → winit)
// ---------------------------------------------------------------------------

/// Sent by the IPC watch task whenever the `BarState` changes.
#[derive(Debug)]
struct StateChanged;

// ---------------------------------------------------------------------------
// Per-bar-window state
// ---------------------------------------------------------------------------

struct BarWindow {
    window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    /// The GlazeWM `device_name` of the monitor this window lives on.
    device_name: String,
    scale_factor: f64,
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct App {
    cfg: Config,
    renderer: Renderer,
    state_rx: watch::Receiver<BarState>,
    #[allow(dead_code)]
    proxy: EventLoopProxy<StateChanged>,
    bars: HashMap<WindowId, BarWindow>,
    sb_ctx: Option<SbContext<Arc<Window>>>,
    /// Set to `true` when the watch channel has a new value we haven't rendered yet.
    dirty: bool,
}

impl App {
    fn new(
        cfg: Config,
        state_rx: watch::Receiver<BarState>,
        proxy: EventLoopProxy<StateChanged>,
    ) -> Self {
        Self {
            cfg,
            renderer: Renderer::new(),
            state_rx,
            proxy,
            bars: HashMap::new(),
            sb_ctx: None,
            dirty: true,
        }
    }

    /// Create one bar window for each connected monitor.
    fn create_windows(&mut self, event_loop: &ActiveEventLoop) {
        let monitors: Vec<MonitorHandle> = event_loop.available_monitors().collect();
        tracing::info!("Creating bar windows for {} monitor(s).", monitors.len());

        for monitor in monitors {
            if let Err(e) = self.create_bar_for_monitor(event_loop, &monitor) {
                tracing::warn!("Failed to create bar for monitor: {e:#}");
            }
        }
    }

    fn create_bar_for_monitor(
        &mut self,
        event_loop: &ActiveEventLoop,
        monitor: &MonitorHandle,
    ) -> Result<()> {
        let scale = monitor.scale_factor();
        let bar_size_phys = (self.cfg.bar_size as f64 * scale) as u32;

        let monitor_pos = monitor.position();
        let monitor_size = monitor.size();

        let (win_x, win_y, win_w, win_h) = bar_geometry(
            self.cfg.position,
            monitor_pos,
            monitor_size,
            bar_size_phys,
        );

        let mut attrs = Window::default_attributes()
            .with_title("glazeid")
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(win_x, win_y))
            .with_inner_size(PhysicalSize::new(win_w, win_h))
            // Transparent background so OS compositing doesn't flash white.
            .with_transparent(false);

        // Pin to a specific monitor when winit supports it.
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_no_redirection_bitmap(false);
        }

        let window = Arc::new(event_loop.create_window(attrs)?);

        // Grab the softbuffer context lazily (needs a valid window first).
        let ctx = self
            .sb_ctx
            .get_or_insert_with(|| {
                SbContext::new(window.clone()).expect("softbuffer context")
            });

        let mut surface = Surface::new(ctx, window.clone()).expect("softbuffer surface");

        // Resize the surface buffer immediately.
        surface
            .resize(
                NonZeroU32::new(win_w.max(1)).unwrap(),
                NonZeroU32::new(win_h.max(1)).unwrap(),
            )
            .ok();

        let device_name = monitor_device_name(monitor);
        tracing::debug!(
            device_name,
            x = win_x,
            y = win_y,
            w = win_w,
            h = win_h,
            "Created bar window."
        );

        let id = window.id();
        self.bars.insert(
            id,
            BarWindow {
                window,
                surface,
                device_name,
                scale_factor: scale,
            },
        );

        Ok(())
    }

    /// Redraw every bar window using the current `BarState`.
    fn redraw_all(&mut self) {
        let state = self.state_rx.borrow_and_update();
        for bar in self.bars.values_mut() {
            redraw_bar(bar, &state, &self.cfg, &self.renderer);
        }
        self.dirty = false;
    }
}

fn redraw_bar(
    bar: &mut BarWindow,
    state: &BarState,
    cfg: &Config,
    renderer: &Renderer,
) {
    let size = bar.window.inner_size();
    let w = size.width;
    let h = size.height;

    if w == 0 || h == 0 {
        return;
    }

    if bar
        .surface
        .resize(
            NonZeroU32::new(w).unwrap(),
            NonZeroU32::new(h).unwrap(),
        )
        .is_err()
    {
        tracing::warn!("Failed to resize surface for {}", bar.device_name);
        return;
    }

    let mut surface_buf = match bar.surface.buffer_mut() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Failed to get surface buffer: {e}");
            return;
        }
    };

    let empty = MonitorWorkspaces::default();
    let monitor_state = state
        .monitors
        .get(&bar.device_name)
        .unwrap_or(&empty);

    renderer.render(
        &mut surface_buf,
        w,
        h,
        bar.scale_factor as f32,
        &monitor_state.workspaces,
        cfg,
    );

    if let Err(e) = surface_buf.present() {
        tracing::warn!("Failed to present surface buffer: {e}");
    }
}

// ---------------------------------------------------------------------------
// winit ApplicationHandler impl
// ---------------------------------------------------------------------------

impl ApplicationHandler<StateChanged> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.bars.is_empty() {
            self.create_windows(event_loop);
            self.dirty = true;
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    let state = self.state_rx.borrow();
                    let empty = MonitorWorkspaces::default();
                    let ws = state
                        .monitors
                        .get(&bar.device_name)
                        .unwrap_or(&empty);
                    let size = bar.window.inner_size();
                    if size.width > 0 && size.height > 0 {
                        if bar
                            .surface
                            .resize(
                                NonZeroU32::new(size.width).unwrap(),
                                NonZeroU32::new(size.height).unwrap(),
                            )
                            .is_ok()
                        {
                            if let Ok(mut buf) = bar.surface.buffer_mut() {
                                self.renderer.render(
                                    &mut buf,
                                    size.width,
                                    size.height,
                                    bar.scale_factor as f32,
                                    &ws.workspaces,
                                    &self.cfg,
                                );
                                let _ = buf.present();
                            }
                        }
                    }
                }
            }
            WindowEvent::Resized(_) => {
                // Re-render on resize.
                self.dirty = true;
                self.redraw_all();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    bar.scale_factor = scale_factor;
                }
                self.dirty = true;
                self.redraw_all();
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: StateChanged) {
        // IPC task notified us of a state change.
        self.dirty = true;
        self.redraw_all();
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.dirty {
            self.redraw_all();
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn bar_geometry(
    position: BarPosition,
    monitor_pos: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    bar_size: u32,
) -> (i32, i32, u32, u32) {
    match position {
        BarPosition::Top => (
            monitor_pos.x,
            monitor_pos.y,
            monitor_size.width,
            bar_size,
        ),
        BarPosition::Bottom => (
            monitor_pos.x,
            monitor_pos.y + monitor_size.height as i32 - bar_size as i32,
            monitor_size.width,
            bar_size,
        ),
        BarPosition::Left => (
            monitor_pos.x,
            monitor_pos.y,
            bar_size,
            monitor_size.height,
        ),
        BarPosition::Right => (
            monitor_pos.x + monitor_size.width as i32 - bar_size as i32,
            monitor_pos.y,
            bar_size,
            monitor_size.height,
        ),
    }
}

/// Extract a stable device identifier from a `MonitorHandle`.
///
/// winit's monitor name is used as the key; falls back to `"primary"` when
/// unavailable.
fn monitor_device_name(monitor: &MonitorHandle) -> String {
    monitor.name().unwrap_or_else(|| "primary".into())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Initialise tracing, defaulting to INFO unless `RUST_LOG` is set.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::load()?;

    // Build a tokio runtime for the IPC background task.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    // Create the winit event loop with our custom user-event type.
    let event_loop = EventLoop::<StateChanged>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();

    // Spawn the IPC task; it sends a `StateChanged` user event whenever the
    // watch channel is updated.
    let state_rx = rt.block_on(async { spawn_ipc_watcher(&cfg, proxy.clone()) });

    let mut app = App::new(cfg, state_rx, proxy);

    event_loop.run_app(&mut app)?;

    Ok(())
}

/// Spawn the IPC client and a watcher task that fires `StateChanged` user
/// events into the winit event loop whenever workspace state changes.
fn spawn_ipc_watcher(
    cfg: &Config,
    proxy: EventLoopProxy<StateChanged>,
) -> watch::Receiver<BarState> {
    let rx = client::spawn(cfg.glazewm_port, cfg.reconnect_delay_ms);
    let mut watcher_rx = rx.clone();

    tokio::spawn(async move {
        loop {
            // `changed()` resolves whenever the sender pushes a new value.
            if watcher_rx.changed().await.is_err() {
                // Sender dropped — the IPC task exited.
                break;
            }
            // Fire a wake-up into the winit event loop.
            if proxy.send_event(StateChanged).is_err() {
                // Event loop has exited.
                break;
            }
        }
    });

    rx
}
