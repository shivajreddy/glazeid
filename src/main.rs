
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
///
/// Window placement uses the monitor geometry reported by GlazeWM (logical
/// pixels) rather than winit's `monitor.size()`, which can return inflated
/// physical pixel counts on HiDPI displays — especially on macOS.
mod client;
mod config;
mod ipc;
mod renderer;
mod sys_tray;

#[cfg(target_os = "macos")]
mod macos_surface;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use client::{BarState, MonitorGeometry, MonitorWorkspaces, WorkspaceInfo};
use config::{BarPosition, Config};
use renderer::{ContentSize, Renderer};
use sys_tray::Tray;
use tokio::sync::watch;
use tray_icon::menu::MenuEvent;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowId, WindowLevel},
};
#[cfg(not(target_os = "windows"))]
use winit::dpi::{LogicalPosition, LogicalSize};

// softbuffer is only used on non-macOS platforms
#[cfg(not(target_os = "macos"))]
use std::num::NonZeroU32;
#[cfg(not(target_os = "macos"))]
use softbuffer::{Context as SbContext, Surface};
#[cfg(target_os = "windows")]
use winit::dpi::{PhysicalPosition, PhysicalSize};

// ---------------------------------------------------------------------------
// User events (IPC → winit)
// ---------------------------------------------------------------------------

/// Sent by the IPC watch task whenever the `BarState` changes.
#[derive(Debug)]
struct StateChanged;

// ---------------------------------------------------------------------------
// Platform-abstracted rendering surface
// ---------------------------------------------------------------------------

enum BarSurface {
    #[cfg(target_os = "macos")]
    Macos(macos_surface::MacosSurface),
    #[cfg(not(target_os = "macos"))]
    Softbuffer(Surface<Arc<Window>, Arc<Window>>),
}

impl BarSurface {
    /// Render into the surface and present. Returns false on error.
    fn render_and_present(
        &mut self,
        w: u32,
        h: u32,
        scale: f32,
        workspaces: &[WorkspaceInfo],
        cfg: &Config,
        renderer: &Renderer,
    ) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            BarSurface::Macos(s) => {
                s.resize(w, h);
                renderer.render(s.pixels_mut(), w, h, scale, workspaces, cfg);
                s.present();
                true
            }
            #[cfg(not(target_os = "macos"))]
            BarSurface::Softbuffer(s) => {
                if s.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap()).is_err() {
                    return false;
                }
                match s.buffer_mut() {
                    Ok(mut buf) => {
                        renderer.render(&mut buf, w, h, scale, workspaces, cfg);
                        buf.present().is_ok()
                    }
                    Err(_) => false,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-bar-window state
// ---------------------------------------------------------------------------

struct BarWindow {
    window: Arc<Window>,
    surface: BarSurface,
    /// GlazeWM `device_name` of the monitor this bar lives on.
    device_name: String,
    /// Scale factor reported by GlazeWM for this monitor.
    scale_factor: f32,
    /// Last rendered content size (physical px) — used to skip redundant resizes.
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
    #[cfg(not(target_os = "macos"))]
    sb_ctx: Option<SbContext<Arc<Window>>>,
    dirty: bool,
    /// Tray icon — installed once on first `resumed`, kept alive for its Drop.
    tray: Option<Tray>,
    /// Whether windows have been created yet. We defer creation until the first
    /// real IPC state arrives so that we have valid geometry and workspace data.
    windows_created: bool,
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
            #[cfg(not(target_os = "macos"))]
            sb_ctx: None,
            dirty: false,
            tray: None,
            windows_created: false,
        }
    }

