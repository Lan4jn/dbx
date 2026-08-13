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

本节只部署用于 Edge 自动领证的在线 CA。Edge 节点实际领证步骤见 [Edge 节点证书生成与领取](edge-certificate.md)，DBX 用户身份签发见 [DBX Client 证书生成与交付](client-certificate.md)。

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

从离线主机通过加密介质只导出以下三个文件：

| 离线主机源文件                                              | 在线主机目标文件                                     |
| ----------------------------------------------------------- | ---------------------------------------------------- |
| `/secure/dbx-gateway-pki-offline/edge/ca.crt.pem`           | `/var/lib/dbx-gateway-pki/edge/ca.crt.pem`           |
| `/secure/dbx-gateway-pki-offline/edge/ca.key.encrypted.pem` | `/var/lib/dbx-gateway-pki/edge/ca.key.encrypted.pem` |
| `/secure/dbx-pki-password`                                  | `/etc/dbx-gateway-pki/password`                      |

假设加密介质在在线主机挂载为 `/mnt/secure-transfer`，执行：

```bash
install -d -o dbx-gateway-pki -g dbx-gateway-pki -m 0700 /var/lib/dbx-gateway-pki/edge
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0644 \
  /mnt/secure-transfer/edge/ca.crt.pem \
  /var/lib/dbx-gateway-pki/edge/ca.crt.pem
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0600 \
  /mnt/secure-transfer/edge/ca.key.encrypted.pem \
  /var/lib/dbx-gateway-pki/edge/ca.key.encrypted.pem
install -o dbx-gateway-pki -g dbx-gateway-pki -m 0600 \
  /mnt/secure-transfer/dbx-pki-password \
  /etc/dbx-gateway-pki/password
```

`/mnt/secure-transfer` 只是示例挂载点。密码目标文件必须与初始化完整 PKI 时使用的 `/secure/dbx-pki-password` 内容相同。

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

以下是命令摘要；完整的文件用途、交付和 DBX 导入步骤见 [DBX Client 证书生成与交付](client-certificate.md)。

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

### 导入 DBX 桌面端

在 DBX 中打开 `设置 > 隧道 > 新增 Gateway`：

1. 在“导入身份”区域填写身份显示名称。
2. 点击“选择 PKCS#12”，选择签发目录中的 `client.p12`。文件选择器只接受 `.p12` 和 `.pfx`。
3. 点击密码框右侧的文件按钮选择 bundle 密码文件，DBX 会自动填入密码，也可手工输入。
4. 确认证书文件已选择且密码框非空，点击该行最右侧的“导入”。
5. 导入成功后，从“客户端身份”下拉框选择该身份。DBX 会立即清空导入密码；PKCS#12 私钥和密码不会写入连接 JSON、SQLite 普通字段、导出文件或云同步快照。
6. 导入专用 Server CA PEM；需要双重固定时再填写 Main Server 公钥的 SPKI SHA-256 Pin。Pin 格式为 64 位小写十六进制，不是证书指纹的 Base64 文本。
7. 点击“测试 Main”。成功只表示 Main URL、客户端证书和服务端 CA/SPKI 校验通过；数据库路由在具体连接的“传输”选项卡中选择。

若提示密码错误或 PKCS#12 无法解析，重新核对 bundle 密码和文件完整性，不要尝试把私钥 PEM 粘贴进 Gateway 档案。若提示身份过期、吊销或不存在，应由 PKI 管理员重新签发，导入新 identity，并把引用该身份的 Gateway 档案迁移后再删除旧 identity。

删除入口仍在 `设置 > 隧道` 的身份列表。DBX 会在确认框中显示引用数量；删除系统钥匙串 identity 后，所有引用它的 Gateway 档案和数据库连接都会拒绝测试/连接，不会从历史配置或缓存回退读取私钥。

### 在连接中选择授权路由

导入身份并保存 Gateway 档案后，新建或编辑数据库连接，进入 `传输 > 添加 Gateway`。选择共享档案并刷新授权路由，按 Edge 分组选择在线 target。离线 Edge 可见但不可选；Main 不可达、身份缺失、route 未选择或 ACL 已移除时，测试和保存都会 fail closed。具体连接只保存 `profile_id`、`edge_id` 和 `target_id`，不会复制 Main URL、CA、SPKI 或身份材料。

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
