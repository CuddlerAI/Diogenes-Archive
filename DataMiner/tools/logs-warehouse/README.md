# logs-warehouse：客户端行为日志仓库工具

设计权威：[docs/23](../../docs/25-client-behavior-logs-warehouse.md)。

| 文件 | 作用 |
|---|---|
| `provision.sh` | GCP 资源（GCS 桶 + 生命周期、BQ 数据集×2、BigLake 连接、导出 SA + IAM）。幂等，已于 2026-07-25 实际执行。 |
| `lifecycle.json` | 桶生命周期：staging/ 60d、production/ 400d 删除兜底（不是删除机制本身，见 docs/23 §7.3）。 |
| `apply-ddl.sh <env> {tables\|schedule\|verify\|all}` | BQ 外部表 + 原生表 + 每小时 MERGE scheduled query + 验收查询。 |
| `ddl/client_events_raw.sql` | BigLake 外部表（Day-1 即查，含重复）。 |
| `ddl/client_events.sql` | 原生表（事件日分区、(kind, device_id) 聚簇、payload JSON、authoritative:false）。 |
| `ddl/load_merge.sql` | raw → 原生的去重 MERGE（回看 2 天）。 |
| `ddl/purge_cascade.sql` | 删除级联的 BQ 段模板（按 consent_id / annotator_id）。 |

已建资源（hakkolite / us-central1）：

- `gs://dataminer-logs`（UBLA、PAP、生命周期规则）
- `dataminer_logs_staging` / `dataminer_logs_production` + 各自的
  `client_events_raw`（外部表）与 `client_events`（原生表）
- BigLake 连接 `us-central1.dataminer-logs-gcs`
- SA `dataminer-log-exporter@hakkolite.iam.gserviceaccount.com`
  （桶 objectAdmin + BQ jobUser + 两数据集 WRITER；org policy 禁 SA key，
  部署用 workload identity，本机用 impersonation）
- scheduled query `dataminer-logs-merge-staging` / `-production`（每小时）