    /// Create one bar window per monitor reported by GlazeWM.
    fn create_windows(&mut self, event_loop: &ActiveEventLoop) {
        let state = self.state_rx.borrow().clone();

        if state.monitors.is_empty() {
            tracing::warn!("No monitors in GlazeWM state; skipping window creation.");
            return;
        }

        tracing::info!(
            "Creating bar windows for {} monitor(s) from GlazeWM state.",
            state.monitors.len()
        );

        let device_names: Vec<String> = state.monitors.keys().cloned().collect();
        for device_name in device_names {
            let mw = state.monitors.get(&device_name).unwrap();
            if let Err(e) = self.create_bar_for_glazewm_monitor(
                event_loop,
                &device_name,
                &mw.geometry,
                &mw.workspaces,
            ) {
                tracing::warn!("Failed to create bar for monitor {device_name}: {e:#}");
            }
        }

        self.windows_created = true;
    }

    fn create_bar_for_glazewm_monitor(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_name: &str,
        geo: &MonitorGeometry,
        workspaces: &[WorkspaceInfo],
    ) -> Result<()> {
        let scale = geo.scale_factor;
        let content = self.renderer.measure(workspaces, &self.cfg, scale);

        let (win_x, win_y) = bar_position_logical(
            self.cfg.position,
            self.cfg.offset_percent,
            geo,
            content,
            scale,
        );

        tracing::debug!(
            device_name, scale,
            geo_x = geo.x, geo_y = geo.y, geo_w = geo.width, geo_h = geo.height,
            win_x, win_y, content_w = content.width, content_h = content.height,
            "Creating bar window."
        );

        // GlazeWM reports monitor geometry in physical pixels on Windows and
        // logical pixels on macOS. Use the matching winit coordinate type so
        // the window lands in the right place on both platforms.
        #[allow(unused_mut)]
        let mut attrs = {
            #[cfg(target_os = "windows")]
            {
                Window::default_attributes()
                    .with_title("glazeid")
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_transparent(true)
                    // On Windows, use Normal level so fullscreen apps naturally
                    // cover the bar. AlwaysOnTop would overlay fullscreen games/apps.
                    .with_window_level(WindowLevel::Normal)
                    .with_position(PhysicalPosition::new(win_x as i32, win_y as i32))
                    .with_inner_size(PhysicalSize::new(content.width.max(1), content.height.max(1)))
            }
            #[cfg(not(target_os = "windows"))]
            {
                let logical_w = (content.width as f32 / scale).ceil() as u32;
                let logical_h = (content.height as f32 / scale).ceil() as u32;
                Window::default_attributes()
                    .with_title("glazeid")
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_transparent(true)
                    .with_window_level(WindowLevel::AlwaysOnTop)
                    .with_position(LogicalPosition::new(win_x, win_y))
                    .with_inner_size(LogicalSize::new(logical_w.max(1), logical_h.max(1)))
            }
        };

        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_skip_taskbar(true);
        }

        let window = Arc::new(event_loop.create_window(attrs)?);

        // On macOS, raise the window above the menu bar and reposition if needed.
        #[cfg(target_os = "macos")]
        if self.cfg.position == config::BarPosition::Top {
            let menubar_h = macos_raise_above_menubar(&window);
            // Shift the window up by the menu bar height so it overlaps the menu bar.
            let new_y = win_y - menubar_h;
            window.set_outer_position(LogicalPosition::new(win_x, new_y));
        }

        // Build the platform-specific surface.
        let surface = {
            #[cfg(target_os = "macos")]
            {
                let s = macos_surface::MacosSurface::new(&window)
                    .ok_or_else(|| anyhow::anyhow!("Failed to create macOS surface"))?;
                BarSurface::Macos(s)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let ctx = self.sb_ctx.get_or_insert_with(|| {
                    SbContext::new(window.clone()).expect("softbuffer context")
                });
                let mut s = Surface::new(ctx, window.clone()).expect("softbuffer surface");
                s.resize(
                    NonZeroU32::new(content.width.max(1)).unwrap(),
                    NonZeroU32::new(content.height.max(1)).unwrap(),
                ).ok();
                BarSurface::Softbuffer(s)
            }
        };

        let id = window.id();
        self.bars.insert(
            id,
            BarWindow {
                window,
                surface,
                device_name: device_name.to_string(),
                scale_factor: scale,
                last_size: content,
            },
        );

