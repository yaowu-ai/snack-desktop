use std::path::Path;
use std::process::Command;

#[cfg(target_os = "macos")]
use objc2_app_kit::NSWorkspace;
#[cfg(target_os = "macos")]
use objc2_foundation::{NSString, NSURL};
use tauri::Url;

pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    run_open_command(build_open_command(path))
}

pub(crate) fn reveal_path(path: &Path) -> Result<(), String> {
    run_open_command(build_reveal_command(path))
}

pub(crate) fn open_external_url(url: &Url) -> Result<(), String> {
    run_open_command(build_open_url_command(url.as_str()))
}

#[cfg(target_os = "macos")]
pub(crate) fn open_system_settings_url(url: &Url) -> Result<(), String> {
    let value = NSString::from_str(url.as_str());
    let native_url =
        NSURL::URLWithString(&value).ok_or_else(|| "failed to open URL".to_string())?;
    if NSWorkspace::sharedWorkspace().openURL(&native_url) {
        Ok(())
    } else {
        Err("failed to open URL".to_string())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn open_system_settings_url(url: &Url) -> Result<(), String> {
    open_external_url(url)
}

fn run_open_command(mut command: Command) -> Result<(), String> {
    command
        .spawn()
        .map_err(|_| "failed to open downloaded file".to_string())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_open_command(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(path);
    command
}

#[cfg(target_os = "macos")]
fn build_reveal_command(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg("-R").arg(path);
    command
}

#[cfg(windows)]
fn build_open_command(path: &Path) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg("start").arg("").arg(path);
    command
}

#[cfg(windows)]
fn build_reveal_command(path: &Path) -> Command {
    let mut command = Command::new("explorer.exe");
    command.arg(format!("/select,{}", path.to_string_lossy()));
    command
}

#[cfg(all(not(target_os = "macos"), not(windows)))]
fn build_open_command(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
}

#[cfg(all(not(target_os = "macos"), not(windows)))]
fn build_reveal_command(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path.parent().unwrap_or_else(|| Path::new("/")));
    command
}

#[cfg(target_os = "macos")]
fn build_open_url_command(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(windows)]
fn build_open_url_command(url: &str) -> Command {
    let mut command = Command::new("explorer.exe");
    command.arg(url);
    command
}

#[cfg(all(not(target_os = "macos"), not(windows)))]
fn build_open_url_command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}
