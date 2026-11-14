import { createHash, randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { normalize } from "./app-invites.js";
import { PostgresAppRepository } from "./app-repository.js";
import type { SqlClient } from "./sql.js";
import {
  activateCredentialSql,
  enrollDeviceSql,
  findCredentialByEmailSql,
  findSessionSql,
  findSignupChallengeForResendSql,
  inviteRedeemableSql,
  redeemInviteSql,
  registerLoginFailureSql,
  rotateDeviceCredentialSql,
  type CredentialRow,
} from "./app-repository.js";

// The public surface is the one an attacker can reach, so its statements run
// against a real PostgreSQL engine rather than being argued from the SQL text.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const argon2Hash = "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA";

let db: PGlite;
let operatorId: string;
const deviceProfileCatalogId = "dev-device-profile";

async function insertAnnotator(status: "active" | "suspended" = "active") {
  const id = randomUUID();
  await db.query(`INSERT INTO annotators(id, alias, status) VALUES ($1, $2, $3)`, [
    id,
    `annotator-${id.slice(0, 8)}`,
    status,
  ]);
  return id;
}

async function insertCredential(
  annotatorId: string,
  overrides: { status?: string; email?: string } = {},
) {
  const status = overrides.status ?? "pending_verification";
  const verified = status === "pending_verification" ? "NULL" : "transaction_timestamp()";
  // The table refuses `suspended` without a timestamp, so the fixture has to
  // build the same shape the service would.
  const suspended = status === "suspended" ? "transaction_timestamp()" : "NULL";
  const email = overrides.email ?? `user-${annotatorId.slice(0, 8)}@example.com`;
  await db.query(
    `INSERT INTO annotator_credentials(
       annotator_id, email, password_hash, status, email_verified_at, suspended_at
     ) VALUES ($1, $2, $3, $4, ${verified}, ${suspended})`,
    [annotatorId, email, argon2Hash, status],
  );
  return email;
}

async function insertInvite(code: string, maxUses = 1) {
  await db.query(`INSERT INTO invite_codes(code, created_by, max_uses) VALUES ($1, $2, $3)`, [
    code,
    operatorId,
    maxUses,
  ]);
  return code;
}

async function redeem(code: string, annotatorId: string) {
  const result = await db.query<{ code: string }>(redeemInviteSql, [code, annotatorId]);
  return result.rows.length > 0;
}

async function issueSession(annotatorId: string, overrides: { expired?: boolean } = {}) {
  const token = `token-${randomUUID()}`;
  const tokenHash = createHash("sha256").update(token).digest("hex");
  // `expires_at > created_at` is enforced, so an already-expired session cannot
  // be inserted at "now" — it has to be backdated the way a real one would have
  // aged. The constraint is doing its job; the fixture has to respect it.
  const [created, expires] = overrides.expired
    ? ["transaction_timestamp() - interval '2 hours'", "transaction_timestamp() - interval '1 hour'"]
    : ["transaction_timestamp()", "transaction_timestamp() + interval '1 hour'"];
  await db.query(
    `INSERT INTO annotator_sessions(id, annotator_id, token_hash, created_at, expires_at)
     VALUES ($1, $2, $3, ${created}, ${expires})`,
    [randomUUID(), annotatorId, tokenHash],
  );
  return tokenHash;
}

async function findSession(tokenHash: string) {
  const result = await db.query<{ annotator_id: string }>(findSessionSql, [tokenHash]);
  return result.rows[0] ?? null;
}

async function enroll(annotatorId: string, clientDeviceId: string, fingerprint: string) {
  const result = await db.query<{ id: string; created: boolean }>(enrollDeviceSql, [
    randomUUID(),
    clientDeviceId,
    annotatorId,
    fingerprint,
    "10.0.26100",
    "10DE:2684",
    "566.36",
    "{}",
    "[]",
    "0.1.0",
  ]);
  return result.rows[0]?.created ? result.rows[0].id : null;
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
    [operatorId, argon2Hash],
  );
  const deviceProfileVersionId = randomUUID();
  await db.query(
    `INSERT INTO trusted_catalog_versions(
       id, kind, catalog_id, version, game_id, content_hash, snapshot,
       status, approved_at, approved_by
     ) VALUES ($1, 'device_profile', $2, '1', NULL, $3, '{}'::jsonb,
               'approved', transaction_timestamp(), 'test')`,
    [deviceProfileVersionId, deviceProfileCatalogId, `sha256:${"a".repeat(64)}`],
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("invite redemption", () => {
  it("accepts the code a user actually types, in any form", async () => {
    const code = await insertInvite("DM23456789ABCD");

    // The client sends the display form; normalize is the only thing standing
    // between that and the stored primary key.
    expect(await redeem(normalize("dm-2345-6789-abcd"), await insertAnnotator())).toBe(true);
    expect(await redeem(code, await insertAnnotator())).toBe(false);
  });

  it("spends a single-use code exactly once under a concurrent race", async () => {
    const code = await insertInvite("DM22222222AAAA");
    const contenders = await Promise.all([
      insertAnnotator(),
      insertAnnotator(),
      insertAnnotator(),
      insertAnnotator(),
    ]);

    const outcomes = await Promise.all(contenders.map((id) => redeem(code, id)));

    expect(outcomes.filter(Boolean)).toHaveLength(1);
    const row = await db.query<{ used_count: number }>(
      `SELECT used_count FROM invite_codes WHERE code = $1`,
      [code],
    );
    expect(row.rows[0]?.used_count).toBe(1);
  });

  it("refuses unknown, revoked, and expired codes the same way", async () => {
    // Indistinguishable at the SQL layer too, not just in the error message:
    // every one of these simply matches no row.
    expect(await redeem("DMZZZZZZZZZZZZ", await insertAnnotator())).toBe(false);

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
});

// The read-only gate signup runs before it looks an address up, so that an
// unusable code is refused identically whether or not the address is taken.
describe("invite gate", () => {
  async function redeemable(code: string) {
    const result = await db.query<{ code: string }>(inviteRedeemableSql, [code]);
    return result.rows.length > 0;
  }

  it("agrees with redemption about which codes are usable", async () => {
    const open = await insertInvite("DM66666666EEEE");
    expect(await redeemable(open)).toBe(true);

    const spent = await insertInvite("DM77777777FFFF");
    await redeem(spent, await insertAnnotator());
    expect(await redeemable(spent)).toBe(false);

    const revoked = await insertInvite("DM88888888GGGG");
    await db.query(
      `UPDATE invite_codes SET status = 'revoked', revoked_at = transaction_timestamp(),
              revoked_by = $2 WHERE code = $1`,
      [revoked, operatorId],
    );
    expect(await redeemable(revoked)).toBe(false);

    const expired = await insertInvite("DM99999999HHHH");
    await db.query(
      `UPDATE invite_codes SET expires_at = transaction_timestamp() - interval '1 second'
        WHERE code = $1`,
      [expired],
    );
    expect(await redeemable(expired)).toBe(false);

    expect(await redeemable("DMZZZZZZZZZZZZ")).toBe(false);
  });

  it("does not itself spend a use", async () => {
    const code = await insertInvite("DMBBBBBBBBJJJJ");
    await redeemable(code);
    await redeemable(code);
    const row = await db.query<{ used_count: number }>(
      `SELECT used_count FROM invite_codes WHERE code = $1`,
      [code],
    );
    expect(row.rows[0]?.used_count).toBe(0);
    // And redemption still works afterwards.
    expect(await redeem(code, await insertAnnotator())).toBe(true);
  });
});

describe("login lockout", () => {
  const policy = { threshold: 10, baseMinutes: 15, maxMinutes: 240 };

  // Through the repository method, never the statement directly. Driving the
  // SQL with hand-written parameters is what let the units drift: the statement
  // and the call site each looked right on their own while the pair was off by
  // sixty, and a test that supplies its own arguments cannot see that.
  async function fail(annotatorId: string, times: number) {
    const repository = new PostgresAppRepository(
      { query: (text, values) => db.query(text, values as unknown[]) },
      async (run) => run({ query: (text, values) => db.query(text, values as unknown[]) }),
    );
    for (let attempt = 0; attempt < times; attempt += 1) {
      await repository.registerLoginFailure(annotatorId, policy);
    }
    const row = await db.query<{ failed_login_count: number; locked_until: Date | null }>(
      `SELECT failed_login_count, locked_until FROM annotator_credentials WHERE annotator_id = $1`,
      [annotatorId],
    );
    return row.rows[0];
  }

  it("locks for exactly the configured minutes", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    const before = Date.now();
    const row = await fail(annotatorId, policy.threshold);

    expect(row?.failed_login_count).toBe(policy.threshold);
    const lockedFor = (row?.locked_until as Date).getTime() - before;
    // 15 minutes, not 15 seconds and not 15 hours. The statement's interval unit
    // and the call site's conversion have to agree, and each of those has been
    // wrong on its own at some point.
    expect(lockedFor).toBeGreaterThan(14 * 60_000);
    expect(lockedFor).toBeLessThan(16 * 60_000);
  });

  it("holds the lock steady between thresholds and doubles at the next one", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    const first = await fail(annotatorId, policy.threshold);
    const between = await fail(annotatorId, 5);
    // Failures 11–15 do not extend the lock; the ladder steps per threshold.
    expect(between?.locked_until).toEqual(first?.locked_until);

    const before = Date.now();
    const second = await fail(annotatorId, 5);
    const lockedFor = (second?.locked_until as Date).getTime() - before;
    expect(lockedFor).toBeGreaterThan(29 * 60_000);
    expect(lockedFor).toBeLessThan(31 * 60_000);
  });

  it("caps the ladder instead of overflowing out of it", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    // Far enough up that an int multiplication would exceed 2^31 and raise
    // "integer out of range", which made every later failed login a 500.
    await db.query(`UPDATE annotator_credentials SET failed_login_count = 419 WHERE annotator_id = $1`, [
      annotatorId,
    ]);

    const before = Date.now();
    const row = await fail(annotatorId, 1);

    const lockedFor = (row?.locked_until as Date).getTime() - before;
    expect(lockedFor).toBeGreaterThan(239 * 60_000);
    expect(lockedFor).toBeLessThan(241 * 60_000);
  });

  // `power(int, int)` is the double-precision overload whatever the surrounding
  // casts say, so an unclamped exponent overflows float8 — later than the int
  // overflow above, but reachable: 30 wrong passwords an hour gets there in a
  // fortnight, and from then on the account cannot even fail to log in.
  it("survives a failure count no ladder should ever reach", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);
    await db.query(
      `UPDATE annotator_credentials SET failed_login_count = 99999 WHERE annotator_id = $1`,
      [annotatorId],
    );

    const before = Date.now();
    const row = await fail(annotatorId, 1);

    const lockedFor = (row?.locked_until as Date).getTime() - before;
    expect(lockedFor).toBeGreaterThan(239 * 60_000);
    expect(lockedFor).toBeLessThan(241 * 60_000);
  });
});