        Ok(())
    }

    /// Redraw every bar window, resizing and repositioning as needed.
    fn redraw_all(&mut self) {
        let state = self.state_rx.borrow_and_update().clone();
        let empty = MonitorWorkspaces::default();
        let ids: Vec<WindowId> = self.bars.keys().copied().collect();

        for id in ids {
            let bar = self.bars.get_mut(&id).unwrap();
            let mw = state.monitors.get(&bar.device_name).unwrap_or(&empty);
            redraw_bar(bar, &mw.workspaces, &mw.geometry, &self.cfg, &self.renderer);
        }

        self.dirty = false;
    }
}

/// Render a single bar window, resizing and repositioning the OS window when
/// the content size has changed.
fn redraw_bar(
    bar: &mut BarWindow,
    workspaces: &[WorkspaceInfo],
    geo: &MonitorGeometry,
    cfg: &Config,
    renderer: &Renderer,
) {
    let scale = bar.scale_factor;
    let content = renderer.measure(workspaces, cfg, scale);

    if content != bar.last_size {
        let (win_x, win_y) = bar_position_logical(cfg.position, cfg.offset_percent, geo, content, scale);

        #[cfg(target_os = "windows")]
        {
            bar.window.set_outer_position(PhysicalPosition::new(win_x as i32, win_y as i32));
            let _ = bar.window.request_inner_size(PhysicalSize::new(content.width.max(1), content.height.max(1)));
        }
        #[cfg(not(target_os = "windows"))]
        {
            let logical_w = (content.width as f32 / scale).ceil() as u32;
            let logical_h = (content.height as f32 / scale).ceil() as u32;
            bar.window.set_outer_position(LogicalPosition::new(win_x, win_y));
            let _ = bar.window.request_inner_size(LogicalSize::new(logical_w.max(1), logical_h.max(1)));
        }

        bar.last_size = content;
    }

    let w = content.width.max(1);
    let h = content.height.max(1);

    bar.surface.render_and_present(w, h, scale, workspaces, cfg, renderer);
}

// ---------------------------------------------------------------------------
// winit ApplicationHandler impl
// ---------------------------------------------------------------------------

