-- 0034 · session 根产物跟上 recording profile 合并（issue #825 后续故障）。
--
-- 0022 把 capture profile + settings profile 合成了 recording profile，但只给
-- catalog_kind 增加了新值，漏掉了 session_artifact_kind。客户端与 API 契约随后都
-- 开始声明 `recording_profile`，生产在补齐 session_artifacts 权限后因此从 42501
-- 继续失败为 22P02（invalid input value for enum）。
--
-- 枚举只加不删：旧 session_artifacts 行仍可能如实携带 capture_profile 或
-- settings_profile，改写或删除旧标签都会篡改历史身份。PostgreSQL 15 允许事务内
-- ADD VALUE；本事务不使用新值，提交后服务请求即可使用。

BEGIN;

ALTER TYPE session_artifact_kind
    ADD VALUE IF NOT EXISTS 'recording_profile';

INSERT INTO schema_migrations(version, name)
VALUES (34, 'session_artifact_recording_profile');

COMMIT;
