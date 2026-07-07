#[cfg(windows)]
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewWindow};

#[cfg(windows)]
use crate::attention::DesktopAttentionState;
#[cfg(windows)]
use crate::constants::TRAY_ATTENTION_ICON;
#[cfg(any(target_os = "macos", windows))]
use crate::constants::{
    ABOUT_ICON, NAVIGATION_MENU_BACK_ID, NAVIGATION_MENU_ID, TRAY_DEFAULT_ICON, TRAY_ID,
    TRAY_MENU_QUIT_ID, TRAY_MENU_SHOW_ID,
};
#[cfg(any(target_os = "macos", windows))]
use crate::navigation::navigate_back;

#[cfg(any(target_os = "macos", windows))]
fn navigation_back_accelerator() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+["
    } else {
        "Alt+Left"
    }
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn setup_navigation_menu(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuItem, Submenu};

    let back = MenuItem::with_id(
        app,
        NAVIGATION_MENU_BACK_ID,
        "后退",
        true,
        Some(navigation_back_accelerator()),
    )?;
    let navigation = Submenu::with_id_and_items(app, NAVIGATION_MENU_ID, "导航", true, &[&back])?;
    let menu = match app.menu() {
        Some(menu) => menu,
        None => default_app_menu(app)?,
    };
    menu.append(&navigation)?;
    app.set_menu(menu)?;

    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn about_metadata(app: &tauri::App) -> tauri::menu::AboutMetadata<'static> {
    let package_info = app.package_info();
    let config = app.config();

    tauri::menu::AboutMetadata {
        name: Some(package_info.name.clone()),
        version: Some(package_info.version.to_string()),
        copyright: config.bundle.copyright.clone(),
        authors: config
            .bundle
            .publisher
            .clone()
            .map(|publisher| vec![publisher]),
        icon: Some(ABOUT_ICON),
        ..Default::default()
    }
}

#[cfg(any(target_os = "macos", windows))]
fn default_app_menu(app: &tauri::App) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, PredefinedMenuItem, Submenu};

    let package_name = app.package_info().name.clone();

    let window_menu = Submenu::with_id_and_items(
        app,
        "Window",
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            #[cfg(target_os = "macos")]
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    let help_menu = Submenu::with_id_and_items(
        app,
        "Help",
        "Help",
        true,
        &[
            #[cfg(not(target_os = "macos"))]
            &PredefinedMenuItem::about(app, None, Some(about_metadata(app)))?,
        ],
    )?;

    Menu::with_items(
        app,
        &[
            #[cfg(target_os = "macos")]
            &Submenu::with_items(
                app,
                package_name,
                true,
                &[
                    &PredefinedMenuItem::about(app, None, Some(about_metadata(app)))?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            #[cfg(windows)]
            &Submenu::with_items(
                app,
                "File",
                true,
                &[
                    &PredefinedMenuItem::close_window(app, None)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            #[cfg(target_os = "macos")]
            &Submenu::with_items(
                app,
                "View",
                true,
                &[&PredefinedMenuItem::fullscreen(app, None)?],
            )?,
            &window_menu,
            &help_menu,
        ],
    )
}

#[cfg(windows)]
pub(crate) fn setup_windows_tray(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::TrayIconBuilder;

    let attention_state = Arc::new(DesktopAttentionState {
        active: AtomicBool::new(false),
    });
    app.manage(attention_state.clone());

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let about = PredefinedMenuItem::about(app, Some("关于 Snack"), Some(about_metadata(app)))?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &about,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

            let should_show = match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => true,
                _ => false,
            };

            if should_show {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    register_status_menu_events(app);

    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let mut attention_frame = false;
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let active = attention_state
                .active
                .load(std::sync::atomic::Ordering::Relaxed);
            let Some(tray) = handle.tray_by_id(TRAY_ID) else {
                continue;
            };
            if active {
                attention_frame = !attention_frame;
                let icon = if attention_frame {
                    TRAY_ATTENTION_ICON
                } else {
                    TRAY_DEFAULT_ICON
                };
                let _ = tray.set_icon(Some(icon));
            } else if attention_frame {
                attention_frame = false;
                let _ = tray.set_icon(Some(TRAY_DEFAULT_ICON));
            }
        }
    });

    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn should_show_window_on_reopen(has_visible_windows: bool) -> bool {
    !has_visible_windows
}

#[cfg(any(target_os = "macos", windows))]
fn register_status_menu_events(app: &mut tauri::App) {
    app.on_menu_event(|app, event| match event.id().as_ref() {
        NAVIGATION_MENU_BACK_ID => {
            if let Some(window) = app.get_webview_window("main") {
                navigate_back(&window);
            }
        }
        TRAY_MENU_SHOW_ID => show_main_window(app),
        TRAY_MENU_QUIT_ID => app.exit(0),
        _ => {}
    });
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn install_close_to_status_menu(window: &WebviewWindow) {
    let close_window = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = close_window.hide();
        }
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn install_close_to_status_menu(_window: &WebviewWindow) {}

#[cfg(target_os = "macos")]
pub(crate) fn setup_macos_status_menu(app: &mut tauri::App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::TrayIconBuilder;

    let show = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示 Snack", true, None::<&str>)?;
    let about = PredefinedMenuItem::about(app, Some("关于 Snack"), Some(about_metadata(app)))?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &about,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TRAY_DEFAULT_ICON)
        .icon_as_template(true)
        .tooltip("Snack")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .build(app)?;

    register_status_menu_events(app);

    Ok(())
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::should_show_window_on_reopen;

    #[test]
    fn reopens_hidden_main_window_from_dock() {
        assert!(should_show_window_on_reopen(false));
    }

    #[test]
    fn does_not_steal_focus_when_reopen_finds_visible_windows() {
        assert!(!should_show_window_on_reopen(true));
    }
}
