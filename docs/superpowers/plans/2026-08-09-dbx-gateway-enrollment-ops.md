# DBX Gateway 自动领证与部署运维实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 在可用的 Gateway 核心上增加 Edge 自动领证、在线受限 PKI、证书续期与吊销、固定 HTTPS 回退、安全限制、Linux 发布包和完整中文部署运维文档。

**架构：** PKI 使用 SQLite 原子保存一次性令牌哈希、签发记录和吊销状态，通过本机 Unix Socket 或远程 mTLS 接收 Main 转交的 CSR。Main 在同一 TLS 端口严格区分保留路径与普通 HTTPS，普通请求只转发到单个固定上游。运行时配置采用先验证后原子替换，证书吊销会主动关闭对应会话。

**技术栈：** Rust、SQLite/rusqlite、Axum、hyper-util、rustls、Tokio、Notify/SIGHUP、GitHub Actions、Bash、systemd、Markdown。

---

## 前置条件

先完成 [DBX Gateway 核心与离线 PKI 实现计划](./2026-08-09-dbx-gateway-core.md)，并确认：

```bash
cargo test -p dbx-gateway --all-targets
```

全部通过。实现时持续对照 `docs/superpowers/specs/2026-08-09-dbx-gateway-design.md`。

## 文件结构

- 创建：`crates/dbx-gateway/src/state.rs`，管理 SQLite schema、令牌、路由快照和签发状态。
- 创建：`crates/dbx-gateway/src/enrollment.rs`，实现注册令牌与 CSR 签发流程。
- 创建：`crates/dbx-gateway/src/pki/service.rs`，实现 Unix Socket 与远程 mTLS PKI 服务。
- 创建：`crates/dbx-gateway/src/reverse_proxy.rs`，实现固定上游 HTTP/1.1、HTTP/2、SSE 与 WebSocket 转发。
- 创建：`crates/dbx-gateway/src/limits.rs`，实现速率、并发、帧、缓冲和总内存限制。
- 创建：`crates/dbx-gateway/src/health.rs`，实现仅管理监听地址可见的健康检查。
- 修改：`crates/dbx-gateway/src/main_gateway.rs`，接入注册、回退、ACL、持久状态与重载。
- 修改：`crates/dbx-gateway/src/edge_gateway.rs`，接入首次领证、续期和安全恢复。
- 修改：`crates/dbx-gateway/src/tls.rs`，接入 CRL、证书轮换和活动会话关闭。
- 修改：`crates/dbx-gateway/src/config.rs`，补齐 PKI、回退、ACL、限制、健康检查与热重载配置。
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`，实现 `serve` 与 enrollment 命令。
- 创建：`crates/dbx-gateway/tests/enrollment.rs`，覆盖令牌、CSR、续期和吊销。
- 创建：`crates/dbx-gateway/tests/reverse_proxy.rs`，覆盖普通 HTTPS 回退与保留路径隔离。
- 创建：`crates/dbx-gateway/tests/operations.rs`，覆盖重载、限制、健康检查和日志。
- 创建：`scripts/package-gateway.sh`，组装双架构 Gateway 发布包。
- 创建：`scripts/verify-gateway-package.sh`，在干净 Linux 环境验证包内容和帮助命令。
- 修改：`.github/workflows/release.yml`，构建并上传 Gateway x86_64/aarch64 产物。
- 创建：`examples/dbx-gateway/main.toml`、`edge.toml`、`pki.toml`，提供一致的示例配置。
- 创建：`examples/dbx-gateway/systemd/dbx-gateway-main.service`、`dbx-gateway-edge.service`、`dbx-gateway-pki.service`。
- 创建：`docs/dbx-gateway.md`，提供总览、拓扑和文档入口。
- 创建：`docs/dbx-gateway/main-gateway.md`、`edge-gateway.md`、`pki.md`、`configuration.md`、`operations.md`。

### 任务 1：建立持久状态与一次性注册令牌

**文件：**
- 创建：`crates/dbx-gateway/src/state.rs`
- 创建：`crates/dbx-gateway/src/enrollment.rs`
- 创建：`crates/dbx-gateway/tests/enrollment.rs`
- 修改：`crates/dbx-gateway/Cargo.toml`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`

- [ ] **步骤 1：编写令牌原子性测试**

