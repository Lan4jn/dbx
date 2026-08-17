# DBX Gateway 客户端接入实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 为 DBX 桌面端增加独立 `dbx_gateway` 传输层、系统密钥库证书身份、Gateway 共享配置、授权路由选择和真实数据库连接转发。

**架构：** dbx-core 定义 Gateway 配置、WSS 客户端和可注入身份提供器；Tauri 使用跨平台 OS keyring 实现身份提供器并解析 PKCS#12，Web 端使用拒绝明文回退的 unavailable 实现。Gateway 是传输链的最后一层，可经前置 SSH/Proxy 到达 Main；共享配置保存 Main 与证书身份，数据库连接引用共享配置并单独保存 `edge_id/target_id`。

**技术栈：** Rust、dbx-core、Tauri、rustls、tokio-tungstenite、keyring、p12-keystore、Vue 3、Pinia、TypeScript、Vitest、Lucide。

---

## 前置条件

先完成：

- [DBX Gateway 核心与离线 PKI 实现计划](./2026-08-09-dbx-gateway-core.md)
- [DBX Gateway 自动领证与部署运维实现计划](./2026-08-09-dbx-gateway-enrollment-ops.md)

Main 必须提供 DBX Client mTLS、路由发现和数据路径。实现前阅读：

- `crates/dbx-core/src/models/connection.rs`
- `crates/dbx-core/src/db/transport_layer_tunnel.rs`
- `crates/dbx-core/src/connection.rs`
- `crates/dbx-core/src/connection_secrets.rs`
- `src-tauri/src/lib.rs`
- `src-tauri/src/commands/keychain.rs`
- `apps/desktop/src/types/database.ts`
- `apps/desktop/src/lib/connection/tunnelProfiles.ts`
- `apps/desktop/src/components/connection/TunnelProfileManager.vue`
- `apps/desktop/src/components/connection/ConnectionDialog.vue`

## 文件结构

- 修改：`crates/dbx-core/Cargo.toml`，加入 WSS 客户端依赖，并以关闭默认 feature 的方式复用 Gateway 协议类型。
- 修改：`crates/dbx-core/src/models/connection.rs`，增加 `DbxGatewayConfig` 与枚举分支。
- 创建：`crates/dbx-core/src/db/dbx_gateway.rs`，实现身份接口、路由发现和本地监听器。
- 修改：`crates/dbx-core/src/db/mod.rs`，导出 Gateway 模块。
- 修改：`crates/dbx-core/src/db/transport_layer_tunnel.rs`，把 Gateway 接入有序传输链。
- 修改：`crates/dbx-core/src/connection.rs`，持有 `DbxGatewayManager` 并提供默认 unavailable 身份提供器。
- 修改：`crates/dbx-core/src/connection_secrets.rs`、`storage.rs`、`cloud_sync.rs`，保证身份私钥不进入连接 JSON 或同步快照。
- 修改：`src-tauri/Cargo.toml`，加入 keyring 与 p12-keystore。
- 创建：`src-tauri/src/gateway_identity.rs`，实现 OS keyring 身份提供器。
- 创建：`src-tauri/src/commands/gateway.rs`，提供导入、列表、删除、路由发现和 profile 测试命令。
- 修改：`src-tauri/src/commands/mod.rs`、`src-tauri/src/lib.rs`，注册命令并注入身份提供器。
- 修改：`crates/dbx-core/src/storage.rs`，保存仅限本机的非秘密身份元数据。
- 修改：`crates/dbx-web/src/routes/tunnel_profiles.rs`，对 Gateway profile 测试返回明确的桌面端限定错误。
- 修改：`apps/desktop/src/types/database.ts`，增加 Gateway 类型、身份元数据和路由类型。
- 创建：`apps/desktop/src/lib/connection/gatewayProfiles.ts`，封装默认值、验证、路由分组和 profile/layer 合并。
- 修改：`apps/desktop/src/lib/connection/tunnelProfiles.ts`，接入 Gateway profile。
- 修改：`apps/desktop/src/lib/connection/connectionAttemptTimeout.ts`，计入 Gateway 超时。
- 修改：`apps/desktop/src/lib/backend/api.ts`、`tauri.ts`、`http.ts`，增加 Gateway API。
- 修改：`apps/desktop/src/stores/tunnelProfileStore.ts`，加载身份与授权路由。
- 修改：`apps/desktop/src/components/connection/TunnelProfileManager.vue`，增加 Gateway profile 与证书导入。
- 修改：`apps/desktop/src/components/connection/ConnectionDialog.vue`，增加 Gateway 传输层和路由选择。
- 修改：`apps/desktop/src/i18n/locales/en.ts`、`zh-CN.ts`、`zh-TW.ts`、`ja.ts`、`ko.ts`、`es.ts`、`it.ts`、`pt-BR.ts`、`fallback.ts`。
- 修改：`apps/desktop/src/lib/__tests__/connection/tunnelProfiles.spec.ts`、`connectionAttemptTimeout.spec.ts`。
- 创建：`apps/desktop/src/lib/__tests__/connection/gatewayProfiles.spec.ts`。
- 创建：`apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts`。
- 修改：`docs/dbx-gateway.md`、`docs/dbx-gateway/pki.md`，补充正式 DBX 客户端导入和选路步骤。

