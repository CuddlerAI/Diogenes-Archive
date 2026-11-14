import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";

import { normalize } from "./app-invites.js";
import { InviteAdminRepository, InviteAdminService } from "./invite-admin.js";
import type { AuthenticatedSession, ConsoleRepository } from "./domain.js";
import type { SqlClient } from "./sql.js";

// The invite writes touch invite_codes, which the console (governance) role owns,
// so they run against a real engine like the rest of that surface.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;
let operatorId: string;

const context = { requestId: "req_test", ipHash: "0".repeat(64), userAgent: null };

function actor(overrides: { role?: "viewer" | "operator" | "admin"; sudo?: boolean } = {}): AuthenticatedSession {
  return {
    operator: {
      id: operatorId,
      email: "ops@diogenesmodel.com",
      role: overrides.role ?? "admin",
    },
    session: { sudoActive: overrides.sudo ?? true },
  } as unknown as AuthenticatedSession;
}

function service() {
  const client: SqlClient = { query: (text, values) => db.query(text, values as unknown[]) };
  const auditSink = { recordAudit: vi.fn(async () => undefined) } as unknown as ConsoleRepository;
  const svc = new InviteAdminService(new InviteAdminRepository(client), auditSink);
  return { svc, auditSink };
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  operatorId = randomUUID();
  await db.query(
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, 'ops@diogenesmodel.com', $2, transaction_timestamp(), 'admin', 'active')`,
    [operatorId, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("invite creation", () => {
  it("mints the requested count, returns plaintext once, and stores normalized rows", async () => {
    const { svc, auditSink } = service();

    const created = await svc.create(actor(), { count: 3, note: "batch-1", expiresAt: null }, context);

    expect(created.codes).toHaveLength(3);
    expect(created.summaries).toHaveLength(3);
    for (const code of created.codes) {
      expect(code).toMatch(/^DM-[0-9A-Z]{4}-[0-9A-Z]{4}-[0-9A-Z]{4}$/);
      const stored = await db.query(`SELECT 1 FROM invite_codes WHERE code = $1`, [normalize(code)]);
      expect(stored.rows).toHaveLength(1);
    }
    // The audit records the count and hashes, never the plaintext.
    expect(auditSink.recordAudit).toHaveBeenCalledOnce();
    const call = (auditSink.recordAudit as ReturnType<typeof vi.fn>).mock.calls[0]![0];
    expect(call.details.count).toBe(3);
    expect(JSON.stringify(call)).not.toContain(created.codes[0]!);
  });

  it("attributes the code to its creating operator", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);

    expect(created.summaries[0]?.createdBy).toBe("ops@diogenesmodel.com");
    expect(created.summaries[0]?.status).toBe("active");
    expect(created.summaries[0]?.usedCount).toBe(0);
  });

  it("refuses a viewer and refuses an operator who has not re-entered their password", async () => {
    const { svc } = service();

    // Issuing an invite is an authorization decision (11 §12.3).
    await expect(svc.create(actor({ role: "viewer" }), { count: 1, note: null, expiresAt: null }, context)).rejects.toThrow();
    await expect(svc.create(actor({ sudo: false }), { count: 1, note: null, expiresAt: null }, context)).rejects.toThrow();
  });
});

describe("invite listing", () => {
  it("shows the display form and filters by status", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: "listing", expiresAt: null }, context);
    const code = normalize(created.codes[0]!);
    await svc.revoke(actor(), code, context);

    const all = await svc.list(actor(), {});
    const shown = all.find((invite) => invite.code === code);
    expect(shown?.displayCode).toBe(created.codes[0]);
    expect(shown?.status).toBe("revoked");

    const active = await svc.list(actor(), { status: "active" });
    expect(active.every((invite) => invite.status === "active")).toBe(true);
    expect(active.find((invite) => invite.code === code)).toBeUndefined();
  });

  it("is readable by a viewer", async () => {
    const { svc } = service();
    await expect(svc.list(actor({ role: "viewer" }), {})).resolves.toBeInstanceOf(Array);
  });
});

describe("invite revocation", () => {
  it("revokes an active code and records who and when", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);
    const code = normalize(created.codes[0]!);

    const revoked = await svc.revoke(actor(), code, context);
    expect(revoked.status).toBe("revoked");
    expect(revoked.revokedBy).toBe("ops@diogenesmodel.com");
    expect(revoked.revokedAt).not.toBeNull();
  });

  it("accepts the display form as well as the normalized form", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);

    // Operator pastes the code exactly as it was handed out.
    const revoked = await svc.revoke(actor(), created.codes[0]!, context);
    expect(revoked.status).toBe("revoked");
  });

  it("refuses to revoke an unknown, already-revoked, or spent code with one message", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);
    const code = normalize(created.codes[0]!);
    await svc.revoke(actor(), code, context);

    // Unknown.
    await expect(svc.revoke(actor(), "DMZZZZZZZZZZZZ", context)).rejects.toMatchObject({
      code: "invite_not_revocable",
    });
    // Already revoked.
    await expect(svc.revoke(actor(), code, context)).rejects.toMatchObject({
      code: "invite_not_revocable",
    });

    // Spent codes are left as they are — used_count is what stops further use,
    // and revoking after the fact would misrepresent history.
    const spent = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);
    const spentCode = normalize(spent.codes[0]!);
    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'redeemer')`, [annotatorId]);
    await db.query(
      `UPDATE invite_codes SET used_count = 1, used_at = transaction_timestamp(),
              used_by_annotator_id = $2 WHERE code = $1`,
      [spentCode, annotatorId],
    );
    await expect(svc.revoke(actor(), spentCode, context)).rejects.toMatchObject({
      code: "invite_not_revocable",
    });
  });

  it("refuses a viewer and a non-sudo operator", async () => {
    const { svc } = service();
    const created = await svc.create(actor(), { count: 1, note: null, expiresAt: null }, context);

    await expect(svc.revoke(actor({ role: "viewer" }), created.codes[0]!, context)).rejects.toThrow();
    await expect(svc.revoke(actor({ sudo: false }), created.codes[0]!, context)).rejects.toThrow();
  });
});

