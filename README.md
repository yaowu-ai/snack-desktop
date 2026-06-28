# Snack Desktop

如何打包：

```bash
npm run dev -- prod
npm run dev -- qa
npm run build -- prod
npm run build -- qa
```

CI 打包规则：

```text
push 到 test 分支 -> QA 测试包，上传 GitHub Actions artifacts
push v* 或 *.*.* tag -> 正式包，tag 指向的 commit 必须属于 GitHub prod 分支历史，上传 GitHub Release
```

打包脚本会从 GitHub ref 推导环境：`test` 分支使用 `qa`，tag 使用 `prod`。
本地显式传参或设置 `SNACK_ENV` 时，以本地输入为准。

## 正式发版

正式包通过 GitHub tag 触发，不需要在本地上传安装包。tag 必须指向 GitHub
`prod` 分支历史中的 commit，否则 CI 会直接失败。

推荐流程：

```bash
git fetch origin-github prod --tags
git checkout main
git merge --ff-only origin-github/prod
git tag 0.1.3 origin-github/prod
git push origin-github 0.1.3
```

如果上一次 tag 指向了旧 commit，重跑 GitHub Actions 不会使用新代码。需要重新打一个新版本
tag，例如从 `0.1.2` 改为 `0.1.3`，并确保新 tag 指向当前 GitHub `prod`。

CI 上传到 GitHub Release 的产物命名规则：

```text
Snack_{version}_windows_x64.exe
Snack_{version}_windows_x64.exe.sig
Snack_{version}_macos_arm64.dmg
Snack_{version}_macos_arm64.app.tar.gz
Snack_{version}_macos_arm64.app.tar.gz.sig
Snack_{version}_macos_x64.dmg
Snack_{version}_macos_x64.app.tar.gz
Snack_{version}_macos_x64.app.tar.gz.sig
```

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

CI 会手动为 updater release 产物生成签名，并随 GitHub Release 上传 `.sig`。
默认关闭 Tauri CLI 自带的 updater artifact 生成，避免打包阶段重复处理签名。
如果需要本地显式启用 Tauri CLI 自带 updater artifact 生成，可设置：

```text
SNACK_CREATE_UPDATER_ARTIFACTS=true
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

`SNACK_PROD_HOST`、`SNACK_QA_HOST`、`SNACK_PROD_UPDATER_ENDPOINT`、
`SNACK_QA_UPDATER_ENDPOINT` 为空时会使用默认值：

```text
prod host: snack.mechlabs.cn
qa host: qasnack.mechlabs.cn
prod updater: https://snack.mechlabs.cn/api/desktop-updates/update?currentVersion={{current_version}}&target={{target}}&arch={{arch}}
qa updater: https://qasnack.mechlabs.cn/api/desktop-updates/update?currentVersion={{current_version}}&target={{target}}&arch={{arch}}
```

updater endpoint 中的 `{{current_version}}`、`{{target}}`、`{{arch}}` 是 Tauri
运行时占位符，放进 GitHub secret 或本地环境变量时也保持这种写法。

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
README.md
```