### 任务 1：扩展跨端配置模型并保持秘密隔离

**文件：**
- 修改：`crates/dbx-core/src/models/connection.rs`
- 修改：`crates/dbx-core/src/connection_secrets.rs`
- 修改：`crates/dbx-core/src/storage.rs`
- 修改：`crates/dbx-core/src/cloud_sync.rs`
- 修改：`apps/desktop/src/types/database.ts`

- [ ] **步骤 1：编写模型与同步失败测试**

Rust 测试覆盖：旧 JSON 不受影响；Gateway profile round-trip；连接引用共享 profile 时保留自身 route；`scrub_secrets` 不需要处理私钥字段，因为模型根本不存在私钥；cloud sync 只含 `identity_id`、CA/pin 和逻辑 route。

```rust
#[test]
fn gateway_profile_resolution_preserves_connection_route() {
    let reference = gateway_layer("layer-1", "profile-1", "edge-prod-01", "postgres-primary");
    let profile = gateway_profile("profile-1", "wss://gateway.example.com/_dbx/client", "identity-1");
    let resolved = reference.resolved_from_profile(&profile);
    assert!(matches!(resolved, TransportLayerConfig::DbxGateway(config)
        if config.edge_id == "edge-prod-01" && config.target_id == "postgres-primary"));
}
```

- [ ] **步骤 2：运行模型测试并确认失败**

运行：`cargo test -p dbx-core gateway_profile_`

预期：FAIL，`DbxGateway` 分支尚不存在。

- [ ] **步骤 3：实现 Gateway 配置类型**

Rust 与 TypeScript 使用相同字段：

```rust
pub struct DbxGatewayConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub profile_id: String,
    pub main_url: String,
    pub identity_id: String,
    pub server_ca_pem: String,
    pub server_spki_sha256: String,
    pub connect_timeout_secs: u64,
    pub edge_id: String,
    pub target_id: String,
}
```

`resolved_from_profile` 对 Gateway 使用 profile 的 Main/identity/CA/pin/timeout，但保留引用层的 id、enabled、profile_id、edge_id、target_id。profile 自身的 edge/target 必须为空。所有枚举 match、endpoint、name、enabled、profile storage 和测试 helper 都增加显式分支。

- [ ] **步骤 4：运行模型与存储测试**

运行：`cargo test -p dbx-core gateway_`

运行：`cargo test -p dbx-core connection_secrets::tests`

运行：`cargo test -p dbx-core storage::tests`

运行：`cargo test -p dbx-core cloud_sync::tests`

