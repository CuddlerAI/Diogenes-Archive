-- 0021 · 设备钉扣从「某一版本行的 UUID」改成「profile_id 字符串」（docs/57 §2.1，解 #328）。
--
-- 缺陷：`devices.certified_profile_version_id` 钉的是 `trusted_catalog_versions`
-- 的行 UUID，而 `createSession` 要求这个 UUID 与客户端本次声明的版本行**严格相等**。
-- 一份目录里同一个 profile_id 不能有两个版本（客户端 store 按 id 建索引），所以
-- 「发一版新 device profile」= 替换旧版本 = 所有已绑定机器下次建会话 409
-- `device_profile_mismatch`。而 enroll 是 `ON CONFLICT DO NOTHING`，重新绑定不更新
-- 这一列，全仓也没有任何生产路径 UPDATE 它——**没有恢复路径**。
--
-- 这不是理论风险：驱动白名单里的版本会被 NVIDIA 自动更新甩掉，换句话说这个死结
-- 由机器自己扣扳机，不需要任何人做错事（#328 故障 B）。
--
-- 改法：钉扣降级为「这台机器属于哪条 device profile 谱系」，不再声称「它是在
-- 具体哪一版下被接纳的」。后者不丢，它本来就记在别处，而且记得更细：
--   * 语义契约漂不动——encoder/input/pointer 三个字段是客户端编译期常量，取别的值
--     的目录根本加载不进任何客户端（catalog.rs 的 validate_device）；
--   * 每个会话**逐次**记下自己声明的版本行（sessions.device_catalog_version_id），
--     可训练判定链读的就是会话行，与设备钉扣无关；
--   * enroll 当时的实测硬件（gpu_pci_id / gpu_driver / windows_build / 显示器与
--     输入设备快照）原样留在本表。
-- 设备行上那个 UUID 的档案价值与 sessions 行完全重复，因此不保留（docs/57 §4）。
--
-- enroll 侧的把关**不放松**：客户端声明的 (id, version, content_hash) 仍须命中一行
-- status='approved' 的 device_profile，只是入库存的是那一行的 catalog_id 字符串。
-- 未发版，直接改列，不留兼容层。

BEGIN;

ALTER TABLE devices ADD COLUMN certified_profile_id text;

-- 回填：旧列指向的那一行，它的 catalog_id 就是 profile_id 字符串。外键保证了
-- 每一行都能 join 上，所以回填之后可以立刻上 NOT NULL。
UPDATE devices d
   SET certified_profile_id = t.catalog_id
  FROM trusted_catalog_versions t
 WHERE t.id = d.certified_profile_version_id;

ALTER TABLE devices
    ALTER COLUMN certified_profile_id SET NOT NULL,
    ADD CONSTRAINT devices_certified_profile_id_not_blank
        CHECK (btrim(certified_profile_id) <> '');

-- 旧列连同它的复合外键一起走。`certified_profile_kind` 只为那个外键而存在
-- （它有一条 `= 'device_profile'` 的 CHECK，除此之外没有任何读者），外键没了它
-- 就是纯粹的死列。DROP COLUMN 会自动带走涉及该列的外键与 CHECK，所以这里不点名
-- 那条复合外键——它的名字是 PostgreSQL 按 63 字符上限截出来的，猜不得。
ALTER TABLE devices
    DROP COLUMN certified_profile_version_id,
    DROP COLUMN certified_profile_kind;

COMMENT ON COLUMN devices.certified_profile_id IS
    'Which device profile lineage this machine was admitted under (trusted_catalog_versions.catalog_id). Deliberately not a version pin: bumping a driver floor or a GPU model list must not strand already-enrolled machines (#328). The version each session actually ran under is recorded per session on sessions.device_catalog_version_id, and the hardware measured at enrollment is in this row.';

INSERT INTO schema_migrations(version, name)
VALUES (21, 'device_profile_pin_by_id');

COMMIT;
