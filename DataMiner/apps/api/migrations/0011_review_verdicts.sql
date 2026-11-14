-- 0011 · review verdicts（docs/33 §5，console 数据审查面的判定台账）
--
-- clip 级 QA 已有 clips.quality_state，但它只覆盖走摄取面、有 DB 行的 clip——
-- 桶里真实语料是带外进桶的，没有 clip 行可写（研究文 19 §5.2）。这张表是
-- 人工逐帧审查结论的通用落点：主体用对象 key 目录前缀标识（RAW segment /
-- dm-1 chunk / session），不依赖登记行存在。
--
-- 追加式：同 (source, subject_key) 最新一行生效，历史即复核轨迹。
-- 硬边界（docs/33 §5/§10）：本表不改 quality_state、不改 trainability、
-- 不进入任何 admission/合规判定路径——它是给人读的台账，Forge 的 QA 协议
-- 落地时按 subject_key 迁移或对照。

BEGIN;

CREATE TABLE review_verdicts (
    id            uuid PRIMARY KEY,
    -- 配置里的审查源 id（如 raw-prod / training-dev），不是桶名：桶名可迁移，
    -- 源 id 是审计与 UI 使用的稳定标识。
    source        text NOT NULL CHECK (char_length(source) BETWEEN 1 AND 32),
    subject_kind  text NOT NULL CHECK (subject_kind IN ('raw_segment', 'dataset_chunk', 'session')),
    -- 对象 key 目录前缀（以 '/' 结尾的审查单元），最长对齐 R2 key 上限。
    subject_key   text NOT NULL CHECK (char_length(subject_key) BETWEEN 1 AND 1024),
    result        text NOT NULL CHECK (result IN ('passed', 'failed')),
    note          text CHECK (note IS NULL OR char_length(note) <= 280),
    operator_id   uuid NOT NULL REFERENCES operators(id),
    request_id    text,
    created_at    timestamptz NOT NULL DEFAULT transaction_timestamp()
);

-- 最新判定查询与前缀扫描都走这一条：等值 source + subject_key，按时间倒序。
CREATE INDEX review_verdicts_subject_idx
    ON review_verdicts (source, subject_key, created_at DESC);

INSERT INTO schema_migrations(version, name)
VALUES (11, 'review_verdicts');

COMMIT;
