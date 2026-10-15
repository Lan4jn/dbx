# Win7 标题栏恢复与 Gateway 诊断增强开发报告

## 1. 文档状态

- 日期：2026-09-09
- 当前状态：方案已确认，生产代码尚未按本报告实施
- 实施顺序：同步上游 → 建立失败测试 → 恢复 Win7 标题栏 → 增强 Gateway stderr 日志 → 全量验证
- 当前本地分支：`main`
- 当前本地提交：`f4a9e4139`
- 当前工作区：仅有未跟踪的 `release/` 编译产物

## 2. 目标

1. Win7/Win8 legacy 构建恢复 Windows 原生标题栏。
2. modern Windows 继续使用现有自绘标题栏。
3. 在开发前合并最新 `upstream/main`，保留本项目 Gateway、多实例、数据目录和 legacy 构建能力。
4. Gateway 运行时故障以单行结构化文本写入 `stderr`，由 systemd/journald 收集。
5. 日志覆盖实际故障边界，同时禁止输出 Token、私钥、证书正文、SQL、数据库数据和数据库目标地址。

## 3. 已确认问题

### 3.1 Win7 标题栏回归

项目仍声明 Cargo feature：

```toml
legacy-native-window-decorations = []
```

但当前运行时逻辑将 Windows 和 Linux 都固定为无原生 decorations：

```rust
fn native_window_decorations_override(target_os: &str) -> Option<bool> {
    match target_os {
        "windows" | "linux" => Some(false),
        _ => None,
    }
}
```

当前 legacy 本地构建和 CI/release 构建也只启用 `custom-protocol`，未启用 `legacy-native-window-decorations`。因此该 feature 实际不可达。

历史提交 `bc1c09967` 已提供过正确设计：legacy Windows 使用原生标题栏，前端检测原生 decorations 后隐藏自绘窗口按钮。后续同步上游时这条接线被覆盖。

### 3.2 Gateway 错误被吞掉

Gateway CLI 目前只在进程启动失败时向 `stderr` 打印最终错误。进入长期运行后，大量错误被转换为布尔值、通用错误码或直接丢弃，例如：

- Edge 首次领证失败后直接退出。
- Edge 证书重新加载失败后直接退出。
- Edge 控制连接失败后只进入退避重试。
- Edge 控制通道断开原因丢失。
- Ticket 本地过期、目标不存在、目标解析失败、TCP/Unix 连接失败均只返回 `DataChannelFailed`。
- Edge 数据 WebSocket 建立失败和数据转发失败被压缩为 `false`。
- Main TLS 握手失败、身份识别失败、连接容量不足、限流拒绝均无日志。
- Main 路由拒绝、Edge 离线、Ticket 等待超时只有客户端错误码。
- Main/Edge 收到 HUP 后，配置读取、校验或 reload 失败会被忽略。
- PKI Unix/远程请求处理和 TLS 接受失败会被忽略。

这会把不同根因统一表现为 `TargetUnavailable`、`EdgeOffline` 或无输出，生产排障只能猜测。

## 4. 总体设计

### 4.1 同步策略

实施前先同步，不在旧代码上修复：

```bash
git fetch origin upstream
git merge upstream/main --no-edit
```

要求：

- 不使用 `git reset --hard` 或强制覆盖。
- 不提交 `release/`。
- 冲突按双方功能取并集处理。
- 合并后先运行基线测试，再开始功能修改。
- 本地三个未推送提交必须保留：两个时钟偏差文档提交及 `f4a9e4139` 实现提交。

### 4.2 Win7 标题栏

恢复已有 feature，不引入新配置：

```rust
fn use_native_window_decorations_for_platform(target_os: &str) -> bool {
    cfg!(feature = "legacy-native-window-decorations") && target_os == "windows"
}

fn native_window_decorations_override(target_os: &str) -> Option<bool> {
    match target_os {
        "windows" => Some(use_native_window_decorations_for_platform(target_os)),
        "linux" => Some(false),
        _ => None,
    }
}
```

legacy 构建启用：

```text
custom-protocol,legacy-native-window-decorations
```

modern 构建仍只启用：

```text
custom-protocol
```

桌面前端启动时通过已有 Tauri command 查询是否使用原生 decorations；为 `true` 时隐藏应用内 `WindowControls`，避免双标题栏。

### 4.3 Gateway 日志

不新增日志框架。复用 `serde_json` 和标准 `eprintln!`，在 `error.rs` 提供一个小型格式化入口，输出一行 JSON：

```json
{"level":"error","component":"edge","event":"target_connect_failed","code":"target_unavailable","edge_id":"edge-db01","target_id":"postgres","detail":"connection_refused"}
```

journald 已提供时间戳、PID、unit 和主机名，因此应用日志不重复生成这些字段。

日志级别：

