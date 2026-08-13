# Main Gateway 部署

Main 是公网入口、客户端与 Edge 的身份验证点及透明字节中继。Main 会看到数据库协议明文，因此主机应使用最小管理员范围、磁盘加密、审计和严格出站规则。

DBX 用户身份的单独签发与交付步骤见 [DBX Client 证书生成与交付](client-certificate.md)，Edge 身份见 [Edge 节点证书生成与领取](edge-certificate.md)。

## 安装

在 Main Linux 主机以 `root` 执行，预期创建不可登录服务账号和目录：

```bash
groupadd --system dbx-gateway
useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway --shell /usr/sbin/nologin dbx-gateway
install -d -o root -g dbx-gateway -m 0750 /etc/dbx-gateway /etc/dbx-gateway/certs
install -d -o dbx-gateway -g dbx-gateway -m 0750 /var/lib/dbx-gateway /run/dbx-gateway
install -m 0755 bin/dbx-gateway /usr/bin/dbx-gateway
install -m 0644 examples/main.toml /etc/dbx-gateway/main.toml
```

若用户或目录已存在，命令会提示冲突；核对其 UID、GID、home 和 shell 后跳过对应创建命令，不要删除已有数据目录。

在保存**完整离线 PKI**的主机签发 Server 证书。在线 Edge-only PKI 只有 `edge` CA，不能执行这一步：

有域名时：

```bash
dbx-gateway-pki server issue \
  --data-dir /secure/dbx-gateway-pki-offline \
  --password-file /secure/dbx-pki-password \
  --identity gateway.example.com \
  --dns-san gateway.example.com \
  --output-dir /secure/export/main-server
```

没有域名、客户端和 Edge 直接连接固定 IP 时：

```bash
dbx-gateway-pki server issue \
  --data-dir /secure/dbx-gateway-pki-offline \
  --password-file /secure/dbx-pki-password \
  --identity main-gateway \
  --dns-san localhost \
  --ip-san 10.235.10.53 \
  --output-dir /secure/export/main-server-ip
```

纯 IP 场景必须使用 `--ip-san`。当前 CLI 要求至少有一个 DNS SAN，因此示例保留 `--dns-san localhost` 作为占位。`--dns-san 10.235.10.53` 会生成 `DNS:10.235.10.53`，不能通过 IP 地址校验。

安装前检查：

```bash
openssl x509 -in /secure/export/main-server-ip/certificate.pem \
  -noout -text | grep -A2 "Subject Alternative Name"
```

应包含 `IP Address:10.235.10.53`。

预期输出 `issued server certificate <serial>`。将 `certificate.pem`、`chain.pem`、`private-key.pem` 通过受控通道送到 Main；Server 私钥只能在 PKI 签发主机与 Main 间短暂传递，导入后删除导出副本。若输出目录已存在，工具会拒绝覆盖，改用新的空目录。

在 Main 主机以 `root` 安装证书：

```bash
install -o root -g dbx-gateway -m 0644 certificate.pem /etc/dbx-gateway/certs/main.pem
cat chain.pem >> /etc/dbx-gateway/certs/main.pem
install -o dbx-gateway -g dbx-gateway -m 0600 private-key.pem /etc/dbx-gateway/certs/main.key
install -o root -g dbx-gateway -m 0644 edge-ca.crt.pem /etc/dbx-gateway/certs/edge-ca.pem
install -o root -g dbx-gateway -m 0644 client-ca.crt.pem /etc/dbx-gateway/certs/client-ca.pem
```

其中 `edge-ca.crt.pem` 来自离线 PKI 的 `edge/ca.crt.pem`，`client-ca.crt.pem` 来自 `client/ca.crt.pem`。DBX 客户端和 Edge 主机信任 Main 时使用签发输出中的 `chain.pem`。

预期私钥为 `0600`，PEM 链首张是 `gateway.example.com` 叶证书。失败时用 `openssl x509 -in ... -noout -subject -issuer -dates` 检查证书，不要临时放宽私钥权限。

校验配置：

```bash
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config
```

预期 `configuration is valid`。端口低于 1024 时，可由 systemd 添加最小的 `AmbientCapabilities=CAP_NET_BIND_SERVICE`，或把 Main 监听改为 `8443` 并由防火墙做端口重定向；不要以 root 运行 Main。

## HTTPS 回退

`fallback_upstream` 只接受一个固定绝对 HTTP(S) URL，例如：

```toml
fallback_upstream = "https://www.example.com"
```

