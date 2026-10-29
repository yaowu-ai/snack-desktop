# 录音悬浮窗几何修复与验收

> 后续实现已改为同一窗口内前后翻卡，当前行为与本轮验证见 [录音与转写卡片交接](recording-card-handoff-20261030.md)。下文保留此前独立提示窗口的历史实现和验收范围。

## 行为

本次修复 `fix/recording-presence-1027` 中为并发提示预留永久空白导致的上移受限和阴影裁切。保留已合入的其他功能，不改租户门槛、录音数据、纪要策略或桥接版本。

- 胶囊外窗恢复 263×148，内容为 (33,13) 起的 197×72；默认上方、右侧各 24px，底部透明留白 63px。阴影缩小为 `0 8px 18px -6px rgba(0,0,0,.20)`。
- 交接提示使用单独的 360×96 被动窗口，优先在胶囊上方，空间不足时放下方，并限制在当前屏幕工作区。
- 提示出现、消失或跟随拖动时不改变胶囊尺寸与位置；首次展示先确认实际坐标到位，防止闪现在默认位置。已显示时继续跟随，不反复隐藏。
- 提示不抢焦点、鼠标穿透；只有只读状态及事件订阅权限。提示失败不阻塞主胶囊或生命周期。移动事件离开当前回调栈后刷新，避免 UI 同步派发造成锁重入。

## 验收结果

| 检查 | 结果与边界 |
| --- | --- |
| `cargo test --lib --locked --jobs 2` | 277 项通过；包括 7 项纯几何边界测试、旧生命周期和控制身份回归 |
| 共享语言资源离线校验 | 通过，指纹保持 `a7d194a69880d22419c64cb8dcf804b8fbd406f0f2490d864a92af36870ce3ef` |
| Edge 真实页面、模拟原生状态 | 15 项通过：尺寸、3秒、不重播、切换清理、纪要隐藏及提示前后位置不变 |
| 原生 Tauri 隔离测试 | 使用生产窗口构造、毛玻璃、定位、提示与移动回调，合成录音状态，不初始化录音/账号/快捷键，不接触用户录音数据 |
| 阴影 | 原生窗口中观察到柔和阴影，没有原截图的矩形底部裁切 |
| 上下拖动 | 自动化工具无法拖动透明窗口；用户随后手动试验并反馈“正常了”，原生坐标记录也包含向下和回到顶部的移动 |
| Toast 首帧、出现和消失 | 首次可见已位于目标坐标；整个提示期间胶囊均为 `(2440,68,526,296)` 物理像素，提示位置恒为 `(2220,254)`，结束后胶囊不移动 |
| 跨显示器/DPI | 负坐标及2倍缩放通过几何测试；未做多显示器实机拖动 |

测试位于 `/Users/gaoxiang/Desktop/Snack/outputs/recording-overlay-geometry-20261030/`：`native-tests-final.log`、`edge-ui-result.txt`、`edge-ui-result.png`、`native-positions-verified.jsonl`、`native-verification.json`。隔离测试源码和程序只留在输出目录，不加入生产分支。测试程序和临时服务已退出；本轮没有替换已安装验收包。

## Applicable Specs

- `code-quality/spec`：纯坐标计算及边界测试在 `src-tauri/src/meeting/note_overlay_geometry.rs:21-133`；窗口创建、定位与展示分开，见 `src-tauri/src/meeting/note_overlay.rs:1005-1143`。
- `project-structure/spec`：继续沿用 meeting 原生层与现有绑定，不新增 Website 业务；资源由现有协议提供，见 `src-tauri/src/meeting/overlay.rs:96-101`。
- `ui-components/spec`：胶囊和独立提示样式在 `src-tauri/src/meeting/note_overlay.html:15-17`、`src-tauri/src/meeting/note_notice.html:10-18`；现有紧凑字体、胶囊资产继续遵循指定视觉。
- `i18n-resources/spec`：没有手改共享资源；新窗口加入现有语言刷新，见 `src-tauri/src/meeting/overlay_locale.rs:76-82`。
- `git-workflow/spec`：在指定分支保留已有合并结果，无 `test` 合并或 rebase。
- 数据库、缓存、查询、聊天流规范 N/A：本轮不涉及。

已尝试 `openspec list --specs`，当前环境无该 CLI；直接读取 Website 中规范索引和上述规范。机械设计检查因缺少 HTML parser 降级为正则检查，唯一提示为既有紧凑字号层级；实际几何与阴影以 Edge 和原生窗口验收为准。
