-- 0025 · 一台设备可持有多条 device profile 资格。
--
-- `devices.certified_profile_id` 是 0021 为了解开「profile 升版导致全部设备失效」
-- 加入的谱系钉扣，但它仍然是单值：同一台采集机切换到另一个游戏专属 profile 时，
-- 摄取面会把合法目录声明拒绝成 device_profile_mismatch。
--
-- 新表把稳定设备身份与 profile 资格拆开。旧列暂时保留为首个/历史钉扣，供尚未迁移
-- 的只读界面和运维查询兼容；摄取授权从本迁移起只认本表中的 active binding。

BEGIN;

CREATE TABLE device_profile_bindings (
    device_id uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    profile_id text NOT NULL,
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'revoked')),
    validated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    validation_method text NOT NULL,
    validation_evidence jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    revoked_at timestamptz,
    PRIMARY KEY (device_id, profile_id),
    CONSTRAINT device_profile_bindings_profile_not_blank
        CHECK (btrim(profile_id) <> ''),
    CONSTRAINT device_profile_bindings_method_not_blank
        CHECK (btrim(validation_method) <> ''),
    CONSTRAINT device_profile_bindings_status_revocation_pair
        CHECK (
            (status = 'active' AND revoked_at IS NULL)
            OR (status = 'revoked' AND revoked_at IS NOT NULL)
        ),
    CONSTRAINT device_profile_bindings_evidence_object
        CHECK (jsonb_typeof(validation_evidence) = 'object')
);

CREATE INDEX device_profile_bindings_active_profile
    ON device_profile_bindings(profile_id, device_id)
    WHERE status = 'active' AND revoked_at IS NULL;

-- Every existing device keeps exactly the qualification it had before this
-- migration. Adding the relation must never strand an already-enrolled client.
INSERT INTO device_profile_bindings(
    device_id,
    profile_id,
    status,
    validated_at,
    validation_method,
    validation_evidence,
    created_at
)
SELECT
    id,
    certified_profile_id,
    'active',
    created_at,
    'legacy_single_profile_backfill',
    jsonb_build_object('source', 'devices.certified_profile_id'),
    created_at
FROM devices;

COMMENT ON TABLE device_profile_bindings IS
    'Auditable many-to-many device profile qualifications. Session admission requires an active row matching the declared trusted profile lineage; revoking one profile does not affect any other profile on the same device.';

COMMENT ON COLUMN devices.certified_profile_id IS
    'Deprecated compatibility snapshot of the first profile lineage admitted for this device. Do not use for session authorization; active qualifications live in device_profile_bindings.';

INSERT INTO schema_migrations(version, name)
VALUES (25, 'device_profile_bindings');

COMMIT;
