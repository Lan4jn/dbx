# PKI 与证书运维

本页的可执行步骤只覆盖同机 Unix Socket：在线 PKI 与 Main 必须位于同一主机。远程 RA mTLS 的配置字段不是完整部署步骤，缺少专用证书体系时不要启用远程监听。

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

Main 与在线 PKI 同机，因此这里同时创建两个服务用户。若组或用户已由 Main 安装步骤创建，先用 `getent group dbx-gateway` 和 `getent passwd <用户名>` 核对 GID、home 与 shell，再跳过对应创建命令；不要删除或重建已有账户。

然后在在线 PKI 主机以 `root` 创建尚不存在的用户和目录：

```bash
groupadd --system dbx-gateway
useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway --shell /usr/sbin/nologin dbx-gateway
useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway-pki --shell /usr/sbin/nologin dbx-gateway-pki
install -d -o root -g dbx-gateway -m 0750 /etc/dbx-gateway-pki
install -d -o dbx-gateway-pki -g dbx-gateway -m 0700 /var/lib/dbx-gateway-pki
install -m 0755 bin/dbx-gateway-pki /usr/bin/dbx-gateway-pki
install -o root -g dbx-gateway -m 0640 examples/pki.toml /etc/dbx-gateway-pki/pki.toml
```

从离线主机通过加密介质只导出以下三个文件：

| 离线主机源文件                                              | 在线主机目标文件                                     |
| ----------------------------------------------------------- | ---------------------------------------------------- |
| `/secure/dbx-gateway-pki-offline/edge/ca.crt.pem`           | `/var/lib/dbx-gateway-pki/edge/ca.crt.pem`           |
| `/secure/dbx-gateway-pki-offline/edge/ca.key.encrypted.pem` | `/var/lib/dbx-gateway-pki/edge/ca.key.encrypted.pem` |
| `/secure/dbx-pki-password`                                  | `/etc/dbx-gateway-pki/password`                      |

假设加密介质在在线主机挂载为 `/mnt/secure-transfer`，执行：

