## 总体原则
本项目定位为Snack的桌面端，我们要保持桌面端纯粹，只提供原生SDK让web去调用
让所有业务API有一个总体的兼容入口，低版本入口会提示升级版本
界面、交互写到web上
配置、常量由服务返回

## 多语言资源规范

共享语言资源与规范由 Website 维护，规范见 `h-snack-feat/openspec/specs/i18n-resources/spec.md`（`h-snack-feat` 等价于 `h-snack-website`）。中英文入口统一具名导出，英文未开放模块回退中文；不得在 Desktop 另设开放白名单或手改 `src-tauri/resources/i18n`。需要采用新资源时，在 Website 执行 `npm run i18n:sync` 自动生成指纹快照，并在 Desktop 通过 `npm run i18n:check` 离线验证。Desktop 可保留随包旧快照，独立构建不依赖相邻 Website、Git 历史或手工资源版本；资源随桌面应用自身版本发布。
