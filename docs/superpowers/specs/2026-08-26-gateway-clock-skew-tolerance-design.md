# DBX Gateway 时钟偏差容忍设计

## 背景

Main 为每次数据库路由签发一次性 Session Ticket。Ticket 在 Main 侧保持 15 秒有效期，并绑定客户端、Edge、目标和 Session ID。Edge 收到 `OpenDataChannel` 后，会先使用本机墙上时钟比较 `expires_at_unix_ms`，再连接目标并建立数据 WebSocket。

当前 Edge 使用严格比较：只要 Edge 本机时间晚于 Main 写入的到期时间，就立即返回 `DataChannelFailed`。当 Edge 时钟比 Main 快超过约 15 秒时，即使目标数据库、Main 控制通道和证书均正常，客户端也会收到 `TargetUnavailable`。

## 目标

- Main 与 Edge 的墙上时钟偏差在 10 分钟以内时，Edge 不因本地预检查误拒绝路由。
- Main 继续按自身单调时钟执行 15 秒 Ticket 有效期。
- Ticket 仍只能消费一次，且继续绑定原客户端、Edge 和目标。
- 超过 10 分钟的正向时钟偏差仍由 Edge 提前拒绝。

## 非目标

- 不把 Main 的 Ticket TTL 改为 10 分钟。
- 不改变 Gateway 协议、序列化字段或错误码。
- 不新增配置项、环境变量或命令行参数。
- 不以此替代 NTP；生产主机仍应保持时间同步。
- 不处理 Edge 时钟落后导致的本地提前接受；Main 始终是最终授权者。

## 方案

在 `crates/dbx-gateway/src/edge_gateway.rs` 中增加固定的 10 分钟时钟偏差容忍值，并将当前比较提取为一个纯函数：

```rust
const MAX_CLOCK_SKEW_MILLIS: i64 = 10 * 60 * 1_000;

fn ticket_expired_for_edge(expires_at_unix_ms: i64, now_unix_ms: i64) -> bool {
    now_unix_ms > expires_at_unix_ms.saturating_add(MAX_CLOCK_SKEW_MILLIS)
}
```

`run_data_channel` 使用该函数：

```rust
if ticket_expired_for_edge(expires_at_unix_ms, unix_time_ms()) {
    return Some(failed());
}
```

使用 `saturating_add` 避免异常时间戳在加容忍值时发生整数溢出。

## 行为边界

| Edge 当前时间 | Edge 本地预检查 | Main 最终检查 |
| --- | --- | --- |
| 不晚于 Ticket 到期时间 | 允许尝试 | 仍按 15 秒 TTL 判断 |
| 晚于到期时间，差值不超过 10 分钟 | 允许尝试 | 仍按 15 秒 TTL 判断 |
| 恰好晚 10 分钟 | 允许尝试 | 仍按 15 秒 TTL 判断 |
| 晚于到期时间超过 10 分钟 | 返回 `DataChannelFailed` | 不建立数据通道 |

## 安全边界

Main 的 `SessionTickets::consume` 继续使用 Main 本机的 `tokio::time::Instant` 检查 15 秒 TTL，并在成功消费后删除 Ticket。因此 Edge 的 10 分钟容忍只影响是否发起数据连接，不会扩大 Main 接受 Ticket 的时间窗口，也不会允许 Ticket 重复使用。

不应修改以下代码：

- `MainGateway::bind` 中 `SessionTickets::new(Duration::from_secs(15))`。
- `SessionTickets::consume` 的到期和绑定校验。
- `MainToEdge::OpenDataChannel` 协议字段。

## 测试

在 `edge_gateway.rs` 现有单元测试模块中增加一个边界测试，直接测试纯函数：

- 到期时间之前：未过期。
- 到期后恰好 10 分钟：未过期。
- 到期后 10 分钟加 1 毫秒：过期。
- 接近 `i64::MAX` 的到期时间：不溢出且不误判。

保留并运行 `protocol.rs` 中现有 `session_ticket_expires` 测试，以证明 Main 的 15 秒/到期语义未被改动。

## 验收标准

1. 新边界测试在修改生产代码前失败，修改后通过。
2. `cargo test --locked -p dbx-gateway --lib ticket_expired_for_edge` 通过。
3. `cargo test --locked -p dbx-gateway --lib session_ticket_expires` 通过。
4. `cargo fmt -p dbx-gateway -- --check` 通过。
5. diff 中只包含 `edge_gateway.rs` 的最小实现和测试；不包含协议、配置或 Main TTL 变更。

## 部署说明

该行为位于 Edge 二进制中。发布后必须至少替换并重启所有 Edge 服务；为避免二进制漂移，建议 Main 和 Edge 一并部署同一构建产物。无需修改 TOML 配置。
