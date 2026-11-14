/**
 * 已批准 build 名单的治理操作（migration 0018）。
 *
 * 负责人裁决（2026-07-28）取消了 authoring 侧的身份钉硬门（#278）：游戏可以
 * 只靠进程名被识别，指纹不再是准入前提。把关因此挪到这里——客户端绑定时
 * **实测**的 executable SHA-256 必须出现在本名单且为 approved，对应的 clip
 * 才可能可训练（`clip_trainability` 的 `executable_unapproved`）。
 *
 * 采集不会因此停摆：新 build 照常采集入库，只是先落 pending 等治理点头。
 * 游戏更新不再需要"先真机取证再恢复采集"，指纹自己会随会话送上门。
 *
 * 纪律：
 * - 只升不降。`approve` 不会复活 revoked 行——撤销是治理行为，要显式
 *   `revoke`/重新审批，不能被一次例行批准顺手抹掉；
 * - 审批必须可归属（表上的 CHECK 也这么要求）；
 * - 撤销保留行与审计痕迹，而不是删除。
 */
export type QueryFn = (
  text: string,
  params?: readonly unknown[],
) => Promise<{ rows: Record<string, unknown>[] }>;

export type ExecutableStatus = "pending" | "approved" | "revoked";

export type ExecutableRow = {
  gameId: string;
  sha256: string;
  status: ExecutableStatus;
  source: "authored" | "observed";
  approvedBy: string | null;
  approvedAt: string | null;
  firstSeenAt: string | null;
  note: string | null;
};

const SHA256_PATTERN = /^sha256:[0-9a-f]{64}$/;

function toRow(row: Record<string, unknown>): ExecutableRow {
  return {
    gameId: String(row.game_id),
    sha256: String(row.executable_sha256),
    status: String(row.status) as ExecutableStatus,
    source: String(row.source) as "authored" | "observed",
    approvedBy: row.approved_by === null ? null : String(row.approved_by),
    approvedAt: row.approved_at === null ? null : new Date(String(row.approved_at)).toISOString(),
    firstSeenAt: row.first_seen_at === null ? null : new Date(String(row.first_seen_at)).toISOString(),
    note: row.note === null ? null : String(row.note),
  };
}

/** 列出名单；不传 status 时列全部，便于治理侧先看 pending 再决定。 */
export async function listExecutables(
  query: QueryFn,
  filter: { gameId?: string; status?: ExecutableStatus } = {},
): Promise<ExecutableRow[]> {
  const conditions: string[] = [];
  const params: unknown[] = [];
  if (filter.gameId !== undefined) {
    params.push(filter.gameId);
    conditions.push(`game_id = $${params.length}`);
  }
  if (filter.status !== undefined) {
    params.push(filter.status);
    conditions.push(`status = $${params.length}`);
  }
  const where = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
  const result = await query(
    `SELECT game_id, executable_sha256, status, source, approved_by, approved_at, first_seen_at, note
       FROM approved_game_executables
       ${where}
      ORDER BY game_id, first_seen_at DESC`,
    params,
  );
  return result.rows.map(toRow);
}

export type ApprovalResult = {
  action: "approved" | "already_approved" | "refused_revoked" | "not_found";
  row: ExecutableRow | null;
};

/**
 * 把一条 pending 指纹批为 approved。
 *
 * 只作用于已存在的行：名单条目来自 catalog authoring 或真实采集观测，
 * 不接受凭空手输一个指纹——那等于绕开"这个 build 真的被人跑过"的证据。
 */
export async function approveExecutable(
  query: QueryFn,
  input: { gameId: string; sha256: string; approver: string; note?: string },
): Promise<ApprovalResult> {
  if (!SHA256_PATTERN.test(input.sha256)) {
    throw new Error("executable sha256 must be sha256:<64 lowercase hex>");
  }
  if (input.approver.trim() === "") {
    throw new Error("approver is required so the approval is attributable");
  }
  const existing = await query(
    `SELECT game_id, executable_sha256, status, source, approved_by, approved_at, first_seen_at, note
       FROM approved_game_executables WHERE game_id = $1 AND executable_sha256 = $2`,
    [input.gameId, input.sha256],
  );
  const current = existing.rows[0];
  if (current === undefined) {
    return { action: "not_found", row: null };
  }
  const row = toRow(current);
  if (row.status === "approved") {
    return { action: "already_approved", row };
  }
  if (row.status === "revoked") {
    // 复活撤销过的 build 必须是显式治理决定，不能是一次例行批准的副作用。
    return { action: "refused_revoked", row };
  }
  const updated = await query(
    `UPDATE approved_game_executables
        SET status = 'approved',
            approved_by = $3,
            approved_at = transaction_timestamp(),
            note = COALESCE($4, note)
      WHERE game_id = $1 AND executable_sha256 = $2 AND status = 'pending'
      RETURNING game_id, executable_sha256, status, source, approved_by, approved_at, first_seen_at, note`,
    [input.gameId, input.sha256, input.approver, input.note ?? null],
  );
  const updatedRow = updated.rows[0];
  if (updatedRow === undefined) {
    return { action: "not_found", row: null };
  }
  return { action: "approved", row: toRow(updatedRow) };
}

export type RevocationResult = {
  action: "revoked" | "already_revoked" | "not_found";
  row: ExecutableRow | null;
};

/**
 * 撤销一个 build。行保留（审计痕迹），其后续 clip 立即失去训练资格——
 * `clip_trainability` 是读时派生的视图，不需要回填任何东西。
 */
export async function revokeExecutable(
  query: QueryFn,
  input: { gameId: string; sha256: string; actor: string; note?: string },
): Promise<RevocationResult> {
  if (input.actor.trim() === "") {
    throw new Error("actor is required so the revocation is attributable");
  }
  const updated = await query(
    `UPDATE approved_game_executables
        SET status = 'revoked',
            note = COALESCE($4, 'revoked by ' || $3)
      WHERE game_id = $1 AND executable_sha256 = $2 AND status <> 'revoked'
      RETURNING game_id, executable_sha256, status, source, approved_by, approved_at, first_seen_at, note`,
    [input.gameId, input.sha256, input.actor, input.note ?? null],
  );
  const row = updated.rows[0];
  if (row !== undefined) {
    return { action: "revoked", row: toRow(row) };
  }
  const existing = await query(
    `SELECT game_id, executable_sha256, status, source, approved_by, approved_at, first_seen_at, note
       FROM approved_game_executables WHERE game_id = $1 AND executable_sha256 = $2`,
    [input.gameId, input.sha256],
  );
  const current = existing.rows[0];
  if (current === undefined) {
    return { action: "not_found", row: null };
  }
  return { action: "already_revoked", row: toRow(current) };
}
