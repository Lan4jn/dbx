# PKI 与证书运维

DBX Gateway 使用角色隔离证书：Server 证书只用于 Main 服务端，DBX Client 和 Edge 都是 `clientAuth`，并分别由 Client CA 与 Edge CA 签发。证书身份放在唯一 URI SAN 中，不能仅靠 CN 冒用。

## 离线 Root CA

在断网、磁盘加密且有离线备份的 PKI 初始化主机，以专用管理员执行：

```bash
umask 077
printf '%s\n' 'REPLACE_WITH_LONG_RANDOM_CA_PASSWORD' > /secure/dbx-pki-password
chmod 0600 /secure/dbx-pki-password
dbx-gateway-pki init \
  --data-dir /secure/dbx-gateway-pki-offline \
  --password-file /secure/dbx-pki-password
```

预期输出 `initialized DBX Gateway PKI`，目录权限为 `0700`，并生成 `root`、`server`、`edge`、`client` 四个角色目录。所有 CA 私钥均以 PKCS#8 加密保存，文件权限 `0600`。初始化拒绝覆盖已有目录；失败时检查目标目录是否为空，不要删除未知 PKI 后重试。

Root 私钥和完整库始终留在离线主机。Server/Client 的签发、续期和吊销也在该主机完成。在线主机只接收 `edge` 子目录，不能获得：

```text
root/ca.key.encrypted.pem
server/ca.key.encrypted.pem
client/ca.key.encrypted.pem
```

Root CA 有效期长于三个中间 CA。叶证书有效期不能超过签发中间 CA，工具会 fail closed。

## 在线 Edge CA

先在在线 PKI 主机以 `root` 创建用户和目录：

```bash
groupadd --system dbx-gateway
useradd --system --home-dir /var/lib/dbx-gateway-pki --shell /usr/sbin/nologin dbx-gateway-pki
usermod -a -G dbx-gateway dbx-gateway-pki
install -d -o root -g dbx-gateway-pki -m 0750 /etc/dbx-gateway-pki
install -d -o dbx-gateway-pki -g dbx-gateway-pki -m 0700 /var/lib/dbx-gateway-pki
install -m 0755 bin/dbx-gateway-pki /usr/bin/dbx-gateway-pki
install -m 0640 examples/pki.toml /etc/dbx-gateway-pki/pki.toml
```

从离线主机通过加密介质只导出 `edge/ca.crt.pem` 和 `edge/ca.key.encrypted.pem`。在在线主机执行：

```bash
install -d -o dbx-gateway-pki -g dbx-gateway-pki -m 0700 /var/lib/dbx-gateway-pki/edge
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0644 ca.crt.pem /var/lib/dbx-gateway-pki/edge/
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0600 ca.key.encrypted.pem /var/lib/dbx-gateway-pki/edge/
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0600 /secure-transfer/password /etc/dbx-gateway-pki/password
```

在线 `serve` 使用专门的 Edge-only 打开模式；缺少 Root/Server/Client 目录是正常状态。若在线目录出现这些私钥，立即停止服务、按密钥暴露事件处理并轮换相应 CA。

查询 Main 服务用户 UID/GID 并修改 `pki.toml`：

```bash
id -u dbx-gateway
id -g dbx-gateway
```

预期输出数字，把它们分别写入 `allowed_uid` 与 `allowed_gid`。安装并启动：

```bash
install -m 0644 systemd/dbx-gateway-pki.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now dbx-gateway-pki.service
systemctl status dbx-gateway-pki.service --no-pager
```

预期 `/run/dbx-gateway/pki.sock` 为 `0660`，只有配置的 Main UID/GID 可调用。远程部署必须使用 RA mTLS，并在 `allowed_ra_uri_sans` 中固定 Main RA 身份；不要把在线 PKI 暴露为普通 HTTPS 签发 API。

## DBX Client PKCS#12

在离线 PKI 主机以 PKI 管理员执行：

```bash
umask 077
printf '%s\n' 'REPLACE_WITH_BUNDLE_PASSWORD' > /secure/client-bundle-password
dbx-gateway-pki client issue \
  --data-dir /secure/dbx-gateway-pki-offline \
  --password-file /secure/dbx-pki-password \
  --bundle-password-file /secure/client-bundle-password \
  --identity desktop-admin-01 \
  --output-dir /secure/export/desktop-admin-01
```

预期输出 `issued client certificate <serial>`，导出目录包含 `certificate.pem`、`chain.pem`、`private-key.pem` 和 `client.p12`。把 `client.p12` 与密码分渠道交付给 DBX 用户，导入后删除传输副本。遗失设备时按 Client 角色吊销对应 serial，并重新签发新 identity 或新证书。

## 续期与吊销

Edge 在证书到期前 `renew_before_days`（默认 30 天）用当前 mTLS 身份提交新 CSR。PKI 从已认证证书读取权威 Edge ID，忽略 CSR 中试图声明的其他 ID；新私钥仍只在 Edge 本地生成。过期或已吊销证书不能续期，必须创建新的 replace token。

手工吊销 Edge，在在线 PKI 主机以 `dbx-gateway-pki` 用户执行：

```bash
dbx-gateway-pki edge revoke \
  --data-dir /var/lib/dbx-gateway-pki \
  --password-file /etc/dbx-gateway-pki/password \
  --serial REPLACE_WITH_EDGE_SERIAL \
  --reason key_compromise
```

预期输出 `revoked edge certificate; CRL number <n>`，并原子更新签发记录和 `edge/crl.pem`。将规范化 serial 加入 Main 的 `revoked_edge_serials`，验证配置后向 Main 发送 HUP；对应控制和数据通道会关闭。保留 CRL 作为审计和向其他验证器分发的标准文件。

一次性令牌可在未消费前撤销：

```bash
dbx-gateway-pki enrollment revoke \
  --data-dir /var/lib/dbx-gateway-pki \
  --token-id REPLACE_WITH_TOKEN_UUID
```

预期输出 `revoked enrollment token <uuid>`。明文 token 不存库，无法找回，只能撤销并创建新的 10 分钟 token。

## 备份与恢复

离线备份应包含完整 `/secure/dbx-gateway-pki-offline`、密码的独立密封副本、签发清单和恢复说明。在线备份包含：

```text
/var/lib/dbx-gateway-pki/edge
/var/lib/dbx-gateway-pki/gateway-state.sqlite3
/etc/dbx-gateway-pki/pki.toml
```

在在线 PKI 主机以 `root` 执行一致性备份：

```bash
systemctl stop dbx-gateway-pki.service
tar -C / -czf /secure-backup/dbx-gateway-pki-online.tar.gz \
  var/lib/dbx-gateway-pki etc/dbx-gateway-pki/pki.toml
sha256sum /secure-backup/dbx-gateway-pki-online.tar.gz > /secure-backup/dbx-gateway-pki-online.tar.gz.sha256
systemctl start dbx-gateway-pki.service
```

预期服务恢复 active，checksum 文件单独保存。恢复到新主机时先校验 SHA-256，按原 UID/GID 和权限解包，再启动 PKI、Main，最后让 Edge 重连。SQLite 和 `edge/issued` 必须来自同一备份时间点；不一致时停止在线签发，从可信离线记录和证书清单重建，不要猜测或删除吊销记录。

若 Edge CA 私钥泄露，普通备份恢复不够：离线生成新 PKI/Edge CA，替换 Main 信任链，为每个 Edge 发 replace token 重新领证，并撤销旧链。
