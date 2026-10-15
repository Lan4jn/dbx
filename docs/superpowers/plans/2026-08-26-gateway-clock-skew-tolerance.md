# DBX Gateway 10 分钟时钟偏差容忍实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 允许 Edge 与 Main 的墙上时钟相差最多 10 分钟，同时保持 Main 的一次性 Session Ticket 仅有 15 秒有效期。

**架构：** 只放宽 Edge 在发起数据通道前的本地预检查。Main 的 `SessionTickets` 继续使用自身单调时钟执行 15 秒 TTL、绑定校验和一次性消费，因此安全授权窗口不变。

**技术栈：** Rust、Tokio、现有 `dbx-gateway` 单元测试。

**设计规格：** `docs/superpowers/specs/2026-08-26-gateway-clock-skew-tolerance-design.md`

---

## 文件结构

- 修改：`crates/dbx-gateway/src/edge_gateway.rs`
  - 定义固定的 10 分钟毫秒容忍值。
  - 提供纯函数完成无溢出的边界判断。
  - 在 `run_data_channel` 中复用该判断。
  - 在现有 `#[cfg(test)] mod tests` 中增加一个边界测试。

不创建新模块，不修改 Main、协议、配置或文档。此行为只有一个调用点和一个测试点，拆分文件会增加无收益的抽象。

### 任务 1：用失败测试锁定 10 分钟边界

**文件：**
- 修改：`crates/dbx-gateway/src/edge_gateway.rs:682-720`
- 测试：`crates/dbx-gateway/src/edge_gateway.rs` 内现有 `tests` 模块

- [ ] **步骤 1：扩展测试模块导入**

将：

```rust
use super::{control_websocket_config, verify_server_pin};
```

改为：

```rust
use super::{control_websocket_config, ticket_expired_for_edge, verify_server_pin};
```

此时生产函数尚不存在，测试编译应失败。

- [ ] **步骤 2：添加边界测试**

在现有 `tests` 模块中加入：

```rust
#[test]
fn edge_ticket_expiration_allows_ten_minutes_of_positive_clock_skew() {
    let expires_at = 1_000_000_i64;

    assert!(!ticket_expired_for_edge(expires_at, expires_at - 1));
    assert!(!ticket_expired_for_edge(expires_at, expires_at + 10 * 60 * 1_000));
    assert!(ticket_expired_for_edge(expires_at, expires_at + 10 * 60 * 1_000 + 1));
    assert!(!ticket_expired_for_edge(i64::MAX - 1, i64::MAX));
}
```

- [ ] **步骤 3：运行测试并确认红灯**

运行：

```bash
cargo test --locked -p dbx-gateway --lib \
  edge_gateway::tests::edge_ticket_expiration_allows_ten_minutes_of_positive_clock_skew -- --exact
```

预期：编译失败，错误明确指出 `ticket_expired_for_edge` 尚未定义。若测试通过，停止实施并检查是否已有等价实现。

### 任务 2：实现最小时钟偏差判断

**文件：**
- 修改：`crates/dbx-gateway/src/edge_gateway.rs:36-39`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs:586-597`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs:667-672`

- [ ] **步骤 1：定义固定容忍值**

在现有连接超时常量附近加入：

```rust
const MAX_CLOCK_SKEW_MILLIS: i64 = 10 * 60 * 1_000;
```

不要将其加入配置结构；用户要求的是统一行为，不需要每台 Edge 单独调参。

- [ ] **步骤 2：增加纯判断函数**

在 `unix_time_ms` 附近加入：

```rust
fn ticket_expired_for_edge(expires_at_unix_ms: i64, now_unix_ms: i64) -> bool {
    now_unix_ms > expires_at_unix_ms.saturating_add(MAX_CLOCK_SKEW_MILLIS)
}
```

严格使用 `>`，确保恰好 10 分钟仍被允许。严格使用 `saturating_add`，确保异常的最大时间戳不会溢出。

- [ ] **步骤 3：替换 Edge 的严格比较**

将：

```rust
if unix_time_ms() > expires_at_unix_ms {
    return Some(failed());
}
```

改为：

```rust
if ticket_expired_for_edge(expires_at_unix_ms, unix_time_ms()) {
    return Some(failed());
}
```

不要修改 `run_data_channel` 的其他目标解析、TCP/Unix 连接或失败映射逻辑。

- [ ] **步骤 4：运行边界测试并确认绿灯**

运行：

```bash
cargo test --locked -p dbx-gateway --lib \
  edge_gateway::tests::edge_ticket_expiration_allows_ten_minutes_of_positive_clock_skew -- --exact
```

