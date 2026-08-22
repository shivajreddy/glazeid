/// Windows light/dark theme detection.
///
/// The taskbar follows the *system* theme
/// (`SystemUsesLightTheme`), not the app theme (`AppsUseLightTheme`), so
/// that is the value glazeid reads. There is no polling: the hidden message
/// window receives the `WM_SETTINGCHANGE` broadcast whenever settings change
/// and re-reads this value (a single registry call).
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
};

use crate::taskbar::wide;

/// `true` when the Windows system theme (taskbar) is dark.
///
/// Defaults to dark if the value cannot be read (matches glazeid's
/// dark-friendly default colors).
pub fn is_dark() -> bool {
    let subkey = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide("SystemUsesLightTheme");

    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };

    if status == 0 {
        data == 0 // 1 = light theme, 0 = dark theme
    } else {
        true
    }
}
