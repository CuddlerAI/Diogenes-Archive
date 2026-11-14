-- 定时 load：raw 外部表 → 原生表，MERGE 即去重（docs/23 §7.1）。
-- 每小时跑一次（scheduled query，SA dataminer-log-exporter@）。回看 2 天的
-- raw 分区：导出退避上限 1h，重放/补投都远在窗口内；更老的补投靠手工重放。
-- 同一批重复投递、导出器重放、GCS 对象重传 —— 最终原生表一行。
MERGE `hakkolite.dataminer_logs_${ENV}.client_events` T
USING (
  SELECT * EXCEPT (rn, dt, hour)
  FROM (
    SELECT r.*,
           ROW_NUMBER() OVER (PARTITION BY r.event_id ORDER BY r.received_at DESC) AS rn
    FROM `hakkolite.dataminer_logs_${ENV}.client_events_raw` r
    WHERE r.dt BETWEEN DATE_SUB(CURRENT_DATE('UTC'), INTERVAL 2 DAY) AND CURRENT_DATE('UTC')
  )
  WHERE rn = 1
) S
ON T.event_id = S.event_id
WHEN NOT MATCHED THEN
  INSERT (event_id, event_date, occurred_at, received_at, device_id, annotator_id,
          consent_id, session_id, kind, severity, client_version, environment,
          export_schema_version, payload, retention_state, loaded_at)
  VALUES (S.event_id, DATE(S.occurred_at), S.occurred_at, S.received_at, S.device_id,
          S.annotator_id, S.consent_id, S.session_id, S.kind, S.severity,
          S.client_version, S.environment, S.export_schema_version, S.payload,
          S.retention_state, CURRENT_TIMESTAMP());