预期：PASS，现有 SSH/Proxy/HTTP Tunnel 测试不变。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-core/src/models/connection.rs crates/dbx-core/src/connection_secrets.rs crates/dbx-core/src/storage.rs crates/dbx-core/src/cloud_sync.rs apps/desktop/src/types/database.ts
git commit -m "feat(gateway): add DBX gateway transport model"
```

### 任务 2：定义身份提供器并实现 OS 密钥库存储

**文件：**
- 创建：`crates/dbx-core/src/db/dbx_gateway.rs`
- 修改：`crates/dbx-core/src/db/mod.rs`
- 修改：`src-tauri/Cargo.toml`
- 创建：`src-tauri/src/gateway_identity.rs`
- 创建：`src-tauri/src/commands/gateway.rs`
- 修改：`src-tauri/src/commands/mod.rs`
- 修改：`src-tauri/src/lib.rs`
- 修改：`crates/dbx-core/src/storage.rs`

- [ ] **步骤 1：编写身份提供器与导入测试**

dbx-core 使用内存 fake provider 测试 identity ID 解析；Tauri 测试使用 keyring mock credential builder，导入 PKCS#12 后断言 key/cert chain 可解析、列表只返回非秘密元数据、删除后无法解析。Linux keyring unavailable 必须返回错误，不允许写明文文件。

- [ ] **步骤 2：运行身份测试并确认失败**

运行：`cargo test -p dbx-core dbx_gateway::tests::identity_`

运行：`cargo test -p dbx --no-default-features gateway_identity::tests`

预期：FAIL，provider 与 Tauri command 尚不存在。

- [ ] **步骤 3：实现可注入身份边界**

dbx-core 只认识 DER，不依赖 OS keyring：

```rust
#[derive(Clone, Zeroize)]
#[zeroize(drop)]
pub struct GatewayClientIdentity {
    pub certificate_chain_der: Vec<Vec<u8>>,
    pub private_key_pkcs8_der: Vec<u8>,
}

#[async_trait::async_trait]
pub trait GatewayIdentityProvider: Send + Sync {
    async fn load(&self, identity_id: &str) -> Result<GatewayClientIdentity, String>;
}
```

提供 `UnavailableGatewayIdentityProvider` 给 dbx-web 和现有 AppState 构造器。Tauri 使用 `keyring` 的跨平台后端，service 固定 `fun.dbx.gateway`，account 为随机 identity UUID；secret value 是带版本字段的 base64 JSON。PKCS#12 使用纯 Rust p12-keystore 解析，导入密码只存在命令调用内存中。

非秘密 `GatewayIdentityMetadata { id, name, subject, expires_at, fingerprint_sha256 }` 通过 Storage 的专用 load/save 方法保存到本地 SQLite app settings，不进入 cloud sync。删除先移除 keyring 项，再移除 metadata；keyring unavailable 或 locked 时整体失败。

- [ ] **步骤 4：运行身份测试**

运行：`cargo test -p dbx-core dbx_gateway::tests::identity_`

运行：`cargo test -p dbx --no-default-features gateway_identity::tests`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-core/src/db/dbx_gateway.rs crates/dbx-core/src/db/mod.rs crates/dbx-core/src/storage.rs src-tauri/Cargo.toml src-tauri/src/gateway_identity.rs src-tauri/src/commands/gateway.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs Cargo.lock
git commit -m "feat(gateway): store client identities in OS keyring"
```

### 任务 3：实现 DBX Gateway 本地监听器和 WSS 客户端

**文件：**
- 修改：`crates/dbx-core/Cargo.toml`
- 修改：`crates/dbx-core/src/db/dbx_gateway.rs`
- 修改：`crates/dbx-core/src/connection.rs`
- 测试：`crates/dbx-core/src/db/dbx_gateway.rs`

- [ ] **步骤 1：编写 manager 集成测试**

使用 Gateway crate 的测试 Main/Edge server 和内存 identity provider，启动 `DbxGatewayManager` 本地 listener；连接 listener 后随机二进制经 Main/Edge/echo 返回。覆盖错误 pin、错误 identity、route denied、Edge offline、target unavailable、停止 listener 关闭任务。

- [ ] **步骤 2：运行 manager 测试并确认失败**

运行：`cargo test -p dbx-core dbx_gateway::tests::manager_ -- --nocapture`

预期：FAIL，manager 尚未实现。

- [ ] **步骤 3：实现独立 Gateway manager**

在 `crates/dbx-core/Cargo.toml` 中添加 `dbx-gateway = { path = "../dbx-gateway", default-features = false }` 复用 `protocol` 与 `error` 类型，并添加 WSS/rustls 客户端依赖。公开 API：

```rust
pub async fn start_tunnel(
    &self,
    layer_id: &str,
    dial_host: &str,
    dial_port: u16,
    config: &DbxGatewayConfig,
    progress: Option<TransportProgressSender>,
) -> Result<u16, String>;

pub async fn list_routes(&self, config: &DbxGatewayConfig) -> Result<Vec<GatewayEdgeRoutes>, String>;
pub async fn test_profile(&self, config: &DbxGatewayConfig) -> Result<String, String>;
pub async fn stop_tunnel(&self, layer_id: &str);
```

