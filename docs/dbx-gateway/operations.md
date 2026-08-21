# DBX Gateway 运维手册

本手册默认 Main 为 `gateway.example.com:443`，Edge 为 `edge-prod-01`，目标为 `postgres-primary`。生产操作先记录变更单、当前版本、证书 serial 和回滚点。

## 日常检查

在 Main 主机以监控用户执行：

```bash
curl -fsS http://127.0.0.1:9080/healthz
systemctl is-active dbx-gateway-main.service dbx-gateway-pki.service
journalctl -u dbx-gateway-main.service --since '-15 min' --no-pager
```

预期健康 JSON 为 `status: ok`，服务为 `active`。`online_edges` 只表示 Edge 控制通道在线，不代表主动登录数据库；`database_checks` 固定为 0，避免健康探测产生数据库账号、审计和锁负担。

在 Edge 主机执行：

```bash
systemctl is-active dbx-gateway-edge.service
journalctl -u dbx-gateway-edge.service --since '-15 min' --no-pager
ss -tnp | grep dbx-gateway
```

预期 Edge 有到 Main 443 的出站连接；只有 DBX 正在访问时才出现到数据库的连接。数据库侧看到 Edge IP 和 `dbx-gateway` 发起的连接，不会看到原 DBX 客户端 IP。

## 抓包验收

在测试环境准备唯一标记，例如 `DBX_CAPTURE_SQL_7f35` 和 `DBX_CAPTURE_PASSWORD_91ab`，不要使用真实生产凭据。分别在 DBX 客户端侧和 Edge 到 Main 网卡抓包：

```bash
sudo tcpdump -i any -s 0 -w /tmp/dbx-client-main.pcap host gateway.example.com and port 443
sudo tcpdump -i any -s 0 -w /tmp/dbx-edge-main.pcap host gateway.example.com and port 443
```

通过 DBX 连接 `edge-prod-01 / postgres-primary` 执行带测试标记的查询后停止抓包。在抓包主机执行：

```bash
strings /tmp/dbx-client-main.pcap | grep -E 'DBX_CAPTURE_SQL_7f35|DBX_CAPTURE_PASSWORD_91ab'
strings /tmp/dbx-edge-main.pcap | grep -E 'DBX_CAPTURE_SQL_7f35|DBX_CAPTURE_PASSWORD_91ab'
```

预期两个命令均无输出且退出码为 1。抓包仍会显示 IP、端口、连接时间、包长、时序和可能的 SNI，这是 TLS 元数据，不是数据库内容。

在受控 echo target 或测试数据库审计日志中应能看到 SQL 标记，证明数据确实到达目标而不是测试没有发送。Main/Edge 是 TLS 终止端，进程内能读取转发字节；主机 EDR、调试器或 root 用户也可能读取。网络中间节点本身不能从 TLS 包得知“DBX 进程名”，但若节点同时拥有终端代理、NAT/流量日志或主机遥测，可以把五元组关联到进程。

若数据库不支持 TLS，再抓 Edge 到数据库：

```bash
sudo tcpdump -i any -s 0 -A host 10.20.30.40 and port 5432
```

原生协议可能暴露标记。解决方法是启用数据库 TLS、把 Edge 部署到数据库同机并用 Unix Socket，或缩短到可信隔离区；Main 链路加密无法覆盖这段独立连接。

抓包文件包含敏感元数据，验收后安全删除：

```bash
shred -u /tmp/dbx-client-main.pcap /tmp/dbx-edge-main.pcap
```

文件系统不保证 `shred` 的介质（SSD/COW）应使用加密临时盘并销毁密钥。

## 到期监控

Main 健康接口提供 `server_certificate_not_after_unix`。监控系统应在 30 天、14 天和 7 天告警。人工检查：

```bash
openssl x509 -in /etc/dbx-gateway/certs/main.pem -noout -serial -dates -subject -issuer
openssl x509 -in /var/lib/dbx-gateway/edge.pem -noout -serial -dates -subject -issuer
```

第一条在 Main 主机、第二条在 Edge 主机执行，预期 `notAfter` 晚于续期窗口。Main Server 证书由现有 CA/certbot/企业平台续期，当前版本不内置 ACME。替换 PEM 后先 `check-config` 再 HUP；若证书私钥不匹配，重载失败并继续旧证书。

Edge 默认提前 30 天自动续期。监控日志中持续的 renewal 失败、`identity_rejected` 或证书进入 14 天窗口都应告警。已过期或吊销的 Edge 需要 replace token，不能依赖自动续期恢复。

PKI 主机检查 CRL 和状态：

```bash
sudo -u dbx-gateway-pki openssl crl -in /var/lib/dbx-gateway-pki/edge/crl.pem -noout -lastupdate -nextupdate -crlnumber
sudo -u dbx-gateway-pki sqlite3 /var/lib/dbx-gateway-pki/gateway-state.sqlite3 'PRAGMA integrity_check;'
```

CRL 尚未生成时第一条可不存在；SQLite 预期输出 `ok`。不要直接修改表内容。

