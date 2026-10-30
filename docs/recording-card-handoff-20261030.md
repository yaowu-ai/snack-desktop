# 录音与转写卡片交接

## 已确认的交互

在 `fix/recording-presence-1027` 实现用户确认的前后翻卡方案。上一条仍在转写、新录音开始时，新录音卡用 560ms 小角度翻转到前面；旧卡退后并显示 188×64、36px 圆角的绿色勾提示。最新中性文案为“上一条录音已保留 / 首页「会议录音」可查看”，只说明上一条的保存位置，不承诺转写成功。

提示完全展开后停留 3 秒，然后旧卡和提示一起消失，只保留当前录音卡。按最新验收反馈，不再用 220ms 收到背面、不保留“上一条转写”入口，也不继续显示上一条的失败或完成状态。上一条仍可从首页“会议录音”查看。系统减弱动态时使用 80ms 短过渡。

2026-10-30 验收补充：并发交接的首次翻卡和 3 秒提示在新录音笔记位于前台时也立即展示；交接时限结束后恢复“当前笔记可见则隐藏悬浮窗”的常规规则。创建交接时仍要求同一来源、同一账号、当前录音有效且上一条仍在转写；已开始的中性提示不会因上一条转写完成或失败提前结束，不改变已过期提示不重播的规则。

同日混合 DPI 修复：macOS 首次隐藏窗口定位及翻卡位置补偿使用目标屏幕缩放率将物理目标转成显式逻辑坐标，不依赖尚未显示窗口的 backing scale，避免 Retina 屏幕上首次显示时坐标翻倍移出屏幕；其他平台保持原物理坐标路径。

原生窗口为 263×192，主胶囊仍为 197×72。上方有空间时向上展开，否则向下展开；`capsuleTop` 为 97 或 13，并同步补偿窗口原点，保持胶囊在屏幕上的位置。翻卡期间两个卡片的操作区均为 inert；交接时限结束后原生释放旧卡展示绑定并恢复单卡背景与位置。

## 身份与生命周期

- 原生主 binding 始终属于当前录音 B；暂停、停止、离线停止回退都校验 B 的 recordingId。
- 旧卡 A 仅在同 origin、同 scope、之前未关闭或隐藏且仍在转写时进入交接。交接导致的 retire 不自动复活 A 的结果提示；此前主动关闭的任务不进入卡组。
- 交接期间旧卡事件仍只允许同来源、同账号的只读 Open，并携带 `recordingId=A`、`currentRecordingId=B`；Website 独立解析旧笔记路由，ACK 沿 B 的同步参数发送。交接结束后释放 A 的展示绑定，旧入口不能继续操作。
- 560ms 翻卡加 3 秒提示结束、B 结束、账号/租户/站点改变时同时移除交接提示和旧卡绑定；旧录音与转写任务不会被删除或停止，已退役的旧结果提示不恢复。
- `handoffNoticeId` 只触发一次。原生允许 560ms 动画和 3 秒提示；前端状态刷新及语言刷新均不延长提示。晚到的初始化结果不能覆盖较新的事件。
- 旧独立提示窗口不再创建；普通单卡继续使用原生毛玻璃。双卡期间关闭固定位置的原生背景，由不透明 CSS 卡片承载动画，避免文字透叠或留底影。

## 早期验证记录与边界

- `cargo test --lib meeting::note_overlay`：37 项通过，包含生命周期、只读旧卡动作、当前录音身份和高 DPI / 负坐标几何。
- Playwright Chromium：`note-overlay.spec.ts`、`overlay-i18n.spec.ts` 共 26 项通过，覆盖前后切换、3 秒、精确动画时长、键盘 inert、收起途中刷新、上下展开和语言切换。
- Website 相关 Jest：3 套 58 项通过；共享语言校验 5 套 51 项通过；相关 ESLint、`git diff --check`、JavaScript 语法及 Desktop 离线语言校验通过。
- Desktop 快照由 Website 单目标导出，指纹 `2451dfb748053ef35805e2b17823d121bf2807137fe7292b1faecc26d3980758`，没有手改快照或更改英文模块开放范围。
- 本轮没有打包、安装、部署或实际录音设备验收。WebKit 下载无进展后已停止，不能把 Chromium 页面测试当作 WKWebView / Windows 实机验收。
- 设计检测器缺少 HTML parser，降级检查仅提示紧凑字号层级；本控件按已确认的 197px 胶囊设计保留 11/12/14/16px 字号，实际布局由页面测试检查。

最新“提示到期即移除旧卡”修改新增原生纯策略回归：未展示的有效交接仍可初始化；到期、缺少提示、身份/尝试不匹配、B 离开录音阶段时释放旧卡，当前录音绑定保持不变。本轮原生45项、Chromium界面34项、共享语言51项回归均通过；上述早期数字仅作历史记录。当前实包视觉效果由用户手动验收。

## 规范落实

- `code-quality/spec`：动画状态与页面主体分文件；过期回调由 generation 取消，见 `src-tauri/src/meeting/note-overlay-deck.js`。
- `project-structure/spec`：复用 meeting 原生协议和 Website 已有 owner / 路由解析；只读旧卡校验见 `src-tauri/src/meeting/note_overlay.rs:1549-1599` 与 Website `src/lib/standard-recording-presence-actions.ts:63-105`。
- `ui-components/spec`：使用自然语言、现有图标、紧凑圆角和轻阴影，见 `src-tauri/src/meeting/note_overlay.html:47-60`。此处为原生离线 WebView 的既有 HTML 胶囊，不新增 React / shadcn 依赖或模态框布局。
- `i18n-resources/spec`：共享源在 Website `src/lib/i18n/dictionaries/{zh,en}/desktop-overlay.ts:17-21`，按单目标导出生成 Desktop 的 `src-tauri/resources/i18n`，离线验证通过。
- 数据库、缓存、查询与聊天流规范 N/A：未涉及。未创建或合并分支，未引入 `test` / `origin/test`。

已尝试 `openspec list --specs`；本机没有该 CLI，因此直接读取 Website 规范索引及相关规范。此前独立提示的验收记录保留在 `recording-overlay-geometry-20261030.md`，不替代本次新交互的验收。
