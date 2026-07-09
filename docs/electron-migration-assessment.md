# DBX 改用 Electron 的可行性评估报告

日期：2026-07-08

## 结论

DBX 技术上可以引入 Electron，但不建议把 Electron 作为当前 Tauri 桌面端的直接全量替代。

更稳妥的路线是：

1. 保留现有 Tauri 作为主线，尤其保留当前 `src-tauri-legacy` 用于 Windows 7 / Windows 8。
2. 如果主要目标是摆脱现代 Windows 上的 WebView2 依赖，可以单独做一个 Electron modern-only 试验版。
3. 不建议使用旧版 Electron 维护 Windows 7 / Windows 8，因为 Electron 23 起已经移除 Windows 7 / 8 / 8.1 支持，能运行这些系统的 Electron 22 已经不在当前官方支持周期内。

## 当前项目结构判断

当前 DBX 不是一个简单的“Vue 前端加壳”项目，而是三层结构：

1. 前端层：`apps/desktop`，基于 Vue、Vite、Pinia、CodeMirror 等。
2. Tauri 桌面桥接层：`src-tauri` 和 `src-tauri-legacy`，负责窗口、托盘、深链、更新、文件系统、系统对话框、剪贴板、WebView2 检测/安装提示、数据目录、关闭行为，以及大量 Rust command。
3. Rust 核心后端层：`crates/dbx-core` 和 `crates/dbx-web`，负责数据库连接、查询、导入导出、驱动/agent、AI、Redis、Mongo、Nacos、WebDAV 等核心能力。

关键证据：

- `package.json` 仍以 Tauri 为桌面运行时，依赖 `@tauri-apps/api` 与多个 Tauri 插件。
- `src-tauri/src/lib.rs` 注册了 `deep-link`、`clipboard`、`dialog`、`fs`、`shell`、`updater`、`process`、`window-state` 等插件，并集中注册大量 `invoke_handler` command。
- `apps/desktop/src/lib/backend/tauri.ts` 是大型 Tauri command 适配层，当前前端大量能力通过 `invoke(...)` 进入 Rust。
- `apps/desktop/src/lib/backend/http.ts` 和 `crates/dbx-web/src/main.rs` 已经存在 HTTP 后端路线，这是未来做 Electron 壳的主要复用点。

## Electron 能解决什么

Electron 的直接收益主要在现代 Windows：

- Electron 自带 Chromium，不依赖系统 WebView2 Runtime。
- Windows 10 / 11 上用户无需处理 WebView2 安装、缺失、版本兼容、全局/用户级安装等问题。
- 前端调试、Chromium 行为一致性、DevTools 体验会更直接。

但这些收益不等于迁移成本低。DBX 现在大量桌面能力已经绑定在 Tauri 插件和 Rust command 上，Electron 只替换“桌面壳”，不会自动替换这些 command、插件、安全边界和打包逻辑。

## 关键限制：Windows 7 / Windows 8

这是本项目最重要的判断点。

Electron 官方 README 当前平台支持写明：Windows 支持从 Windows 10 开始，并说明 Windows 7 / 8 / 8.1 支持已在 Electron 23 移除。Electron breaking changes 也明确说明，Electron v23.0.0 及更高版本要求 Windows 10 或更高版本。

因此：

- 如果 DBX 必须继续支持 Windows 7 / Windows 8，Electron 不能作为统一替代方案。
- 如果强行使用 Electron 22 或更老版本支持 Windows 7 / Windows 8，会落入过期 Chromium / Node / Electron 组合，安全和维护风险明显高于当前 legacy Tauri 路线。
- 当前项目已经维护了 `src-tauri-legacy` 和 legacy WebView2 安装包路线，这比引入 EOL Electron 更可控。

## 可选迁移方案

### 方案 A：保留 Tauri，继续收敛 WebView2 包装逻辑

这是最低风险方案。

保留现有 modern / legacy 四包构建方式，继续优化 WebView2 检测、提示、安装和日志。优点是保留 Windows 7 / 8 能力，Rust command 不需要迁移，当前构建体系延续性最好。缺点是现代 Windows 仍然要面对 WebView2 Runtime 生态问题。

适用条件：Windows 7 / 8 兼容仍是硬要求，且不希望增加第二套桌面壳。

### 方案 B：新增 Electron modern-only 试验版，legacy 继续用 Tauri

这是我推荐的路线。

做法是新增一个 Electron 桌面入口，只面向 Windows 10 / 11、macOS、Linux 的现代系统。Electron 主进程负责窗口、托盘、菜单、更新、文件对话框等桌面壳能力；核心业务尽量复用 `crates/dbx-web`，由 Electron 启动本地 Rust sidecar 或嵌入式 HTTP 服务，前端通过现有 HTTP backend 访问。

优点：

- 现代包可以彻底绕开 WebView2 Runtime 依赖。
- 不破坏现有 legacy Tauri 包。
- 可以用真实包大小、启动速度、内存占用、稳定性数据做决策。

缺点：

