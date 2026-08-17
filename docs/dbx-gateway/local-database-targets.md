# Edge 本机数据库目标配置

本文说明数据库服务与 Edge Gateway 部署在同一台 Linux 主机时，如何配置 `edge.toml`，以及 DBX 支持的常见数据库应选择哪个本机监听端口。

## 地址含义

Edge 目标中的 `127.0.0.1` 和 `::1` 指 **Edge 主机自身**。DBX 客户端不需要能够访问这个回环地址，Main Gateway 也不会连接它。实际链路是：

```text
DBX -> Main Gateway -> Edge Gateway -> 127.0.0.1:数据库端口
```

本机 TCP 目标推荐保留 `allow_remote = false`：

```toml
[targets.postgres-local]
display_name = "PostgreSQL Local"
address = "127.0.0.1:5432"
allow_remote = false
```

IPv6 回环地址必须带方括号：

```toml
address = "[::1]:5432"
```

`127.0.0.1`、`localhost`、`::1` 都会被 Gateway 识别为 loopback，不需要设置 `allow_remote = true`。不要写 `0.0.0.0` 或 `[::]`；它们是监听地址，不是安全的连接目标。

若数据库支持 Unix Socket，优先使用 Socket，可避免 Edge 到数据库之间产生 TCP 流量。例如 PostgreSQL：

```toml
[targets.postgres-local]
display_name = "PostgreSQL Local Socket"
address = { unix = "/run/postgresql/.s.PGSQL.5432" }
```

Socket 路径必须对运行 Edge 的 `dbx-gateway` 用户可访问。MySQL/MariaDB 也可以使用实际的 MySQL Socket 路径；路径因发行版和数据库配置而异。

## 配置多个目标

一个 Edge 可以注册多个逻辑目标。每个 `[targets.<target-id>]` 对应一个固定 TCP 端口或 Unix Socket：

```toml
[targets.mysql-local]
display_name = "MySQL Local"
address = "127.0.0.1:3306"
allow_remote = false

[targets.postgres-local]
display_name = "PostgreSQL Local"
address = "127.0.0.1:5432"
allow_remote = false

[targets.redis-local]
display_name = "Redis Standalone Local"
address = "127.0.0.1:6379"
allow_remote = false
```

`target-id` 只能使用字母、数字、`-`、`_`、`.`，并应长期保持稳定。修改显示名不会影响 DBX 连接；删除或更改 target ID 会使引用该路由的连接失效。

修改 `/etc/dbx-gateway/edge.toml` 后先检查配置，再重启 Edge：

```bash
sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/edge.toml check-config
systemctl restart dbx-gateway-edge.service
systemctl status dbx-gateway-edge.service --no-pager
```

## 关系型数据库

下表端口来自 DBX 当前内置连接模板。若数据库实际修改过端口，应在 Edge target 和 DBX 连接中使用实际值。