describe("signup challenge resend", () => {
  async function lookup(challengeId: string) {
    const result = await db.query<{ annotator_id: string; email: string }>(
      findSignupChallengeForResendSql,
      [challengeId],
    );
    return result.rows[0] ?? null;
  }

  async function issueChallenge(
    annotatorId: string,
    overrides: { purpose?: string; expired?: boolean; consumed?: boolean } = {},
  ) {
    const id = randomUUID();
    const expires = overrides.expired
      ? "transaction_timestamp() - interval '1 hour'"
      : "transaction_timestamp() + interval '10 minutes'";
    const created = overrides.expired
      ? "transaction_timestamp() - interval '2 hours'"
      : "transaction_timestamp()";
    await db.query(
      `INSERT INTO annotator_email_challenges(
         id, annotator_id, purpose, code_salt, code_hash, created_at, expires_at, consumed_at
       ) VALUES ($1, $2, $3, $4, $5, ${created}, ${expires},
                 ${overrides.consumed ? "transaction_timestamp()" : "NULL"})`,
      [
        id,
        annotatorId,
        overrides.purpose ?? "signup_verify",
        Buffer.alloc(32, 1),
        "d".repeat(64),
        ],
    );
    return id;
  }

  it("resolves an expired challenge, because that is exactly when a resend is asked for", async () => {
    const annotatorId = await insertAnnotator();
    const email = await insertCredential(annotatorId);
    const challengeId = await issueChallenge(annotatorId, { expired: true });

    const found = await lookup(challengeId);
    expect(found).toEqual({ id: challengeId, annotator_id: annotatorId, email });
  });

  it("refuses to re-mail an account that already verified", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });
    const challengeId = await issueChallenge(annotatorId);

    expect(await lookup(challengeId)).toBeNull();
  });

  it("refuses a suspended credential and a suspended annotator", async () => {
    const suspendedCredential = await insertAnnotator();
    await insertCredential(suspendedCredential, { status: "suspended" });
    expect(await lookup(await issueChallenge(suspendedCredential))).toBeNull();

    const suspendedAnnotator = await insertAnnotator("suspended");
    await insertCredential(suspendedAnnotator);
    expect(await lookup(await issueChallenge(suspendedAnnotator))).toBeNull();
  });

  it("refuses a consumed challenge and one issued for another purpose", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    expect(await lookup(await issueChallenge(annotatorId, { consumed: true }))).toBeNull();
    expect(await lookup(await issueChallenge(annotatorId, { purpose: "password_reset" }))).toBeNull();
  });
});