- 会同时维护 Tauri 和 Electron 两套桌面壳。
- 桌面能力需要从 Tauri 插件映射到 Electron IPC 或 HTTP endpoint。
- 自动更新、签名、便携版目录策略、日志、崩溃处理都要重新设计和验证。

适用条件：现代系统希望减少 WebView2 用户问题，但仍保留 Windows 7 / 8 支持。

### 方案 C：全量替换为 Electron

不建议。

这意味着替换 Tauri command 桥、重做所有桌面插件能力、重做打包和更新链路，并且无法使用当前受支持 Electron 版本覆盖 Windows 7 / 8。除非项目明确放弃 Windows 7 / 8，并接受更大的安装包和内存占用，否则收益不够覆盖风险。

## 迁移工作量评估

按方案 B 估算，新增 Electron modern-only 试验版至少需要处理以下模块：

1. Electron main/preload 基础工程：窗口、菜单、托盘、生命周期、日志、崩溃处理。
2. 后端启动：打包 `dbx-web` 或新增 Rust sidecar，启动时分配本地端口，传入 `DBX_DATA_DIR`，并处理进程退出。
3. 前端运行时识别：现有 `isTauriRuntime()` 需要扩展为 desktop runtime 抽象，避免业务组件直接判断 Tauri。
4. Tauri API 替换：文件对话框、文件读写、shell open、剪贴板、窗口控制、应用重启、更新检查、深链、拖拽文件、系统字体。
5. API 适配：优先复用 `apps/desktop/src/lib/backend/http.ts`，缺失的 desktop-only command 通过 Electron IPC 或补 HTTP route。
6. 安全加固：Electron renderer 禁用 Node integration，开启 contextIsolation，通过 preload 暴露最小 API，校验 IPC sender。
7. 打包发布：electron-builder 或 Electron Forge 配置，现代 Windows x64 便携包、安装包、签名、更新源、CI。
8. 回归验证：数据库连接、导入导出、大文件、agent/JDBC、WebDAV、更新、设置目录、多开窗口、关闭到托盘。

粗略工作量：

- POC：3 到 5 天。目标是打开现有前端、启动 Rust HTTP 后端、完成连接/查询主流程。
- 可测试试验版：2 到 4 周。目标是覆盖主要桌面能力和打包。
- 生产替代：6 到 10 周以上。取决于 updater、签名、legacy 分支、CI、自动化测试和异常恢复要求。

## 风险清单

1. Windows 7 / 8 风险：受支持 Electron 版本不能覆盖老系统。
2. 安全风险：Electron IPC 如果暴露过宽，会比当前 Tauri command 权限模型更容易出问题。
3. 包体积风险：Electron 会随包携带 Chromium，包体积和磁盘占用显著增加。
4. 内存风险：Electron renderer + main + Rust sidecar 会比当前 Tauri 更重。
5. 双运行时风险：如果 Tauri legacy 和 Electron modern 并存，桌面行为和 bug 修复需要同步。
6. 更新链路风险：当前 Tauri updater 逻辑不能直接复用，需要重做 Electron 更新策略。
7. 测试风险：现有测试大量 mock `isTauriRuntime()`，需要抽象成通用 desktop runtime 后再补 Electron 覆盖。

## 推荐决策

短期不建议“改用 Electron”作为主线目标。

建议先做一个受控试验：

1. 保持 `src-tauri` 和 `src-tauri-legacy` 不动。
2. 新增 `apps/electron` 或 `src-electron`，只产出现代系统包。
3. Electron 首版只走本地 HTTP 后端，不重写 Rust 核心。
4. 明确 Electron 试验版不支持 Windows 7 / Windows 8。
5. 用 POC 数据决定是否长期维护 Electron modern 包。

如果用户的核心目标是“现代系统不再处理 WebView2”，方案 B 值得做。如果核心目标是“降低维护成本并统一所有 Windows 版本”，Electron 反而会增加维护成本，不建议迁移。

## 建议的 POC 验收标准

POC 不应一上来追求全功能，建议只验证这些硬指标：

1. Windows 10 / 11 上可启动，无 WebView2 依赖。
2. 可使用 `~/.dbx` 或配置的 `DBX_DATA_DIR`。
3. 可连接至少 SQLite、PostgreSQL、MySQL、Redis 中的两个主流程。
4. 查询、分页、取消查询、保存连接可用。
5. 便携包可在另一台 Windows 10 / 11 主机运行。
6. 与当前 Tauri modern 包对比启动时间、包体积、内存占用。

只有这些指标明显优于当前 Tauri modern 包，才值得继续推进 Electron modern 试验版。

## 参考资料

- Electron README 平台支持：<https://github.com/electron/electron#platform-support>
- Electron breaking changes，Electron 23 移除 Windows 7 / 8 / 8.1 支持：<https://www.electronjs.org/docs/latest/breaking-changes#planned-breaking-api-changes-230>
- Electron release 支持策略：<https://www.electronjs.org/docs/latest/tutorial/electron-timelines>
- Electron 安全建议：<https://www.electronjs.org/docs/latest/tutorial/security>
- Tauri Windows 依赖与 WebView2 说明：<https://v2.tauri.app/start/prerequisites/#windows>
