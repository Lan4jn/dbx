# DBX Gateway 核心与离线 PKI 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 交付可在 Linux 上运行的 `dbx-gateway` 与 `dbx-gateway-pki`，通过手工签发证书完成 Main、Edge 注册、心跳和一个 WSS 对应一个数据库连接的数据转发。

**架构：** 新增一个 Workspace crate，同时生成 Gateway 与 PKI 两个二进制。Main 使用 rustls 终止 TLS，Edge 通过出站 WSS/mTLS 控制通道注册；每个数据库流由独立 Edge 数据通道配对到本地 TCP 或 Unix Socket。第一份计划使用离线签发证书，不包含自动领证、普通 HTTPS 回退和 DBX 桌面端接入。

**技术栈：** Rust、Tokio、Axum、hyper-util、rustls、tokio-rustls、tokio-tungstenite、Clap、Serde/TOML、rcgen、x509-parser、p12-keystore、tracing。

---

## 计划边界

本计划完成后，可以使用测试客户端或 `websocat` 风格的 Rust 集成测试经 Main、Edge 连接本地 echo server。生产自动领证、反向代理、热重载、发布包和详细部署文档由 [第 2 份计划](./2026-08-09-dbx-gateway-enrollment-ops.md) 完成；DBX 客户端配置与 UI 由 [第 3 份计划](./2026-08-09-dbx-gateway-client-integration.md) 完成。

实现前阅读：

- `docs/superpowers/specs/2026-08-09-dbx-gateway-design.md`
- `Cargo.toml`
- `crates/dbx-web/src/main.rs`
- `crates/dbx-core/src/backend_error.rs`

## 文件结构

- 修改：`Cargo.toml`，把新 crate 加入 Workspace。
- 创建：`crates/dbx-gateway/Cargo.toml`，声明两个 binary 与共享依赖。
- 创建：`crates/dbx-gateway/src/lib.rs`，导出配置、协议、PKI、TLS、Main 和 Edge 模块。
- 创建：`crates/dbx-gateway/src/error.rs`，定义稳定错误码与安全展示消息。
- 创建：`crates/dbx-gateway/src/config.rs`，解析和校验 Main、Edge、PKI TOML。
- 创建：`crates/dbx-gateway/src/protocol.rs`，定义版本化控制消息、阶段事件与会话 ID。
- 创建：`crates/dbx-gateway/src/tls.rs`，加载证书、校验角色、提取 URI SAN 身份。
- 创建：`crates/dbx-gateway/src/pki/mod.rs`，组织离线 PKI 命令。
- 创建：`crates/dbx-gateway/src/pki/store.rs`，管理 CA、证书记录和 CRL 文件。
- 创建：`crates/dbx-gateway/src/pki/issue.rs`，签发 Server、Edge、DBX Client 证书和 PKCS#12。
- 创建：`crates/dbx-gateway/src/main_gateway.rs`，实现 Main TLS 接入、Edge 注册表和会话配对。
- 创建：`crates/dbx-gateway/src/edge_gateway.rs`，实现 Edge 控制通道、重连和本地目标连接。
- 创建：`crates/dbx-gateway/src/stream.rs`，实现有背压的双向复制与关闭语义。
- 创建：`crates/dbx-gateway/src/bin/dbx-gateway.rs`，提供 `serve` 和 `check-config`。
- 创建：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`，提供离线 PKI CLI。
- 创建：`crates/dbx-gateway/tests/config.rs`，覆盖配置兼容和安全默认值。
- 创建：`crates/dbx-gateway/tests/pki.rs`，覆盖证书角色、SAN、PKCS#12 与吊销。
- 创建：`crates/dbx-gateway/tests/gateway_flow.rs`，覆盖 Main/Edge/echo server 全链路。

### 任务 1：建立 crate 与两个 CLI

**文件：**
- 修改：`Cargo.toml`
- 创建：`crates/dbx-gateway/Cargo.toml`
- 创建：`crates/dbx-gateway/src/lib.rs`
- 创建：`crates/dbx-gateway/src/error.rs`
- 创建：`crates/dbx-gateway/src/bin/dbx-gateway.rs`
- 创建：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`

- [ ] **步骤 1：编写 CLI 结构测试**

在两个 binary 文件的 `#[cfg(test)]` 模块中使用 `clap::CommandFactory`，断言 Gateway 包含 `serve`、`check-config`，PKI 包含 `init`、`server`、`client`、`edge`，并断言每个命令可生成长帮助文本。PKI 的 `enrollment` 与 `serve` 在第 2 份计划加入，第一阶段不创建空子命令。