describe("signup transaction", () => {
  const terms = {
    policyVersion: "v1",
    purpose: "staging capture",
    termsHash: "c".repeat(64),
    allowsTrainableUse: false,
    scopes: ["input", "video"],
  };

  function repository() {
    const client: SqlClient = { query: (text, values) => db.query(text, values as unknown[]) };
    const transaction = async <T>(run: (tx: SqlClient) => Promise<T>): Promise<T> => {
      await db.query("BEGIN");
      try {
        const result = await run(client);
        await db.query("COMMIT");
        return result;
      } catch (error) {
        await db.query("ROLLBACK");
        throw error;
      }
    };
    return new PostgresAppRepository(client, transaction);
  }

  it("creates the annotator, credential, consent, and ledger row in one commit", async () => {
    const code = await insertInvite("DMAAAAAAAAAAAA");
    const result = await repository().signup({
      code,
      email: "signup@example.com",
      passwordHash: argon2Hash,
      alias: "Signer",
      consent: terms,
      ipHash: "f".repeat(64),
      userAgent: "test-agent",
    });

    expect(result).not.toBeNull();
    const consent = await db.query<{ terms_hash: string }>(
      `SELECT terms_hash FROM consents WHERE annotator_id = $1`,
      [result!.annotatorId],
    );
    expect(consent.rows[0]?.terms_hash).toBe(terms.termsHash);
    const use = await db.query<{ code: string }>(
      `SELECT code FROM annotator_invite_uses WHERE annotator_id = $1`,
      [result!.annotatorId],
    );
    expect(use.rows[0]?.code).toBe(code);
  });

  it("stamps the consent's trainable authorization from the configured terms decision", async () => {
    // docs/49 E2: the terms version carries the decision. false stays the
    // hardcoded-era behaviour; true is the explicit teaser-terms deployment.
    const blocked = await repository().signup({
      code: await insertInvite("DMTRAAAAFFFF22"),
      email: "no-trainable@example.com",
      passwordHash: argon2Hash,
      alias: "NoTrain",
      consent: { ...terms, allowsTrainableUse: false },
      ipHash: "f".repeat(64),
      userAgent: "test-agent",
    });
    const granted = await repository().signup({
      code: await insertInvite("DMTRAAAATTTT33"),
      email: "trainable@example.com",
      passwordHash: argon2Hash,
      alias: "Train",
      consent: { ...terms, allowsTrainableUse: true },
      ipHash: "f".repeat(64),
      userAgent: "test-agent",
    });

    const rows = await db.query<{ annotator_id: string; allows_trainable_use: boolean }>(
      `SELECT annotator_id, allows_trainable_use FROM consents WHERE annotator_id = ANY($1::uuid[])`,
      [[blocked!.annotatorId, granted!.annotatorId]],
    );
    const byAnnotator = new Map(rows.rows.map((row) => [row.annotator_id, row.allows_trainable_use]));
    expect(byAnnotator.get(blocked!.annotatorId)).toBe(false);
    expect(byAnnotator.get(granted!.annotatorId)).toBe(true);
  });

  // The annotator row is inserted before the code is redeemed, so a bad code has
  // to roll it back — otherwise every rejected signup leaks an orphan annotator.
  it("leaves no annotator behind when the code cannot be redeemed", async () => {
    const before = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM annotators`,
    );

    const result = await repository().signup({
      code: "DMZZZZZZZZZZZZ",
      email: "orphan@example.com",
      passwordHash: argon2Hash,
      alias: "Ghost",
      consent: terms,
      ipHash: null,
      userAgent: null,
    });

    expect(result).toBeNull();
    const after = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM annotators`,
    );
    expect(after.rows[0]?.count).toBe(before.rows[0]?.count);
    const credential = await db.query(
      `SELECT 1 FROM annotator_credentials WHERE email = 'orphan@example.com'`,
    );
    expect(credential.rows).toHaveLength(0);
  });
});