覆盖：数据库只保存 Argon2id 哈希；默认 10 分钟过期；Edge ID 绑定；并发 20 次消费只有 1 次成功；撤销和过期失败；明文令牌只由 create 命令返回一次；同一 Edge 已有证书时必须使用 `--replace`。

```rust
#[tokio::test]
async fn enrollment_token_is_consumed_once() {
    let state = TestState::new().await;
    let token = state.enrollments.create("edge-prod-01", Duration::from_secs(600), false).await.unwrap();
    let results = futures::future::join_all((0..20).map(|_| {
        let state = state.clone();
        let token = token.secret.clone();
        async move { state.enrollments.consume("edge-prod-01", &token).await }
    })).await;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
}
```

- [ ] **步骤 2：运行测试并确认失败**

运行：`cargo test -p dbx-gateway --test enrollment token_ -- --nocapture`

预期：FAIL，state/enrollment 模块尚不存在。

- [ ] **步骤 3：实现 SQLite schema 与令牌命令**

新增 `rusqlite` bundled 依赖。单事务 schema 包含 `enrollments`、`issued_certificates`、`revocations`、`edge_routes` 四张表。令牌表只存 `token_id`、Argon2id hash、Edge ID、创建/过期/消费时间和 replace 标记。

```rust
pub struct EnrollmentToken {
    pub id: uuid::Uuid,
    pub edge_id: String,
    pub secret: zeroize::Zeroizing<String>,
    pub expires_at: time::OffsetDateTime,
}

pub async fn consume_token(
    &self,
    claimed_edge_id: &str,
    secret: &str,
) -> Result<ConsumedEnrollment, GatewayError>;
```

使用 `BEGIN IMMEDIATE` 完成读取、哈希校验和消费更新时间，确保并发只有一个成功。`--replace` 先标记该 Edge 现有活动证书为 revoked，并生成新 CRL；CLI 要求 TTY 确认，或显式 `--yes`。

- [ ] **步骤 4：运行令牌测试**

运行：`cargo test -p dbx-gateway --test enrollment token_ -- --nocapture`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/state.rs crates/dbx-gateway/src/enrollment.rs crates/dbx-gateway/src/bin/dbx-gateway-pki.rs crates/dbx-gateway/tests/enrollment.rs crates/dbx-gateway/Cargo.toml Cargo.lock
git commit -m "feat(gateway): add one-time edge enrollment tokens"
```

### 任务 2：实现在线受限 PKI 服务

**文件：**
- 创建：`crates/dbx-gateway/src/pki/service.rs`
- 修改：`crates/dbx-gateway/src/pki/mod.rs`
- 修改：`crates/dbx-gateway/src/pki/issue.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway-pki.rs`
- 测试：`crates/dbx-gateway/tests/enrollment.rs`

- [ ] **步骤 1：编写 PKI 服务权限测试**

测试 Unix Socket 只有配置的 Main uid/gid 可访问；远程监听必须同时配置 TLS Server 证书、Main RA CA 与允许的 RA URI SAN；在线服务只能签 Edge clientAuth，尝试签 Server 或 DBX Client 必须返回 `route_denied`。

- [ ] **步骤 2：运行服务测试并确认失败**

运行：`cargo test -p dbx-gateway --test enrollment pki_service_ -- --nocapture`

预期：FAIL，PKI serve 尚不存在。

- [ ] **步骤 3：实现 Unix Socket 与远程 mTLS API**

只暴露一个签发操作：

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollCsrRequest {
    pub token: Zeroizing<String>,
    pub claimed_edge_id: String,
    #[serde(with = "base64_bytes")]
    pub csr_der: Vec<u8>,
}

#[derive(Serialize)]
pub struct EnrollCsrResponse {
    pub edge_id: String,
    pub serial_hex: String,
    pub certificate_pem: String,
    pub chain_pem: String,
}
```

PKI 原子消费令牌，从令牌记录读取权威 Edge ID，比较 claimed ID，验证 CSR 签名，忽略 CSR CN/SAN，再签固定 URI SAN。Unix Socket 权限固定 `0660`；远程 API 只接受 RA CA 签发且 URI SAN 位于 allowlist 的 Main。

- [ ] **步骤 4：运行服务测试和 help 检查**

运行：`cargo test -p dbx-gateway --test enrollment pki_service_ -- --nocapture`