TLS SNI 与 HTTP Host 来自 `main_url`；TCP 实际连接使用 `dial_host/dial_port`，以支持前置 SSH/Proxy。客户端只信 profile 中专用 CA 或 SPKI pin，不加载系统 root，不提供 ignore-errors。每个本地 TCP accepted stream 新建一个 WSS；阶段事件通过可选 sender 报告，sender 缺失时不影响连接。

- [ ] **步骤 4：运行 manager 测试**

运行：`cargo test -p dbx-core dbx_gateway::tests::manager_ -- --nocapture`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-core/Cargo.toml crates/dbx-core/src/db/dbx_gateway.rs crates/dbx-core/src/connection.rs Cargo.lock
git commit -m "feat(gateway): connect DBX through gateway WSS"
```

### 任务 4：接入有序传输层生命周期

**文件：**
- 修改：`crates/dbx-core/src/db/transport_layer_tunnel.rs`
- 修改：`crates/dbx-core/src/connection.rs`
- 测试：`crates/dbx-core/src/db/transport_layer_tunnel.rs`
- 测试：`crates/dbx-core/src/connection.rs`

- [ ] **步骤 1：编写链路规划测试**

覆盖：Gateway 单独使用；Proxy -> Gateway；SSH -> Gateway；Gateway 后面还有层时拒绝；两个 Gateway 拒绝；Gateway dial endpoint 使用前一层 localhost，但 SNI 保留 Main hostname；reset/stop 能关闭 Gateway listener。

```rust
#[test]
fn gateway_must_be_last_transport_layer() {
    let error = validate_transport_layers(&[gateway_layer(), ssh_layer("ssh", "bastion", 22)]).unwrap_err();
    assert!(error.contains("DBX Gateway must be the final transport layer"));
}
```

- [ ] **步骤 2：运行链路测试并确认失败**

运行：`cargo test -p dbx-core gateway_transport_`

预期：FAIL，枚举尚未接入 chain。

- [ ] **步骤 3：把 manager 接入 AppState 和 start/stop**

`AppState` 新增 `dbx_gateway_tunnels: DbxGatewayManager`。保留所有现有构造器并默认注入 unavailable provider；新增带 `Arc<dyn GatewayIdentityProvider>` 的构造器供 Tauri 使用，dbx-web 继续使用默认构造器。

`start_transport_layers`、`stop_transport_layers` 和所有 Redis/RabbitMQ/Nacos reset 路径显式传入 Gateway manager。Gateway 只能作为最后一层；前置层将目标设为 Main URL host/port，Gateway manager 的 dial endpoint 使用前置层本地端口。

- [ ] **步骤 4：运行 dbx-core 相关测试**

运行：`cargo test -p dbx-core gateway_transport_`

预期：PASS。

运行：`cargo test -p dbx-core connection_secrets::tests`

运行：`cargo test -p dbx-core storage::tests`

运行：`cargo test -p dbx-core cloud_sync::tests`

预期：PASS，无现有传输层回归。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-core/src/db/transport_layer_tunnel.rs crates/dbx-core/src/connection.rs
git commit -m "feat(gateway): integrate gateway transport lifecycle"
```

### 任务 5：提供身份和路由的 Tauri API

**文件：**
- 修改：`src-tauri/src/commands/gateway.rs`
- 修改：`src-tauri/src/lib.rs`
- 修改：`apps/desktop/src/lib/backend/api.ts`
- 修改：`apps/desktop/src/lib/backend/tauri.ts`
- 修改：`apps/desktop/src/lib/backend/http.ts`
- 修改：`crates/dbx-web/src/routes/tunnel_profiles.rs`

- [ ] **步骤 1：编写 API 测试**

Tauri command 测试：import/list/delete identity；list routes 只返回 ACL 允许路由；test profile 只验证 Main mTLS，不连接数据库。HTTP backend 对 import/list/delete/routes 返回稳定的“桌面端系统密钥库不可用”错误，不能把 PKCS#12 上传到 dbx-web。

- [ ] **步骤 2：运行 API 测试并确认失败**

