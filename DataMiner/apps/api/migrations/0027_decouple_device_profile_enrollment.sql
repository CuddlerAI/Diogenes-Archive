-- 0027 · 设备登记与 device profile 准入解耦（Issue #453）。
--
-- 设备行只表达稳定身份、credential、状态与撤销。具体游戏要求的 device profile
-- 由已验签 catalog 选定，并在每次录制前由客户端 preflight 实测；服务端会话准入
-- 只认精确 non-revoked catalog identity 及 game_profile 交叉引用。
--
-- 旧列与 device_profile_bindings 保留为历史审计数据，不删除、不回填 NULL，也不再
-- 授权新 session。新登记设备不需要伪造一个与任何游戏相关的 profile 快照。

BEGIN;

ALTER TABLE devices
    ALTER COLUMN certified_profile_id DROP NOT NULL;

COMMENT ON COLUMN devices.certified_profile_id IS
    'Deprecated historical snapshot of the profile lineage recorded by legacy enrollment. Nullable for newly enrolled devices and never used for session authorization.';

COMMENT ON TABLE device_profile_bindings IS
    'Historical audit of legacy device/profile qualifications. Retained without destructive cleanup; session admission no longer reads this table.';

INSERT INTO schema_migrations(version, name)
VALUES (27, 'decouple_device_profile_enrollment');

COMMIT;