预期：PASS。

运行：`cargo run -p dbx-gateway --bin dbx-gateway-pki -- serve --help`

预期：显示 `--config`，不提供签发 Server/Client 的网络 API。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/pki crates/dbx-gateway/src/bin/dbx-gateway-pki.rs crates/dbx-gateway/tests/enrollment.rs
git commit -m "feat(gateway): serve restricted edge PKI"
```

### 任务 3：实现 Main 代理注册与 Edge 首次领证

**文件：**
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs`
- 修改：`crates/dbx-gateway/src/config.rs`
- 测试：`crates/dbx-gateway/tests/enrollment.rs`

- [ ] **步骤 1：编写自动领证全流程测试**

测试 Edge 本地生成私钥，使用 Main pin 和令牌访问注册路径；Main 拒绝不在允许列表的 Edge ID；PKI 签发后 Edge 原子保存证书并用 mTLS 重连；Main 与 PKI 都没有 Edge 私钥；响应中断后原令牌不能重放，`--replace` 可恢复并吊销孤儿证书。

- [ ] **步骤 2：运行自动领证测试并确认失败**

运行：`cargo test -p dbx-gateway --test enrollment bootstrap_ -- --nocapture`

预期：FAIL，Main 注册路径和 Edge bootstrap 尚不存在。

- [ ] **步骤 3：实现注册代理和原子证书落盘**

Main 注册路径只允许服务器单向 TLS，请求体上限 256 KiB，并按来源 IP 与 Edge ID 限速。Main 检查 `allowed_edge_ids` 后，经 Unix Socket 或 RA mTLS 转交 PKI。

Edge 启动状态机固定为：

```rust
pub enum EdgeCredentialState {
    Enrolled { certificate: PathBuf, private_key: PathBuf },
    Bootstrap { token_file: PathBuf },
    Unavailable { reason: String },
}
```

私钥先写同目录临时文件、`fsync`、权限设为 `0600`，证书响应验证通过后 rename。注册成功后删除 token 文件并 fsync 父目录；删除失败时退出，不进入长期运行。

- [ ] **步骤 4：运行自动领证测试**

运行：`cargo test -p dbx-gateway --test enrollment bootstrap_ -- --nocapture`

预期：PASS，测试断言 Main/PKI 临时目录中不存在 Edge 私钥。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/edge_gateway.rs crates/dbx-gateway/src/config.rs crates/dbx-gateway/tests/enrollment.rs
git commit -m "feat(gateway): enroll edge certificates through main"
```

### 任务 4：实现续期、吊销和安全热重载

**文件：**
- 修改：`crates/dbx-gateway/src/enrollment.rs`
- 修改：`crates/dbx-gateway/src/tls.rs`
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs`
- 修改：`crates/dbx-gateway/src/config.rs`
- 测试：`crates/dbx-gateway/tests/enrollment.rs`
- 创建：`crates/dbx-gateway/tests/operations.rs`

- [ ] **步骤 1：编写生命周期测试**

覆盖：有效 Edge 证书可用新 CSR 续期同一 ID；过期/吊销证书必须使用新 token；CRL 重载后关闭对应控制与数据通道；错误 TOML/证书重载失败时继续旧配置；ACL 和目标删除后拒绝新会话并关闭已有会话。

- [ ] **步骤 2：运行生命周期测试并确认失败**

运行：`cargo test -p dbx-gateway --test enrollment renewal_ -- --nocapture`

运行：`cargo test -p dbx-gateway --test operations reload_ -- --nocapture`

预期：FAIL，续期与 SIGHUP 尚未实现。

- [ ] **步骤 3：实现先校验后替换的重载**

配置状态使用 `arc_swap::ArcSwap<RuntimeConfig>`。SIGHUP 只在完整解析、证书私钥匹配、CRL 签名、ACL 引用和目标策略全部通过后替换。监听地址、模式、数据目录变化返回 `restart_required` 并保留旧配置。

续期必须用当前 Edge mTLS 身份认证，PKI 比较 CSR 请求对应的 Edge ID；新证书原子写入后 Edge 建立新控制通道，成功后关闭旧通道。吊销通过 session registry 按证书序列号取消活动任务。

- [ ] **步骤 4：运行生命周期测试**

运行：`cargo test -p dbx-gateway --test enrollment renewal_ -- --nocapture`

