use tauri::{AppHandle, Url, WebviewWindow, WebviewWindowBuilder};

const ALLOWED_WEB_ORIGINS: &[&str] = &[
    "https://snack.mechlabs.cn",
    "https://qasnack.mechlabs.cn",
    "http://localhost:3000",
    "http://127.0.0.1:3000",
];

#[tauri::command]
fn exit_after_force_update_cancel(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|err| err.to_string())?;

    if !is_allowed_web_origin(&url) {
        return Err("origin is not allowed to close the desktop shell".to_string());
    }

    app.exit(0);
    Ok(())
}

fn is_allowed_web_origin(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };

    let origin = match url.port() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    };

    ALLOWED_WEB_ORIGINS.contains(&origin.as_str())
}

fn desktop_user_agent() -> String {
    format!(
        "{} SnackDesktop/{}/{}",
        env!("SNACK_DESKTOP_BASE_UA").trim(),
        env!("SNACK_DESKTOP_ARCH"),
        env!("SNACK_DESKTOP_VERSION")
    )
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![exit_after_force_update_cancel])
        .setup(|app| {
            let window_config = app
                .config()
                .app
                .windows
                .first()
                .expect("missing main window config");

            let user_agent = desktop_user_agent();

            WebviewWindowBuilder::from_config(app, window_config)?
                .user_agent(&user_agent)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Snack desktop client");
}