运行：`cargo test -p dbx --no-default-features commands::gateway::tests`

运行：`cargo test -p dbx-web --no-default-features tunnel_profiles::tests::gateway_`

预期：FAIL，命令/API 尚不存在。

- [ ] **步骤 3：实现命令与前端 API 类型**

Tauri 命令固定为：

```text
import_gateway_identity(path, password, name)
list_gateway_identities()
delete_gateway_identity(identity_id)
list_gateway_routes(profile)
test_gateway_profile(profile)
```

所有阻塞 keyring 与 PKCS#12 操作放入 `spawn_blocking`。password 不进入日志和 error display。`http.ts` 对这些桌面能力直接抛出本地 unsupported 错误，不发送文件或密码到 HTTP 后端。

- [ ] **步骤 4：运行 API 测试和 TypeScript 类型检查**

运行：`cargo test -p dbx --no-default-features commands::gateway::tests`

运行：`cargo test -p dbx-web --no-default-features tunnel_profiles::tests::gateway_`

运行：`pnpm typecheck`

预期：全部通过。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/commands/gateway.rs src-tauri/src/lib.rs apps/desktop/src/lib/backend/api.ts apps/desktop/src/lib/backend/tauri.ts apps/desktop/src/lib/backend/http.ts crates/dbx-web/src/routes/tunnel_profiles.rs
git commit -m "feat(gateway): expose client identity and route APIs"
```

### 任务 6：实现前端 Gateway profile 逻辑

**文件：**
- 创建：`apps/desktop/src/lib/connection/gatewayProfiles.ts`
- 修改：`apps/desktop/src/lib/connection/tunnelProfiles.ts`
- 修改：`apps/desktop/src/lib/connection/connectionAttemptTimeout.ts`
- 修改：`apps/desktop/src/stores/tunnelProfileStore.ts`
- 创建：`apps/desktop/src/lib/__tests__/connection/gatewayProfiles.spec.ts`
- 修改：`apps/desktop/src/lib/__tests__/connection/tunnelProfiles.spec.ts`
- 修改：`apps/desktop/src/lib/__tests__/connection/connectionAttemptTimeout.spec.ts`

- [ ] **步骤 1：编写纯函数测试**

覆盖默认 profile、URL 必须是 `wss://`、CA/pin 至少一个、route 按 Edge 分组、离线 Edge 保留但不可新选、profile reference 保留 connection route、Gateway timeout 计入测试 deadline。

```typescript
it("keeps the route on a gateway profile reference", () => {
  const layer = gatewayProfileReferenceLayer(profile, { id: "layer-1", edge_id: "edge-prod-01", target_id: "postgres-primary" });
  expect(layer.profile_id).toBe(profile.id);
  expect(layer.edge_id).toBe("edge-prod-01");
  expect(layer.target_id).toBe("postgres-primary");
});
```

- [ ] **步骤 2：运行前端测试并确认失败**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/gatewayProfiles.spec.ts apps/desktop/src/lib/__tests__/connection/tunnelProfiles.spec.ts apps/desktop/src/lib/__tests__/connection/connectionAttemptTimeout.spec.ts`

预期：FAIL，helper/type 尚不存在。

- [ ] **步骤 3：实现最小纯函数与 store 状态**

`gatewayProfiles.ts` 导出 `createDbxGatewayProfile`、`validateDbxGatewayProfile`、`gatewayProfileReferenceLayer`、`groupGatewayRoutes`。`TunnelProfileStore` 增加 `gatewayIdentities`、按 profile ID 缓存 routes、`refreshGatewayRoutes(profileId)`，过期请求使用现有 test guard 方式丢弃。

- [ ] **步骤 4：运行前端逻辑测试**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/gatewayProfiles.spec.ts apps/desktop/src/lib/__tests__/connection/tunnelProfiles.spec.ts apps/desktop/src/lib/__tests__/connection/connectionAttemptTimeout.spec.ts`