运行：`cargo test -p dbx-gateway --test operations reload_ -- --nocapture`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src crates/dbx-gateway/tests/enrollment.rs crates/dbx-gateway/tests/operations.rs
git commit -m "feat(gateway): reload and revoke gateway identities"
```

### 任务 5：实现固定 HTTPS 回退

**文件：**
- 创建：`crates/dbx-gateway/src/reverse_proxy.rs`
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/config.rs`
- 创建：`crates/dbx-gateway/tests/reverse_proxy.rs`

- [ ] **步骤 1：编写代理行为测试**

启动固定测试上游，覆盖 HTTP/1.1、HTTP/2、流式 request/response body、SSE、WebSocket echo、Host/X-Forwarded-For/X-Forwarded-Proto。断言任何保留路径在匿名、错误证书或错误角色下都返回 `404`，并且上游请求计数保持 0。

- [ ] **步骤 2：运行代理测试并确认失败**

运行：`cargo test -p dbx-gateway --test reverse_proxy -- --nocapture`

预期：FAIL，普通路径当前返回 `404`。

- [ ] **步骤 3：实现单固定上游转发**

配置只接受一个绝对 `http://` 或 `https://` upstream URL，拒绝 username/password、fragment 和动态目标参数。代理逐跳头按 RFC 规则移除，保留流式 body；WebSocket 使用双向帧复制。请求分类顺序必须是：先匹配所有保留路径，再进入普通回退，任何认证失败都不能落到上游。

```rust
pub async fn fallback(
    State(proxy): State<Arc<FixedUpstreamProxy>>,
    request: Request<Body>,
) -> Result<Response<Body>, GatewayError>;
```

- [ ] **步骤 4：运行代理测试**

运行：`cargo test -p dbx-gateway --test reverse_proxy -- --nocapture`

预期：PASS，SSE 第一条事件在上游连接关闭前到达客户端，WebSocket echo 保留 binary/text 类型。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/reverse_proxy.rs crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/config.rs crates/dbx-gateway/tests/reverse_proxy.rs
git commit -m "feat(gateway): proxy ordinary HTTPS to fixed upstream"
```

### 任务 6：实现限制、健康检查和安全日志

**文件：**
- 创建：`crates/dbx-gateway/src/limits.rs`
- 创建：`crates/dbx-gateway/src/health.rs`
- 修改：`crates/dbx-gateway/src/main_gateway.rs`
- 修改：`crates/dbx-gateway/src/edge_gateway.rs`
- 修改：`crates/dbx-gateway/src/config.rs`
- 修改：`crates/dbx-gateway/src/bin/dbx-gateway.rs`
- 测试：`crates/dbx-gateway/tests/operations.rs`

- [ ] **步骤 1：编写资源限制测试**

覆盖每客户端并发、每 Edge 并发、连接速率、最大控制帧、最大数据帧、单连接缓冲、全局内存预算和空闲超时。目标策略测试 DNS 解析后的每一个 IP，拒绝未显式放行的非 loopback、link-local、组播、未指定地址和云元数据地址，并在连接前重新解析、重新校验，防止 DNS 重绑定。健康检查只绑定管理地址，报告进程、证书到期、PKI、在线 Edge，不主动登录数据库。

- [ ] **步骤 2：运行运维测试并确认失败**

运行：`cargo test -p dbx-gateway --test operations limits_ -- --nocapture`

预期：FAIL，limits/health 尚不存在。

- [ ] **步骤 3：实现 fail-closed 限制与结构化日志**

令牌桶按证书身份或来源 IP 建立；并发使用 owned semaphore permit，确保异常路径释放。全局缓冲预算在分配前获取 permit。`TargetPolicy::resolve_and_validate` 使用系统解析器取得全部候选 IP，对每个 IP 应用地址分类规则，连接时只使用通过校验的结果，不再次按域名隐式解析。JSON 日志字段固定为 `request_id`、`peer_role`、`peer_id`、`cert_serial`、`edge_id`、`target_id`、`stage`、`bytes_in`、`bytes_out`、`error_code`，不得包含帧内容、SQL、token、PEM 或密码。

- [ ] **步骤 4：运行限制测试和秘密扫描**

运行：`cargo test -p dbx-gateway --test operations -- --nocapture`

预期：PASS。

运行测试进程并搜索日志中的测试 token、`BEGIN PRIVATE KEY` 和 SQL 标记，预期均无匹配。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/src/limits.rs crates/dbx-gateway/src/health.rs crates/dbx-gateway/src/main_gateway.rs crates/dbx-gateway/src/edge_gateway.rs crates/dbx-gateway/src/config.rs crates/dbx-gateway/src/bin/dbx-gateway.rs crates/dbx-gateway/tests/operations.rs
git commit -m "feat(gateway): enforce runtime limits and health checks"
```

