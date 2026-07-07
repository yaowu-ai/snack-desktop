use std::path::Path;
use std::process::Command;

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
