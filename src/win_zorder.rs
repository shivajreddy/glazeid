/// Windows-only z-order and visibility management for the bar.
///
/// # Why this exists
///
/// The taskbar (`Shell_TrayWnd`) is a `WS_EX_TOPMOST` window, so the bar must be
/// topmost too in order to draw above it. But "topmost" is a *band*, not a fixed
/// position: whenever an app goes fullscreen the shell re-raises the taskbar to
/// the front of that band, above the bar — and never lowers it again. The bar
/// therefore looks correct until the first fullscreen app, then stays buried
/// under the taskbar until restart. Measured against an Edge kiosk session:
///
/// ```text
///   before fullscreen   bar z#4   taskbar z#5   bar visible
///   during fullscreen   bar z#5   taskbar z#4   taskbar not painted, bar visible
///   after  fullscreen   bar z#5   taskbar z#4   bar buried
/// ```
///
/// No static window level survives that, so `App::apply_z_order` calls in here
/// on a slow tick to reclaim the position.
///
/// # Why the bar is hidden rather than lowered during fullscreen
///
/// The obvious way to keep the bar off a fullscreen app is to drop it to
/// `HWND_BOTTOM`. That does not work. Lowering the window clears
/// `WS_EX_TOPMOST`, and glazeid can never get that style back: it never takes
/// focus, so it is never the foreground process, and Windows refuses topmost
/// promotion from a background process. `SetWindowPos` still reports success —
/// it returns non-zero with `GetLastError() == 0` while silently leaving the
/// style bit clear, which is how this went unnoticed at first.
///
/// Re-*ordering* within the band is fine; it is re-*acquiring* the style that is
/// blocked. So the bar stays topmost for its whole life and is simply hidden
/// while a fullscreen window owns its monitor.
///
/// `SWP_HIDEWINDOW`/`SWP_SHOWWINDOW` are used rather than winit's
/// `set_visible`, which re-shows with `SW_SHOW` and would activate the window,
/// stealing focus from whatever the user just returned to. `SWP_NOACTIVATE`
/// keeps that from happening.
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTONULL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetShellWindow, GetWindow, GetWindowRect, IsWindowVisible,
    SetWindowPos, GW_HWNDPREV, HWND_TOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW,
};

/// Window classes that span a whole monitor but must never count as a
/// fullscreen app — otherwise the bar would vanish whenever the desktop is
/// focused.
const SHELL_CLASSES: [&str; 3] = ["Progman", "WorkerW", "Shell_TrayWnd"];

/// Primary and per-monitor taskbar classes.
const TASKBAR_CLASSES: [&str; 2] = ["Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

/// Cycle guard for the z-order walk. The bar stays in the topmost band, so only
/// a handful of windows are ever ahead of it and the loop ends quickly.
const MAX_Z_WALK: usize = 512;

/// `true` when a fullscreen window currently covers this bar's monitor.
pub fn monitor_is_fullscreen(window: &Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let covered = unsafe { fullscreen_monitor() };
    !covered.is_null() && unsafe { monitor_of(hwnd) } == covered
}

/// `true` when the bar window is currently hidden.
///
/// Read back from the OS instead of remembered, so a request the system quietly
/// ignored is simply retried on the next tick.
pub fn is_hidden(window: &Window) -> bool {
    match hwnd_of(window) {
        Some(hwnd) => unsafe { IsWindowVisible(hwnd) == 0 },
        None => false,
    }
}

/// Show or hide the bar without activating it and without touching
/// `WS_EX_TOPMOST`.
pub fn set_hidden(window: &Window, hidden: bool) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let show = if hidden { SWP_HIDEWINDOW } else { SWP_SHOWWINDOW };
    unsafe {
        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, show | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
    }
    tracing::debug!(hidden, "Bar visibility changed.");
}

/// `true` when a taskbar on this bar's monitor has moved in front of it.
pub fn taskbar_is_above(window: &Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    unsafe {
        let ours = monitor_of(hwnd);
        // GW_HWNDPREV walks towards the front of the z-order.
        let mut cur = GetWindow(hwnd, GW_HWNDPREV);
        for _ in 0..MAX_Z_WALK {
            if cur.is_null() {
                break;
            }
            if class_matches(cur, &TASKBAR_CLASSES) && monitor_of(cur) == ours {
                return true;
            }
            cur = GetWindow(cur, GW_HWNDPREV);
        }
    }
    false
}

/// Move the bar back to the front of the topmost band.
///
/// This only re-orders an already-topmost window, which the system permits;
/// see the module docs for why the style must never be dropped.
pub fn raise_to_front(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    tracing::debug!("Taskbar had risen above the bar; reclaimed front of topmost band.");
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

/// Monitor fully covered by the foreground window, or null if it is not
/// fullscreen.
unsafe fn fullscreen_monitor() -> HMONITOR {
    let null = std::ptr::null_mut();

    let hwnd = GetForegroundWindow();
    if hwnd.is_null() || hwnd == GetShellWindow() || IsWindowVisible(hwnd) == 0 {
        return null;
    }
    if class_matches(hwnd, &SHELL_CLASSES) {
        return null;
    }

    let mut rect: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return null;
    }

    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
    if monitor.is_null() {
        return null;
    }

    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return null;
    }

    // rcMonitor is the full monitor rect rather than the work area, so this is
    // only true when the window covers the taskbar too.
    let m = info.rcMonitor;
    let covers =
        rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom;

    if covers {
        monitor
    } else {
        null
    }
}

unsafe fn monitor_of(hwnd: HWND) -> HMONITOR {
    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
}

unsafe fn class_matches(hwnd: HWND, classes: &[&str]) -> bool {
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..len as usize]);
    classes.contains(&class.as_str())
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
        _ => None,
    }
}
