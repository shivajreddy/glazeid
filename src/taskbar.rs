/// Taskbar discovery and GlazeWM-monitor matching.
///
/// glazeid embeds one bar per monitor *inside* that monitor's taskbar window:
/// `Shell_TrayWnd` on the primary monitor and `Shell_SecondaryTrayWnd` on
/// every other monitor (present when "show taskbar on all displays" is
/// enabled). Monitors without a taskbar window get no bar.
///
/// Taskbars are matched to GlazeWM monitors by display device name
/// (e.g. `\\.\DISPLAY1`), which both GlazeWM and `GetMonitorInfoW` report;
/// the full monitor rectangle is used as a fallback.
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW};

use crate::client::MonitorGeometry;

/// A taskbar window and the monitor it lives on.
pub struct Taskbar {
    pub hwnd: HWND,
    /// Display device name of the taskbar's monitor, e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    /// Full monitor rectangle in physical pixels (fallback matching key).
    pub monitor_rect: RECT,
}

/// UTF-16 NUL-terminated string for Win32 APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Enumerate all taskbar windows currently present.
pub fn find_all() -> Vec<Taskbar> {
    let mut out = Vec::new();

    unsafe {
        let primary = FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null());
        if !primary.is_null() {
            if let Some(tb) = describe(primary) {
                out.push(tb);
            }
        }

        let class = wide("Shell_SecondaryTrayWnd");
        let mut prev: HWND = std::ptr::null_mut();
        loop {
            let hwnd = FindWindowExW(std::ptr::null_mut(), prev, class.as_ptr(), std::ptr::null());
            if hwnd.is_null() {
                break;
            }
            if let Some(tb) = describe(hwnd) {
                out.push(tb);
            }
            prev = hwnd;
        }
    }

    out
}

/// Find the taskbar sitting on the given GlazeWM monitor.
pub fn for_monitor(device_name: &str, geo: &MonitorGeometry) -> Option<Taskbar> {
    let mut all = find_all();

    if let Some(i) = all
        .iter()
        .position(|t| t.device_name.eq_ignore_ascii_case(device_name))
    {
        return Some(all.swap_remove(i));
    }

    // Fallback: match by monitor rect. GlazeWM reports physical pixels on
    // Windows — the same space as `rcMonitor`.
    all.into_iter().find(|t| {
        let r = &t.monitor_rect;
        r.left == geo.x
            && r.top == geo.y
            && (r.right - r.left) == geo.width
            && (r.bottom - r.top) == geo.height
    })
}

/// Resolve the monitor device name + rect for a taskbar window.
unsafe fn describe(hwnd: HWND) -> Option<Taskbar> {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_null() {
        return None;
    }

    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
        return None;
    }

    let len = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    let device_name = String::from_utf16_lossy(&info.szDevice[..len]);

    Some(Taskbar {
        hwnd,
        device_name,
        monitor_rect: info.monitorInfo.rcMonitor,
    })
}
