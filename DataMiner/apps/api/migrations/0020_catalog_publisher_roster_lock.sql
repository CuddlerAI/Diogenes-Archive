-- 0020 · 让 publisher 角色真的能发布：把 roster 加锁查询包进 SECURITY DEFINER 函数。
--
-- 缺陷（2026-07-29 生产首发时实测发现）：`dataminer_catalog_publisher` 按
-- grant-console-roles.sql 只拿到 `SELECT ON trusted_catalog_versions`，而
-- `publishCatalogRelease` 在事务里跑的是
--
--     SELECT id FROM trusted_catalog_versions WHERE … FOR SHARE
--
-- PostgreSQL 的行锁子句（FOR SHARE / FOR UPDATE / …）除 SELECT 外还要求
-- UPDATE 级权限，于是这条必然 `permission denied for table
-- trusted_catalog_versions`。实测：纯 SELECT 返回 9 行，加 FOR SHARE 即被拒。
-- **这个角色至今一次都没成功发布过**——生产首发是临时改用迁移角色跑完的。
--
-- 为什么不直接 GRANT UPDATE：那等于让 publisher 能改写 trusted_catalog_versions
-- 的 status / approved_for_trainable，正是分级授权特意不给它的权力。发布者只该
-- 能「读取并钉住」这些行，不该能改它们。
--
-- 于是照 0014 / 0018 的先例用 SECURITY DEFINER：加锁以函数所有者（迁移角色）
-- 身份发生，publisher 只拿 EXECUTE。行锁仍然记在**调用者的事务**上——SECURITY
-- DEFINER 只切换权限上下文，不新开事务，所以 FOR SHARE 的语义一点没变。
--
-- 为什么没有测试拦住它：catalog-publisher.test.ts 跑在 PGlite 上，以超级用户
-- 身份执行，任何权限差异都照不到。本迁移配套在 dataminer-console-ci 的
-- migrations-postgres job 里加了真角色断言（真 PostgreSQL + 真 GRANT）。

BEGIN;

-- 参数名与调用点一一对应，函数体无动态 SQL，值只从参数来；
-- `SET search_path` 关掉 search_path 注入面（同 0018）。
CREATE FUNCTION lock_publishable_catalog_versions(
    p_ids uuid[],
    p_require_approved boolean
)
RETURNS SETOF uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT id
      FROM trusted_catalog_versions
     WHERE id = ANY(p_ids)
       AND status <> 'revoked'
       AND (p_require_approved = false OR status = 'approved')
     FOR SHARE;
$$;

COMMENT ON FUNCTION lock_publishable_catalog_versions(uuid[], boolean) IS
    'Locks the roster rows a release is about to pin, so their eligibility cannot change between the check and the insert. SECURITY DEFINER because row-level locks need UPDATE-grade privilege, which the publisher role must not have on this table (it may pin these rows, never rewrite them).';

-- 与 publish_catalog_release 同样的把关：默认不对 PUBLIC 开放，具体授予放在
-- grant-console-roles.sql 里，和其余角色权限同一处维护。
REVOKE EXECUTE ON FUNCTION lock_publishable_catalog_versions(uuid[], boolean) FROM PUBLIC;

INSERT INTO schema_migrations(version, name)
VALUES (20, 'catalog_publisher_roster_lock');

COMMIT;
