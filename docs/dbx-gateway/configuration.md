# DBX Gateway 配置参考

配置使用 TOML，未知字段会被拒绝。相对路径以 TOML 文件所在目录为基准解析。私钥和密码文件必须是真实普通文件、不能是符号链接，在 Unix 上权限必须为 `0600`。

## Main 字段

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `mode` | 必填 | 固定为 `main`。 |
| `listen` | 必填 | TLS 监听地址，例如 `0.0.0.0:443`。修改后需重启。 |
| `certificate` | 必填 | Server 证书 PEM，可含中间链。 |
| `private_key` | 必填 | 与 Server 证书匹配的私钥 PEM，权限 `0600`。 |
| `edge_ca_certificate` | 必填 | 只信任 Edge 身份的 CA PEM。 |
| `client_ca_certificate` | 必填 | 只信任 DBX Client 身份的 CA PEM。 |
| `edge_path` | `/_dbx/edge` | Edge 控制路径；数据路径自动为 `<edge_path>/data`。 |
| `dbx_path` | `/_dbx/client` | DBX 客户端数据路径。 |
| `fallback_upstream` | 无 | 单个固定绝对 `http://` 或 `https://` 上游；禁止凭据和 fragment。 |
| `health_listen` | 无 | 仅允许 loopback，例如 `127.0.0.1:9080`。提供 `/healthz`，不登录数据库。 |
| `state_file` | 无 | Main 的最后已知 Edge/逻辑目标 SQLite；不保存数据库地址。 |
| `allowed_edge_ids` | 空 | 空表示证书 CA 范围内均可注册；生产建议显式列出。 |
| `revoked_edge_serials` | 空 | 吊销序列号列表；重载后关闭对应活动会话。 |
| `client_route_acl` | 空 | DBX Client 证书身份到逻辑路由的白名单；支持 `edge/target`、`edge/*`、`*/*`。空表保持兼容并允许全部已注册逻辑路由。 |
| `max_connections` | `1024` | Main 总 TCP/TLS 连接上限。 |
| `max_streams_per_edge` | `256` | 单 Edge 活动数据通道上限。 |
| `max_streams_per_client` | `32` | 单 DBX 客户端证书身份活动通道上限。 |
| `connection_rate_per_second` | `64` | 单来源 IP 每秒补充的连接令牌数。 |
| `connection_rate_burst` | `128` | 单来源 IP 可突发连接数。 |
| `global_buffer_budget_bytes` | `268435456` | 数据通道全局缓冲预算，至少 2 MiB。 |
| `tls_handshake_timeout_secs` | `10` | TLS 握手超时。 |
| `http_header_timeout_secs` | `10` | 首个 HTTP 请求头超时。 |

生产环境建议显式配置客户端路由权限。证书 URI SAN 为 `urn:dbx-gateway:client:desktop-prod` 时，身份 key 是 `desktop-prod`：

```toml
[client_route_acl]
desktop-prod = ["edge-prod-01/postgres-primary", "edge-reporting/*"]
```

路由发现和实际打开数据通道使用同一套 ACL；未授权目标不会出现在 DBX 中，直接请求也会返回 `route_denied`。规则只包含逻辑 ID，不包含数据库地址。

`[enrollment]` 字段：

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `path` | `/_dbx/enroll` | 匿名但带一次性令牌的首次领证路径。 |
| `renewal_path` | `/_dbx/renew` | 使用当前 Edge mTLS 身份的续期路径。 |
| `allowed_edge_ids` | 必填 | 可通过在线 PKI 领证的 Edge ID 白名单。 |
| `pki` | 必填 | Unix Socket 或远程 RA mTLS 端点。 |

Unix 端点只设置 `unix_socket`。远程端点必须同时设置 `remote_address`、`server_name`、`ca_certificate`、`certificate` 和 `private_key`。

## Edge 字段

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `mode` | 必填 | 固定为 `edge`。 |
| `edge_id` | 必填 | 例如 `edge-prod-01`，必须与证书 URI SAN 一致。 |
| `main_url` | 必填 | `wss://gateway.example.com/_dbx/edge`。 |
| `certificate` | 必填路径 | 已领证时读取；首次启动时原子创建。 |
| `private_key` | 必填路径 | Edge 本地生成，永不发送给 Main/PKI，权限 `0600`。 |
| `ca_certificate` | 必填 | Main Server 证书的信任 CA。 |
| `bootstrap` | 无 | 证书不存在时必须提供。 |
| `targets` | 必填 | 以逻辑目标 ID 为 key 的目标表。 |

`[bootstrap]` 字段：`token_file`、`enrollment_url`、`server_spki_sha256` 必填，`renew_before_days` 默认 `30`。`server_spki_sha256` 是从 Main 当前服务端证书公钥计算的 64 位十六进制 SHA-256，不是 CA 或整张证书的指纹，必须从可信渠道核对。`token_file` 的内容必须是 `enrollment create` 输出最后一行的完整 `<Token ID>.<秘密部分>`；第一行单独显示的 Token ID 不能用于领证。

每个 `[targets.<id>]` 支持：

- `display_name`：UI 显示名，默认使用目标 ID。
- `address`：`"127.0.0.1:5432"`、`{ tcp = "localhost:5432" }` 或 `{ unix = "/run/postgresql/.s.PGSQL.5432" }`。
- `allow_remote`：默认 `false`。非 loopback TCP 必须显式设为 `true`；即使设为 true，解析后的未指定、组播、link-local 和云元数据地址仍会拒绝。

## PKI 字段

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `data_dir` | 必填 | `/var/lib/dbx-gateway-pki`，包含加密 CA 私钥、签发记录和 CRL。 |
| `password_file` | 必填 | 解密在线 CA 的密码文件，权限 `0600`。 |
| `state_file` | `gateway-state.sqlite3` | 注册令牌、证书状态与吊销记录 SQLite 文件。 |
| `unix` | 无 | 推荐的本机 Main 接入方式。 |
| `remote` | 无 | Main 与 PKI 分机部署时使用的 RA mTLS。 |

`[unix]` 包含 `path`、`allowed_uid`、`allowed_gid`，Socket 权限固定为 `0660`。示例 UID/GID 必须改成部署主机上 `dbx-gateway` 的真实数值。

`[remote]` 包含 `listen`、`certificate`、`private_key`、`main_ra_ca_certificate`、`allowed_ra_uri_sans`。远程接口仍只能签 Edge `clientAuth`，不能签 Server 或 DBX Client。

## 校验与重载

安装文件时通常使用 `root`，但读取配置的是 systemd unit 中 `User=` 指定的运行账户。Main 和 Edge 的默认运行账户都是 `dbx-gateway`；若改过 unit，先查询实际值：

```bash
systemctl show dbx-gateway-edge.service -p User -p Group
```

然后以同一个运行账户校验对应配置：

```bash
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/edge.toml check-config
```

只在当前主机执行对应角色的命令。预期输出为 `configuration is valid`。出现 `configuration file could not be read` 时，检查配置文件是否允许 unit 的 `User`/`Group` 读取，并检查 `/etc/dbx-gateway` 等父目录是否允许该账户进入。失败时不要重启服务；按错误信息修正路径、权限或字段。配置验证后执行 `systemctl reload` 若 unit 配置了 reload，或向进程发送 `SIGHUP`。监听地址、限额和管理监听等不可变字段变化会返回 `restart_required`，需要安排重启；错误重载会继续使用旧配置。