impl ApplicationHandler<StateChanged> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Set the app icon here — NSApp is guaranteed to be ready once the
        // event loop has started and resumed is called.
        #[cfg(target_os = "macos")]
        set_macos_app_icon();

        if self.tray.is_none() {
            match Tray::new() {
                Ok(t) => self.tray = Some(t),
                Err(e) => tracing::warn!("Failed to create tray icon: {e:#}"),
            }
        }

        if !self.windows_created {
            let has_monitors = !self.state_rx.borrow().monitors.is_empty();
            if has_monitors {
                self.create_windows(event_loop);
                self.dirty = true;
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
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    let state = self.state_rx.borrow();
                    let empty = MonitorWorkspaces::default();
                    let mw = state.monitors.get(&bar.device_name).unwrap_or(&empty);
                    let workspaces = mw.workspaces.clone();
                    let geo = mw.geometry.clone();
                    drop(state);
                    redraw_bar(bar, &workspaces, &geo, &self.cfg, &self.renderer);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(bar) = self.bars.get_mut(&window_id) {
                    bar.scale_factor = scale_factor as f32;
                    bar.last_size = ContentSize { width: 0, height: 0 };
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: StateChanged) {
        if !self.windows_created {
            self.create_windows(event_loop);
        }
        self.dirty = true;
        self.redraw_all();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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

/// Compute the (x, y) bar origin in the same coordinate space GlazeWM uses.
///
/// On macOS: GlazeWM reports logical pixels → divide content (physical) by
///           scale to get logical content size for offset math.
/// On Windows: GlazeWM reports physical pixels → content is already physical,
///             no division needed.
fn bar_position_logical(
    position: BarPosition,
    offset_percent: f32,
    geo: &MonitorGeometry,
    content: ContentSize,
    scale: f32,
) -> (f32, f32) {
    // On Windows GlazeWM coordinates are physical; on macOS they are logical.
    #[cfg(target_os = "windows")]
    let (content_w, content_h) = (content.width as f32, content.height as f32);
    #[cfg(not(target_os = "windows"))]
    let (content_w, content_h) = (content.width as f32 / scale, content.height as f32 / scale);

    let _ = scale; // suppress unused warning on Windows

    let monitor_w = geo.width as f32;
    let monitor_h = geo.height as f32;

    let offset_px = (monitor_w * offset_percent / 100.0)
        .min(monitor_w - content_w)
        .max(0.0);

    let x = geo.x as f32 + offset_px;
    let y = match position {
        BarPosition::Top => geo.y as f32,
        BarPosition::Bottom => geo.y as f32 + monitor_h - content_h,
    };

    (x, y)
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

    #[allow(unused_mut)]
    let mut event_loop_builder = EventLoop::<StateChanged>::with_user_event();

    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        event_loop_builder.with_activation_policy(ActivationPolicy::Accessory);
    }

    let event_loop = event_loop_builder.build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let state_rx = rt.block_on(async { spawn_ipc_watcher(&cfg, proxy.clone()) });

    let mut app = App::new(cfg, state_rx, proxy);
    event_loop.run_app(&mut app)?;

    Ok(())
}

/// Raise the window to `NSStatusWindowLevel` (above the menu bar) and return
/// the menu bar height in logical pixels so the caller can shift `y` up.
///
/// `NSMainMenuWindowLevel = 24`, `NSStatusWindowLevel = 25` — placing at 25
/// puts the window in the same Z-layer as menu bar extras (clock, wifi, etc.).
#[cfg(target_os = "macos")]
fn macos_raise_above_menubar(window: &Window) -> f32 {
    use objc2::msg_send;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSScreen, NSStatusWindowLevel};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Raise the NSWindow level above the menu bar.
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::AppKit(h) = handle.as_raw() {
            unsafe {
                let view: *mut objc2::runtime::AnyObject = h.ns_view.as_ptr().cast();
                let ns_window: *mut objc2::runtime::AnyObject = msg_send![view, window];
                if !ns_window.is_null() {
                    // NSStatusWindowLevel = 25, one above NSMainMenuWindowLevel = 24.
                    let _: () = msg_send![ns_window, setLevel: NSStatusWindowLevel];
                }
            }
        }
    }

    // Return the menu bar height (logical px) from the main screen.
    if let Some(mtm) = MainThreadMarker::new() {
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let full_h = screen.frame().size.height as f32;
            let visible_frame = screen.visibleFrame();
            let visible_h = visible_frame.size.height as f32;
            // visibleFrame.origin.y = dock height (0 if dock is on side or hidden).
            let dock_h = visible_frame.origin.y as f32;
            let menubar_h = full_h - visible_h - dock_h;
            tracing::debug!("macOS menu bar height: {menubar_h} logical px");
            return menubar_h;
        }
    }

    24.0 // sensible fallback
}

/// Set the macOS application icon from the embedded PNG so it appears in
/// Activity Monitor and the Force Quit dialog.
///
/// Plain binaries (not .app bundles) have no Info.plist/icns, so the icon
/// must be set programmatically via NSApp.
#[cfg(target_os = "macos")]
fn set_macos_app_icon() {
    use objc2::AnyThread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const LOGO_PNG: &[u8] = include_bytes!("../resources/glazeid.png");

    let Some(mtm) = MainThreadMarker::new() else { return };

    unsafe {
        let data = NSData::with_bytes(LOGO_PNG);
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else { return };
        let app = NSApplication::sharedApplication(mtm);
        app.setApplicationIconImage(Some(&image));
    }
}

fn spawn_ipc_watcher(
    cfg: &Config,
    proxy: EventLoopProxy<StateChanged>,
) -> watch::Receiver<BarState> {
    let rx = client::spawn(cfg.glazewm_port, cfg.reconnect_delay_ms);
    let mut watcher_rx = rx.clone();

    tokio::spawn(async move {
        loop {
            if watcher_rx.changed().await.is_err() { break; }
            if proxy.send_event(StateChanged).is_err() { break; }
        }
    });

    rx
}