预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add apps/desktop/src/lib/connection/gatewayProfiles.ts apps/desktop/src/lib/connection/tunnelProfiles.ts apps/desktop/src/lib/connection/connectionAttemptTimeout.ts apps/desktop/src/stores/tunnelProfileStore.ts apps/desktop/src/lib/__tests__/connection
git commit -m "feat(gateway): manage gateway profiles and routes"
```

### 任务 7：在隧道设置中加入 Gateway 与证书导入

**文件：**
- 修改：`apps/desktop/src/components/connection/TunnelProfileManager.vue`
- 修改：`apps/desktop/src/i18n/locales/en.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-CN.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-TW.ts`
- 修改：`apps/desktop/src/i18n/locales/ja.ts`
- 修改：`apps/desktop/src/i18n/locales/ko.ts`
- 修改：`apps/desktop/src/i18n/locales/es.ts`
- 修改：`apps/desktop/src/i18n/locales/it.ts`
- 修改：`apps/desktop/src/i18n/locales/pt-BR.ts`
- 修改：`apps/desktop/src/i18n/locales/fallback.ts`
- 创建：`apps/desktop/src/lib/__tests__/connection/tunnelProfileGatewayUi.spec.ts`

- [ ] **步骤 1：编写 UI 源码/编译测试**

断言 profile manager 包含 Gateway 类型按钮、Main URL、CA/pin、identity selector、PKCS#12 导入、删除和 Test Main；浏览器运行时禁用导入并显示桌面端限定说明；测试按钮不要求 edge route。

- [ ] **步骤 2：运行 UI 测试并确认失败**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/tunnelProfileGatewayUi.spec.ts`

预期：FAIL，组件尚无 Gateway UI。

- [ ] **步骤 3：实现设置界面**

使用现有 Button/Input/Select/Tooltip 和 Lucide `Network`、`ShieldCheck`、`Upload`、`Trash2` 图标。PKCS#12 导入使用文件选择器过滤 `p12`/`pfx`，密码使用 PasswordInput，成功后立即清空密码 ref。删除 identity 前显示其被多少 profile 引用；确认后调用 backend delete。

Gateway profile 字段只显示 Main URL、identity、CA PEM 导入、SPKI pin、超时。不得显示 edge/target，因为 route 属于具体数据库连接。

- [ ] **步骤 4：运行 UI 测试、类型检查和 lint**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/tunnelProfileGatewayUi.spec.ts`

运行：`pnpm typecheck`

运行：`pnpm lint`

预期：全部通过。

- [ ] **步骤 5：Commit**

```bash
git add apps/desktop/src/components/connection/TunnelProfileManager.vue apps/desktop/src/i18n/locales apps/desktop/src/lib/__tests__/connection/tunnelProfileGatewayUi.spec.ts
git commit -m "feat(gateway): configure gateway identities in settings"
```

### 任务 8：在连接编辑器中加入 Gateway 路由选择

**文件：**
- 修改：`apps/desktop/src/components/connection/ConnectionDialog.vue`
- 创建：`apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts`
- 修改：`apps/desktop/src/i18n/locales/en.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-CN.ts`
- 修改：`apps/desktop/src/i18n/locales/zh-TW.ts`
- 修改：`apps/desktop/src/i18n/locales/ja.ts`
- 修改：`apps/desktop/src/i18n/locales/ko.ts`
- 修改：`apps/desktop/src/i18n/locales/es.ts`
- 修改：`apps/desktop/src/i18n/locales/it.ts`
- 修改：`apps/desktop/src/i18n/locales/pt-BR.ts`
- 修改：`apps/desktop/src/i18n/locales/fallback.ts`
- 修改：`docs/dbx-gateway.md`
- 修改：`docs/dbx-gateway/pki.md`

- [ ] **步骤 1：编写连接编辑器测试**

断言可添加 Gateway 层；Gateway 只能位于最后；选择共享 Gateway profile 后显示分组、可搜索 route；在线 target 可选，离线 Edge 显示但 disabled；刷新图标不会改变表单布局；保存的 layer reference 包含 profile ID 与 route，不包含 Main URL/identity 私钥。

- [ ] **步骤 2：运行连接编辑器测试并确认失败**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts`

预期：FAIL，Gateway UI 尚不存在。

- [ ] **步骤 3：实现 Gateway 层编辑体验**

复用现有传输层列表，不创建嵌套卡片。添加 `Network` 图标的 Gateway 类型；选中后显示共享 profile selector、搜索 input、按 Edge 分组的 route menu 和 RefreshCw icon button/tooltip。长 Edge/target 名称使用 truncate，菜单提供 title/tooltip；固定高度/宽度避免 loading icon 改变布局。

