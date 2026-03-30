
/// glazeid — a minimal GlazeWM workspace bar.
///
/// One window is created per monitor.  Its size is driven entirely by content:
/// width = sum of all workspace pill widths, height = cap-height + vertical
/// padding.  Placement is controlled by `position` (top/bottom) and
/// `offset_percent` (how far along the edge from the left, 0 = left-most).
///
/// A background tokio task maintains a WebSocket connection to GlazeWM and
/// publishes `BarState` updates through a `watch` channel.  The winit event
/// loop wakes on a `UserEvent` and redraws + repositions windows only when
/// state has changed.
mod client;
mod config;
mod ipc;
mod renderer;
mod sys_tray;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Result;
use client::{BarState, MonitorWorkspaces, WorkspaceInfo};
use config::{BarPosition, Config};
use renderer::{ContentSize, Renderer};
use softbuffer::{Context as SbContext, Surface};
use sys_tray::Tray;
use tokio::sync::watch;
use tray_icon::menu::MenuEvent;
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
    /// GlazeWM `device_name` of the monitor this bar lives on.
    device_name: String,
    scale_factor: f64,
    /// Monitor geometry in physical pixels (position + size).
    monitor_pos: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    /// Last rendered content size — used to detect whether a window resize is
    /// needed before painting.
    last_size: ContentSize,
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
    dirty: bool,
    /// Tray icon — installed once on first `resumed`, kept alive for its Drop.
    tray: Option<Tray>,
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
            tray: None,
        }
    }

    /// Create one bar window per connected monitor.
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
        let monitor_pos = monitor.position();
        let monitor_size = monitor.size();
        let device_name = monitor_device_name(monitor);

        // Compute initial size from the current state (likely empty on first
        // call — that's fine, window will resize on first real state update).
        let state = self.state_rx.borrow();
        let empty = MonitorWorkspaces::default();
        let ws = state.monitors.get(&device_name).unwrap_or(&empty);
        let content = self
            .renderer
            .measure(&ws.workspaces, &self.cfg, scale as f32);
        drop(state);

        let (win_x, win_y) = bar_position(
            self.cfg.position,
            self.cfg.offset_percent,
            monitor_pos,
            monitor_size,
            content,
        );

        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("glazeid")
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(win_x, win_y))
            .with_inner_size(PhysicalSize::new(content.width.max(1), content.height.max(1)));

        // On Windows: remove the taskbar button by setting the tool-window
        // style (WS_EX_TOOLWINDOW).
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_skip_taskbar(true);
        }

        let window = Arc::new(event_loop.create_window(attrs)?);

        let ctx = self
            .sb_ctx
            .get_or_insert_with(|| SbContext::new(window.clone()).expect("softbuffer context"));

        let mut surface = Surface::new(ctx, window.clone()).expect("softbuffer surface");
        surface
            .resize(
                NonZeroU32::new(content.width.max(1)).unwrap(),
                NonZeroU32::new(content.height.max(1)).unwrap(),
            )
            .ok();

        tracing::debug!(
            device_name,
            x = win_x,
            y = win_y,
            w = content.width,
            h = content.height,
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
                monitor_pos,
                monitor_size,
                last_size: content,
            },
        );

        Ok(())
    }

    /// Redraw every bar window, resizing and repositioning as needed.
    fn redraw_all(&mut self) {
        let state = self.state_rx.borrow_and_update().clone();
        let empty = MonitorWorkspaces::default();

        // Collect keys to avoid borrowing `self` mutably in the loop.
        let ids: Vec<WindowId> = self.bars.keys().copied().collect();

        for id in ids {
            let bar = self.bars.get_mut(&id).unwrap();
            let ws = state.monitors.get(&bar.device_name).unwrap_or(&empty);
            redraw_bar(
                bar,
                &ws.workspaces,
                &self.cfg,
                &self.renderer,
            );
        }

        self.dirty = false;
    }
}