describe("credential lookup", () => {
  it("returns suspended accounts so login can spend the same time on them", async () => {
    // Returning null for a suspended account would make login measurably faster
    // for it, which leaks exactly what the uniform error message hides.
    const annotatorId = await insertAnnotator();
    const email = await insertCredential(annotatorId, { status: "suspended" });
    await db.query(
      `UPDATE annotator_credentials SET suspended_at = transaction_timestamp()
        WHERE annotator_id = $1`,
      [annotatorId],
    );

    const result = await db.query<CredentialRow>(findCredentialByEmailSql, [email]);
    expect(result.rows[0]?.status).toBe("suspended");
  });

  it("surfaces the annotator's own status alongside the credential", async () => {
    const annotatorId = await insertAnnotator("suspended");
    const email = await insertCredential(annotatorId, { status: "active" });

    const result = await db.query<CredentialRow>(findCredentialByEmailSql, [email]);
    expect(result.rows[0]?.annotator_status).toBe("suspended");
  });

  it("finds nothing for an address that was never registered", async () => {
    const result = await db.query<CredentialRow>(findCredentialByEmailSql, ["ghost@example.com"]);
    expect(result.rows).toHaveLength(0);
  });
});

describe("activation", () => {
  it("activates a pending account once and ignores a replay", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    const first = await db.query<{ annotator_id: string }>(activateCredentialSql, [annotatorId]);
    const replay = await db.query<{ annotator_id: string }>(activateCredentialSql, [annotatorId]);

    expect(first.rows).toHaveLength(1);
    expect(replay.rows).toHaveLength(0);
  });

  it("refuses to resurrect a suspended account", async () => {
    // A verification link that arrives after an operator suspended the account
    // must not undo the suspension.
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });
    await db.query(
      `UPDATE annotator_credentials
          SET status = 'suspended', suspended_at = transaction_timestamp()
        WHERE annotator_id = $1`,
      [annotatorId],
    );

    const result = await db.query<{ annotator_id: string }>(activateCredentialSql, [annotatorId]);
    expect(result.rows).toHaveLength(0);
  });
});

