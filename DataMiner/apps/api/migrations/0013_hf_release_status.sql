-- 0013 · HF 发布状态对账（docs/33 §6）
--
-- R2 与 HF 是**两个不相交的集合**（docs/18 §3.2：上传门显式跳过 consented
-- 会话——能上 HF 的 first-party 数据刻意不进 R2）。所以这里存的不是流水线
-- 进度，而是**对账事实**：HF 上到底有什么（hf_release_items 即本地缓存的
-- repo 清单），以及人工过程态标注（hf_manual_marks）。绝大多数 R2 对象的
-- 正确状态是「无对应」；出现匹配反而要人来检查是不是发错了。
--
-- 硬边界：三张表都不进任何 admission/合规判定路径；BQ 行为日志仓库
-- authoritative:false 的纪律（docs/25 §7.2）在此同样成立——对账结果永不
-- 回喂治理决策。

BEGIN;

-- 每次对账一行：谁在什么时候用哪个 revision 刷新了清单。
CREATE TABLE hf_release_checks (
    id          uuid PRIMARY KEY,
    repo_id     text NOT NULL CHECK (repo_id ~ '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'),
    revision    text NOT NULL,
    operator_id uuid NOT NULL REFERENCES operators(id),
    stats       jsonb NOT NULL DEFAULT '{}'::jsonb,
    checked_at  timestamptz NOT NULL DEFAULT transaction_timestamp()
);

-- repo 清单缓存：每个文件一行，session_key / sample_key 是从 teaser 布局
-- （{split}/{game_slug}/{session}/cNNNN/…）解析出的匹配键。整 repo 替换式
-- 刷新（DELETE + INSERT 同事务），revision 记录来源。
CREATE TABLE hf_release_items (
    repo_id     text NOT NULL,
    path        text NOT NULL,
    revision    text NOT NULL,
    size_bytes  bigint,
    session_key text,
    sample_key  text,
    checked_at  timestamptz NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (repo_id, path)
);

CREATE INDEX hf_release_items_session_idx ON hf_release_items (session_key)
    WHERE session_key IS NOT NULL;
CREATE INDEX hf_release_items_sample_idx ON hf_release_items (sample_key)
    WHERE sample_key IS NOT NULL;

-- 人工过程态（如「准备发布中」）：对账拿不到的状态，追加式，最新行生效。
-- 人工态会腐化（docs/33 辩论 §8.5 已认），UI 在对账后要把与自动态冲突的
-- 人工态标灰提醒复核——这张表只记事实，不做仲裁。
CREATE TABLE hf_manual_marks (
    id          uuid PRIMARY KEY,
    source      text NOT NULL CHECK (char_length(source) BETWEEN 1 AND 32),
    subject_key text NOT NULL CHECK (char_length(subject_key) BETWEEN 1 AND 1024),
    mark        text NOT NULL CHECK (char_length(mark) BETWEEN 1 AND 64),
    note        text CHECK (note IS NULL OR char_length(note) <= 280),
    operator_id uuid NOT NULL REFERENCES operators(id),
    created_at  timestamptz NOT NULL DEFAULT transaction_timestamp()
);

CREATE INDEX hf_manual_marks_subject_idx
    ON hf_manual_marks (source, subject_key, created_at DESC);

INSERT INTO schema_migrations(version, name)
VALUES (13, 'hf_release_status');

COMMIT;