/// Render a single bar window, resizing and repositioning the OS window when
/// the content size has changed.
fn redraw_bar(
    bar: &mut BarWindow,
    workspaces: &[WorkspaceInfo],
    cfg: &Config,
    renderer: &Renderer,
) {
    let scale = bar.scale_factor as f32;
    let content = renderer.measure(workspaces, cfg, scale);

    // Resize the OS window only when dimensions actually changed.
    if content != bar.last_size {
        let (win_x, win_y) = bar_position(
            cfg.position,
            cfg.offset_percent,
            bar.monitor_pos,
            bar.monitor_size,
            content,
        );
        bar.window
            .set_outer_position(PhysicalPosition::new(win_x, win_y));
        let _ = bar
            .window
            .request_inner_size(PhysicalSize::new(content.width.max(1), content.height.max(1)));
        bar.last_size = content;
    }

    let w = content.width.max(1);
    let h = content.height.max(1);

    if bar.surface.resize(
        NonZeroU32::new(w).unwrap(),
        NonZeroU32::new(h).unwrap(),
    ).is_err() {
        tracing::warn!("Failed to resize surface for {}", bar.device_name);
        return;
    }

    let mut buf = match bar.surface.buffer_mut() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Failed to get surface buffer: {e}");
            return;
        }
    };

    renderer.render(&mut buf, w, h, scale, workspaces, cfg);

    if let Err(e) = buf.present() {
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
        if self.tray.is_none() {
            match Tray::new() {
                Ok(t) => self.tray = Some(t),
                Err(e) => tracing::warn!("Failed to create tray icon: {e:#}"),
            }
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
                // Explicit OS redraw request — paint with current state.
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    let state = self.state_rx.borrow();
                    let empty = MonitorWorkspaces::default();
                    let ws = state.monitors.get(&bar.device_name).unwrap_or(&empty);
                    let workspaces = ws.workspaces.clone();
                    drop(state);
                    redraw_bar(bar, &workspaces, &self.cfg, &self.renderer);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    bar.scale_factor = scale_factor;
                    // Force a full remeasure next redraw.
                    bar.last_size = ContentSize { width: 0, height: 0 };
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: StateChanged) {
        self.dirty = true;
        self.redraw_all();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Poll tray menu events — tray-icon posts them to a channel that must
        // be drained on the main thread.
        if let Some(tray) = &self.tray {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == tray.quit_id {
                    event_loop.exit();
                }
            }
        }

        if self.dirty {
            self.redraw_all();
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Compute the physical pixel (x, y) origin for the bar window.
///
/// For `Bottom` and `Top`, the bar is placed along the horizontal edge of the
/// monitor.  `offset_percent` shifts it from the left: `0.0` = left-most,
/// `50.0` = centred, `100.0` = right edge (clamped so the bar never extends
/// past the right edge of the monitor).
fn bar_position(
    position: BarPosition,
    offset_percent: f32,
    monitor_pos: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    content: ContentSize,
) -> (i32, i32) {
    let offset_px =
        ((monitor_size.width as f32 * offset_percent / 100.0) as i32)
            // Clamp so the bar never clips past the right edge.
            .min(monitor_size.width as i32 - content.width as i32)
            .max(0);

    let x = monitor_pos.x + offset_px;

    let y = match position {
        BarPosition::Top => monitor_pos.y,
        BarPosition::Bottom => {
            monitor_pos.y + monitor_size.height as i32 - content.height as i32
        }
    };

    (x, y)
}

/// Extract a stable device identifier from a `MonitorHandle`.
fn monitor_device_name(monitor: &MonitorHandle) -> String {
    monitor.name().unwrap_or_else(|| "primary".into())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::load()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    let event_loop = EventLoop::<StateChanged>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();

    let state_rx = rt.block_on(async { spawn_ipc_watcher(&cfg, proxy.clone()) });

    let mut app = App::new(cfg, state_rx, proxy);
    event_loop.run_app(&mut app)?;

    Ok(())
}

/// Spawn the IPC client and a watcher that fires `StateChanged` into the winit
/// event loop on every state update.
fn spawn_ipc_watcher(
    cfg: &Config,
    proxy: EventLoopProxy<StateChanged>,
) -> watch::Receiver<BarState> {
    let rx = client::spawn(cfg.glazewm_port, cfg.reconnect_delay_ms);
    let mut watcher_rx = rx.clone();

    tokio::spawn(async move {
        loop {
            if watcher_rx.changed().await.is_err() {
                break;
            }
            if proxy.send_event(StateChanged).is_err() {
                break;
            }
        }
    });

    rx
}
