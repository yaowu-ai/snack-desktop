fn main() {
    configure_default_locale();
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

    // The screencapturekit Swift bridge links libswift_Concurrency.dylib,
    // which macOS resolves from the dyld shared cache at /usr/lib/swift.
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_audio_tap();
    }
    tauri_build::build()
}

fn configure_default_locale() {
    println!("cargo:rerun-if-env-changed=NEXT_PUBLIC_DEFAULT_LOCALE");
    println!("cargo:rerun-if-env-changed=SNACK_REGION");
    println!("cargo:rerun-if-changed=../.env.domestic");
    println!("cargo:rerun-if-changed=../.env.global");
    let region = std::env::var("SNACK_REGION").unwrap_or_else(|_| "domestic".into());
    let source = match region.as_str() {
        "domestic" => include_str!("../.env.domestic"),
        "global" => include_str!("../.env.global"),
        _ => panic!("SNACK_REGION must be domestic or global"),
    };
    let locale = std::env::var("NEXT_PUBLIC_DEFAULT_LOCALE").unwrap_or_else(|_| {
        source
            .lines()
            .find_map(|line| line.strip_prefix("NEXT_PUBLIC_DEFAULT_LOCALE="))
            .expect("locale env configuration")
            .trim()
            .into()
    });
    assert!(
        matches!(locale.as_str(), "zh" | "en"),
        "NEXT_PUBLIC_DEFAULT_LOCALE must be zh or en"
    );
    println!("cargo:rustc-env=NEXT_PUBLIC_DEFAULT_LOCALE={locale}");
}

fn build_audio_tap() {
    use std::{path::PathBuf, process::Command};
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("unsupported macOS architecture: {other:?}"),
    };
    println!("cargo:rerun-if-changed=src/meeting/audio_tap.m");
    let object = out.join("audio_tap.o");
    assert!(Command::new("xcrun")
        .args([
            "clang",
            "-fobjc-arc",
            "-fblocks",
            "-arch",
            arch,
            "-mmacosx-version-min=12.3",
            "-Wall",
            "-Werror",
            "-c",
            "src/meeting/audio_tap.m",
            "-o",
        ])
        .arg(&object)
        .status()
        .expect("compile Core Audio tap")
        .success());
    assert!(Command::new("xcrun")
        .args(["ar", "crs"])
        .arg(out.join("libsnack_audio_tap.a"))
        .arg(object)
        .status()
        .expect("archive Core Audio tap")
        .success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=snack_audio_tap");
    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=Foundation");
}