| DBX 数据库或兼容产品 | 默认端口 | Edge `address` 示例 | 备注 |
|---|---:|---|---|
| MySQL、MariaDB、GreatSQL、PolarDB MySQL、TDSQL、GoldenDB、Dolt | 3306 | `127.0.0.1:3306` | 使用对应的 MySQL 兼容驱动配置。 |
| TiDB | 4000 | `127.0.0.1:4000` | SQL 端口。 |
| OceanBase MySQL / Oracle 模式 | 2883 | `127.0.0.1:2883` | 在 DBX 中选择正确的兼容模式。 |
| PostgreSQL、Apache Cloudberry、openGauss、GaussDB、Vastbase | 5432 | `127.0.0.1:5432` | 也可使用 PostgreSQL Unix Socket。 |
| CockroachDB、KWDB | 26257 | `127.0.0.1:26257` | SQL 端口。 |
| QuestDB | 8812 | `127.0.0.1:8812` | PostgreSQL Wire Protocol 端口。 |
| SQL Server | 1433 | `127.0.0.1:1433` | 命名实例使用固定 TCP 端口后再配置 target。 |
| Oracle | 1521 | `127.0.0.1:1521` | DBX 必须使用 Service Name 或 SID 模式；TNS 模式不能与 Gateway 组合。 |
| ClickHouse | 8123 | `127.0.0.1:8123` | DBX 当前使用 HTTP 接口。 |
| Doris、SelectDB、StarRocks | 9030 | `127.0.0.1:9030` | MySQL 协议端口。 |
| Manticore Search | 9306 | `127.0.0.1:9306` | MySQL 协议端口。 |
| Databend | 8000 | `127.0.0.1:8000` | 使用实际启用的 DBX 接口端口。 |
| Amazon Redshift | 5439 | `127.0.0.1:5439` | 本机部署较少见，表中仅表示端口映射方法。 |
| 达梦 Dameng | 5236 | `127.0.0.1:5236` | 数据库监听端口。 |
| 人大金仓 KingbaseES | 54321 | `127.0.0.1:54321` | 数据库监听端口。 |
| 瀚高 HighGo | 5866 | `127.0.0.1:5866` | 数据库监听端口。 |
| 优炫 UXDB | 52025 | `127.0.0.1:52025` | 数据库监听端口。 |
| 崖山 YashanDB | 1688 | `127.0.0.1:1688` | 数据库监听端口。 |
| GBase 8a | 5258 | `127.0.0.1:5258` | 使用 GBase 8a 驱动配置。 |
| GBase 8s、Informix | 9088 | `127.0.0.1:9088` | 还需在 DBX 中填写对应 Server 名称。 |
| SAP HANA | 30015 | `127.0.0.1:30015` | 实际端口可能随实例号变化。 |
| Teradata | 1025 | `127.0.0.1:1025` | 使用实际数据库端口。 |
| Vertica | 5433 | `127.0.0.1:5433` | 数据库监听端口。 |
| Firebird | 3050 | `127.0.0.1:3050` | 数据库监听端口。 |
| Exasol | 8563 | `127.0.0.1:8563` | 数据库监听端口。 |
| H2 Server | 9092 | `127.0.0.1:9092` | 只适用于 H2 TCP Server 模式，不适用于本地文件模式。 |
| IBM DB2 | 50000 | `127.0.0.1:50000` | 使用实例实际服务端口。 |
| Dremio | 31010 | `127.0.0.1:31010` | DBX 内置 Dremio JDBC 模板端口。 |
| IRIS | 1972 | `127.0.0.1:1972` | SuperServer 端口。 |
| 科蓝 SUNDB | 22000 | `127.0.0.1:22000` | 数据库监听端口。 |
| 神通 OSCAR | 2003 | `127.0.0.1:2003` | 数据库监听端口。 |
| 虚谷 XuguDB | 5138 | `127.0.0.1:5138` | 数据库监听端口。 |
| 自定义 JDBC | 按驱动 | `127.0.0.1:<port>` | 仅适用于可归结为单个固定 TCP 端点的 JDBC URL。 |

## 搜索、时序与其他服务

