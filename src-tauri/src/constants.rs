#[cfg(any(target_os = "macos", windows))]
use tauri::include_image;

#[cfg(any(target_os = "macos", windows))]
pub(crate) const TRAY_ID: &str = "main-tray";
#[cfg(any(target_os = "macos", windows))]
pub(crate) const TRAY_MENU_SHOW_ID: &str = "show";
#[cfg(any(target_os = "macos", windows))]
pub(crate) const TRAY_MENU_QUIT_ID: &str = "quit";
#[cfg(any(target_os = "macos", windows))]
pub(crate) const NAVIGATION_MENU_BACK_ID: &str = "navigation-back";
#[cfg(any(target_os = "macos", windows))]
pub(crate) const NAVIGATION_MENU_ID: &str = "navigation";

#[cfg(target_os = "macos")]
pub(crate) const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/white.png");
#[cfg(windows)]
pub(crate) const TRAY_DEFAULT_ICON: tauri::image::Image<'_> = include_image!("./icons/32x32.png");
#[cfg(any(target_os = "macos", windows))]
pub(crate) const TRAY_ATTENTION_ICON: tauri::image::Image<'_> =
    include_image!("./icons/tray-attention.png");
#[cfg(any(target_os = "macos", windows))]
pub(crate) const ABOUT_ICON: tauri::image::Image<'_> = include_image!("./icons/icon.png");
