-- 删除级联的 BQ 段（docs/23 §7.3 第 3 层）。
-- consent 撤回 purge 时由级联作业执行；@consent_id 为受影响 consent。
-- 外部表(raw)不在此删——raw 收敛靠 GCS 对象重写 + 生命周期 TTL 兜底。
DELETE FROM `hakkolite.dataminer_logs_${ENV}.client_events`
WHERE consent_id = @consent_id;

-- annotator tombstone 级联（覆盖 consent_id IS NULL 的 enroll 前事件）：
-- DELETE FROM `hakkolite.dataminer_logs_${ENV}.client_events`
-- WHERE annotator_id = @annotator_id;
