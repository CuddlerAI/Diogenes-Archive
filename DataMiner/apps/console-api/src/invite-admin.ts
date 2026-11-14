import { createHash } from "node:crypto";

import type { AuthenticatedSession, ConsoleRepository } from "./domain.js";
import { forbidden, notFound } from "./errors.js";
import { generateBatch, normalize, pretty } from "./app-invites.js";
import type { RequestContext } from "./auth.js";
import { requireCapability } from "./rbac.js";
import type { SqlClient } from "./sql.js";

/**
 * Invite administration is an operator write on the governance side: issuing a
 * code is an authorization decision (11 §12.3), so it takes identity.write and
 * sudo, exactly like device registration. Redemption happens on the annotator
 * surface under a different role; this module never redeems, only mints, lists,
 * and revokes.
 */

export type InviteSummary = {
  code: string;
  displayCode: string;
  note: string | null;
  status: "active" | "revoked";
  maxUses: number;
  usedCount: number;
  expiresAt: string | null;
  createdBy: string | null;
  createdAt: string;
  revokedBy: string | null;
  revokedAt: string | null;
  usedByAnnotatorId: string | null;
  usedAt: string | null;
};

/** The plaintext codes are returned exactly once, at creation. */
export type CreatedInvites = {
  codes: string[];
  summaries: InviteSummary[];
};

type InviteRow = {
  code: string;
  note: string | null;
  status: "active" | "revoked";
  max_uses: number;
  used_count: number;
  expires_at: Date | null;
  created_by: string | null;
  created_by_email: string | null;
  created_at: Date;
  revoked_by: string | null;
  revoked_by_email: string | null;
  revoked_at: Date | null;
  used_by_annotator_id: string | null;
  used_at: Date | null;
};

const iso = (value: Date | null): string | null => (value ? value.toISOString() : null);

function toSummary(row: InviteRow): InviteSummary {
  return {
    code: row.code,
    displayCode: pretty(row.code),
    note: row.note,
    status: row.status,
    maxUses: Number(row.max_uses),
    usedCount: Number(row.used_count),
    expiresAt: iso(row.expires_at),
    createdBy: row.created_by_email,
    createdAt: row.created_at.toISOString(),
    revokedBy: row.revoked_by_email,
    revokedAt: iso(row.revoked_at),
    usedByAnnotatorId: row.used_by_annotator_id,
    usedAt: iso(row.used_at),
  };
}

const SELECT_INVITE = `SELECT ic.code, ic.note, ic.status, ic.max_uses, ic.used_count,
              ic.expires_at, ic.created_by, cb.email AS created_by_email, ic.created_at,
              ic.revoked_by, rb.email AS revoked_by_email, ic.revoked_at,
              ic.used_by_annotator_id, ic.used_at
         FROM invite_codes ic
         LEFT JOIN operators cb ON cb.id = ic.created_by
         LEFT JOIN operators rb ON rb.id = ic.revoked_by`;

export class InviteAdminRepository {
  readonly #pool: SqlClient;

  constructor(pool: SqlClient) {
    this.#pool = pool;
  }