- `error`：当前操作失败且不会自动成功完成。
- `warn`：连接中断、配置 reload 失败或自动重试。
- `info`：仅记录服务启动、成功 reload、Edge 注册/离线；不记录心跳和每个数据帧。

### 4.4 日志安全边界

允许记录：

- `component`、`event`、`GatewayErrorCode`
- `edge_id`、`client_id`、`target_id`
- 连接阶段、HTTP 状态、`std::io::ErrorKind`
- 重试延迟秒数、是否超时、是否达到容量限制

禁止记录：

- Enrollment Token 或 Token hash
- 私钥、密码、证书 PEM/DER
- 完整数据库地址、Unix Socket 路径
- SQL、数据库协议数据、请求/响应正文
- SPKI pin、完整证书指纹
- 环境变量全集或配置文件全文

所有自由文本字段必须通过 JSON 序列化转义，禁止手工拼接未转义的证书身份或配置值。

## 5. Gateway 事件清单

### 5.1 Edge

| 事件 | 级别 | 必要字段 |
| --- | --- | --- |
| `bootstrap_failed` | error | `edge_id`, `code`, `detail` |
| `credential_reload_failed` | error | `edge_id`, `code` |
| `renewal_failed` | warn | `edge_id`, `code`, `detail` |
| `control_connect_failed` | warn | `edge_id`, `code`, `retry_secs` |
| `control_protocol_failed` | warn | `edge_id`, `detail` |
| `control_disconnected` | warn | `edge_id`, `registered`, `retry_secs` |
| `ticket_expired_locally` | warn | `edge_id`, `target_id`, `skew_ms` |
| `target_missing` | error | `edge_id`, `target_id` |
| `target_policy_rejected` | error | `edge_id`, `target_id`, `code` |
| `target_connect_failed` | error | `edge_id`, `target_id`, `io_kind` 或 `timeout` |
| `data_connect_failed` | error | `edge_id`, `target_id`, `code` |
| `data_open_send_failed` | error | `edge_id`, `target_id` |
| `data_relay_failed` | warn | `edge_id`, `target_id`, `code`, `detail` |

### 5.2 Main

| 事件 | 级别 | 必要字段 |
| --- | --- | --- |
| `listener_accept_failed` | warn | `io_kind` |
| `connection_rate_limited` | warn | `peer_ip` |
| `connection_capacity_exceeded` | warn | 无 |
| `tls_handshake_failed` | warn | `peer_ip`, `detail` |
| `peer_identity_rejected` | warn | `peer_ip`, `code` |
| `websocket_upgrade_failed` | warn | `role`, `path` |
| `edge_registration_rejected` | warn | `edge_id`, `code` |
| `edge_registered` | info | `edge_id`, `target_count` |
| `edge_disconnected` | warn | `edge_id` |
| `route_denied` | warn | `client_id`, `edge_id`, `target_id` |
| `edge_offline` | warn | `client_id`, `edge_id`, `target_id` |
| `route_capacity_exceeded` | warn | `client_id`, `edge_id`, `scope` |
| `route_ticket_timeout` | error | `client_id`, `edge_id`, `target_id` |
| `edge_data_failed` | error | `edge_id`, `target_id` |
| `relay_failed` | warn | `client_id`, `edge_id`, `target_id`, `code` |
| `reload_failed` | error | `mode`, `code`, `detail` |
| `reload_succeeded` | info | `mode` |

### 5.3 PKI

| 事件 | 级别 | 必要字段 |
| --- | --- | --- |
| `unix_accept_failed` | warn | `io_kind` |
| `unix_peer_rejected` | warn | `uid`, `gid` |
| `request_decode_failed` | warn | `transport`, `code` |
| `request_failed` | error | `transport`, `operation`, `code` |
| `remote_tls_rejected` | warn | `peer_ip`, `code` |
| `response_send_failed` | warn | `transport`, `io_kind` |

PKI 日志只记录 Token ID 也没有排障必要，因此 Token ID 一并不记录。

## 6. 实施任务

### 任务 1：同步上游并建立基线

1. 获取 `origin` 和 `upstream`。
2. 合并 `upstream/main`。
3. 解决冲突并确认本项目功能仍在。
4. 运行：

```bash
pnpm typecheck
pnpm vitest run apps/desktop/src/composables/__tests__/useWindowControls.spec.ts \
  apps/desktop/src/lib/__tests__/windowsInstallerTemplate.spec.ts
cargo test --locked --manifest-path crates/dbx-gateway/Cargo.toml --no-default-features --lib
```

5. 单独提交上游合并，不与功能修改混在一起。

### 任务 2：恢复 Win7 原生标题栏

修改文件：