无 profile、身份缺失、Main 不可达、route 未选择、Edge 离线时阻止测试/保存并给出明确错误。配置变更清除最近测试结果。不要在本任务实现链路示意图；它在 Gateway 稳定后按独立规格实现。同步更新两份 Gateway 文档，写明 DBX 中导入 PKCS#12、创建 Gateway profile、刷新授权路由、选择 Edge/target、测试连接和删除身份的真实菜单路径与错误处理。

- [ ] **步骤 4：运行 UI 验证**

运行：`pnpm vitest run apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts`

运行：`pnpm typecheck && pnpm lint && pnpm build`

预期：全部通过。

启动：`pnpm dev -- --host 127.0.0.1`

使用浏览器分别在 1440x900、1024x768、390x844 截图，检查 profile selector、分组 route、离线状态、错误提示无重叠和横向溢出。桌面能力在浏览器预览中显示不可用提示，不要求真实 keyring。

- [ ] **步骤 5：Commit**

```bash
git add apps/desktop/src/components/connection/ConnectionDialog.vue apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts apps/desktop/src/i18n/locales docs/dbx-gateway.md docs/dbx-gateway/pki.md
git commit -m "feat(gateway): select gateway routes per connection"
```

### 任务 9：完整连接与回归验证

**文件：**
- 修改：`crates/dbx-core/src/db/dbx_gateway.rs`
- 修改：`src-tauri/src/commands/gateway.rs`
- 修改：`apps/desktop/src/lib/__tests__/connection/connectionDialogGateway.spec.ts`

- [ ] **步骤 1：运行 Rust 全量相关测试**

运行：`cargo fmt --check`

运行：`cargo clippy -p dbx-core -p dbx -p dbx-web --all-targets -- -D warnings`

运行：`cargo test -p dbx-core --lib`

运行：`cargo test -p dbx --no-default-features`

运行：`cargo test -p dbx-web --no-default-features`

预期：全部通过。

- [ ] **步骤 2：运行前端全量验证**

运行：`pnpm test`

运行：`pnpm typecheck`

运行：`pnpm lint`

运行：`pnpm build`

预期：全部通过。

- [ ] **步骤 3：执行真实 Main/Edge/数据库连接**

使用第 2 份计划生成的 Main、Edge 和 DBX Client PKCS#12：导入 identity；创建 Gateway profile；刷新并选择 `edge-prod-01/postgres-primary`；测试 PostgreSQL 或 MySQL 连接；保存后重新打开并连接。验证数据库服务看到的源 IP 是 Edge，Main 与 Edge 日志只含逻辑路由和字节计数。

- [ ] **步骤 4：执行密钥与同步检查**

导出连接 JSON、SQLite 数据、WebDAV/Gist 快照并搜索测试私钥 DER/base64、PKCS#12 密码和证书私钥标记。预期无匹配；只允许出现 `identity_id`、公开证书/CA、SPKI pin 与逻辑 route。删除 OS keyring identity 后连接必须 fail closed，不能读取旧缓存私钥。

- [ ] **步骤 5：Commit**

```bash
git add crates/dbx-core src-tauri apps/desktop/src crates/dbx-web
git commit -m "test(gateway): verify DBX client integration"
```

## 阶段完成标准

- DBX 桌面端可导入密码保护 PKCS#12，私钥只存 OS keyring。
- 连接配置、SQLite 普通字段、导出和云同步不包含 Gateway 私钥或导入密码。
- Gateway profile 保存 Main URL、身份 ID、专用 CA/SPKI pin 和超时；具体连接保存逻辑 route。
- DBX 只显示当前客户端证书 ACL 允许的 routes，离线 Edge 不可用于新选择。
- Gateway 可单独使用，也可作为有序传输链最后一层经 SSH/Proxy 连接 Main。
- 真实数据库连接经 Main/Edge 成功，数据库看到 Edge 源地址。
- dbx-web 不上传 PKCS#12、不回退明文存储，并对 Gateway 身份能力给出明确不可用错误。
- 连接编辑器在桌面和移动宽度下无文本重叠或布局跳动。
- 链路示意图仍作为下一份独立设计与实现，不混入本计划。