```bash
install -d -o dbx-gateway-pki -g dbx-gateway -m 0700 /var/lib/dbx-gateway-pki/edge
install -o dbx-gateway-pki -g dbx-gateway -m 0644 \
  /mnt/secure-transfer/edge/ca.crt.pem \
  /var/lib/dbx-gateway-pki/edge/ca.crt.pem
install -o dbx-gateway-pki -g dbx-gateway -m 0600 \
  /mnt/secure-transfer/edge/ca.key.encrypted.pem \
  /var/lib/dbx-gateway-pki/edge/ca.key.encrypted.pem
install -o dbx-gateway-pki -g dbx-gateway -m 0600 \
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

预期 `/run/dbx-gateway/pki.sock` 为 `0660`，只有配置的 Main UID/GID 可调用。按本页流程不要启用 TCP 监听，也不要把在线 PKI 暴露为普通 HTTPS 签发 API；远程 RA mTLS 只能在另有完整证书生命周期方案时部署。

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

预期输出 `issued client certificate <serial>`，导出目录包含 `certificate.pem`、`chain.pem`、`private-key.pem` 和 `client.p12`。把 `client.p12` 与密码分渠道交付给 DBX 用户，导入后删除传输副本。设备遗失时必须从 Main ACL 删除旧 identity、重启 Main，并使用新的 Client identity 签发替代证书；不能只重签相同 identity。

### 导入 DBX 桌面端

在 DBX 中打开 `设置 > 隧道 > 新增 Gateway`：

1. 在“导入身份”区域填写身份显示名称。
2. 点击“选择 PKCS#12”，选择签发目录中的 `client.p12`。文件选择器只接受 `.p12` 和 `.pfx`。
3. 点击密码框右侧的文件按钮选择 bundle 密码文件，DBX 会自动填入密码，也可手工输入。
4. 确认证书文件已选择且密码框非空，点击该行最右侧的“导入”。
5. 导入成功后，从“客户端身份”下拉框选择该身份。DBX 会立即清空导入密码；PKCS#12 私钥和密码不会写入连接 JSON、SQLite 普通字段、导出文件或云同步快照。
6. 导入专用 Server CA PEM；需要双重固定时再填写 Main Server 公钥的 SPKI SHA-256 Pin。Pin 格式为 64 位小写十六进制，不是证书指纹的 Base64 文本。
7. 点击“测试”。成功只表示 Main URL、客户端证书和服务端 CA/SPKI 校验通过；数据库路由在具体连接的“传输”选项卡中选择。

若提示密码错误或 PKCS#12 无法解析，重新核对 bundle 密码和文件完整性，不要尝试把私钥 PEM 粘贴进 Gateway 档案。若提示身份过期、吊销或不存在，应由 PKI 管理员重新签发，导入新 identity，并把引用该身份的 Gateway 档案迁移后再删除旧 identity。

删除入口仍在 `设置 > 隧道` 的身份列表。DBX 会在确认框中显示引用数量；删除系统钥匙串 identity 后，所有引用它的 Gateway 档案和数据库连接都会拒绝测试/连接，不会从历史配置或缓存回退读取私钥。

### 在连接中选择授权路由

导入身份并保存 Gateway 档案后，新建或编辑数据库连接，进入 `传输 > 添加 Gateway`。选择共享档案并刷新授权路由，按 Edge 分组选择在线 target。离线 Edge 可见但不可选；Main 不可达、身份缺失、route 未选择或 ACL 已移除时，测试和保存都会 fail closed。具体连接只保存 `profile_id`、`edge_id` 和 `target_id`，不会复制 Main URL、CA、SPKI 或身份材料。

## 续期与吊销

Edge 在证书到期前 `renew_before_days`（默认 30 天）用当前 mTLS 身份提交新 CSR。PKI 从已认证证书读取权威 Edge ID，忽略 CSR 中试图声明的其他 ID；新私钥仍只在 Edge 本地生成。过期或已吊销证书不能续期，必须创建新的 replace token。

手工吊销 Edge，在在线 PKI 主机以 `dbx-gateway-pki` 用户执行：

```bash
sudo -u dbx-gateway-pki dbx-gateway-pki edge revoke \
  --data-dir /var/lib/dbx-gateway-pki \
  --password-file /etc/dbx-gateway-pki/password \
  --state-file /var/lib/dbx-gateway-pki/gateway-state.sqlite3 \
  --serial REPLACE_WITH_EDGE_SERIAL \
  --reason key_compromise
```

`--state-file` 必须与在线 `pki.toml` 完全一致，用于同时阻止该证书续期。预期输出 `revoked edge certificate; CRL number <n>`，并更新签发记录、`edge/crl.pem` 和在线状态。Main 当前不会自动读取 `edge/crl.pem`，因此还必须在 `/etc/dbx-gateway/main.toml` 顶层加入规范化 serial：

```toml
revoked_edge_serials = ["REPLACE_WITH_NORMALIZED_EDGE_SERIAL"]
```

在 Main 主机验证并重载：

```bash
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config
systemctl kill -s HUP dbx-gateway-main.service
journalctl -u dbx-gateway-main.service -n 50 --no-pager
```

成功重载后，对应控制和数据通道会关闭。保留 CRL 作为审计和向其他验证器分发的标准文件，但它不能代替 Main blocklist。

若命令报告 CRL 已更新但在线 revocation state 未更新，当前处于部分提交状态：Main blocklist 步骤仍要执行，并且必须用完全相同的参数重试，直到 SQLite 更新成功。重复使用同一 serial 和 reason 是幂等恢复路径；不要改 reason，也不要删除 CRL、签发记录或 SQLite 行。

Main 当前不读取 Client CRL，也没有 Client serial 吊销列表。`dbx-gateway-pki client revoke` 只能更新离线 PKI 的签发记录和 Client CRL；要阻断遗失的客户端，必须从 Main ACL 移除旧 Client identity、为替代证书使用新 identity，并重启 Main 终止既有会话。具体步骤见 [DBX Client 证书生成与交付](client-certificate.md)。

一次性令牌可在未消费前撤销：

```bash
sudo -u dbx-gateway-pki dbx-gateway-pki enrollment revoke \
  --data-dir /var/lib/dbx-gateway-pki \
  --token-id REPLACE_WITH_TOKEN_UUID