```rust
#[test]
fn gateway_help_exposes_required_commands() {
    let command = Cli::command();
    let names = command.get_subcommands().map(|item| item.get_name()).collect::<Vec<_>>();
    assert_eq!(names, ["serve", "check-config"]);
    assert!(Cli::command().render_long_help().to_string().contains("--config"));
}
```

两个 CLI 还要分别断言 `Cli::command().get_version().is_some()`，保证 `--version` 始终可用。

- [ ] **步骤 2：运行测试并确认失败**

运行：`cargo test -p dbx-gateway --bins`

预期：FAIL，Cargo 报告 Workspace 中不存在 `dbx-gateway` package。

- [ ] **步骤 3：创建最小 CLI 与错误类型**

把 `crates/dbx-gateway` 加入 Workspace。两个 binary 只解析参数并调用共享库函数；不要在 binary 中实现协议逻辑。公共错误结构固定为：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayErrorCode {
    ConfigInvalid,
    TlsRejected,
    IdentityRejected,
    ProtocolMismatch,
    RouteDenied,
    EdgeOffline,
    TargetUnavailable,
    CapacityExceeded,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct GatewayError {
    pub code: GatewayErrorCode,
    pub message: String,
}
```

依赖使用仓库现有大版本，新增 `clap`、`toml`、`thiserror`、`tokio-rustls`、`tokio-tungstenite`、`hyper-util`、`rcgen`、`x509-parser`、`p12-keystore`、`time` 和 `zeroize`。禁用 OpenSSL 传输后端，统一使用 rustls。crate 默认启用 `server` feature；`error` 与 `protocol` 不受 feature 控制，Main、Edge、PKI 和 binary 依赖声明为 `server` feature。第 3 份计划可让 dbx-core 使用 `default-features = false` 复用协议类型，而不把服务端与 PKI 依赖带入桌面/浏览器二进制。

- [ ] **步骤 4：验证 CLI 测试通过**

运行：`cargo test -p dbx-gateway --bins`

预期：PASS，两个测试通过；`cargo run -p dbx-gateway --bin dbx-gateway -- --help` 显示两个子命令。

- [ ] **步骤 5：Commit**

```bash
git add Cargo.toml Cargo.lock crates/dbx-gateway
git commit -m "feat(gateway): add gateway and PKI CLIs"
```

### 任务 2：实现 TOML 配置和启动前检查

**文件：**
- 创建：`crates/dbx-gateway/src/config.rs`
- 创建：`crates/dbx-gateway/tests/config.rs`
- 修改：`crates/dbx-gateway/src/lib.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway.rs`

- [ ] **步骤 1：编写配置失败测试**

测试必须覆盖：未知字段被拒绝、Main 缺少 Server 证书失败、Edge 默认只允许 loopback/Unix Socket、非 loopback 未设置 `allow_remote = true` 时失败、相对路径以 TOML 所在目录解析、私钥权限不是 `0600` 时失败。

```rust
#[test]
fn remote_edge_target_requires_explicit_opt_in() {
    let error = load_edge_config(
        r#"mode = "edge"
edge_id = "edge-prod-01"
main_url = "wss://main.example.com/_dbx/control"
[targets.postgres]
address = "10.0.0.8:5432"
"#,
    )
    .unwrap_err();
    assert_eq!(error.code, GatewayErrorCode::ConfigInvalid);
    assert!(error.message.contains("allow_remote"));
}
```

- [ ] **步骤 2：运行配置测试并确认失败**

运行：`cargo test -p dbx-gateway --test config`

预期：FAIL，`config` 模块或 `load_edge_config` 尚不存在。

- [ ] **步骤 3：实现带安全默认值的配置类型**

使用 `#[serde(deny_unknown_fields)]` 和带 tag 的模式枚举。核心形状固定为：

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayConfig {
    Main(MainConfig),
    Edge(EdgeConfig),
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeTarget {
    pub display_name: String,
    pub address: TargetAddress,
    #[serde(default)]
    pub allow_remote: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum TargetAddress {
    Tcp { tcp: String },
    Unix { unix: std::path::PathBuf },
}
```

`check-config` 调用同一加载器和证书检查器，不启动长期任务。配置错误退出码为 2，运行时启动错误退出码为 1，成功为 0。配置检查不打印私钥、令牌或密码。

- [ ] **步骤 4：运行配置测试和格式检查**

运行：`cargo test -p dbx-gateway --test config`

预期：PASS。

运行：`cargo fmt --check --package dbx-gateway`

预期：退出码 0。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/config.rs crates/dbx-gateway/src/lib.rs crates/dbx-gateway/src/bin/dbx-gateway.rs crates/dbx-gateway/tests/config.rs
git commit -m "feat(gateway): validate external configuration"
```

### 任务 3：实现离线 PKI 与角色分离

**文件：**
- 创建：`crates/dbx-gateway/src/pki/mod.rs`
- 创建：`crates/dbx-gateway/src/pki/store.rs`
- 创建：`crates/dbx-gateway/src/pki/issue.rs`
- 创建：`crates/dbx-gateway/tests/pki.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`

- [ ] **步骤 1：编写证书角色测试**

在临时目录初始化 PKI，签发 1 张 Main、1 张 Edge、1 张 DBX Client 证书。使用 x509-parser 断言：Main 只有 `serverAuth`；Edge 与 Client 只有 `clientAuth`；Edge URI SAN 为 `urn:dbx-gateway:edge:edge-prod-01`；3 个角色由不同中间 CA 签发。再导入 Client PKCS#12，断言证书链和 PKCS#8 私钥可读。

```rust
assert_eq!(edge.uri_sans(), ["urn:dbx-gateway:edge:edge-prod-01"]);
assert_eq!(edge.extended_key_usage(), ExtendedUsage::ClientAuth);
assert_ne!(edge.issuer(), client.issuer());
assert!(load_pkcs12(&client_bundle, "bundle-password").is_ok());
```

- [ ] **步骤 2：运行 PKI 测试并确认失败**

运行：`cargo test -p dbx-gateway --test pki`

预期：FAIL，PKI API 尚不存在。

- [ ] **步骤 3：实现 PKI 文件布局与签发命令**

`init` 生成离线 Root、Server、Edge、DBX Client 3 个中间 CA。CA 私钥使用加密 PKCS#8 PEM；密码从交互输入或 systemd credential 文件读取，不接受命令行明文密码。证书签发 API 使用下列输入，CSR 身份字段不直接决定授权身份：

```rust
pub struct EdgeIssueRequest<'a> {
    pub edge_id: &'a str,
    pub csr_der: &'a [u8],
    pub validity: time::Duration,
}

pub struct IssuedCertificate {
    pub serial_hex: String,
    pub certificate_pem: String,
    pub chain_pem: String,
}
```

离线 `edge issue` 可在本机生成私钥和 CSR；在线注册后续只传 CSR。`client issue` 额外生成 AES-256/HMAC-SHA256 保护的 PKCS#12。`revoke` 更新角色对应的签发记录并重新生成签名 CRL；CRL number 单调递增。

- [ ] **步骤 4：运行 PKI 测试与 CLI 冒烟测试**

运行：`cargo test -p dbx-gateway --test pki`

预期：PASS。

运行：`cargo run -p dbx-gateway --bin dbx-gateway-pki -- edge --help`

预期：显示 `issue`、`renew`、`revoke`，且不显示任何私钥内容。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/pki crates/dbx-gateway/src/bin/dbx-gateway-pki.rs crates/dbx-gateway/tests/pki.rs Cargo.lock
git commit -m "feat(gateway): add role-separated offline PKI"
```

### 任务 4：定义版本化协议与防重放会话 ID

**文件：**
- 创建：`crates/dbx-gateway/src/protocol.rs`
- 修改：`crates/dbx-gateway/src/lib.rs`
- 测试：`crates/dbx-gateway/src/protocol.rs`

- [ ] **步骤 1：编写协议序列化和状态机测试**

覆盖主版本不兼容、未知次版本能力忽略、Edge 注册目标不包含真实地址、会话 ID 绑定 Edge/目标/DBX 身份、过期与重复消费失败。

```rust
#[test]
fn session_ticket_is_single_use_and_identity_bound() {
    let mut tickets = SessionTickets::new(Duration::from_secs(15));
    let ticket = tickets.issue("edge-1", "postgres", "client-1");
    assert!(tickets.consume(&ticket, "edge-1", "postgres", "client-1").is_ok());
    assert_eq!(tickets.consume(&ticket, "edge-1", "postgres", "client-1").unwrap_err().code,
        GatewayErrorCode::IdentityRejected);
}
```

- [ ] **步骤 2：运行协议测试并确认失败**

运行：`cargo test -p dbx-gateway protocol::tests`

预期：FAIL，协议类型尚不存在。

- [ ] **步骤 3：实现协议消息**

消息使用 JSON 控制帧和 WSS 二进制数据帧。公开类型固定包含：

```rust
pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;

pub enum ClientMessage {
    OpenRoute { version: ProtocolVersion, request_id: uuid::Uuid, edge_id: String, target_id: String },
}

pub enum MainToEdge {
    OpenDataChannel { session_id: SessionId, target_id: String, expires_at_unix_ms: i64 },
    HeartbeatAck { unix_ms: i64 },
}

pub enum Stage {
    MainAuthenticated,
    RouteAuthorized,
    EdgeChannelReady,
    TargetConnected,
    StreamReady,
}
```

所有反序列化入口先检查帧大小，再解析 JSON。未认证链路不返回结构化内部错误。

- [ ] **步骤 4：运行协议测试**

运行：`cargo test -p dbx-gateway protocol::tests`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/protocol.rs crates/dbx-gateway/src/lib.rs
git commit -m "feat(gateway): define versioned gateway protocol"
```

### 任务 5：实现 TLS 角色校验和 Main 接入层

**文件：**
- 创建：`crates/dbx-gateway/src/tls.rs`
- 创建：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/lib.rs`
- 测试：`crates/dbx-gateway/tests/gateway_flow.rs`

- [ ] **步骤 1：编写 TLS 身份测试**

启动随机端口 Main，验证：无证书可以完成 TLS 但保留路径返回 `404`；Edge CA 证书只能访问 Edge 路径；DBX Client CA 证书只能访问 DBX 路径；错误 CA、错误 EKU、过期证书和错误 URI SAN 在握手或身份分类阶段失败。

- [ ] **步骤 2：运行 TLS 测试并确认失败**

运行：`cargo test -p dbx-gateway --test gateway_flow tls_ -- --nocapture`

预期：FAIL，Main listener 尚不存在。

- [ ] **步骤 3：实现可选客户端认证 TLS 接入**

使用 `rustls::server::WebPkiClientVerifier::builder(...).allow_unauthenticated()`，禁用 TLS 1.2 和 0-RTT。自建 TCP accept loop，在 TLS 成功后提取 peer certificate，生成下列 request extension，再把连接交给 hyper-util 的 auto server：

```rust
#[derive(Clone)]
pub enum PeerIdentity {
    Anonymous,
    Edge { edge_id: String, serial: String, fingerprint_sha256: [u8; 32] },
    DbxClient { client_id: String, serial: String, fingerprint_sha256: [u8; 32] },
}
```

保留路径固定为配置值；匿名或角色不匹配请求统一返回无正文 `404`。第一份计划对普通路径也返回 `404`，第 2 份计划再接入固定上游。

- [ ] **步骤 4：运行 TLS 测试**

运行：`cargo test -p dbx-gateway --test gateway_flow tls_ -- --nocapture`

预期：PASS，日志不包含证书 PEM 或请求载荷。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/tls.rs crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/lib.rs crates/dbx-gateway/tests/gateway_flow.rs
git commit -m "feat(gateway): authenticate gateway TLS peers"
```

### 任务 6：实现 Edge 控制通道、注册表和心跳

**文件：**
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 创建：`crates/dbx-gateway/src/edge_gateway.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway.rs`
- 测试：`crates/dbx-gateway/tests/gateway_flow.rs`

- [ ] **步骤 1：编写控制通道测试**

测试 Edge 使用出站 WSS 注册两个逻辑目标；Main 注册表只保存 ID 与显示名称；15 秒心跳周期使用暂停 Tokio 时间测试；45 秒无心跳标记离线；重复在线 Edge ID 被拒绝；Main 重启后 Edge 指数退避并重新注册。

- [ ] **步骤 2：运行控制通道测试并确认失败**

运行：`cargo test -p dbx-gateway --test gateway_flow control_ -- --nocapture`

预期：FAIL，Edge runtime 尚不存在。

- [ ] **步骤 3：实现 Main/Edge 控制面**

Main 注册状态使用 `Arc<RwLock<HashMap<EdgeId, EdgeEntry>>>`，EdgeEntry 只包含逻辑元数据、控制发送端、心跳时间和连接计数。Edge 重连序列为 1、2、4、8、16、32、60 秒并加入 0% 到 20% 抖动，连接成功后重置。

```rust
pub struct RegisteredTarget {
    pub target_id: String,
    pub display_name: String,
}

pub struct EdgeEntry {
    pub identity: EdgeIdentity,
    pub targets: BTreeMap<String, RegisteredTarget>,
    pub last_heartbeat: Instant,
    pub control_tx: mpsc::Sender<MainToEdge>,
    pub active_streams: usize,
}
```

控制通道断开时立即把 Edge 标记离线，并通知数据面关闭该 Edge 的活动会话。

- [ ] **步骤 4：运行控制通道测试**

运行：`cargo test -p dbx-gateway --test gateway_flow control_ -- --nocapture`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/edge_gateway.rs crates/dbx-gateway/src/bin/dbx-gateway.rs crates/dbx-gateway/tests/gateway_flow.rs
git commit -m "feat(gateway): register and monitor edge gateways"
```

### 任务 7：实现单连接数据通道与本地目标

**文件：**
- 创建：`crates/dbx-gateway/src/stream.rs`
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs`
- 测试：`crates/dbx-gateway/tests/gateway_flow.rs`

- [ ] **步骤 1：编写完整数据流测试**

启动 TCP echo server 和 Unix Socket echo server。DBX 测试客户端请求 `edge-prod-01/postgres`，断言依次收到 5 个 Stage，发送随机二进制后原样返回。再覆盖未知路由、Edge 离线、本地连接拒绝、过期 session ID、重放 session ID、控制通道断开关闭数据通道。

- [ ] **步骤 2：运行数据流测试并确认失败**

运行：`cargo test -p dbx-gateway --test gateway_flow data_ -- --nocapture`

预期：FAIL，数据通道配对尚不存在。

- [ ] **步骤 3：实现配对与双向复制**

Main 为 DBX 请求签发 15 秒一次性 session ID，通过控制通道通知 Edge。Edge 建立新的 WSS/mTLS 数据通道，Main 原子消费 ticket 后配对。Edge 只有在本地目标连接成功后才发送 `TargetConnected`。

`stream.rs` 使用固定上限缓冲和关闭传播：

```rust
pub async fn copy_bidirectional_bounded<A, B>(
    left: A,
    right: B,
    idle_timeout: Duration,
) -> Result<StreamStats, GatewayError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin;
```

WSS binary frame 最大值、单方向缓冲和空闲超时来自配置。任一方向 EOF 后发送 close，等待短暂优雅关闭再取消另一方向；绝不重放已发送数据库字节。

- [ ] **步骤 4：运行完整数据流测试**

运行：`cargo test -p dbx-gateway --test gateway_flow data_ -- --nocapture`

预期：PASS，TCP 与 Unix Socket 两组往返通过。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/stream.rs crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/edge_gateway.rs crates/dbx-gateway/tests/gateway_flow.rs
git commit -m "feat(gateway): forward one database stream per WSS"
```

### 任务 8：完成核心阶段验证

**文件：**
- 修改：`crates/dbx-gateway/tests/gateway_flow.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway.rs`

- [ ] **步骤 1：补充进程级冒烟测试**

测试使用临时 PKI 和 TOML 启动两个真实子进程，等待 Main 与 Edge 健康日志，运行二进制测试客户端完成 1 MiB 随机字节往返，然后 SIGTERM 两个进程并断言干净退出。

- [ ] **步骤 2：运行进程测试并修复暴露的问题**

运行：`cargo test -p dbx-gateway --test gateway_flow process_smoke -- --nocapture`

预期：PASS；无后台任务泄漏和端口占用。

- [ ] **步骤 3：运行完整 crate 验证**

运行：`cargo fmt --check --package dbx-gateway`

预期：退出码 0。

运行：`cargo clippy -p dbx-gateway --all-targets -- -D warnings`

预期：退出码 0。

运行：`cargo test -p dbx-gateway --all-targets`

预期：全部测试通过。

- [ ] **步骤 4：检查安全输出**

以测试私钥、令牌和 SQL 标记字符串运行进程测试，再搜索捕获日志。预期：日志不包含 PEM 私钥头、PKCS#12 密码或数据帧标记，只包含身份 ID、逻辑路由、阶段、字节计数和稳定错误码。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway
git commit -m "test(gateway): verify core gateway data path"
```

## 阶段完成标准

- `dbx-gateway --help`、`dbx-gateway-pki --help` 和所有子命令帮助可用。
- `check-config` 能拒绝未知字段、错误证书、过宽私钥权限和未授权远程目标。
- 手工签发证书后，Edge 可通过出站 WSS/mTLS 注册 Main。
- Main 能配对 DBX 测试客户端和 Edge 的独立数据 WSS。
- TCP 与 Unix Socket 本地目标均可透明转发任意二进制。
- Main 与 Edge 都可以读取被转发字节，外部测试连接只承载 TLS/WSS。
- 当前阶段不包含自动领证、普通 HTTPS 回退、正式发布包和 DBX UI。
