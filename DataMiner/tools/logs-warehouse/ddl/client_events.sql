-- 原生表：去重后的分析面（docs/23 §7.1）。
-- 按事件日分区、(kind, device_id) 聚簇；payload JSON 兜底列 + schema_version 列。
-- analytics-only：label authoritative=false，任何治理判断以 Cloud SQL 为准。
CREATE TABLE IF NOT EXISTS `hakkolite.dataminer_logs_${ENV}.client_events`
(
  event_id STRING NOT NULL,
  event_date DATE NOT NULL,
  occurred_at TIMESTAMP NOT NULL,
  received_at TIMESTAMP,
  device_id STRING,
  annotator_id STRING,
  consent_id STRING,
  session_id STRING,
  kind STRING NOT NULL,
  severity STRING,
  client_version STRING,
  environment STRING,
  export_schema_version STRING,
  payload JSON,
  retention_state STRING,
  loaded_at TIMESTAMP
)
PARTITION BY event_date
CLUSTER BY kind, device_id
OPTIONS (
  description = 'DataMiner client behaviour logs, deduplicated by event_id. analytics-only replica; never feeds governance or trainability (docs/23 §7.2).',
  labels = [('authoritative', 'false'), ('managed-by', 'dataminer-logs-warehouse')]
);
