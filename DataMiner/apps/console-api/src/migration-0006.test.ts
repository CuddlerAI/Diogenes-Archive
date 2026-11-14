import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// Same discipline as migration.test.ts: the constraints and triggers below are
// executed against a real PostgreSQL engine, not argued from the SQL text.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const argon2Hash = "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA";

let db: PGlite;
let operatorId: string;

async function insertOperator() {
  const id = randomUUID();
  await db.query(
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, $2, $3, transaction_timestamp(), 'admin', 'active')`,
    [id, `ops-${id.slice(0, 8)}@diogenesmodel.com`, argon2Hash],
  );
  return id;
}

async function insertAnnotator() {
  const id = randomUUID();
  await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, $2)`, [
    id,
    `annotator-${id.slice(0, 8)}`,
  ]);
  return id;
}

async function insertInvite(code: string, overrides: { maxUses?: number } = {}) {
  await db.query(`INSERT INTO invite_codes(code, created_by, max_uses) VALUES ($1, $2, $3)`, [
    code,
    operatorId,
    overrides.maxUses ?? 1,
  ]);
  return code;
}

// The redemption path exactly as the service will run it: one conditional
// UPDATE, then the ledger row. rowcount is expressed through RETURNING so the
// behaviour is identical on any PostgreSQL client (console-api's SqlClient
// deliberately exposes only `rows`).
async function redeem(code: string, annotatorId: string) {
  const claimed = await db.query<{ code: string }>(
    `UPDATE invite_codes
        SET used_count = used_count + 1,
            used_at = COALESCE(used_at, transaction_timestamp()),
            used_by_annotator_id = COALESCE(used_by_annotator_id, $2)
      WHERE code = $1
        AND status = 'active'
        AND used_count < max_uses
        AND (expires_at IS NULL OR expires_at > transaction_timestamp())
      RETURNING code`,
    [code, annotatorId],
  );
  if (claimed.rows.length === 0) return false;
  await db.query(
    `INSERT INTO annotator_invite_uses(id, code, annotator_id) VALUES ($1, $2, $3)`,
    [randomUUID(), code, annotatorId],
  );
  return true;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
  operatorId = await insertOperator();
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("annotator migration 0006", () => {
  it("applies on top of every earlier migration and records itself in the ledger", async () => {
    const ledger = await db.query<{ version: number; name: string }>(
      `SELECT version, name FROM schema_migrations ORDER BY version`,
    );

    // This suite owns migration 0006, not the repository tail. Later
    // migrations may be present as long as the chain through 0006 is intact.
    expect(ledger.rows.map((row) => row.version).slice(0, 6)).toEqual([1, 2, 3, 4, 5, 6]);
    expect(ledger.rows.find((row) => row.version === 6)?.name).toBe("annotator_identity");
  });
});

describe("invite codes", () => {
  it("accepts only the normalized code shape", async () => {
    await expect(insertInvite("DM23456789ABCD")).resolves.toBeDefined();

    // Display form with separators, lowercase, ambiguous characters, and wrong
    // length are all refused: normalization happens before the insert, so a
    // non-normalized value reaching the table is a bug worth failing on.
    for (const bad of ["DM-2345-6789-ABCD", "dm23456789abcd", "DM234567890ABC", "DM23456789ABC"]) {
      await expect(insertInvite(bad)).rejects.toThrow();
    }
  });

  it("redeems once and refuses every later attempt", async () => {
    const code = await insertInvite("DM22222222AAAA");
    const first = await insertAnnotator();
    const second = await insertAnnotator();

    expect(await redeem(code, first)).toBe(true);
    expect(await redeem(code, second)).toBe(false);

    const row = await db.query<{ used_count: number; used_by_annotator_id: string }>(
      `SELECT used_count, used_by_annotator_id FROM invite_codes WHERE code = $1`,
      [code],
    );
    expect(row.rows[0]).toEqual({ used_count: 1, used_by_annotator_id: first });
  });

  it("keeps the first redeemer denormalized while the ledger holds every use", async () => {
    const code = await insertInvite("DM33333333BBBB", { maxUses: 2 });
    const first = await insertAnnotator();
    const second = await insertAnnotator();

    expect(await redeem(code, first)).toBe(true);
    expect(await redeem(code, second)).toBe(true);
    expect(await redeem(code, await insertAnnotator())).toBe(false);

    const invite = await db.query<{ used_count: number; used_by_annotator_id: string }>(
      `SELECT used_count, used_by_annotator_id FROM invite_codes WHERE code = $1`,
      [code],
    );
    // COALESCE means the denormalized column still names the *first* redeemer.
    expect(invite.rows[0]).toEqual({ used_count: 2, used_by_annotator_id: first });

    const uses = await db.query<{ annotator_id: string }>(
      `SELECT annotator_id FROM annotator_invite_uses WHERE code = $1 ORDER BY used_at, id`,
      [code],
    );
    expect(uses.rows.map((row) => row.annotator_id).sort()).toEqual([first, second].sort());
  });

  it("refuses a revoked code and an expired code", async () => {
    const revoked = await insertInvite("DM44444444CCCC");
    await db.query(
      `UPDATE invite_codes SET status = 'revoked', revoked_at = transaction_timestamp(),
              revoked_by = $2 WHERE code = $1`,
      [revoked, operatorId],
    );
    expect(await redeem(revoked, await insertAnnotator())).toBe(false);

    const expired = await insertInvite("DM55555555DDDD");
    await db.query(
      `UPDATE invite_codes SET expires_at = transaction_timestamp() - interval '1 second'
        WHERE code = $1`,
      [expired],
    );
    expect(await redeem(expired, await insertAnnotator())).toBe(false);
  });

  it("refuses to mark a code revoked without recording when", async () => {
    const code = await insertInvite("DM66666666EEEE");

    await expect(
      db.query(`UPDATE invite_codes SET status = 'revoked' WHERE code = $1`, [code]),
    ).rejects.toThrow();
  });

  it("refuses a used_count that outruns max_uses, and a use count with no redeemer", async () => {
    const code = await insertInvite("DM77777777FFFF");

    await expect(
      db.query(`UPDATE invite_codes SET used_count = 2 WHERE code = $1`, [code]),
    ).rejects.toThrow();
    // used_count > 0 without used_at would leave the console showing an unused
    // code that cannot be redeemed.
    await expect(
      db.query(`UPDATE invite_codes SET used_count = 1 WHERE code = $1`, [code]),
    ).rejects.toThrow();
  });

  it("records one redemption per annotator per code", async () => {
    const code = await insertInvite("DM88888888GGGG", { maxUses: 3 });
    const annotatorId = await insertAnnotator();
    await redeem(code, annotatorId);

    await expect(
      db.query(`INSERT INTO annotator_invite_uses(id, code, annotator_id) VALUES ($1, $2, $3)`, [
        randomUUID(),
        code,
        annotatorId,
      ]),
    ).rejects.toThrow();
  });

  it("tombstones rather than deletes", async () => {
    const code = await insertInvite("DM99999999HHHH");

    await expect(db.query(`DELETE FROM invite_codes WHERE code = $1`, [code])).rejects.toThrow(
      /tombstoned, not deleted/,
    );
  });
});

describe("annotator credentials", () => {
  async function insertCredential(
    annotatorId: string,
    overrides: { status?: string; verified?: boolean } = {},
  ) {
    const status = overrides.status ?? "pending_verification";
    const verifiedAt = overrides.verified ? "transaction_timestamp()" : "NULL";
    await db.query(
      `INSERT INTO annotator_credentials(annotator_id, email, password_hash, status, email_verified_at)
       VALUES ($1, $2, $3, $4, ${verifiedAt})`,
      [annotatorId, `user-${annotatorId.slice(0, 8)}@example.com`, argon2Hash, status],
    );
  }

  it("stores addresses in one canonical form", async () => {
    const annotatorId = await insertAnnotator();

    await expect(
      db.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, 'Mixed@Example.com', $2)`,
        [annotatorId, argon2Hash],
      ),
    ).rejects.toThrow();
    await expect(
      db.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, 'not-an-address', $2)`,
        [annotatorId, argon2Hash],
      ),
    ).rejects.toThrow();
  });

  it("refuses a password that is not an argon2id PHC string", async () => {
    const annotatorId = await insertAnnotator();

    await expect(
      db.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, 'plain@example.com', 'hunter2')`,
        [annotatorId],
      ),
    ).rejects.toThrow();
  });

  // D5 + D6 at the table level: verification is the only gate to active, and it
  // is a real gate.
  it("refuses an active account whose address was never verified", async () => {
    const annotatorId = await insertAnnotator();

    await expect(insertCredential(annotatorId, { status: "active" })).rejects.toThrow();
    await expect(
      insertCredential(annotatorId, { status: "active", verified: true }),
    ).resolves.toBeUndefined();
  });

  it("refuses a second account on the same address", async () => {
    const first = await insertAnnotator();
    const second = await insertAnnotator();
    await db.query(
      `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
       VALUES ($1, 'shared@example.com', $2)`,
      [first, argon2Hash],
    );

    await expect(
      db.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, 'shared@example.com', $2)`,
        [second, argon2Hash],
      ),
    ).rejects.toThrow();
  });

  it("gives one annotator at most one credential row", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    await expect(
      db.query(
        `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
         VALUES ($1, 'second@example.com', $2)`,
        [annotatorId, argon2Hash],
      ),
    ).rejects.toThrow();
  });
});

