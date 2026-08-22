/// glazeid — a minimal GlazeWM workspace widget embedded in the Windows
/// taskbar.
///
/// One bar window is created per monitor and parented *into* that monitor's
/// taskbar (`Shell_TrayWnd` / `Shell_SecondaryTrayWnd`), so the shell manages
/// visibility: fullscreen apps, auto-hide and z-order all behave correctly
/// without the polling the old overlay implementation needed.
///
/// A background tokio task maintains a WebSocket connection to GlazeWM and
/// publishes `BarState` updates through a `watch` channel; a hidden
/// message-handling window is woken via `PostMessage` and syncs the bar
/// windows on the main thread. The same hidden window receives the
/// `TaskbarCreated` broadcast and re-embeds every bar when Explorer restarts.
#[cfg(not(target_os = "windows"))]
compile_error!(
    "glazeid 0.8+ is Windows-only. For the macOS/overlay version use v0.7.x \
     (branch backup/v0.7.0-overlay)."
);

mod bar;
mod client;
mod config;
mod ipc;
mod renderer;
mod sys_tray;
mod taskbar;
mod theme;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{bail, Context, Result};
use bar::BarWindow;
use client::BarState;
use config::Config;
use renderer::Renderer;
use sys_tray::Tray;
use taskbar::wide;
use tokio::sync::{mpsc, watch};
use tray_icon::menu::MenuEvent;

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, TranslateMessage, MSG, WM_APP,
    WM_SETTINGCHANGE, WNDCLASSW,
};

/// Posted by the IPC watch task whenever `BarState` changes.
const WM_APP_STATE_CHANGED: u32 = WM_APP + 1;

/// Registered message broadcast by the shell when the taskbar is (re)created.
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// Owned by the main thread; accessed from the message window's wndproc.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct App {
    cfg: Config,
    renderer: Renderer,
    state_rx: watch::Receiver<BarState>,
    /// Commands (e.g. `focus --workspace 3`) forwarded to the IPC task.
    cmd_tx: mpsc::UnboundedSender<String>,
    /// Bar windows keyed by GlazeWM monitor `device_name`.
    bars: HashMap<String, BarWindow>,
    /// Whether the Windows system theme (taskbar) is currently dark.
    dark: bool,
}

impl App {
    /// Bring the bar windows in line with the latest GlazeWM state: drop bars
    /// for dead windows/monitors, create missing ones, redraw the rest.
    fn sync_bars(&mut self) {
        let state = self.state_rx.borrow_and_update().clone();

        self.bars.retain(|dev, bar| {
            if !bar.is_alive() {
                tracing::info!("Bar on {dev} died (taskbar gone?); dropping it.");
                return false;
            }
            if !state.monitors.contains_key(dev) {
                tracing::info!("Monitor {dev} no longer reported by GlazeWM; dropping bar.");
                return false;
            }
            true
        });

        for (dev, mw) in &state.monitors {
            if !self.bars.contains_key(dev) {
                let Some(tb) = taskbar::for_monitor(dev, &mw.geometry) else {
                    tracing::debug!("Monitor {dev} has no taskbar; skipping.");
                    continue;
                };
                match BarWindow::create(&tb, mw.geometry.scale_factor) {
                    Ok(b) => {
                        tracing::info!("Embedded bar into taskbar on {dev}.");
                        self.bars.insert(dev.clone(), b);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to embed bar on {dev}: {e:#}");
                        continue;
                    }
                }
            }

            if let Some(b) = self.bars.get_mut(dev) {
                b.scale = mw.geometry.scale_factor;
                b.redraw(&mw.workspaces, &self.cfg, &self.renderer, self.dark);
            }
        }
    }

    /// Explorer restarted: every bar window died with the old taskbar.
    fn reembed_all(&mut self) {
        tracing::info!("Taskbar (re)created; re-embedding all bars.");
        self.bars.clear();
        self.sync_bars();
    }

    /// A system setting changed — re-check the light/dark theme and repaint
    /// if it flipped. One registry read; no polling.
    fn on_setting_change(&mut self) {
        let dark = theme::is_dark();
        if dark != self.dark {
            self.dark = dark;
            tracing::info!(
                "Windows theme changed to {}; repainting.",
                if dark { "dark" } else { "light" }
            );
            self.sync_bars();
        }
    }
}

/// Map a click on a bar window to its workspace and ask GlazeWM to focus it.
///
/// Called from the bar wndproc (same thread). Uses a shared borrow — clicks
/// never mutate state directly; the focus change comes back as a GlazeWM
/// event and redraws through the normal path.
pub fn handle_bar_click(hwnd: HWND, x: i32) {
    APP.with(|cell| {
        let Ok(guard) = cell.try_borrow() else {
            return;
        };
        let Some(app) = guard.as_ref() else {
            return;
        };
        let Some(bar) = app.bars.values().find(|b| b.hwnd() == hwnd) else {
            return;
        };
        if let Some(name) = bar.workspace_at(x as f32) {
            tracing::debug!("Pill clicked; focusing workspace {name}.");
            let _ = app.cmd_tx.send(format!("focus --workspace {name}"));
        }
    });
}

// ---------------------------------------------------------------------------
// Hidden message window
// ---------------------------------------------------------------------------

fn create_message_window() -> Result<HWND> {
    unsafe {
        let class = wide("glazeid_msg");

        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(msg_wndproc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = class.as_ptr();
        if RegisterClassW(&wc) == 0 {
            bail!("RegisterClassW(glazeid_msg) failed (error {})", GetLastError());
        }

        // A real (never-shown) top-level window rather than a message-only
        // window: message-only windows do not receive broadcasts such as
        // TaskbarCreated.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            bail!("CreateWindowExW(glazeid_msg) failed (error {})", GetLastError());
        }
        Ok(hwnd)
    }
}

unsafe extern "system" fn msg_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_STATE_CHANGED {
        dispatch_app(hwnd, msg, App::sync_bars);
        return 0;
    }

