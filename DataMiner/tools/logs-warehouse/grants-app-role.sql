-- /app/v1 面角色（dataminer_app_<env>）为 client-events 导出管线需要的增量授权。
-- 环境级 GRANT，不进 schema migration（11 §S1b 的约定：角色授权随部署环境走，
-- PGlite 验证不了）。以 schema owner（dataminer_console_<env>）身份执行：
--
--   psql ... -U dataminer_console_staging -d dataminer_staging -f grants-app-role.sql
--
-- 2026-07-25 已在 staging 实际执行。本文件只管 staging：production 的
-- `dataminer_app`（无后缀）以 apps/console-api/scripts/grant-console-roles.sql
-- 为权威（2026-07-28 起含 outbox_jobs INSERT），启用摄取前重跑该文件即可，
-- 不要在 production 上跑本文件。

-- recordClientEvent 的单语句 CTE（docs/23 §5）：
-- INSERT + RETURNING 需要 SELECT（RETURNING 读被插入行的列）。
GRANT SELECT ON client_events TO dataminer_app_staging;
-- 同事务产出导出 outbox 行。仅 INSERT：app 面永远不认领/消费 outbox。
GRANT INSERT ON outbox_jobs TO dataminer_app_staging;