describe("device fleet and telemetry", () => {
  let deviceProfileVersionId: string;

  async function insertDevice(annotatorId: string | null) {
    const id = randomUUID();
    await db.query(
      `INSERT INTO devices(
         id, client_device_id, annotator_id, certified_profile_id,
         credential_fingerprint, status, windows_build, gpu_pci_id, gpu_driver,
         display_snapshot, input_devices_snapshot, client_version
       ) VALUES ($1, $2, $3, $4, $5, 'active', '10.0.26100', '10DE:2684', '566.36',
                 '{}'::jsonb, '[]'::jsonb, '0.1.0')`,
      [
        id,
        `client-${id.slice(0, 8)}`,
        annotatorId,
        "dev-device-profile",
        id.replace(/-/g, "").padEnd(64, "0"),
      ],
    );
    return id;
  }

  beforeAll(async () => {
    deviceProfileVersionId = randomUUID();
    await db.query(
      `INSERT INTO trusted_catalog_versions(
         id, kind, catalog_id, version, game_id, content_hash, snapshot,
         status, approved_at, approved_by
       ) VALUES ($1, 'device_profile', 'dev-device-profile', '1', NULL, $2, '{}'::jsonb,
                 'approved', transaction_timestamp(), 'test')`,
      [deviceProfileVersionId, `sha256:${"a".repeat(64)}`],
    );
  });

  // D4: no upper bound, so this must simply work rather than be enforced against.
  it("lets one annotator bind an unbounded number of devices", async () => {
    const annotatorId = await insertAnnotator();
    for (let index = 0; index < 12; index += 1) {
      await insertDevice(annotatorId);
    }

    const count = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM devices WHERE annotator_id = $1`,
      [annotatorId],
    );
    expect(count.rows[0]?.count).toBe("12");
  });

  it("refuses telemetry that names a consent without its annotator", async () => {
    const deviceId = await insertDevice(null);

    await expect(
      db.query(
        `INSERT INTO client_events(
           event_id, device_id, annotator_id, consent_id, occurred_at, kind, severity, client_version
         ) VALUES ($1, $2, NULL, $3, transaction_timestamp(), 'startup', 'info', '0.1.0')`,
        [randomUUID(), deviceId, randomUUID()],
      ),
    ).rejects.toThrow();
  });

  it("deduplicates a retried event by its client-generated id", async () => {
    const deviceId = await insertDevice(null);
    const eventId = randomUUID();
    const insert = () =>
      db.query(
        `INSERT INTO client_events(
           event_id, device_id, occurred_at, kind, severity, client_version
         ) VALUES ($1, $2, transaction_timestamp(), 'startup', 'info', '0.1.0')`,
        [eventId, deviceId],
      );

    await expect(insert()).resolves.toBeDefined();
    await expect(insert()).rejects.toThrow();
  });

  it("refuses a purged event with no record of when it was purged", async () => {
    const deviceId = await insertDevice(null);
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, occurred_at, kind, severity, client_version
       ) VALUES ($1, $2, transaction_timestamp(), 'startup', 'info', '0.1.0')`,
      [randomUUID(), deviceId],
    );

    await expect(
      db.query(`UPDATE client_events SET retention_state = 'purged' WHERE device_id = $1`, [
        deviceId,
      ]),
    ).rejects.toThrow();
  });

  // Consent-scoped purge reaches telemetry through consent_id, with no new
  // purge_scope value and no change to the purge_jobs CHECK (11 §6.4).
  it("lets a consent-scoped sweep find exactly its own telemetry", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await insertDevice(annotatorId);
    const consentId = randomUUID();
    await db.query(
      `INSERT INTO consents(id, annotator_id, policy_version, purpose, terms_hash, status, accepted_at)
       VALUES ($1, $2, 'v1', 'staging capture', $3, 'active', transaction_timestamp())`,
      [consentId, annotatorId, "c".repeat(64)],
    );
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, annotator_id, consent_id, occurred_at, kind, severity, client_version
       ) VALUES ($1, $2, $3, $4, transaction_timestamp(), 'session_started', 'info', '0.1.0')`,
      [randomUUID(), deviceId, annotatorId, consentId],
    );
    // A signup-time event: no consent yet, so consent purge cannot reach it.
    await db.query(
      `INSERT INTO client_events(
         event_id, device_id, annotator_id, occurred_at, kind, severity, client_version
       ) VALUES ($1, $2, $3, transaction_timestamp(), 'enrolled', 'info', '0.1.0')`,
      [randomUUID(), deviceId, annotatorId],
    );

    const swept = await db.query<{ kind: string }>(
      `UPDATE client_events
          SET retention_state = 'purged', purged_at = transaction_timestamp(), details = '{}'::jsonb
        WHERE consent_id = $1 AND retention_state <> 'purged'
        RETURNING kind`,
      [consentId],
    );
    expect(swept.rows.map((row) => row.kind)).toEqual(["session_started"]);

    // The gap is real and documented: the consent-less row survives and needs
    // the annotator-tombstone sweep plus a TTL instead.
    const left = await db.query<{ kind: string }>(
      `SELECT kind FROM client_events
        WHERE annotator_id = $1 AND retention_state = 'retained'`,
      [annotatorId],
    );
    expect(left.rows.map((row) => row.kind)).toEqual(["enrolled"]);
  });
});