    if msg == WM_SETTINGCHANGE {
        dispatch_app(hwnd, msg, App::on_setting_change);
        return 0;
    }

    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Relaxed);
    if taskbar_created != 0 && msg == taskbar_created {
        dispatch_app(hwnd, msg, App::reembed_all);
        return 0;
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Run `f` against the shared `App`, retrying via `PostMessage` if the
/// wndproc re-entered while the app was already borrowed (possible when a
/// broadcast is delivered during a blocking cross-process call such as
/// `SetParent`).
fn dispatch_app(hwnd: HWND, msg: u32, f: impl FnOnce(&mut App)) {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(app) = guard.as_mut() {
                f(app);
            }
        }
        Err(_) => {
            unsafe { PostMessageW(hwnd, msg, 0, 0) };
        }
    });
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::load()?;

    // Match the taskbar's per-monitor DPI awareness so all window coordinates
    // are true physical pixels (the space GlazeWM reports in).
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;
    let _rt_guard = rt.enter();

    let msg_hwnd = create_message_window()?;
    TASKBAR_CREATED_MSG.store(
        unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        Ordering::Relaxed,
    );

    let (state_rx, cmd_tx) = client::spawn(cfg.glazewm_port, cfg.reconnect_delay_ms);
    spawn_state_watcher(state_rx.clone(), msg_hwnd);

    let tray = match Tray::new(cfg.trayicon_hidden) {
        Ok(Some(t)) => Some(t),
        Ok(None) => {
            tracing::info!("Tray icon hidden by config.");
            None
        }
        Err(e) => {
            tracing::warn!("Failed to create tray icon: {e:#}");
            None
        }
    };

    let dark = theme::is_dark();
    tracing::debug!("Windows system theme: {}.", if dark { "dark" } else { "light" });

    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            cfg,
            renderer: Renderer::new(),
            state_rx,
            cmd_tx,
            bars: HashMap::new(),
            dark,
        });
    });

    tracing::info!("glazeid running as a taskbar widget.");

    // Win32 message loop. Blocks in GetMessageW; woken by window messages,
    // the IPC watcher's PostMessage, and tray menu interaction.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            match GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) {
                0 => break, // WM_QUIT
                -1 => {
                    tracing::error!("GetMessageW failed (error {}).", GetLastError());
                    break;
                }
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // Tray menu events surface after their messages are dispatched.
            if let Some(tray) = &tray {
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == tray.quit_id {
                        tracing::info!("Quit requested from tray menu.");
                        PostQuitMessage(0);
                    }
                }
            }
        }
    }

    // Destroy bar windows (BarWindow::drop) before the runtime shuts down.
    APP.with(|cell| cell.borrow_mut().take());
    drop(tray);
    unsafe { DestroyWindow(msg_hwnd) };

    Ok(())
}

/// Forward `watch` channel updates to the message window as posted messages.
fn spawn_state_watcher(mut rx: watch::Receiver<BarState>, msg_hwnd: HWND) {
    // HWND is a raw pointer and not `Send`; the numeric value is fine to move.
    let hwnd = msg_hwnd as usize;
    tokio::spawn(async move {
        // Cover any state that arrived before this task started.
        unsafe { PostMessageW(hwnd as HWND, WM_APP_STATE_CHANGED, 0, 0) };
        loop {
            if rx.changed().await.is_err() {
                break; // IPC task gone; nothing more to forward.
            }
            if unsafe { PostMessageW(hwnd as HWND, WM_APP_STATE_CHANGED, 0, 0) } == 0 {
                break; // Message window destroyed; shutting down.
            }
        }
    });
}
