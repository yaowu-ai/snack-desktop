# Snack desktop scripts

这些脚本分两种使用场景：本地调试和打包构建。命令都从 `snack-desktop`
项目根目录执行。

`.cmd` 是 Windows 下的便捷入口，内部会转发到对应的 `.ps1` 脚本。后续调整
逻辑时优先改 `.ps1`。

## 本地调试

默认启动 `dev` 环境：

```powershell
.\scripts\dev.ps1
```

指定环境启动：

```powershell
.\scripts\dev.ps1 dev
.\scripts\dev.ps1 test
.\scripts\dev.ps1 prod
```

指定环境和 UA 版本号启动：

```powershell
.\scripts\dev.ps1 dev 1.0.0
.\scripts\dev.ps1 test 1.0.0
```

CMD 入口：

```cmd
scripts\dev.cmd
scripts\dev.cmd test
scripts\dev.cmd prod
scripts\dev.cmd dev 1.0.0
```

执行规则：

```text
1. 设置 SNACK_DESKTOP_ENV。
2. 如果传入第二个参数，则设置 SNACK_DESKTOP_VERSION。
3. 进入 src-tauri 目录执行 cargo run。
4. build.rs 根据环境选择 Web 地址。
5. lib.rs 创建 WebView，并设置桌面端 UA。
```

环境地址：

```text
dev  -> http://localhost:3000
test -> https://qasnack.mechlabs.cn
prod -> https://snack.mechlabs.cn
```

## 打包构建

默认构建 `prod` 环境：

```powershell
.\scripts\build.ps1
```

指定环境构建：

```powershell
.\scripts\build.ps1 test
.\scripts\build.ps1 prod
```

指定环境和 UA 版本号构建：

```powershell
.\scripts\build.ps1 prod 1.0.0
```

CMD 入口：

```cmd
scripts\build.cmd
scripts\build.cmd test
scripts\build.cmd prod 1.0.0
```

执行规则：

```text
1. 设置 SNACK_DESKTOP_ENV。
2. 如果传入第二个参数，则设置 SNACK_DESKTOP_VERSION。
3. 进入 src-tauri 目录执行 cargo tauri build。
4. build.rs 将 Web 地址、平台、架构、版本号写入编译产物。
5. 安装后的客户端会保留打包时写入的 UA 信息。
```

安装包输出目录：

```text
src-tauri\target\release\bundle\
```

## 自动更新框架

桌面端使用 Tauri v2 updater plugin。打包前需要先生成 updater keypair，并在
构建环境设置公钥：

```powershell
npx tauri signer generate
$env:TAURI_UPDATER_PUBKEY = "..."
```

`npm run build -- prod` / `npm run build -- qa` 会通过 `TAURI_CONFIG` 注入对应
Web 地址和 updater endpoint。默认 endpoint 仍指向网站动态 API，后续接入
GitHub Actions 全平台打包后，可以用环境变量切到静态 CDN manifest：

```text
SNACK_PROD_UPDATER_ENDPOINT=https://cdn.example.com/desktop-updates/prod/latest.json
SNACK_QA_UPDATER_ENDPOINT=https://cdn.example.com/desktop-updates/qa/latest.json
```

构建签名包时还需要按 Tauri 要求提供私钥环境变量（例如
`TAURI_SIGNING_PRIVATE_KEY` 和对应密码）。生成的安装包签名内容回填到
`h-snack-website` 的桌面版本管理后，网站会生成 updater manifest。

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
1. 调试或打包命令第二个参数，例如 .\scripts\dev.ps1 dev 1.0.0。
2. SNACK_DESKTOP_VERSION 环境变量。
3. src-tauri/Cargo.toml 中的 Rust 包版本。
```

基础浏览器 UA 默认按平台选择，参考 Pake 的做法：macOS 使用 Safari 风格 UA，
Windows/Linux 使用 Chrome 风格 UA。需要覆盖时设置 `SNACK_DESKTOP_BASE_UA`。

## 维护位置

调整环境、地址、版本号或 UA 时，同步检查这些文件：

```text
scripts/dev.ps1
scripts/build.ps1
scripts/dev.cmd
scripts/build.cmd
src-tauri/build.rs
src-tauri/src/lib.rs
scripts/README.md
```