// "The admin is also an annotator" (docs/11 §4.1 stays: two identities, one
// person, no shared table). The link is derived from a self-issued code rather
// than written onto either row, so registering on the annotator surface needs
// no access to `operators`.
describe("operator's own annotator invite", () => {
  async function otherOperator(role: "viewer" | "operator" | "admin" = "viewer") {
    const id = randomUUID();
    await db.query(
      `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
       VALUES ($1, $2, $3, transaction_timestamp(), $4, 'active')`,
      [id, `${id.slice(0, 8)}@diogenesmodel.com`, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA", role],
    );
    return {
      operator: { id, email: `${id.slice(0, 8)}@diogenesmodel.com`, role, status: "active" },
      session: { sudoActive: false },
    } as unknown as AuthenticatedSession;
  }

  it("mints a code tagged to the operator who asked for it", async () => {
    const { svc, auditSink } = service();
    const me = await otherOperator("operator");

    const issued = await svc.issueSelfInvite(me, context);

    const row = await db.query<{ issued_for_operator_id: string; note: string }>(
      `SELECT issued_for_operator_id, note FROM invite_codes WHERE code = $1`,
      [normalize(issued.code)],
    );
    expect(row.rows[0]?.issued_for_operator_id).toBe(me.operator.id);
    // The note carries who it is for, so the console list reads sensibly.
    expect(row.rows[0]?.note).toContain(me.operator.email);
    expect(auditSink.recordAudit).toHaveBeenCalled();
  });

  // The person this exists for may well be a viewer: they contribute capture
  // data without administering anything. Gating on identity.write would lock
  // exactly them out.
  it("is available to a viewer, and without a step-up", async () => {
    const { svc } = service();
    const viewer = await otherOperator("viewer");

    await expect(svc.issueSelfInvite(viewer, context)).resolves.toMatchObject({
      code: expect.stringMatching(/^DM-/),
    });
  });

  it("returns the same unspent code instead of minting a second", async () => {
    const { svc } = service();
    const me = await otherOperator("operator");

    const first = await svc.issueSelfInvite(me, context);
    const second = await svc.issueSelfInvite(me, context);

    expect(second.code).toBe(first.code);
    const count = await db.query<{ count: string }>(
      `SELECT count(*) AS count FROM invite_codes WHERE issued_for_operator_id = $1`,
      [me.operator.id],
    );
    expect(Number(count.rows[0]?.count)).toBe(1);
  });

  it("lets a new code be issued once the previous one is spent", async () => {
    const { svc } = service();
    const me = await otherOperator("operator");
    const first = await svc.issueSelfInvite(me, context);

    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'the operator')`, [annotatorId]);
    await db.query(
      `UPDATE invite_codes SET used_count = 1, used_at = transaction_timestamp(),
              used_by_annotator_id = $2 WHERE code = $1`,
      [normalize(first.code), annotatorId],
    );

    const second = await svc.issueSelfInvite(me, context);
    expect(second.code).not.toBe(first.code);
  });

  it("refuses a suspended operator", async () => {
    const { svc } = service();
    const me = await otherOperator("operator");
    (me.operator as { status: string }).status = "suspended";

    await expect(svc.issueSelfInvite(me, context)).rejects.toMatchObject({
      code: "operator_inactive",
    });
  });

  it("resolves the link once the code has been redeemed", async () => {
    const { svc } = service();
    const me = await otherOperator("operator");
    const issued = await svc.issueSelfInvite(me, context);

    // Before redemption there is no link: a minted code is not an identity.
    expect((await svc.annotatorLinks(actor())).get(me.operator.id)).toBeUndefined();

    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'ops-as-annotator')`, [
      annotatorId,
    ]);
    await db.query(
      `INSERT INTO annotator_invite_uses(id, code, annotator_id) VALUES ($1, $2, $3)`,
      [randomUUID(), normalize(issued.code), annotatorId],
    );

    const links = await svc.annotatorLinks(actor());
    expect(links.get(me.operator.id)).toEqual({ annotatorId, alias: "ops-as-annotator" });
  });

  it("does not link an ordinary invite handed to an outside contributor", async () => {
    const { svc } = service();
    const ordinary = await svc.create(actor(), { count: 1, note: "external", expiresAt: null }, context);
    const annotatorId = randomUUID();
    await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'outsider')`, [annotatorId]);
    await db.query(
      `INSERT INTO annotator_invite_uses(id, code, annotator_id) VALUES ($1, $2, $3)`,
      [randomUUID(), normalize(ordinary.codes[0]!), annotatorId],
    );

    const links = await svc.annotatorLinks(actor());
    for (const link of links.values()) {
      expect(link.annotatorId).not.toBe(annotatorId);
    }
  });
});
