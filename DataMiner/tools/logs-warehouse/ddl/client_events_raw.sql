-- BigLake 外部表：GCS raw 区上的 Day-1 可查面（docs/23 §7.1）。
-- 含重复（at-least-once 落地）；精确计数用原生表 client_events，或自带
-- QUALIFY ROW_NUMBER() OVER (PARTITION BY event_id) = 1。
-- 应用方式：tools/logs-warehouse/apply-ddl.sh <env>（模板变量 ${ENV}）。
CREATE OR REPLACE EXTERNAL TABLE `hakkolite.dataminer_logs_${ENV}.client_events_raw`
(
  export_schema_version STRING,
  event_id STRING,
  device_id STRING,
  annotator_id STRING,
  consent_id STRING,
  session_id STRING,
  occurred_at TIMESTAMP,
  received_at TIMESTAMP,
  kind STRING,
  severity STRING,
  client_version STRING,
  environment STRING,
  payload JSON,
  retention_state STRING
)
WITH PARTITION COLUMNS (dt DATE, hour INT64)
WITH CONNECTION `us-central1.dataminer-logs-gcs`
OPTIONS (
  format = 'NEWLINE_DELIMITED_JSON',
  -- gzip 而非 zstd：BQ 的 NDJSON 外部表只认 gzip（docs/23 §6）。
  uris = ['gs://dataminer-logs/${ENV}/client_events/*'],
  hive_partition_uri_prefix = 'gs://dataminer-logs/${ENV}/client_events',
  description = 'DataMiner client behaviour logs, raw landing zone. analytics-only, authoritative:false, may contain duplicates (docs/23).'
);
