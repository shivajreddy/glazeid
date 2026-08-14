/// One bar: a layered child window embedded in a taskbar.
///
/// # Why a taskbar child window
///
/// The old implementation floated an always-on-top window over the taskbar
/// and had to fight the shell for z-order (a 1 s polling tick, fullscreen
/// detection, hide/show games — see the `backup/v0.7.0-overlay` branch).
/// Parenting the bar *into* the taskbar makes the shell do all of that for
/// us: the bar moves with the taskbar, disappears with it under fullscreen
/// apps, follows auto-hide, and needs no polling at all.
///
/// # Rendering
///
/// tiny_skia draws into a 32-bpp premultiplied-BGRA DIB section, which is
/// pushed to the window with `UpdateLayeredWindow` (per-pixel alpha;
/// supported for child windows since Windows 8). Pixels with alpha 0 are
/// hit-test transparent, and `WS_EX_TRANSPARENT` makes the rest
/// click-through too, so the bar never eats taskbar input.
use std::sync::Once;

use anyhow::{anyhow, Result};
use windows_sys::Win32::Foundation::{
    GetLastError, SetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, IsWindow, RegisterClassW,
    SetParent, SetWindowPos, UpdateLayeredWindow, GWL_STYLE, HWND_TOP, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, ULW_ALPHA, WNDCLASSW, WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::client::WorkspaceInfo;
use crate::config::Config;
use crate::renderer::Renderer;
use crate::taskbar::{wide, Taskbar};

const BAR_CLASS: &str = "glazeid_bar";
static CLASS_REGISTERED: Once = Once::new();

// ---------------------------------------------------------------------------
// 32/64-bit window-long shims
// ---------------------------------------------------------------------------

// `SetWindowLongPtrW` only exists on 64-bit targets; on 32-bit, LONG_PTR is
// LONG and the non-Ptr variants are the same function.
#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW};
#[cfg(target_pointer_width = "32")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW};

unsafe fn get_style(hwnd: HWND) -> u32 {
    #[cfg(target_pointer_width = "64")]
    return GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    #[cfg(target_pointer_width = "32")]
    return GetWindowLongW(hwnd, GWL_STYLE) as u32;
}

unsafe fn set_style(hwnd: HWND, style: u32) {
    #[cfg(target_pointer_width = "64")]
    SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
    #[cfg(target_pointer_width = "32")]
    SetWindowLongW(hwnd, GWL_STYLE, style as i32);
}

// ---------------------------------------------------------------------------
// DIB-backed memory surface
// ---------------------------------------------------------------------------

/// A 32-bpp top-down DIB section selected into a memory DC.
struct Dib {
    hdc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    ptr: *mut u32,
    w: u32,
    h: u32,
}

impl Dib {
    fn new(w: u32, h: u32) -> Option<Self> {
        unsafe {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32); // negative = top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB as u32;

            let screen_dc = GetDC(std::ptr::null_mut());
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                screen_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            ReleaseDC(std::ptr::null_mut(), screen_dc);

            if bitmap.is_null() || bits.is_null() {
                return None;
            }

            let hdc = CreateCompatibleDC(std::ptr::null_mut());
            if hdc.is_null() {
                DeleteObject(bitmap);
                return None;
            }
            let old_bitmap = SelectObject(hdc, bitmap);

            Some(Self {
                hdc,
                bitmap,
                old_bitmap,
                ptr: bits as *mut u32,
                w,
                h,
            })
        }
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, (self.w * self.h) as usize) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old_bitmap);
            DeleteObject(self.bitmap);
            DeleteDC(self.hdc);
        }
    }
}

// ---------------------------------------------------------------------------
// Bar window
// ---------------------------------------------------------------------------

pub struct BarWindow {
    hwnd: HWND,
    taskbar_hwnd: HWND,
    /// DPI scale of the monitor, from GlazeWM.
    pub scale: f32,
    dib: Option<Dib>,
}

