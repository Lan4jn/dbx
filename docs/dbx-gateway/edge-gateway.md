# Edge Gateway 部署

Edge 部署在数据库附近，主动连接 Main，不需要从 Main 向数据库网段开放入站端口。数据库看到的新连接由 Edge 主机上的 `dbx-gateway` 进程发起，源 IP 是 Edge 主机 IP。

证书首次领取、每个产物的来源和用途见 [Edge 节点证书生成与领取](edge-certificate.md)。

## 安装

在 Edge Linux 主机以 `root` 执行：

```bash
groupadd --system dbx-gateway
useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway --shell /usr/sbin/nologin dbx-gateway
install -d -o root -g dbx-gateway -m 0750 /etc/dbx-gateway /etc/dbx-gateway/certs
install -d -o dbx-gateway -g dbx-gateway -m 0700 /var/lib/dbx-gateway
install -m 0755 bin/dbx-gateway /usr/bin/dbx-gateway
install -o root -g dbx-gateway -m 0640 examples/edge.toml /etc/dbx-gateway/edge.toml
```

以上安装命令由 `root` 执行，但 systemd 启动后的进程使用 unit 中的 `User=dbx-gateway`、`Group=dbx-gateway`。因此 `/etc/dbx-gateway/edge.toml` 和 Main Server CA 必须允许这个**运行账户**读取；配置文件建议为 `root:dbx-gateway 0640`。若修改过 unit 的 `User` 或 `Group`，以实际值为准：

```bash
systemctl show dbx-gateway-edge.service -p User -p Group
```

预期默认输出 `User=dbx-gateway` 和 `Group=dbx-gateway`。服务用户不可登录，数据目录只有该用户可读写。若账号已存在，核对后跳过创建命令。把 Main Server CA 复制为 `/etc/dbx-gateway/certs/main-server-ca.pem`，不要把任何 CA 私钥复制到 Edge。

在 **Main 主机**确认 `main.toml` 中当前服务端证书路径：

```bash
grep '^certificate' /etc/dbx-gateway/main.toml
```

默认路径为 `/etc/dbx-gateway/certs/main.pem`。从这个当前服务端证书计算 SPKI pin：

```bash
openssl x509 -in /etc/dbx-gateway/certs/main.pem -pubkey -noout \
  | openssl pkey -pubin -outform DER \
  | openssl dgst -sha256 -hex \
  | awk '{print $NF}'
```

预期输出单行 64 位十六进制值。它是 Main 当前服务端证书的**公钥摘要**，不是 CA 指纹或整张证书的指纹。通过另一条可信渠道核对后写入 Edge `edge.toml` 的 `[bootstrap].server_spki_sha256`。Main 更换私钥后必须重新计算并更新 Edge；pin 错误会使首次领证 fail closed，不要为“先跑起来”而关闭校验。

## 令牌领证

在 PKI 主机以 `dbx-gateway-pki` 用户创建一次性令牌：

```bash
dbx-gateway-pki enrollment create \
  --data-dir /var/lib/dbx-gateway-pki \
  --edge-id edge-prod-01 \
  --ttl 10m
```

输出格式如下：

```text
enrollment token 11111111-2222-3333-4444-555555555555 for edge-prod-01 expires at <到期时间>
11111111-2222-3333-4444-555555555555.<秘密部分>
```

第一行中的 UUID 是 **Token ID**，只用于审计或撤销，不能单独领证。最后一行是 **完整一次性注册令牌**，格式为 `<Token ID>.<秘密部分>`；两行不是两个可用 Token。只有最后一整行需要通过受控通道传到 Edge，在线 PKI 数据库只保存其 Argon2id 哈希。不要把完整令牌写入工单、聊天记录或 shell history。

在 Edge 主机以 `root` 写入令牌：

```bash
umask 077
printf '%s\n' 'REPLACE_WITH_FULL_TOKEN_ID_DOT_SECRET' > /var/lib/dbx-gateway/enrollment.token
chown dbx-gateway:dbx-gateway /var/lib/dbx-gateway/enrollment.token
```

预期文件权限 `0600`。启动前使用与 systemd unit 相同的运行账户检查配置文件和 CA 是否可读，再校验配置：

