# DBX Client 证书生成与交付

本文只说明 DBX 桌面客户端身份的签发、文件用途和导入。Client 证书必须由保存完整 PKI 的离线主机签发，不能由只持有 Edge CA 的在线 PKI 签发。

## 密码和文件不要混淆

| 路径或文件 | 含义 | 谁可以拿到 |
|---|---|---|
| `/secure/dbx-pki-password` | 解密 CA 私钥的密码 | 仅 PKI 管理员 |
| `/secure/client-bundle-password` | 解锁本次 `client.p12` 的导入密码 | 对应 DBX 用户 |
| `client.p12` | Client 证书、私钥和证书链的 PKCS#12 包 | 对应 DBX 用户 |
| Main 签发输出的 `chain.pem` | DBX 用来验证 Main Server 的 CA PEM | DBX 用户 |

CA 密码和 bundle 密码不是同一个密码。不要把 `/secure/dbx-pki-password` 交给用户。

## 1. 创建本次 PKCS#12 的 bundle 密码

在 **离线 PKI 主机**执行：

```bash
umask 077
openssl rand -base64 32 > /secure/client-bundle-password
chmod 0600 /secure/client-bundle-password
```

这个命令只创建密码文件，不创建证书。

## 2. 签发 DBX Client 身份

仍在 **离线 PKI 主机**执行：

```bash
dbx-gateway-pki client issue \
  --data-dir /secure/dbx-gateway-pki-offline \
  --password-file /secure/dbx-pki-password \
  --bundle-password-file /secure/client-bundle-password \
  --identity desktop-prod \
  --output-dir /secure/export/desktop-prod
```

参数含义：

| 参数 | 读取或生成什么 |
|---|---|
| `--data-dir` | 读取完整离线 PKI |
| `--password-file` | 读取 CA 密码，用于签发 |
| `--bundle-password-file` | 读取 `client.p12` 的保护密码 |
| `--identity` | 写入 Client 证书身份，并用于 Main ACL |
| `--output-dir` | 创建新的输出目录并写入证书文件 |

输出文件：

| 输出文件 | 用途 |
|---|---|
| `/secure/export/desktop-prod/client.p12` | DBX 桌面端应导入的文件 |
| `/secure/export/desktop-prod/certificate.pem` | Client 叶证书，DBX 导入时不需要单独选择 |
| `/secure/export/desktop-prod/chain.pem` | Client CA 链，DBX 导入时不需要单独选择 |
| `/secure/export/desktop-prod/private-key.pem` | Client 私钥 PEM，DBX 导入时不需要单独选择 |

DBX 桌面端只需要 `client.p12` 和它的 bundle 密码。PEM 文件应留在 PKI 管理范围，不要与 `.p12` 一起散发。

## 3. 准备 Main Server CA PEM

DBX 还需要验证 Main Server。应交付 Main Server 签发命令输出的：

```text
/secure/export/main-server/chain.pem
```

不要选以下文件：

- `desktop-prod/chain.pem`：这是 Client 证书链，不是 Main Server CA。
- `main-server/certificate.pem`：这是 Main 叶证书，不是推荐导入的 CA 链。
- `main-server/private-key.pem`：这是 Main 私钥，绝不能交给客户端。
- `edge/ca.crt.pem`：只用于验证 Edge 身份。

## 4. 分渠道交付

向用户交付三项：

1. `/secure/export/desktop-prod/client.p12`
2. `/secure/client-bundle-password` 文件中的密码文本
3. `/secure/export/main-server/chain.pem`

建议通过受控文件通道发送 `client.p12` 和 `chain.pem`，通过另一套安全通信方式发送 bundle 密码。不要把 CA 密码一起发送。

## 5. 在 DBX 桌面端导入

1. 打开 `设置 > 隧道`，新增 Gateway。
2. 在“导入身份”中点击“选择 PKCS#12”，选择 `client.p12`。
3. 点击密码框右侧的文件按钮，选择 `/secure/client-bundle-password` 的交付副本；DBX 会读取文件并自动填入密码框。也可以手工输入 bundle 密码。
4. 确认已选择 PKCS#12 且密码框非空，点击该行最右侧的“导入”。
5. 在 Gateway 档案中选择刚导入的 Client 身份。
6. Main URL 填写 `wss://gateway.example.com/_dbx/client`。
7. “Main Server CA PEM”选择 `/secure/export/main-server/chain.pem` 的交付副本。
8. 点击“测试”，成功后保存。

没有域名时，Main URL 可以填写 `wss://192.0.2.53/_dbx/client`，但 Main Server 证书必须通过 `--ip-san 192.0.2.53` 签发。把 IP 写进 `--dns-san` 不等价，测试会因证书名称不匹配而失败。

`client.p12` 导入成功后，Client 私钥进入操作系统凭据存储。bundle 密码只用于导入，不是以后每次连接 Main 时输入的登录密码。

## 6. 配置 Main ACL

`--identity desktop-prod` 对应 Main 配置中的 Client ACL 身份：

```toml
[client_route_acl]
desktop-prod = ["edge-prod-01/postgres-primary"]
```

修改 Main 配置后先执行 `check-config`，再向服务发送 HUP。身份拼写必须完全一致。
