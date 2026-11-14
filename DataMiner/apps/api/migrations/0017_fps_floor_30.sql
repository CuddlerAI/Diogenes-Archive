-- 0017 · fps 下限对齐客户端与文档（docs/50 P-5a）
--
-- 客户端三处独立强制 fps_num >= 30 * fps_den（capture-core profile 校验、
-- catalog 校验、SQLite journal CHECK），docs/03 §schema 与 docs/08 §preflight
-- 也一直声称云端同款下限；但 0001 实际只写了 `fps_num >= 1 * fps_den`。
-- 服务端弱于客户端意味着非客户端通道（脚本、误配）可以登记 <30fps 的
-- session/run 而不被拒。这里补上**命名**约束收紧到 30；0001 的旧匿名
-- CHECK (>=1) 保留不动——它严格弱于新约束，删除匿名约束反而要按引擎
-- 生成名反查，风险大于收益。
--
-- 现网数据核查：所有已发布 capture profile 均为 30/1（docs/50 §1.2），
-- ALTER 时的存量行校验不会失败。60fps 升档（docs/50 K-3）仍满足本约束。
--
-- 编号取 17 而非 16：0016 已被 0016_view_scale_trainability.sql 占用（#291 已在 main）。
-- migrate.ts 拿文件名数字前缀当 schema_migrations.version 主键，撞号会让本文件在
-- 已应用过 version=16 的库上被整段跳过——约束就此静默失效。

BEGIN;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_fps_floor_30 CHECK (fps_num >= 30 * fps_den);

ALTER TABLE capture_runs
    ADD CONSTRAINT capture_runs_fps_floor_30 CHECK (fps_num >= 30 * fps_den);

-- 账本写入不可省：migrate.ts 只按 schema_migrations 判定是否跳过，而
-- ALTER TABLE ... ADD CONSTRAINT 在 Postgres 没有 IF NOT EXISTS，
-- 漏登记会让第二次 migrate 撞 "constraint already exists" 直接失败。
INSERT INTO schema_migrations(version, name)
VALUES (17, 'fps_floor_30');

COMMIT;
