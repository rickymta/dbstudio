//! System tray icon and menu (close-to-tray behaviour).
//!
//! The icon stays while the app runs in the background so scheduled backups keep firing after
//! the main window is closed. Left click opens the window; the menu has Open and Quit.

use tauri::AppHandle;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use crate::background::show_main_window;

/// Id of the tray icon.
pub const TRAY_ID: &str = "main";
const MENU_OPEN: &str = "open";
const MENU_QUIT: &str = "quit";

/// Create the tray icon.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, MENU_OPEN, "Open DBStudio", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("DBStudio")
        .on_menu_event(|app, event| match event.id.as_ref() {
            MENU_OPEN => show_main_window(app),
            MENU_QUIT => crate::background::request_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .icon(TRAY_ICON)
        // macOS tints a template image itself, so the white glyph stays visible on a light menu bar.
        .icon_as_template(true);
    builder.build(app)?;
    Ok(())
}

/// White database glyph for the menu bar / notification area. The window, taskbar and Dock use
/// the coloured app icon (`icon.icns` / `icon.ico`).
const TRAY_ICON: Image<'static> = tauri::include_image!("icons/tray.png");
