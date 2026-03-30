/// System tray icon for glazeid.
///
/// Creates a tray icon with a single "Quit" menu item.  The icon is loaded
/// from the bundled `resources/glazeid.png` logo, resized to 32×32 at runtime.
///
/// Menu events are polled in the winit `about_to_wait` callback via
/// `MenuEvent::receiver().try_recv()`.
use anyhow::Result;
use image::imageops::FilterType;
use tray_icon::{
    menu::{Menu, MenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

/// Raw logo PNG bundled at compile time.
const LOGO_PNG: &[u8] = include_bytes!("../resources/glazeid.png");

/// Tray icon size expected by Windows (32×32 physical pixels).
const TRAY_SIZE: u32 = 32;

/// Owned tray icon handle.  Drop to remove the icon from the system tray.
pub struct Tray {
    /// Kept alive for its Drop impl which removes the tray icon.
    _icon: TrayIcon,
    /// Menu item ID for "Quit" — compared against incoming `MenuEvent`s.
    pub quit_id: tray_icon::menu::MenuId,
}

impl Tray {
    /// Install the tray icon.  Must be called on the main thread after the
    /// winit event loop has started (required by Win32).
    pub fn new() -> Result<Self> {
        let quit_item = MenuItem::new("Quit glazeid", true, None);
        let quit_id = quit_item.id().clone();

        let menu = Menu::new();
        menu.append(&quit_item)?;

        let icon = load_icon()?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("glazeid")
            .with_icon(icon)
            .build()?;

        Ok(Self {
            _icon: tray,
            quit_id,
        })
    }
}

/// Decode the bundled logo PNG and resize it to `TRAY_SIZE × TRAY_SIZE`.
fn load_icon() -> Result<Icon> {
    let img = image::load_from_memory(LOGO_PNG)?.into_rgba8();
    let img = image::imageops::resize(&img, TRAY_SIZE, TRAY_SIZE, FilterType::Lanczos3);
    let rgba = img.into_raw();
    Ok(Icon::from_rgba(rgba, TRAY_SIZE, TRAY_SIZE)?)
}
