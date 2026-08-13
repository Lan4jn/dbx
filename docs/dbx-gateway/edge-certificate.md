# Edge 节点证书生成与领取

本文只说明 Edge 身份的首次领取、产物和更换。标准流程不把 Edge 私钥从 PKI 主机复制到 Edge；Edge 在本机生成私钥，通过一次性令牌领取证书。

## 文件与角色

| 文件或数据 | 生成位置 | 最终使用位置 | 用途 |
|---|---|---|---|
| `edge/ca.crt.pem` | 离线 PKI | 在线 PKI、Main | 签发或验证 Edge 证书 |
| `edge/ca.key.encrypted.pem` | 离线 PKI | 在线 PKI | 签发 Edge 证书的加密 CA 私钥 |
| 一次性注册令牌 | 在线 PKI | Edge 的 `enrollment.token` | 授权指定 Edge ID 首次领证 |
| `edge.key` | Edge 本机 | Edge 本机 | Edge 私钥，不得传出 |
| `edge.pem` | 在线 PKI 签发，Edge 写盘 | Edge 本机 | Edge 叶证书和证书链 |
| Main 签发输出的 `chain.pem` | 离线 PKI | Edge 的 `main-server-ca.pem` | Edge 验证 Main Server |

## 前置条件

1. 在线 PKI 服务和 Main Gateway 均为 `active (running)`。
2. Main 的 `allowed_edge_ids` 和 `[enrollment].allowed_edge_ids` 都包含目标 Edge ID。
3. Edge 已安装 `dbx-gateway`，并能访问 Main 的 HTTPS 地址。
4. Edge 已取得 Main 签发输出中的 `chain.pem`，不是 `edge/ca.crt.pem`。

## 1. 在 Edge 安装 Main Server CA

在 **Edge 主机**执行。示例假设文件已安全传到 `/mnt/secure-transfer/main-server/chain.pem`：

```bash
install -d -o root -g dbx-gateway -m 0750 /etc/dbx-gateway/certs
install -o root -g dbx-gateway -m 0644 \
  /mnt/secure-transfer/main-server/chain.pem \
  /etc/dbx-gateway/certs/main-server-ca.pem
```

`main-server-ca.pem` 只用于验证 Main，不包含 Edge 身份或私钥。

## 2. 配置 Edge 的证书路径

在 **Edge 主机**编辑 `/etc/dbx-gateway/edge.toml`：

```toml
mode = "edge"
edge_id = "edge-prod-01"
main_url = "wss://gateway.example.com/_dbx/edge"
certificate = "/var/lib/dbx-gateway/edge.pem"
private_key = "/var/lib/dbx-gateway/edge.key"
ca_certificate = "/etc/dbx-gateway/certs/main-server-ca.pem"

[bootstrap]
token_file = "/var/lib/dbx-gateway/enrollment.token"
enrollment_url = "https://gateway.example.com/_dbx/enroll"
server_spki_sha256 = "REPLACE_WITH_MAIN_SPKI_SHA256"
renew_before_days = 30
```

此时 `edge.pem` 和 `edge.key` 尚不存在是正常的，首次启动会自动生成。

没有域名时，URL 使用 Main 证书 `--ip-san` 中的固定 IP，例如：

```toml
main_url = "wss://10.235.10.53/_dbx/edge"

[bootstrap]
enrollment_url = "https://10.235.10.53/_dbx/enroll"
```

如果 Main 证书把 `10.235.10.53` 错误签成 `DNS:10.235.10.53`，连接会报告证书只对 `DnsName("10.235.10.53")` 有效。应在离线 PKI 重新签发包含 `IP Address:10.235.10.53` SAN 的 Main 证书，不能通过关闭校验解决。

## 3. 创建一次性注册令牌

在 **在线 PKI 主机**执行：

```bash
sudo -u dbx-gateway-pki dbx-gateway-pki enrollment create \
  --data-dir /var/lib/dbx-gateway-pki \
  --edge-id edge-prod-01 \
  --ttl 10m
```

命令最后一行是明文令牌，只显示一次。它不是证书、私钥或 CA 密码。`--edge-id` 必须与 Edge 配置中的 `edge_id` 完全一致。

## 4. 把令牌写入 Edge

通过安全渠道把令牌交给 Edge 管理员，然后在 **Edge 主机**执行：

```bash
umask 077
printf '%s\n' '粘贴上一节命令最后一行的令牌' \
  > /var/lib/dbx-gateway/enrollment.token
chown dbx-gateway:dbx-gateway /var/lib/dbx-gateway/enrollment.token
chmod 0600 /var/lib/dbx-gateway/enrollment.token
```

不要把令牌写进 `edge.toml`、工单或长期脚本。

## 5. 启动并自动领证

```bash
sudo -u dbx-gateway dbx-gateway \
  --config /etc/dbx-gateway/edge.toml check-config
systemctl enable --now dbx-gateway-edge.service
journalctl -u dbx-gateway-edge.service -f
```

首次启动自动完成：

1. Edge 本机生成 `edge.key` 和 CSR。
2. Edge 携带一次性令牌把 CSR 发给 Main。
3. Main 经在线 PKI 签发 Edge 证书。
4. Edge 把叶证书和链写入 `edge.pem`。
5. Edge 删除 `enrollment.token`，再用 `edge.pem` 和 `edge.key` 建立 mTLS。

检查产物：

```bash
ls -l /var/lib/dbx-gateway/edge.pem /var/lib/dbx-gateway/edge.key
test ! -e /var/lib/dbx-gateway/enrollment.token && echo "令牌已删除"
openssl x509 -in /var/lib/dbx-gateway/edge.pem \
  -noout -subject -issuer -serial -dates
```

`edge.key` 必须为 `0600`，且只由 `dbx-gateway` 用户读取。

## 更换或重建 Edge 身份

普通续期由 Edge 自动完成，不需要新令牌。只有私钥丢失、主机替换或确认旧证书不可恢复时，才在在线 PKI 主机创建替换令牌：

```bash
sudo -u dbx-gateway-pki dbx-gateway-pki enrollment create \
  --data-dir /var/lib/dbx-gateway-pki \
  --edge-id edge-prod-01 \
  --ttl 10m \
  --replace --yes
```

然后在新 Edge 主机重复“写入令牌”和“启动并自动领证”两节。不要把旧主机的 `edge.key` 复制到新主机。