describe("session resolution", () => {
  it("resolves a live session", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });

    expect(await findSession(await issueSession(annotatorId))).toMatchObject({
      annotator_id: annotatorId,
    });
  });

  it("stops an expired or revoked session", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });

    expect(await findSession(await issueSession(annotatorId, { expired: true }))).toBeNull();

    const revoked = await issueSession(annotatorId);
    await db.query(
      `UPDATE annotator_sessions SET revoked_at = transaction_timestamp() WHERE token_hash = $1`,
      [revoked],
    );
    expect(await findSession(revoked)).toBeNull();
  });

  // The reason this is a table and not a JWT (10 §12.7): suspension has to bite
  // on the next request, not whenever the token happens to expire.
  it("stops an already-issued session the moment the account is suspended", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });
    const tokenHash = await issueSession(annotatorId);
    expect(await findSession(tokenHash)).not.toBeNull();

    await db.query(
      `UPDATE annotator_credentials
          SET status = 'suspended', suspended_at = transaction_timestamp()
        WHERE annotator_id = $1`,
      [annotatorId],
    );
    expect(await findSession(tokenHash)).toBeNull();
  });

  it("stops the session when the annotator itself is suspended", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId, { status: "active" });
    const tokenHash = await issueSession(annotatorId);

    await db.query(`UPDATE annotators SET status = 'suspended' WHERE id = $1`, [annotatorId]);
    expect(await findSession(tokenHash)).toBeNull();
  });

  it("never resolves a session for an account that has not verified its address", async () => {
    const annotatorId = await insertAnnotator();
    await insertCredential(annotatorId);

    expect(await findSession(await issueSession(annotatorId))).toBeNull();
  });
});

