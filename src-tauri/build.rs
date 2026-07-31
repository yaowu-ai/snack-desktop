fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SNACK_DESKTOP_BASE_UA");
    println!("cargo:rerun-if-env-changed=SNACK_DESKTOP_VERSION");

    let version = std::env::var("SNACK_DESKTOP_VERSION")
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| "0.1.0".to_string());

    let platform = match std::env::var("CARGO_CFG_TARGET_OS") {
        Ok(value) => match value.as_str() {
            "windows" => "windows".to_string(),
            "macos" => "macos".to_string(),
            "linux" => "linux".to_string(),
            "android" => "android".to_string(),
            "ios" => "ios".to_string(),
            other => other.to_string(),
        },
        Err(_) => "unknown".to_string(),
    };

    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH") {
        Ok(value) => match value.as_str() {
            "x86_64" => "x64".to_string(),
            "x86" | "i686" => "x86".to_string(),
            "aarch64" => "arm64".to_string(),
            "arm" => "arm".to_string(),
            other => other.to_string(),
        },
        Err(_) => "unknown".to_string(),
    };

    let default_base_ua = match platform.as_str() {
        "macos" => "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        "windows" => "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
        "linux" => "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
        _ => "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36",
    };
    let base_ua = std::env::var("SNACK_DESKTOP_BASE_UA")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_base_ua.to_string());

    println!("cargo:rustc-env=SNACK_DESKTOP_VERSION={version}");
    println!("cargo:rustc-env=SNACK_DESKTOP_BASE_UA={base_ua}");
    println!("cargo:rustc-env=SNACK_DESKTOP_PLATFORM={platform}");
    println!("cargo:rustc-env=SNACK_DESKTOP_ARCH={arch}");

    tauri_build::build()
}
