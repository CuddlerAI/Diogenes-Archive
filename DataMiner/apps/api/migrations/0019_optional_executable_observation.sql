-- 0019 · 游戏程序指纹改为可空 + 缺失原因（docs/55，负责人裁决 2026-07-28）。
--
-- 背景链：#278 拆掉 authoring 侧身份钉硬门；0018（#295）把把关挪到云端事后
-- 审批（approved_game_executables + executable_unapproved blocker）。docs/55 是
-- 最后一段——客户端身份采集从「失败即淘汰候选、不能录」改为「失败即降级留痕、
-- 照常录制」，并借此删掉 COD 的 name-only 特例代码，让所有游戏走同一条路径。
--
-- 因此指纹从「每个 session 必有」变成「拿得到才有」：
--   * 拿到了      -> game_executable_sha256 有值，行为与今天完全一致；
--   * 没拿到      -> 指纹为 NULL，改记一条**可读的失败说明**解释为什么没拿到。
--
-- clip_trainability **一行不改**：0018 的 LEFT JOIN 在指纹为 NULL 时天然不匹配
-- （NULL = anything 恒为 NULL），既有的 executable_unapproved blocker 直接命中。
-- 「没采到」与「采到了但没批准」在控制台里由指纹列是否为空区分，不另设 blocker
-- ——多一个 blocker 名字不改变任何判定，只增加派生面。
--
-- 不变式不变：**训练资格 = 实测 sha256 + 名单已批准**，对所有游戏恒成立。
-- 无指纹会话照采、照传、照入库，永不可训练。

BEGIN;

-- 存量：所有既有行都实测过指纹，DROP NOT NULL 不改变它们，XOR 约束对它们
-- 天然成立（指纹在场 -> reason 必须为 NULL -> 新列默认 NULL）。无需回填。
ALTER TABLE sessions
    ALTER COLUMN game_executable_sha256 DROP NOT NULL,
    ADD COLUMN game_executable_sha256_missing_reason text;

-- 这一列装**给人看的完整失败说明**：阶段前缀 + 整条错误链，含完整镜像路径与
-- 系统错误文本，例如
--   open_process_denied: OpenProcess(9284) failed: Access is denied. (os error 5)
--   executable_read_failed: failed to open C:\Users\<user>\Games\cod.exe ... (os error 32)
--
-- 负责人裁决（2026-07-28）：标注员都是内部员工，排查一个"指纹为什么没测到"的
-- 问题时，路径与原文远比一个错误码有用；这里不做最小化。曾经考虑过压成
-- snake_case 机器码以避免路径（含 Windows 用户名）上云，已明确否决。
--
-- 仍然设上限：这条值来自错误链拼接，失控的嵌套 context 能把一列撑到任意长度。
-- 2000 字符对真实错误链绰绰有余，与隐私无关，只是防写爆。
ALTER TABLE sessions
    ADD CONSTRAINT sessions_executable_missing_reason_is_bounded CHECK (
        game_executable_sha256_missing_reason IS NULL
        OR (btrim(game_executable_sha256_missing_reason) <> ''
            AND length(game_executable_sha256_missing_reason) <= 2000)
    );

-- 指纹与原因严格互斥：有指纹就不该有原因，没指纹就必须说明为什么。
-- 两边都缺 = 客户端漏报且无人知情；两边都在 = 自相矛盾。都拒。
ALTER TABLE sessions
    ADD CONSTRAINT sessions_executable_sha_xor_missing_reason CHECK (
        (game_executable_sha256 IS NULL) = (game_executable_sha256_missing_reason IS NOT NULL)
    );

COMMENT ON COLUMN sessions.game_executable_sha256 IS
    'Build fingerprint measured by the client at bind time. NULL when the client could not observe it (see game_executable_sha256_missing_reason); such sessions are never trainable because the 0018 approval join cannot match a NULL digest.';
COMMENT ON COLUMN sessions.game_executable_sha256_missing_reason IS
    'Human-readable explanation of why no fingerprint was measured: a stage prefix plus the full error chain, including the executable path and the OS error text. Free text on purpose (annotators are internal staff and troubleshooting beats minimisation); bounded to 2000 characters so a runaway error chain cannot blow up the column.';

-- 0018 的登记触发器必须跳过无指纹的会话：approved_game_executables.executable_sha256
-- 是 sha256_digest NOT NULL，插 NULL 会让触发器抛错，**连带整条 sessions 插入
-- 失败**——摄取核心路径会被一个治理表的登记动作打断。WHEN 子句让它在源头不触发。
-- 用 DROP + CREATE 而不是 CREATE OR REPLACE TRIGGER：后者要 PG14+，而这条
-- 迁移没有任何理由去关心引擎版本下限。
DROP TRIGGER sessions_record_observed_executable ON sessions;

CREATE TRIGGER sessions_record_observed_executable
    AFTER INSERT ON sessions
    FOR EACH ROW
    WHEN (NEW.game_executable_sha256 IS NOT NULL)
    EXECUTE FUNCTION record_observed_game_executable();

-- 账本写入不可省（同 0017 的理由）：migrate.ts 只按 schema_migrations 判定是否
-- 跳过，而这里的 ALTER/DROP TRIGGER 都不是幂等语句，漏登记会让第二次 migrate
-- 直接失败。
INSERT INTO schema_migrations(version, name)
VALUES (19, 'optional_executable_observation');

COMMIT;
