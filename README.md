# Snack Desktop

Snack Desktop 是 [Snack](https://snack.mechlabs.cn/) 的桌面客户端。Snack 是面向企业增长团队的 AI 工作台，连接业务数据、团队协作和待办任务，让信息整理、分析、执行与复盘形成持续运转的闭环。

**[访问 Snack 官网并开始使用](https://snack.mechlabs.cn/)**

## Snack 能做什么

Snack 将企业知识、会议文档、IM 沟通、业务数据、流程权限和既有工具系统连接到同一工作流中。它不仅回答问题，也能结合上下文识别风险、生成建议、推进任务，并将过程经验沉淀为团队可复用的资产。

- **统一经营视角**：汇总客户、项目、渠道和反馈等过程数据，帮助团队发现机会与风险，并围绕同一份事实协同决策。
- **团队记忆系统**：将资料、会议、聊天与业务过程沉淀为可检索、可复用的上下文，减少重复梳理与信息断层。
- **长任务执行**：通过 Agent、Skill 和任务记忆拆解复杂工作，持续跟进执行进度，并沉淀下一次交付可用的经验。
- **安全可控的 AI 底座**：结合本地数据安全、本地算力与多级模型路由，在安全要求和成本之间选择合适的模型能力。

适用于出海增长、经营管理、伙伴运营等需要跨数据源、跨角色协作的真实业务场景。无论是投放复盘与线索承接，还是服务反馈归类和行动跟进，Snack 都帮助团队把零散动作组织成可持续优化的增长节奏。

## 界面预览

Snack 将企业增长工作流呈现在同一界面中：

![Snack 企业增长工作流](https://snack.mechlabs.cn/home/growth-flow.png)

从多市场、多语种、多品类的增长运营，到提醒、任务与复盘，Snack 帮助小团队连接过程信息并持续推进关键动作：

![Snack 增长运营场景](https://snack.mechlabs.cn/home/case-growth-hd.png)

在经营管理中，Snack 汇集客户、项目、反馈与过程数据，为团队建立统一的经营认知：

![Snack 经营管理场景](https://snack.mechlabs.cn/home/case-command-hd.png)

更多产品能力、应用场景与使用方式，请访问 **[snack.mechlabs.cn](https://snack.mechlabs.cn/)**。

## 本地打包

```bash
npm run dev -- prod
npm run dev -- qa
npm run build -- prod
npm run build -- qa
```

## 远端打包与发布

GitHub Actions 在推送符合条件的 tag 时触发构建。tag 支持 `v*` 或 `*.*.*` 形式，但版本最终必须是 `x.y.z` 或 `vX.Y.Z`，例如 `1.2.3`、`v1.2.3`。

工作流会检查 tag 指向的提交属于哪个远端分支历史：

| Tag 所在提交 | 构建环境 | GitHub Release |
| --- | --- | --- |
| `prod` 分支历史 | `prod` | 正式发布并标记为 latest |
| `test` 分支历史 | `qa` | 预发布（prerelease） |
| 不属于两者 | 不构建 | 工作流失败 |

每次发布会在 GitHub Release 上传以下签名产物：

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

远端构建使用 Node.js 22。Windows 构建需要 `TAURI_UPDATER_PUBKEY` 和 `TAURI_SIGNING_PRIVATE_KEY`；macOS 构建还需要 Apple 签名与公证相关的 GitHub Secrets。工作流会为 updater 安装包生成并上传 `.sig` 签名文件。

## 许可证

本项目采用 [MIT License](LICENSE)。