  async create(input: {
    codes: string[];
    note: string | null;
    expiresAt: string | null;
    createdBy: string;
  }): Promise<InviteRow[]> {
    const rows: InviteRow[] = [];
    for (const display of input.codes) {
      const result = await this.#pool.query<InviteRow>(
        `WITH inserted AS (
           INSERT INTO invite_codes(code, note, expires_at, created_by)
           VALUES ($1, $2, $3, $4)
           RETURNING code, note, status, max_uses, used_count, expires_at,
                     created_by, created_at, revoked_by, revoked_at,
                     used_by_annotator_id, used_at
         )
         SELECT i.*, cb.email AS created_by_email, NULL::text AS revoked_by_email
           FROM inserted i LEFT JOIN operators cb ON cb.id = i.created_by`,
        [normalize(display), input.note, input.expiresAt, input.createdBy],
      );
      rows.push(result.rows[0]!);
    }
    return rows;
  }

  async list(filter: { status?: "active" | "revoked" }): Promise<InviteRow[]> {
    const result = await this.#pool.query<InviteRow>(
      `${SELECT_INVITE}
        ${filter.status ? "WHERE ic.status = $1" : ""}
        ORDER BY ic.created_at DESC, ic.code`,
      filter.status ? [filter.status] : [],
    );
    return result.rows;
  }

  /**
   * A code an operator mints for their own annotator account.
   *
   * The unique partial index means a second call while one is still unspent
   * raises 23505 rather than quietly handing out another registration
   * credential — the caller turns that into "you already have one".
   */
  async createSelfIssued(input: {
    code: string;
    operatorId: string;
    email: string;
  }): Promise<InviteRow> {
    const result = await this.#pool.query<InviteRow>(
      `WITH inserted AS (
         INSERT INTO invite_codes(code, note, created_by, issued_for_operator_id)
         VALUES ($1, $2, $3, $3)
         RETURNING code, note, status, max_uses, used_count, expires_at,
                   created_by, created_at, revoked_by, revoked_at,
                   used_by_annotator_id, used_at
       )
       SELECT i.*, cb.email AS created_by_email, NULL::text AS revoked_by_email
         FROM inserted i LEFT JOIN operators cb ON cb.id = i.created_by`,
      [normalize(input.code), `self: ${input.email}`, input.operatorId],
    );
    return result.rows[0]!;
  }

  /**
   * Resolves which annotator each operator is, if any.
   *
   * The relation is derived, not stored twice: a self-issued code names the
   * operator, and `annotator_invite_uses` records who redeemed it. Nothing has
   * to write back to `operators` when the registration happens on the annotator
   * surface — which matters, because that surface's database role cannot touch
   * `operators` at all (11 §4.2).
   */
  async annotatorLinks(): Promise<{ operator_id: string; annotator_id: string; alias: string | null }[]> {
    const result = await this.#pool.query<{
      operator_id: string;
      annotator_id: string;
      alias: string | null;
    }>(
      `SELECT ic.issued_for_operator_id AS operator_id, u.annotator_id, a.alias
         FROM invite_codes ic
         JOIN annotator_invite_uses u ON u.code = ic.code
         JOIN annotators a ON a.id = u.annotator_id
        WHERE ic.issued_for_operator_id IS NOT NULL`,
    );
    return result.rows;
  }

  /** The operator's own unspent code, so the console can show it again. */
  async openSelfIssued(operatorId: string): Promise<InviteRow | null> {
    const result = await this.#pool.query<InviteRow>(
      `${SELECT_INVITE}
        WHERE ic.issued_for_operator_id = $1
          AND ic.status = 'active'
          AND ic.used_count = 0`,
      [operatorId],
    );
    return result.rows[0] ?? null;
  }

  async revoke(code: string, operatorId: string): Promise<InviteRow | null> {
    // Revocation exists to stop *further* redemption, so it only applies to an
    // active code that still has uses left. A fully-spent code has nothing to
    // stop, and used_count already records that it was used — revoking it would
    // just misrepresent history. (With max_uses = 1 today, "used_count < max_uses"
    // means unused; the predicate stays correct if multi-use codes are enabled.)
    const updated = await this.#pool.query<{ code: string }>(
      `UPDATE invite_codes
          SET status = 'revoked', revoked_at = transaction_timestamp(), revoked_by = $2
        WHERE code = $1 AND status = 'active' AND used_count < max_uses
        RETURNING code`,
      [code, operatorId],
    );
    if (updated.rows.length === 0) return null;
    const result = await this.#pool.query<InviteRow>(`${SELECT_INVITE} WHERE ic.code = $1`, [code]);
    return result.rows[0] ?? null;
  }
}

export class InviteAdminService {
  readonly #repository: InviteAdminRepository;
  readonly #auditSink: ConsoleRepository;

  constructor(repository: InviteAdminRepository, auditSink: ConsoleRepository) {
    this.#repository = repository;
    this.#auditSink = auditSink;
  }

