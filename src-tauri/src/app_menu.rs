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
    TRAY_MENU_MEETING_ID, TRAY_MENU_QUIT_ID, TRAY_MENU_SHOW_ID,
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
    let selected_site = app
        .config()
        .app
        .windows
        .first()
        .map(|config| crate::site::selected_site(app.handle(), &config.url))
        .unwrap_or_default();
    let site_menu = build_site_menu(app, Some(selected_site))?;
    menu.insert(&site_menu, site_menu_position())?;
    menu.append(&navigation)?;
    app.set_menu(menu)?;

    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
fn build_site_menu(
    app: &tauri::App,
    selected: Option<crate::site::SiteKey>,
) -> tauri::Result<tauri::menu::Submenu<tauri::Wry>> {
    use tauri::menu::{CheckMenuItem, Submenu};

    let items = crate::site::SiteKey::ALL
        .into_iter()
        .map(|site| {
            let is_selected = Some(site) == selected;
            CheckMenuItem::with_id(
                app,
                site.menu_id(),
                site.display_name(),
                true,
                is_selected,
                None::<&str>,
            )
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let item_refs = items
        .iter()
        .map(|item| item as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
        .collect::<Vec<_>>();
    Submenu::with_id_and_items(app, crate::site::SITE_MENU_ID, "站点", true, &item_refs)
}

#[cfg(any(target_os = "macos", windows))]
fn site_menu_position() -> usize {
    1
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
    let meeting = MenuItem::with_id(
        app,
        TRAY_MENU_MEETING_ID,
        "开始 Snack 会议录音",
        true,
        None::<&str>,
    )?;
    let about = PredefinedMenuItem::about(app, Some("关于 Snack"), Some(about_metadata(app)))?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &meeting,
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
        crate::window_state::recover_if_unreachable(&window);
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
        TRAY_MENU_MEETING_ID => crate::meeting::quick_access::request_quick_recording(app),
        TRAY_MENU_QUIT_ID => app.exit(0),
        id => {
            if let Some(site) = crate::site::SiteKey::from_menu_id(id) {
                handle_site_selection(app, site);
            }
        }
    });
}

#[cfg(any(target_os = "macos", windows))]
fn handle_site_selection(app: &AppHandle, site: crate::site::SiteKey) {
    let Some(window) = app.get_webview_window("main") else {
        restore_site_menu_selection(app, Some(crate::site::load_site(app)));
        return;
    };
    let current = window
        .url()
        .ok()
        .and_then(|url| crate::site::SiteKey::from_url(&url));
    let selected = current.or_else(|| Some(crate::site::load_site(app)));
    if current == Some(site) {
        restore_site_menu_selection(app, selected);
        show_main_window(app);
        return;
    }
    if let Some(reason) = site_switch_block_reason(app) {
        restore_site_menu_selection(app, selected);
        show_site_switch_message("暂时无法切换站点", &reason);
        return;
    }
    if !confirm_site_switch(site) {
        restore_site_menu_selection(app, selected);
        return;
    }
    if let Err(error) = switch_site(&window, site) {
        restore_site_menu_selection(app, selected);
        log_site_switch_failure(app, site, &error);
        show_site_switch_message("站点切换失败", "请稍后重试");
        return;
    }
    restore_site_menu_selection(app, selected);
    show_main_window(app);
}

#[cfg(any(target_os = "macos", windows))]
fn site_switch_block_reason(app: &AppHandle) -> Option<String> {
    crate::meeting::site_switch_block_reason(app).or_else(|| {
        crate::record_import::has_pending_automatic_notes(app)
            .then(|| "会议纪要正在生成，完成后再切换站点".to_string())
    })
}

#[cfg(any(target_os = "macos", windows))]
fn confirm_site_switch(site: crate::site::SiteKey) -> bool {
    use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

    let result = MessageDialog::new()
        .set_level(MessageLevel::Info)
        .set_title(site_switch_confirmation_title(site))
        .set_description(site_switch_confirmation_description())
        .set_buttons(MessageButtons::OkCancelCustom(
            "切换".to_string(),
            "取消".to_string(),
        ))
        .show();
    matches!(result, MessageDialogResult::Ok)
        || matches!(result, MessageDialogResult::Custom(ref label) if label == "切换")
}

#[cfg(any(target_os = "macos", windows))]
fn site_switch_confirmation_title(site: crate::site::SiteKey) -> String {
    format!("是否切换到{}？", site.display_name())
}

#[cfg(any(target_os = "macos", windows))]
fn site_switch_confirmation_description() -> &'static str {
    "目标站点未登录时需要登录，当前页面未保存的内容可能丢失。"
}