impl BarWindow {
    /// Create a bar window and embed it into `taskbar`.
    pub fn create(taskbar: &Taskbar, scale: f32) -> Result<Self> {
        unsafe {
            ensure_class();

            let class = wide(BAR_CLASS);
            let title = wide("glazeid");

            // Created as an invisible popup first; SetParent's contract wants
            // the WS_POPUP → WS_CHILD style switch done before re-parenting.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(anyhow!("CreateWindowExW failed (error {})", GetLastError()));
            }

            set_style(hwnd, (get_style(hwnd) & !WS_POPUP) | WS_CHILD);

            // SetParent returns the previous parent, which can legitimately
            // be NULL — disambiguate failure via the last-error code.
            SetLastError(0);
            let prev = SetParent(hwnd, taskbar.hwnd);
            let err = GetLastError();
            if prev.is_null() && err != 0 {
                DestroyWindow(hwnd);
                return Err(anyhow!("SetParent into taskbar failed (error {err})"));
            }

            Ok(Self {
                hwnd,
                taskbar_hwnd: taskbar.hwnd,
                scale,
                dib: None,
            })
        }
    }

    /// Both the bar window and its host taskbar still exist.
    pub fn is_alive(&self) -> bool {
        unsafe { IsWindow(self.hwnd) != 0 && IsWindow(self.taskbar_hwnd) != 0 }
    }

    /// Re-render and re-position the bar inside its taskbar.
    pub fn redraw(&mut self, workspaces: &[WorkspaceInfo], cfg: &Config, renderer: &Renderer) {
        let content = renderer.measure(workspaces, cfg, self.scale);
        let (w, h) = (content.width.max(1), content.height.max(1));

        let stale = self.dib.as_ref().map_or(true, |d| d.w != w || d.h != h);
        if stale {
            match Dib::new(w, h) {
                Some(d) => self.dib = Some(d),
                None => {
                    tracing::warn!("Failed to allocate {w}x{h} DIB section.");
                    return;
                }
            }
        }
        let dib = self.dib.as_mut().unwrap();

        renderer.render(dib.pixels_mut(), w, h, self.scale, workspaces, cfg);

        unsafe {
            // Position inside the taskbar's client area: `offset_percent`
            // along its width, vertically centred.
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(self.taskbar_hwnd, &mut rc);
            let tb_w = (rc.right - rc.left) as f32;
            let tb_h = (rc.bottom - rc.top) as f32;

            let x = (tb_w * cfg.offset_percent / 100.0)
                .min(tb_w - w as f32)
                .max(0.0);
            let y = ((tb_h - h as f32) / 2.0).max(0.0);

            // HWND_TOP keeps the bar above the taskbar's own child surfaces.
            // Re-asserted on every redraw, so no polling is needed.
            SetWindowPos(
                self.hwnd,
                HWND_TOP,
                x as i32,
                y as i32,
                w as i32,
                h as i32,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            let size = SIZE {
                cx: w as i32,
                cy: h as i32,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if UpdateLayeredWindow(
                self.hwnd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
                dib.hdc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            ) == 0
            {
                tracing::warn!("UpdateLayeredWindow failed (error {}).", GetLastError());
            }
        }
    }
}

impl Drop for BarWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(self.hwnd) != 0 {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Window class
// ---------------------------------------------------------------------------

unsafe fn ensure_class() {
    CLASS_REGISTERED.call_once(|| {
        let class = wide(BAR_CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(bar_wndproc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.lpszClassName = class.as_ptr();
        if RegisterClassW(&wc) == 0 {
            tracing::warn!(
                "RegisterClassW({BAR_CLASS}) failed (error {}).",
                GetLastError()
            );
        }
    });
}

/// The bar never handles input (`WS_EX_TRANSPARENT`) and paints exclusively
/// via `UpdateLayeredWindow`, so default handling suffices.
unsafe extern "system" fn bar_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