| DBX 类型 | 默认端口 | Edge `address` 示例 | 备注 |
|---|---:|---|---|
| Redis Standalone | 6379 | `127.0.0.1:6379` | 推荐仅用于 standalone 或固定单入口代理。 |
| MongoDB | 27017 | `127.0.0.1:27017` | 推荐 standalone、direct connection 或固定单入口代理。 |
| RQLite | 4001 | `127.0.0.1:4001` | HTTP API 端口。 |
| Elasticsearch、Easysearch | 9200 | `127.0.0.1:9200` | HTTP API 端口。 |
| Meilisearch | 7700 | `127.0.0.1:7700` | HTTP API 端口。 |
| HBase REST | 8080 | `127.0.0.1:8080` | DBX 当前使用 HBase REST 接口。 |
| Qdrant | 6333 | `127.0.0.1:6333` | HTTP API 端口。 |
| Milvus | 19530 | `127.0.0.1:19530` | 客户端服务端口。 |
| Weaviate | 8080 | `127.0.0.1:8080` | HTTP API 端口。 |
| ChromaDB | 8000 | `127.0.0.1:8000` | HTTP API 端口。 |
| Neo4j | 7687 | `127.0.0.1:7687` | Bolt 端口。 |
| Cassandra | 9042 | `127.0.0.1:9042` | 单入口或固定代理最稳妥。 |
| TDengine | 6041 | `127.0.0.1:6041` | DBX 当前使用 REST/WebSocket 服务端口。 |
| Apache IoTDB | 6667 | `127.0.0.1:6667` | 客户端服务端口。 |
| InfluxDB | 8086 | `127.0.0.1:8086` | HTTP API 端口。 |
| VictoriaMetrics | 8428 | `127.0.0.1:8428` | HTTP API 端口。 |
| etcd | 2379 | `127.0.0.1:2379` | Client API 端口。 |
| ZooKeeper | 2181 | `127.0.0.1:2181` | Client 端口。 |
| Trino、PrestoSQL | 8080 | `127.0.0.1:8080` | Coordinator HTTP 端口。 |
| Apache Hive | 10000 | `127.0.0.1:10000` | HiveServer2 端口。 |
| Apache Spark Thrift Server | 10015 | `127.0.0.1:10015` | 使用实际 Thrift Server 端口。 |
| Apache Kylin | 7070 | `127.0.0.1:7070` | HTTP API 端口。 |
| MQTT | 1883 | `127.0.0.1:1883` | 适用于固定 TCP Broker；TLS Broker 常用端口以实际配置为准。 |
| Nacos | 8848 | `127.0.0.1:8848` | 适用于固定单入口服务。 |
| Consul | 8500 | `127.0.0.1:8500` | HTTP API 端口。 |

## 多节点与多端口限制

Gateway target 是固定端点，不是通用 SOCKS5 代理。以下连接可能在首次连接后访问服务端返回的其他节点或额外端口：

- Redis Sentinel 和 Redis Cluster；
- MongoDB Replica Set、Sharded Cluster；
- Kafka、RocketMQ、Pulsar 等消息队列；
- RabbitMQ 同时使用 AMQP 与 Management API；
- Cassandra、Milvus 等由客户端发现多个节点的部署模式；
- 任何在握手或元数据中返回其他主机地址的驱动。

这类场景不要只配置一个 loopback target 后假设所有节点都会自动经过 Gateway。优先采用以下方式之一：

1. 在 Edge 主机部署数据库官方的单入口代理、负载均衡器或协调入口，并把 target 指向该固定本机端口。
2. 对明确只需要一个固定端口的管理功能单独建立 target，并在 DBX 中创建独立连接。
3. 当前版本明确拒绝 RocketMQ 把 DBX Gateway 当作动态 SOCKS5 层；此类需求继续使用 DBX 支持的 SSH SOCKS5 或独立代理方式。

## 不适用类型

SQLite、DuckDB、Microsoft Access、H2 文件模式等文件型数据库不能通过一个 TCP Gateway target 访问 Edge 主机上的数据库文件。DBX 文件驱动在客户端进程或本地驱动进程中打开文件，Gateway 不提供远程文件系统。

Turso、Cloudflare D1、Databricks SQL、Snowflake、BigQuery 等云服务通常也不属于“Edge 同机数据库”。只有在 Edge 主机上另行部署了受控的固定 TCP/HTTPS 代理时，才应把该代理端口注册为 target。

## DBX 客户端配置

1. 在 `设置 > 隧道` 中创建并测试 Gateway 档案。
2. 新建数据库连接，数据库类型、账号、密码、数据库名、TLS 参数按原数据库填写。
3. 主机和端口建议填写与 Edge target 对应的回环地址和端口，便于管理员识别；启用 Gateway 后，最终目标由所选 Edge route 决定。
4. 在“传输”选项卡添加 Gateway，选择对应档案，刷新路由并选择 `Edge ID / target ID`。
5. 点击“测试连接”。链路节点应显示 DBX、Main、Edge 和数据库目标的状态。

Edge target 不保存数据库账号和密码，也不会识别数据库协议。它只建立到指定 TCP/Unix 端点的透明字节流；认证、数据库 TLS 和驱动行为仍由 DBX 数据库连接配置决定。