#[cfg(any(target_os = "macos", windows))]
fn switch_site(window: &WebviewWindow, site: crate::site::SiteKey) -> Result<(), String> {
    window
        .navigate(site.homepage_url())
        .map_err(|error| error.to_string())
}

#[cfg(any(target_os = "macos", windows))]
pub(crate) fn sync_site_after_load(
    app: &AppHandle,
    window: &WebviewWindow,
    loaded_url: &tauri::Url,
) {
    let Some(site) = crate::site::SiteKey::from_url(loaded_url) else {
        return;
    };
    if crate::site::load_site(app) != site {
        if let Err(error) = crate::site::save_site(app, site) {
            log_site_switch_failure(app, site, &error);
        }
    }
    restore_site_menu_selection(app, Some(site));
    if let Err(error) = window.set_title(site.window_title()) {
        log_site_switch_failure(app, site, &error.to_string());
    }
}

#[cfg(any(target_os = "macos", windows))]
fn restore_site_menu_selection(app: &AppHandle, selected: Option<crate::site::SiteKey>) {
    let Some(menu) = app.menu() else {
        return;
    };
    let Some(site_menu) = menu
        .get(crate::site::SITE_MENU_ID)
        .and_then(|item| item.as_submenu().cloned())
    else {
        return;
    };
    for site in crate::site::SiteKey::ALL {
        let Some(item) = site_menu.get(site.menu_id()) else {
            continue;
        };
        if let Some(item) = item.as_check_menuitem() {
            let is_selected = selected == Some(site);
            let _ = item.set_checked(is_selected);
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
fn show_site_switch_message(title: &str, description: &str) {
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::OkCustom("知道了".to_string()))
        .show();
}

#[cfg(any(target_os = "macos", windows))]
fn log_site_switch_failure(app: &AppHandle, site: crate::site::SiteKey, error: &str) {
    crate::logging::write_app_log(
        app,
        "warn",
        "site-switch",
        "Snack site switch failed",
        Some(&serde_json::json!({ "site": site.menu_id(), "reason": error })),
    );
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
    let meeting = MenuItem::with_id(
        app,
        TRAY_MENU_MEETING_ID,
        "开始 Snack 会议录音",
        true,
        None::<&str>,
    )?;
    let about = PredefinedMenuItem::about(app, Some("关于 Snack"), Some(about_metadata(app)))?;
    let quit = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &meeting,
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
    use super::{
        should_show_window_on_reopen, site_switch_confirmation_description,
        site_switch_confirmation_title,
    };
    use crate::site::SiteKey;

    #[test]
    fn reopens_hidden_main_window_from_dock() {
        assert!(should_show_window_on_reopen(false));
    }

    #[test]
    fn does_not_steal_focus_when_reopen_finds_visible_windows() {
        assert!(!should_show_window_on_reopen(true));
    }

    #[test]
    fn site_switch_confirmation_uses_the_target_site_name() {
        assert_eq!(
            site_switch_confirmation_title(SiteKey::Yaowu),
            "是否切换到要务？"
        );
        assert_eq!(
            site_switch_confirmation_title(SiteKey::Jifuwu),
            "是否切换到即服务？"
        );
        assert_eq!(
            site_switch_confirmation_description(),
            "目标站点未登录时需要登录，当前页面未保存的内容可能丢失。"
        );
    }
}
