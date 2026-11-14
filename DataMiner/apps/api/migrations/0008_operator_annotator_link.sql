-- 0008 · operator ↔ annotator：同一个人的两个身份
--
-- 产品诉求是「管理员也是标注员」——运营的人自己也要用客户端采集数据。
--
-- 做法**不是**把两套身份合表。docs/11 §4.1 拒绝复用的三条理由今天一条没变：
-- 信任等级差两个数量级（operator 能 purge、能签发凭据；annotator 只能上传自己
-- 设备产生的数据）、生命周期不同（邀请制的个位数 vs 自助注册的开放规模）、
-- 而且 annotators 已经被五张表外键引用。合表意味着一次 role 列写错就是权限提升。
--
-- 所以两张表继续物理隔离，这里只补一条**关联**。而且关联本身不新开表：
-- 「邀请码 → 用户」的权威关系 annotator_invite_uses 已经有了，缺的只是
-- 「这张码是发给哪个 operator 自己的」。补上这一列，关联就是一次 join：
--
--   operators → invite_codes.issued_for_operator_id → annotator_invite_uses.annotator_id
--
-- 这样选的关键理由是**权限边界不用动**。§4.2 的矩阵里，用户面角色
-- dataminer_app_staging 对 operators 三张表零权限——连 SELECT 都没有。
-- 如果关联列放在 annotators 或 annotator_credentials 上，注册时就得由用户面写它，
-- 那要么给用户面开一条通往 operator 身份的写路径，要么再造一个回填作业。
-- 放在 invite_codes 上则由治理面在发码时写入，用户面全程只读它已经能读的东西
-- （§4.2 已授 invite_codes 的 SELECT）。零新增授权。

BEGIN;

ALTER TABLE invite_codes
    ADD COLUMN issued_for_operator_id uuid REFERENCES operators(id) ON DELETE RESTRICT;

COMMENT ON COLUMN invite_codes.issued_for_operator_id IS
    'Set when an operator issued this code to create their own annotator account. '
    'Joined through annotator_invite_uses to resolve which annotator an operator is. '
    'NULL for ordinary codes handed to external contributors.';

-- 部分索引：自签码在总量里是极少数，而这一列的两个用途——「这个 operator 关联了
-- 谁」和「他有没有一张还没用的自签码」——都只查非空行。
CREATE INDEX invite_codes_issued_for_operator_idx
    ON invite_codes(issued_for_operator_id)
    WHERE issued_for_operator_id IS NOT NULL;

-- 一个 operator 最多持有一张**未核销**的自签码。
--
-- 不限制总数：核销后那行是历史，第二台机器换新账号之类的将来需求不该被历史挡住。
-- 限制未核销数，是因为一张没用掉的码就是一张躺在那里的注册凭证——重复点击、
-- 或者一个被劫持的 viewer 会话，不应该能攒出一把。
CREATE UNIQUE INDEX invite_codes_one_open_self_issue_idx
    ON invite_codes(issued_for_operator_id)
    WHERE issued_for_operator_id IS NOT NULL
      AND status = 'active'
      AND used_count = 0;

-- 本文件最初漏了这行自记账：迁移能应用，但账本不落 8，下一次 migrate 会
-- 重放本文件并撞上「列已存在」，从而卡死之后的一切迁移。首个生产迁移
-- job（2026-07-26，应用 0008+0009 那次）实测暴露；生产账本已手工补 8。
INSERT INTO schema_migrations(version, name)
VALUES (8, 'operator_annotator_link');

COMMIT;