```

`--token-id` 使用 `enrollment create` 输出第一行中的 UUID，不要填写最后一行的 `<Token ID>.<秘密部分>` 完整令牌。预期输出 `revoked enrollment token <uuid>`。完整令牌不存库，无法找回，只能撤销并创建新的 10 分钟令牌。

## 备份与恢复

离线备份应包含完整 `/secure/dbx-gateway-pki-offline`、密码的独立密封副本、签发清单和恢复说明。在线备份集包含下列文件以及单独密封保存的同一 CA 密码；密码不要放进在线数据压缩包：

```text
/var/lib/dbx-gateway-pki/edge
/var/lib/dbx-gateway-pki/gateway-state.sqlite3
/etc/dbx-gateway-pki/pki.toml
```

如果 `pki.toml` 的 `state_file` 位于 `/var/lib/dbx-gateway-pki` 之外，下面的默认压缩命令不会包含它。必须把该文件单独加入同一时间点的备份，并在恢复时写回配置中的绝对路径，设置为 `dbx-gateway-pki:dbx-gateway 0600`。所有 `enrollment create/revoke` 运维命令也必须传相同的 `--state-file`。

在在线 PKI 主机以 `root` 执行一致性备份：

```bash
systemctl stop dbx-gateway-pki.service
tar -C / -czf /secure-backup/dbx-gateway-pki-online.tar.gz \
  var/lib/dbx-gateway-pki etc/dbx-gateway-pki/pki.toml
sha256sum /secure-backup/dbx-gateway-pki-online.tar.gz > /secure-backup/dbx-gateway-pki-online.tar.gz.sha256
systemctl start dbx-gateway-pki.service
```

预期服务恢复 active，checksum 文件单独保存。恢复到已创建 `dbx-gateway` 组、`dbx-gateway` 用户和 `dbx-gateway-pki` 用户的新主机时，以 `root` 执行：

```bash
systemctl stop dbx-gateway-main.service dbx-gateway-pki.service
sha256sum -c /secure-backup/dbx-gateway-pki-online.tar.gz.sha256
RESTORE_ROOT=$(mktemp -d)
tar -xzf /secure-backup/dbx-gateway-pki-online.tar.gz -C "$RESTORE_ROOT"
install -d -o root -g dbx-gateway -m 0750 /etc/dbx-gateway-pki
install -d -o dbx-gateway-pki -g dbx-gateway -m 0700 /var/lib/dbx-gateway-pki
cp -a "$RESTORE_ROOT/var/lib/dbx-gateway-pki/." /var/lib/dbx-gateway-pki/
chown -R dbx-gateway-pki:dbx-gateway /var/lib/dbx-gateway-pki
install -o root -g dbx-gateway -m 0640 \
  "$RESTORE_ROOT/etc/dbx-gateway-pki/pki.toml" \
  /etc/dbx-gateway-pki/pki.toml
install -o dbx-gateway-pki -g dbx-gateway -m 0600 \
  /mnt/secure-transfer/dbx-pki-password \
  /etc/dbx-gateway-pki/password
rm -rf "$RESTORE_ROOT"
systemctl start dbx-gateway-pki.service
systemctl start dbx-gateway-main.service
systemctl is-active dbx-gateway-pki.service dbx-gateway-main.service
```

`/mnt/secure-transfer/dbx-pki-password` 必须来自与该 Edge CA 匹配的密封密码副本。不要直接依赖压缩包中的旧数字 UID/GID；上面的 `chown` 按新主机账户重新设置属主。SQLite 和 `edge/issued` 必须来自同一备份时间点；不一致时停止在线签发，从可信离线记录和证书清单重建，不要猜测或删除吊销记录。

若 Edge CA 私钥泄露，普通备份恢复不够：离线生成新 PKI/Edge CA，替换 Main 信任链，为每个 Edge 发 replace token 重新领证，并撤销旧链。