### 任务 7：添加示例配置、systemd 和 Linux 发布包

**文件：**
- 创建：`examples/dbx-gateway/main.toml`
- 创建：`examples/dbx-gateway/edge.toml`
- 创建：`examples/dbx-gateway/pki.toml`
- 创建：`examples/dbx-gateway/systemd/dbx-gateway-main.service`
- 创建：`examples/dbx-gateway/systemd/dbx-gateway-edge.service`
- 创建：`examples/dbx-gateway/systemd/dbx-gateway-pki.service`
- 创建：`scripts/package-gateway.sh`
- 创建：`scripts/verify-gateway-package.sh`
- 修改：`.github/workflows/release.yml`

- [ ] **步骤 1：编写打包脚本测试条件**

`verify-gateway-package.sh` 必须检查：两个 binary、3 个 TOML、3 个 unit、6 份文档、SHA-256 文件；运行两个 binary 的 `--help`、`--version` 和 `check-config`；检查 unit 不以 root 运行，并包含 `NoNewPrivileges=true`、`PrivateTmp=true`、`ProtectSystem=strict`、`LimitCORE=0` 和受限 `ReadWritePaths`。

- [ ] **步骤 2：运行验证脚本并确认失败**

运行：`bash scripts/verify-gateway-package.sh dist-gateway/missing.tar.gz`

预期：FAIL，提示缺少发布包。

- [ ] **步骤 3：实现双架构发布流程**

`package-gateway.sh` 接受 `DBX_GATEWAY_TARGET`，从 `target/<target>/release/` 复制两个 binary、examples 和 docs，生成 `DBX_Gateway_<version>_<x64|arm64>.tar.gz` 与 `.sha256`。release workflow 使用 `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl`，运行完整测试后构建并上传两个架构。

systemd unit 分别使用 `dbx-gateway` 和 `dbx-gateway-pki` 用户；PKI unit 只开放 `/var/lib/dbx-gateway-pki` 与 `/run/dbx-gateway/pki.sock`，Main/Edge 无权读取 CA 私钥。

- [ ] **步骤 4：本地验证发布包**

运行：`cargo build -p dbx-gateway --release`

预期：两个 binary 生成。

运行：`DBX_GATEWAY_TARGET=$(rustc -vV | sed -n 's/^host: //p') bash scripts/package-gateway.sh`

预期：生成 tar.gz 与 SHA-256。