普通请求会移除逐跳头并设置 `Host`、`X-Forwarded-For`、`X-Forwarded-Proto`，支持 HTTP/1.1、HTTP/2、流式 body、SSE 和 WebSocket。请求参数不能改变目标主机，因此它不是开放代理。

保留路径先于回退分类。匿名访问 `/_dbx/client`、错误角色访问 `/_dbx/edge` 或错误客户端证书不会落到上游。没有配置回退时，普通路径返回 `404`。

若 Main 前有 Nginx，只使用 `stream` TCP 透传：

```nginx
stream {
    upstream dbx_main { server 127.0.0.1:8443; }
    server {
        listen 443;
        proxy_pass dbx_main;
        proxy_connect_timeout 5s;
        proxy_timeout 1h;
    }
}
```

运行主机为 Nginx 主机，运行用户为 `root`；`nginx -t` 预期成功。不能使用 `http { proxy_pass https://...; }` 终止 TLS，否则 Main 无法取得原始客户端证书。

## ACL

生产配置同时设置：

```toml
allowed_edge_ids = ["edge-prod-01"]

[client_route_acl]
desktop-prod = ["edge-prod-01/postgres-primary"]

[enrollment]
allowed_edge_ids = ["edge-prod-01"]
```

`client_route_acl` 的 key 对应 DBX Client 证书 URI SAN 中的身份，例如 `urn:dbx-gateway:client:desktop-prod`。规则支持 `edge/target`、`edge/*` 和 `*/*`；配置 ACL 后，没有条目的客户端默认拒绝。路由发现与打开数据通道执行同一套检查。

`allowed_edge_ids` 控制已持证 Edge 注册，`enrollment.allowed_edge_ids` 控制自动领证。删除 Edge ID 或把证书序列号加入 `revoked_edge_serials` 后，在 Main 主机以 `root` 执行：

```bash
systemctl kill -s HUP dbx-gateway-main.service
journalctl -u dbx-gateway-main.service -n 50 --no-pager
```

预期新配置原子生效，不再允许新会话，受影响控制和数据通道关闭。若日志出现 `restart_required` 或配置错误，旧配置继续运行；先修正并再次发送 HUP。

防火墙只需允许公网到 Main `443/tcp`；健康端口 `127.0.0.1:9080` 不对外开放。Main 到数据库网段不需要路由，Main 到 PKI 只允许 Unix Socket，分机部署时仅允许 PKI RA mTLS 端口。

## systemd

在 Main 主机以 `root` 安装 unit：

```bash
install -m 0644 systemd/dbx-gateway-main.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now dbx-gateway-main.service
systemctl status dbx-gateway-main.service --no-pager
```

预期状态为 `active (running)`。若启动失败：

```bash
journalctl -u dbx-gateway-main.service -b --no-pager
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config
```

不要移除 unit 中的 `NoNewPrivileges`、`ProtectSystem=strict`、`PrivateTmp`、`LimitCORE=0`。需要新增写目录时，只追加到 `ReadWritePaths`，不要把整个文件系统改成可写。

本机健康检查：

```bash
curl -fsS http://127.0.0.1:9080/healthz
```

预期 JSON 包含 `status=ok`、进程号、Server 证书到期 Unix 时间、PKI 配置状态和在线 Edge 数，`database_checks` 固定为 `0`。健康接口不会登录数据库。

## 升级与回滚

在 Main 主机以 `root` 升级：

```bash
sha256sum -c DBX_Gateway_0.5.75_x64.tar.gz.sha256
tar -xzf DBX_Gateway_0.5.75_x64.tar.gz
/usr/bin/dbx-gateway --version > /var/lib/dbx-gateway/previous-version.txt
cp /usr/bin/dbx-gateway /var/lib/dbx-gateway/dbx-gateway.previous
install -m 0755 DBX_Gateway_0.5.75_x64/bin/dbx-gateway /usr/bin/dbx-gateway
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config
systemctl restart dbx-gateway-main.service
```

预期 checksum 成功、版本为 `0.5.75`、服务恢复 active。升级会中断活动隧道，安排维护窗口。

若新版本无法启动，立即回滚：

```bash
install -m 0755 /var/lib/dbx-gateway/dbx-gateway.previous /usr/bin/dbx-gateway
systemctl restart dbx-gateway-main.service
```

配置回滚应与二进制一起进行。证书和 PKI 状态不随普通二进制回滚删除；恢复前保留完整备份。
