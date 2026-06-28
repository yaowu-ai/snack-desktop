# Snack Desktop Build

`scripts/dev.ps1`、`scripts/build.ps1`、`scripts/dev.cmd` 和 `scripts/build.cmd`
已经废弃。不要再通过它们设置 `SNACK_DESKTOP_ENV` 或直接进入 `src-tauri`
执行 `cargo run` / `cargo tauri build`。

当前入口统一从项目根目录执行：

```bash
npm run dev -- prod
npm run dev -- qa
npm run build -- prod
npm run build -- qa
```

CI 打包规则：

```text
push 到 test 分支 -> QA 测试包，上传 GitHub Actions artifacts
push v* tag -> 正式包，tag 指向的 commit 必须属于 prod 分支历史，上传 GitHub Release
```

打包脚本会从 GitHub ref 推导环境：`test` 分支使用 `qa`，tag 使用 `prod`。
本地显式传参或设置 `SNACK_ENV` 时，以本地输入为准。

## 打包环境变量

必需：

```text
TAURI_UPDATER_PUBKEY
```

兼容旧变量名：

```text
TAURI_PUBLIC_KEY
```

生成 updater 签名包时需要：

```text
TAURI_SIGNING_PRIVATE_KEY
TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

可选覆盖：

```text
SNACK_ENV
SNACK_PROD_HOST
SNACK_QA_HOST
SNACK_PROD_UPDATER_ENDPOINT
SNACK_QA_UPDATER_ENDPOINT
SNACK_DESKTOP_BASE_UA
SNACK_DESKTOP_VERSION
```

`prod` 默认地址是 `snack.mechlabs.cn`，`qa` 默认地址是
`qasnack.mechlabs.cn`。CI 中 `SNACK_PROD_HOST` 和 `SNACK_QA_HOST` 来自
GitHub Actions Variables；本地环境变量会覆盖默认值。

macOS release CI 额外需要：

```text
TARGET_TRIPLE
APP_VERSION
DMG_ARCH_SUFFIX
APPLE_CERTIFICATE
APPLE_CERTIFICATE_PASSWORD
APPLE_SIGNING_IDENTITY
APPLE_ID
APPLE_PASSWORD
APPLE_TEAM_ID
```

## UA 规则

当前 WebView UA 格式：

```text
{base_browser_ua} SnackDesktop/{arch}/{version}
```

示例：

```text
Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SnackDesktop/x64/1.0.0
```

版本号来源：

```text
1. SNACK_DESKTOP_VERSION 环境变量。
2. src-tauri/Cargo.toml 中的 Rust 包版本。
```

基础浏览器 UA 默认按平台选择，参考 Pake 的做法：macOS 使用 Safari 风格 UA，
Windows/Linux 使用 Chrome 风格 UA。需要覆盖时设置 `SNACK_DESKTOP_BASE_UA`。

## 维护位置

调整环境、地址、版本号或 UA 时，同步检查这些文件：

```text
scripts/index.cjs
src-tauri/build.rs
src-tauri/src/lib.rs
scripts/README.md
```