运行：`bash scripts/verify-gateway-package.sh dist-gateway/DBX_Gateway_*.tar.gz`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add examples/dbx-gateway scripts/package-gateway.sh scripts/verify-gateway-package.sh .github/workflows/release.yml
git commit -m "build(gateway): package Linux gateway releases"
```

### 任务 8：编写并实机验证详细中文文档

**文件：**
- 创建：`docs/dbx-gateway.md`
- 创建：`docs/dbx-gateway/main-gateway.md`
- 创建：`docs/dbx-gateway/edge-gateway.md`
- 创建：`docs/dbx-gateway/pki.md`
- 创建：`docs/dbx-gateway/configuration.md`
- 创建：`docs/dbx-gateway/operations.md`
- 修改：`scripts/verify-gateway-package.sh`

- [ ] **步骤 1：先写文档验收清单**

在验证脚本中检查每份文档存在，并检查关键章节标题。要求总览包含信任边界；Main 包含安装、回退、ACL、systemd、升级回滚；Edge 包含令牌领证、loopback/Unix、重连、迁移；PKI 包含离线 Root、在线 Edge CA、续期吊销、备份恢复；配置参考列出每个 TOML 字段；运维包含抓包、到期监控、故障排查和卸载。

- [ ] **步骤 2：运行文档检查并确认失败**

运行：`bash scripts/verify-gateway-package.sh dist-gateway/DBX_Gateway_*.tar.gz`

预期：FAIL，提示缺少文档或章节。

- [ ] **步骤 3：按统一示例编写 6 份文档**

所有章节统一使用：

```text
域名：gateway.example.com
Main 端口：443
Edge ID：edge-prod-01
目标 ID：postgres-primary
配置目录：/etc/dbx-gateway
Gateway 数据目录：/var/lib/dbx-gateway
PKI 数据目录：/var/lib/dbx-gateway-pki
```

每条命令说明运行主机、运行用户、预期输出和失败恢复。明确写出 Main 能读取 SQL/认证交换/结果数据，外部网络只能看到 TLS 元数据；明确写出 Nginx 只能使用 TCP/TLS passthrough；说明公网可使用用户提供的受信 Server 证书，第一版不内置 ACME。文档包含从零部署、自动领证、DBX Client PKCS#12 生成、续期、吊销、`--replace` 恢复、升级、回滚、备份、恢复和卸载的完整命令。

- [ ] **步骤 4：在干净 Linux VM/容器执行文档路径**

按文档依次完成 Root/中间 CA、在线 PKI、Main、普通 HTTPS 回退、Edge 自动领证、本地 echo target、证书续期、吊销、重新注册、升级和回滚。记录执行命令与结果到测试日志，不把真实私钥和 token 提交到仓库。

运行：`bash scripts/verify-gateway-package.sh dist-gateway/DBX_Gateway_*.tar.gz`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add docs/dbx-gateway.md docs/dbx-gateway examples/dbx-gateway scripts/verify-gateway-package.sh
git commit -m "docs(gateway): add deployment and operations guides"
```

### 任务 9：完成生产阶段验证

**文件：**
- 修改：`crates/dbx-gateway/tests/enrollment.rs`
- 修改：`crates/dbx-gateway/tests/reverse_proxy.rs`
- 修改：`crates/dbx-gateway/tests/operations.rs`

- [ ] **步骤 1：运行全部 Gateway 验证**

运行：`cargo fmt --check --package dbx-gateway`

运行：`cargo clippy -p dbx-gateway --all-targets -- -D warnings`

运行：`cargo test -p dbx-gateway --all-targets`

预期：全部退出码 0。

重启 Main 后检查持久化的最后已知 Edge/route 仍可作为离线元数据读取，但在 Edge 重新注册前不能创建新会话；确认状态文件不包含目标真实地址。

- [ ] **步骤 2：执行外部链路抓包验收**

使用测试 SQL 标记和凭据标记经 DBX 测试客户端、Main、Edge、echo target 发送数据。在 DBX 到 Main 与 Edge 到 Main 两侧抓包，搜索标记必须无匹配；在 Main/Edge 受控测试 hook 中必须能看到标记，证明文档的信任边界准确。

- [ ] **步骤 3：执行安全失败验收**

依次测试错误 Main pin、错误 CA、错误 EKU、错误 URI SAN、过期证书、吊销证书、重放 token、篡改 CSR ID、未授权 route、远程目标未 opt-in、超限帧和连接洪泛。预期全部 fail closed，未认证保留路径只返回 TLS 失败或 `404`。

- [ ] **步骤 4：核对发布产物**

从 CI 下载 x64 与 arm64 包，在对应 Linux 主机运行 `--help`、`check-config` 和最小 Main/Edge 转发。校验 SHA-256 与 release asset 一致。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-gateway/tests
git commit -m "test(gateway): verify enrollment and production operations"
```

## 阶段完成标准

- Edge 可使用 10 分钟一次性令牌经 Main 向 PKI 领证，私钥从不离开 Edge。
- 在线 PKI 只能签 Edge clientAuth，Root CA 保持离线。
- 有效证书可续期，过期/吊销/丢失私钥必须重新发 token。
- CRL 或 ACL 重载会关闭受影响会话，错误重载保留旧配置。
- Main 同一端口可转发普通 HTTPS/HTTP2/SSE/WebSocket，保留路径从不回退。
- 限流、并发、帧、缓冲、内存与空闲超时全部可测试。
- Linux x86_64/aarch64 发布包包含二进制、示例、systemd、SHA-256 和 6 份中文文档。
- 新 Linux 环境无需阅读源码即可按文档完成部署、领证、续期、吊销、升级和恢复。