```bash
sudo -u dbx-gateway test -r /etc/dbx-gateway/edge.toml
sudo -u dbx-gateway test -r /etc/dbx-gateway/certs/main-server-ca.pem
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/edge.toml check-config
systemctl enable --now dbx-gateway-edge.service
journalctl -u dbx-gateway-edge.service -f
```

前两条命令没有输出表示可读；返回非零时，检查文件所有者、组和父目录权限。若 unit 使用了其他 `User`/`Group`，把命令中的 `dbx-gateway` 替换为实际 `User`。

首次启动时 Edge 在本地生成私钥，向 Main 提交 CSR 与令牌，验证返回证书后原子写入 `/var/lib/dbx-gateway/edge.pem` 和 `edge.key`，删除 token 文件并以 mTLS 重连。Main 和 PKI 从不收到 Edge 私钥。若 token 文件删除失败，Edge 不会进入长期运行，先修正目录权限再用新的 token 重试。

令牌过期、已消费、Edge ID 不匹配或响应中断后不能重放。确认旧证书不可恢复时，在 PKI 主机执行：

```bash
dbx-gateway-pki enrollment create \
  --data-dir /var/lib/dbx-gateway-pki \
  --edge-id edge-prod-01 \
  --ttl 10m \
  --replace --yes
```

`--replace` 会撤销该 Edge 的现有活动证书并发新令牌。只在确认私钥丢失、孤儿证书或主机替换后使用；普通续期不需要 replace。

## 本地目标

DBX 支持的常见数据库默认端口、完整 `edge.toml` 示例和多节点限制见 [Edge 本机数据库目标配置](local-database-targets.md)。

最安全的目标是 Unix Socket 或 loopback：

```toml
[targets.postgres-primary]
display_name = "PostgreSQL Primary"
address = "127.0.0.1:5432"
allow_remote = false
```

或：

```toml
[targets.postgres-primary]
address = { unix = "/run/postgresql/.s.PGSQL.5432" }
```

当数据库不支持 TLS 时，Edge 到数据库的 TCP 包含原生数据库明文。优先把 Edge 部署在数据库同机并使用 Unix Socket；其次放在同一受控安全区。Edge 到 Main 的 TLS 不能保护 Edge 到数据库这段独立连接。

若数据库必须位于另一内网主机，显式设置：

```toml
[targets.postgres-primary]
address = "10.20.30.40:5432"
allow_remote = true
```

配置加载和每次连接都会解析并检查所有候选 IP，拒绝未 opt-in 的远程地址、未指定地址、组播、link-local 和 `169.254.169.254`。连接时只使用已通过检查的 `SocketAddr`，不会再次按域名解析。

Edge 只向 Main 注册 `postgres-primary` 及显示名，不上传 `10.20.30.40:5432`。修改目标地址后向 Edge 发送 HUP；删除目标会阻止新会话，已有会话应在维护窗口内主动关闭。

## 重连与迁移

Main 暂时不可达时，Edge 使用带抖动、封顶的退避自动重连。网络恢复后无需重新签发证书。观察：

```bash
systemctl status dbx-gateway-edge.service --no-pager
journalctl -u dbx-gateway-edge.service --since '-10 min' --no-pager
```

预期服务保持 running，并在 Main 恢复后重新注册。持续 `identity_rejected` 表示证书、CA、URI SAN、有效期或吊销状态有问题；不要无限重启，先检查证书。

迁移到新 Edge 主机的安全步骤：

1. 在旧主机停止 `dbx-gateway-edge.service`。
2. 在 PKI 吊销旧 Edge 证书，Main 重载吊销序列号。
3. 新主机安装二进制、Main CA 和同一 `edge_id` 配置，但不复制旧私钥。
4. 使用 `enrollment create --replace --yes` 生成新令牌。
5. 新主机本地生成新私钥并领证，确认在线后销毁旧主机数据。

如果只是原主机升级，保留 `edge.pem` 和 `edge.key`，不需要新令牌。先备份这两个文件的加密副本，升级失败时恢复旧二进制和原配置即可。

安装 systemd unit：

```bash
install -m 0644 systemd/dbx-gateway-edge.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now dbx-gateway-edge.service
```

预期 `active (running)`。unit 只允许写 `/var/lib/dbx-gateway`；目标 Unix Socket 还需要操作系统组权限，优先给 `dbx-gateway` 加入数据库 socket 组，不要把 socket 改成全员可写。