预期：`1 passed; 0 failed`。

- [ ] **步骤 5：提交最小实现**

```bash
git add crates/dbx-gateway/src/edge_gateway.rs
git commit -m "fix(gateway): tolerate ten minutes of edge clock skew"
```

### 任务 3：验证 Main 安全窗口未变化

**文件：**
- 验证但不修改：`crates/dbx-gateway/src/main_gateway.rs`
- 验证但不修改：`crates/dbx-gateway/src/protocol.rs`

- [ ] **步骤 1：确认 Main 仍签发 15 秒 Ticket**

运行：

```bash
rg -n 'SessionTickets::new\(Duration::from_secs\(15\)\)' crates/dbx-gateway/src/main_gateway.rs
```

预期：只匹配 Main 初始化 Ticket Store 的现有代码。若没有匹配，停止并重新审查上游变更，不要自行猜测新的 TTL 位置。

- [ ] **步骤 2：运行 Main Ticket 到期测试**

运行：

```bash
cargo test --locked -p dbx-gateway --lib protocol::tests::session_ticket_expires -- --exact
```

预期：`1 passed; 0 failed`。该测试证明 Main 仍拒绝过期 Ticket；不要为了适配 Edge 时钟偏差修改此测试。

- [ ] **步骤 3：运行 Gateway 库测试**

在 Linux 或 macOS 执行：

```bash
cargo test --locked -p dbx-gateway --lib
```

预期：全部通过。Windows 当前无法编译 Gateway `server` feature 中的 Unix Socket 类型，因此不能作为此项完整验证环境。

- [ ] **步骤 4：检查格式和差异范围**

运行：

```bash
cargo fmt -p dbx-gateway -- --check
git diff --check
git diff --stat HEAD^..HEAD
git diff HEAD^..HEAD -- crates/dbx-gateway/src/edge_gateway.rs
```

预期：格式和 whitespace 检查通过；实现提交只修改 `edge_gateway.rs`，且没有改变 `SessionTickets`、协议或配置。

### 任务 4：构建并部署 Gateway Server

**文件：**
- 使用现有脚本：`scripts/package-gateway.sh`
- 使用现有校验：`scripts/verify-gateway-package.sh`

- [ ] **步骤 1：在 Linux 构建发布包**

```bash
bash scripts/package-gateway.sh
```

预期：脚本输出 Gateway Linux 发布包路径且退出码为 0。

- [ ] **步骤 2：验证发布包**

将上一步输出的 tar.gz 路径赋给 `PACKAGE` 后运行：

```bash
bash scripts/verify-gateway-package.sh "$PACKAGE"
```

预期：输出 `gateway package verification passed`。

- [ ] **步骤 3：统一部署 Main 和 Edge 二进制**

在维护窗口内将同一个发布包中的 `dbx-gateway` 安装到 Main 和所有 Edge，随后执行：

```bash
sudo systemctl restart dbx-gateway-main.service
sudo systemctl restart dbx-gateway-edge.service
```

实际部署时分别在对应主机执行存在的 unit；不要在 Main 主机启动 Edge unit，也不要在 Edge 主机启动 Main unit。

- [ ] **步骤 4：确认运行中二进制一致**

Main 主机：

```bash
pid=$(systemctl show dbx-gateway-main -p MainPID --value)
sha256sum /proc/$pid/exe
```

Edge 主机：

```bash
pid=$(systemctl show dbx-gateway-edge -p MainPID --value)
sha256sum /proc/$pid/exe
```

预期：两个 SHA-256 相同。

- [ ] **步骤 5：执行时钟偏差验收**

保持 Main 正常时间。测试环境中的 Edge 时钟比 Main 快但不超过 10 分钟时，使用 DBX 建立 Gateway 数据库连接。

预期：不再因 Edge 本地 Ticket 预检查得到 `TargetUnavailable`。Edge 时钟偏差超过 10 分钟时，路由仍失败。完成测试后立即恢复 NTP，禁止将生产主机长期维持在人为错误时间。

## 实施完成检查

- [ ] 失败测试在生产代码修改前按预期失败。
- [ ] 10 分钟边界测试通过。
- [ ] Main Ticket 到期测试通过。
- [ ] Gateway 库测试在 Linux 或 macOS 通过。
- [ ] Main 的 15 秒 TTL、一次性消费和绑定校验未修改。
- [ ] 没有新增配置项、协议字段、依赖或额外模块。
- [ ] Main 与 Edge 部署同一二进制，运行中 SHA-256 一致。