describe("device enrollment", () => {
  it("enrolls a device against the caller and activates it immediately", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await enroll(annotatorId, "machine-1", "1".repeat(64));

    const row = await db.query<{
      status: string;
      annotator_id: string;
      certified_profile_id: string | null;
    }>(
      `SELECT status, annotator_id, certified_profile_id FROM devices WHERE id = $1`,
      [deviceId],
    );
    // D6: holding a valid invite was the authorization decision, so there is no
    // second operator gate here.
    expect(row.rows[0]).toEqual({
      status: "active",
      annotator_id: annotatorId,
      certified_profile_id: null,
    });
    const bindings = await db.query(
      `SELECT profile_id FROM device_profile_bindings WHERE device_id = $1`,
      [deviceId],
    );
    expect(bindings.rows).toHaveLength(0);
  });

  it("is idempotent for the same machine instead of minting a second credential", async () => {
    const annotatorId = await insertAnnotator();
    await enroll(annotatorId, "machine-2", "2".repeat(64));

    // No row back: the caller returns the existing device with a null token.
    expect(await enroll(annotatorId, "machine-2", "3".repeat(64))).toBeNull();
  });

  // D4: unbounded on purpose. This has to work, not be defended against.
  it("lets one annotator enroll many devices", async () => {
    const annotatorId = await insertAnnotator();
    for (let index = 0; index < 10; index += 1) {
      // Fingerprints are globally unique, so the fixture derives them from a
      // fresh uuid rather than from the loop index.
      const fingerprint = createHash("sha256").update(randomUUID()).digest("hex");
      expect(await enroll(annotatorId, `fleet-${index}`, fingerprint)).not.toBeNull();
    }
  });

  it("rotates only the caller's own device", async () => {
    const owner = await insertAnnotator();
    const stranger = await insertAnnotator();
    const deviceId = await enroll(owner, "machine-3", "4".repeat(64));

    const stolen = await db.query<{ id: string }>(rotateDeviceCredentialSql, [
      deviceId,
      stranger,
      "5".repeat(64),
    ]);
    expect(stolen.rows).toHaveLength(0);

    const own = await db.query<{ id: string }>(rotateDeviceCredentialSql, [
      deviceId,
      owner,
      "5".repeat(64),
    ]);
    expect(own.rows).toHaveLength(1);
  });

  it("refuses to rotate a retired device back into service", async () => {
    const annotatorId = await insertAnnotator();
    const deviceId = await enroll(annotatorId, "machine-4", "6".repeat(64));
    await db.query(
      `UPDATE devices SET status = 'retired', retired_at = transaction_timestamp() WHERE id = $1`,
      [deviceId],
    );

    const result = await db.query<{ id: string }>(rotateDeviceCredentialSql, [
      deviceId,
      annotatorId,
      "7".repeat(64),
    ]);
    expect(result.rows).toHaveLength(0);
  });
});