- `src-tauri/src/lib.rs`
- `src-tauri/src/commands/app_settings.rs`
- `apps/desktop/src/lib/backend/tauri.ts`
- `apps/desktop/src/lib/backend/http.ts`
- `apps/desktop/src/lib/backend/api.ts`
- `apps/desktop/src/composables/useWindowControls.ts`
- `apps/desktop/src/composables/__tests__/useWindowControls.spec.ts`
- `scripts/build-windows-portable.ps1`
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `apps/desktop/src/lib/__tests__/windowsInstallerTemplate.spec.ts`

TDD 顺序：

1. 增加失败测试：native decorations 时 `shouldShowWindowControls` 返回 `false`。
2. 增加失败测试：legacy 本地、CI 和 release 构建参数都包含 `legacy-native-window-decorations`。
3. 运行测试确认失败原因是接线缺失。
4. 恢复 Rust feature 判断和 Tauri command。
5. 恢复前端查询与自绘按钮隐藏。
6. 修改三个 legacy 构建入口；modern 参数不变。
7. 运行测试、typecheck 和两套 Windows 编译。

验收：

- Win7/Win8：系统标题栏可见，拖动、最小化、最大化和关闭正常；应用内不重复显示窗口按钮。
- Modern Windows：继续使用自绘标题栏，视觉和交互不变。
- legacy updater 仍使用 `/dbx/legacy/latest.json`。

### 任务 3：增加 Gateway stderr 诊断

修改文件：

- `crates/dbx-gateway/src/error.rs`
- `crates/dbx-gateway/src/lib.rs`
- `crates/dbx-gateway/src/main_gateway.rs`
- `crates/dbx-gateway/src/edge_gateway.rs`
- `crates/dbx-gateway/src/pki/service.rs`
- `crates/dbx-gateway/tests/gateway_flow.rs`
- `crates/dbx-gateway/tests/enrollment.rs`

TDD 顺序：

1. 为单行 JSON 格式增加测试，验证换行和引号被转义。
2. 为 `TargetUnavailable` 场景增加进程级测试，捕获 Edge stderr 并断言包含 `target_connect_failed`、`edge_id` 和 `target_id`。
3. 断言日志不包含测试目标地址和测试 payload。
4. 为配置 reload 失败增加进程级测试，断言 stderr 包含 `reload_failed`。
5. 实现最小日志格式化入口。
6. 在事件清单所列失败边界调用该入口。
7. 不改变客户端协议错误码和现有重试行为。

验收命令必须在 Linux 或 macOS 运行：

```bash
cargo test --locked -p dbx-gateway --all-targets
```

Windows 无法覆盖 Gateway server 的 Unix Socket 模块，不能作为完整 server 验收环境。

### 任务 4：发布验证

Windows：

```powershell
pwsh -ExecutionPolicy Bypass -File scripts/build-windows-release.ps1 -BuildSet All
```

检查：四份便携包、modern/legacy 安装包、两个 `.sig` 和两个 `latest.json`。

Gateway Linux：

```bash
cargo zigbuild --release -p dbx-gateway --target x86_64-unknown-linux-musl
DBX_GATEWAY_TARGET=x86_64-unknown-linux-musl scripts/package-gateway.sh
scripts/verify-gateway-package.sh dist-gateway/DBX_Gateway_*.tar.gz
```

部署后检查：

```bash
journalctl -u dbx-gateway-main.service -n 100 --no-pager
journalctl -u dbx-gateway-edge.service -n 100 --no-pager
journalctl -u dbx-gateway-pki.service -n 100 --no-pager
```

## 7. 回归风险

- 原生标题栏与自绘按钮同时存在：由前端 command 查询和测试防止。
- legacy 构建未启用 feature：由本地脚本、CI、release 三处静态测试防止。
- 日志泄露敏感信息：由 JSON 格式测试和进程级“不包含”断言防止。
- 自动重连导致日志刷屏：仅每次失败/断线记录一条，并包含下一次退避秒数；不记录心跳。
- 修改错误处理后改变现有协议：日志作为旁路行为，原返回值和错误码保持不变。
- 上游再次覆盖本项目行为：构建参数和标题栏行为进入现有 Windows 模板测试。

## 8. 完成定义

只有同时满足以下条件才算完成：

1. 最新上游已合并，且本项目三个本地提交保留。
2. Win7/Win8 实机显示原生标题栏，没有双窗口按钮。
3. modern Windows 标题栏行为无回归。
4. Gateway 关键失败可通过 `journalctl` 明确区分故障阶段。
5. Gateway 日志不含本报告禁止的敏感数据。
6. Windows 全套发布产物生成并通过现有审计。
7. Gateway 全部测试在 Linux 或 macOS 通过。
8. Linux musl Gateway 包生成并通过 `verify-gateway-package.sh`。

## 9. 建议提交边界

```text
Merge remote-tracking branch 'upstream/main'
fix(windows): restore native decorations for legacy builds
feat(gateway): log operational failures to stderr
```

三类修改分开提交，便于单独回退和审查。