Main 当前不会自动读取 `edge/crl.pem`；Edge 吊销必须同步加入 Main 的 `revoked_edge_serials` 并成功重载。Main 当前不读取 Client CRL，也没有 Client serial 吊销列表；Client 私钥遗失时应删除旧 identity 的 ACL、使用新 identity 签发替代证书，并重启 Main 终止既有会话。不要把“已生成 CRL”当成访问已经被阻断。

## 故障排查

| 现象/错误 | 含义 | 处理 |
|---|---|---|
| `configuration file could not be read` | systemd unit 的运行账户不能读取配置文件或进入父目录 | 用 `systemctl show <unit> -p User -p Group` 查询实际账户，并以该账户执行 `test -r` 和 `check-config`；不要用 `root` 的读取结果代替。 |
| TLS 握手失败 | CA、有效期、EKU、TLS 版本或证书链错误 | 检查双方时间、证书链和 URI SAN；只支持 TLS 1.3。 |
| `certificate not valid for name "10.x.x.x"; certificate is only valid for DnsName("10.x.x.x")` | IP 被错误写入 DNS SAN | 在离线 PKI 重新签发 Main 证书，只使用 `--ip-san 10.x.x.x`；URL 使用同一个 IP，然后替换 Main 的证书与私钥并重启服务。 |
| `identity_rejected` | 证书角色、唯一 URI SAN、serial 或 CA 不符合 | 用 `openssl x509 -text` 核对 `urn:dbx-gateway:edge:edge-prod-01` 或 client URI。 |
| `edge_offline` | Main 没有在线 Edge 控制通道 | 检查 Edge 服务、DNS、防火墙、Main ACL 和证书。 |
| `route_denied` | target ID 未注册、角色错误或地址策略拒绝 | 核对 `postgres-primary`、`allow_remote` 和解析后的所有 IP。 |
| `target_unavailable` | Edge 到数据库连接失败 | 在 Edge 用数据库原生工具或 `nc -vz` 测试，检查 Unix Socket 权限。 |
| `capacity_exceeded` | 客户端、Edge、帧、连接或缓冲预算超限 | 查并发与连接洪泛，先解决异常来源，再评估配置值。 |
| `protocol_mismatch` | 客户端和 Gateway 主版本不兼容或控制帧非法 | 让 DBX App 与 Gateway 跟随同一官方版本线。 |
| `restart_required` | HUP 修改了不可热变更字段 | 保留旧运行态，安排维护窗口重启。 |
| enrollment token rejected | 令牌过期、消费、撤销或 Edge ID 不符 | 撤销旧 token，创建新的 10 分钟 token；必要时明确 replace。 |

统一采集信息：

```bash
dbx-gateway --version
dbx-gateway-pki --version
systemctl status dbx-gateway-main dbx-gateway-edge dbx-gateway-pki --no-pager
journalctl -u dbx-gateway-main -u dbx-gateway-edge -u dbx-gateway-pki --since '-30 min' --no-pager
```

不同主机只运行其对应命令。日志字段仅允许 request ID、角色、证书 serial、Edge/target ID、阶段、字节计数和错误码，不应包含 SQL、token、PEM、密码或帧内容。若发现秘密出现在日志，按安全事件处理并停止上传日志。

连接洪泛时先在防火墙或上游 TCP 负载均衡按来源限速；程序内令牌桶、总连接 semaphore、每身份/Edge 并发和全局缓冲预算作为第二层 fail-closed 保护。不要简单把所有上限调到极大。

## 备份、升级和恢复

每日备份 Main/Edge 配置、证书和版本记录；PKI 按 [PKI 备份与恢复](pki.md#备份与恢复) 执行。Main 不保存目标真实地址，最后已知 Edge/route 只能作为离线元数据展示，Edge 重新注册前不能建立新会话。

升级顺序建议 PKI、Main、Edge、DBX 客户端，每一步验证健康后继续。协议主版本不兼容会 fail closed；同一 `0.5.x` 发布内按发布说明确认兼容性。回滚二进制时同步恢复对应配置，但不要回滚或删除较新的吊销记录。

## 卸载

先确认不再有活动 DBX 会话并完成必要备份。在对应主机以 `root` 执行：

```bash
systemctl disable --now dbx-gateway-main.service dbx-gateway-edge.service dbx-gateway-pki.service 2>/dev/null || true
rm -f /etc/systemd/system/dbx-gateway-main.service
rm -f /etc/systemd/system/dbx-gateway-edge.service
rm -f /etc/systemd/system/dbx-gateway-pki.service
systemctl daemon-reload
rm -f /usr/bin/dbx-gateway /usr/bin/dbx-gateway-pki
```

仅在备份验证完成且吊销相关证书后删除数据：

```bash
rm -rf /etc/dbx-gateway /var/lib/dbx-gateway
rm -rf /etc/dbx-gateway-pki /var/lib/dbx-gateway-pki
userdel dbx-gateway 2>/dev/null || true
userdel dbx-gateway-pki 2>/dev/null || true
groupdel dbx-gateway 2>/dev/null || true
```

不要删除唯一的离线 Root/CA 备份。卸载 Edge 前生成 CRL 并把 serial 加入 Main blocklist；卸载 DBX Client 前按上面的 Client ACL 流程移除访问，不要只运行未被 Main 消费的 Client CRL 命令。