  async create(
    actor: AuthenticatedSession,
    input: { count: number; note: string | null; expiresAt: string | null },
    context: RequestContext,
  ): Promise<CreatedInvites> {
    this.#guard(actor);
    const codes = generateBatch(input.count);
    const rows = await this.#repository.create({
      codes,
      note: input.note,
      expiresAt: input.expiresAt,
      createdBy: actor.operator.id,
    });
    await this.#auditSink.recordAudit({
      actorType: "operator",
      actorId: actor.operator.id,
      action: "invite.created",
      entityType: "invite_batch",
      // The audit records how many and their hashes, never the plaintext — the
      // codes exist in the clear only in the response the operator sees once.
      entityId: createHash("sha256").update(rows.map((row) => row.code).join(",")).digest("hex"),
      requestId: context.requestId,
      details: { count: rows.length, note: input.note, operator: actor.operator.email },
    });
    return { codes, summaries: rows.map(toSummary) };
  }

  async list(
    actor: AuthenticatedSession,
    filter: { status?: "active" | "revoked" },
  ): Promise<InviteSummary[]> {
    requireCapability(actor.operator.role, "console.read");
    const rows = await this.#repository.list(filter);
    return rows.map(toSummary);
  }

  async revoke(
    actor: AuthenticatedSession,
    displayOrNormalized: string,
    context: RequestContext,
  ): Promise<InviteSummary> {
    this.#guard(actor);
    const code = normalize(displayOrNormalized);
    const row = await this.#repository.revoke(code, actor.operator.id);
    if (!row) {
      // Unknown, already revoked, or already spent — one message, no oracle for
      // which codes exist.
      throw notFound("invite_not_revocable", "the invite code was not found or is not active");
    }
    await this.#auditSink.recordAudit({
      actorType: "operator",
      actorId: actor.operator.id,
      action: "invite.revoked",
      entityType: "invite",
      entityId: code,
      requestId: context.requestId,
      details: { operator: actor.operator.email },
    });
    return toSummary(row);
  }

  /**
   * Mints the caller their own annotator invite, so an operator can contribute
   * capture data with the client like anyone else.
   *
   * Deliberately **not** behind `identity.write` or sudo, unlike every other
   * write in this module. Those gate authorizing *someone else*; this authorizes
   * only the caller, at the lowest privilege the system has — an annotator can
   * upload data its own devices produced and nothing more. Requiring the
   * governance capability would mean a `viewer` who is also a contributor could
   * never register, which is exactly the person this exists for.
   *
   * What does bound it: one unspent code at a time (a partial unique index, so
   * a double click or a hijacked session cannot accumulate registration
   * credentials), and an audit row naming the operator.
   */
  async issueSelfInvite(
    actor: AuthenticatedSession,
    context: RequestContext,
  ): Promise<{ code: string; summary: InviteSummary }> {
    if (actor.operator.status !== "active") {
      throw forbidden("operator_inactive", "only an active operator can register as an annotator");
    }

    // Hand back the existing one rather than refusing: the operator asking again
    // has almost always lost the plaintext, and a code that was never redeemed
    // is not a secret worth withholding from the person it was minted for.
    const open = await this.#repository.openSelfIssued(actor.operator.id);
    if (open) return { code: pretty(open.code), summary: toSummary(open) };

    const [code] = generateBatch(1);
    const row = await this.#repository.createSelfIssued({
      code: code!,
      operatorId: actor.operator.id,
      email: actor.operator.email,
    });
    await this.#auditSink.recordAudit({
      actorType: "operator",
      actorId: actor.operator.id,
      action: "invite.self_issued",
      entityType: "invite",
      entityId: row.code,
      requestId: context.requestId,
      details: { operator: actor.operator.email },
    });
    return { code: code!, summary: toSummary(row) };
  }

  /** operator id → the annotator they registered as, for the console to show. */
  async annotatorLinks(actor: AuthenticatedSession): Promise<Map<string, { annotatorId: string; alias: string | null }>> {
    requireCapability(actor.operator.role, "console.read");
    const rows = await this.#repository.annotatorLinks();
    return new Map(rows.map((row) => [row.operator_id, { annotatorId: row.annotator_id, alias: row.alias }]));
  }

  #guard(actor: AuthenticatedSession): void {
    requireCapability(actor.operator.role, "identity.write");
    if (!actor.session.sudoActive) {
      throw forbidden("sudo_required", "re-enter your password to confirm this action");
    }
  }
}
