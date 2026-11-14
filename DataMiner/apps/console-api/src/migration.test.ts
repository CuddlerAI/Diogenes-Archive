import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// These run against a real PostgreSQL engine (PGlite), so the constraints and
// triggers below are executed rather than argued from the SQL text.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;

async function insertOperator(overrides: Record<string, string> = {}) {
  const id = randomUUID();
  const email = overrides.email ?? `ops-${id.slice(0, 8)}@diogenesmodel.com`;
  await db.query(
    `INSERT INTO operators(id, email, password_hash, password_updated_at, role, status)
     VALUES ($1, $2, $3, transaction_timestamp(), 'admin', 'active')`,
    [id, email, "$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA"],
  );
  return id;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("console migration chain", () => {
  it("applies console, annotator identity, and mirror outbox migrations in order", async () => {
    const ledger = await db.query<{ version: number; name: string }>(
      `SELECT version, name FROM schema_migrations ORDER BY version`,
    );

    // The ledger may grow after this PR. Assert ordering, uniqueness, and the
    // migrations whose interaction this suite is responsible for.
    const versions = ledger.rows.map((row) => row.version);
    expect(versions).toEqual([...versions].sort((left, right) => left - right));
    expect(new Set(versions).size).toBe(versions.length);
    expect(ledger.rows).toContainEqual({ version: 5, name: "console_operators" });
    expect(ledger.rows).toContainEqual({ version: 6, name: "annotator_identity" });
    expect(ledger.rows).toContainEqual({ version: 7, name: "mirror_credential_outbox" });
  });

  it("stores addresses in one canonical form", async () => {
    await expect(
      db.query(`INSERT INTO operators(id, email) VALUES ($1, 'Mixed@Diogenesmodel.com')`, [
        randomUUID(),
      ]),
    ).rejects.toThrow();
    await expect(
      db.query(`INSERT INTO operators(id, email) VALUES ($1, 'not-an-address')`, [randomUUID()]),
    ).rejects.toThrow();
  });

  it("refuses a second operator with the same address", async () => {
    await insertOperator({ email: "dup@diogenesmodel.com" });

    await expect(insertOperator({ email: "dup@diogenesmodel.com" })).rejects.toThrow();
  });

  it("refuses an active operator without a password", async () => {
    await expect(
      db.query(`INSERT INTO operators(id, email, status) VALUES ($1, $2, 'active')`, [
        randomUUID(),
        "nopass@diogenesmodel.com",
      ]),
    ).rejects.toThrow();
  });

  it("refuses a password that is not an argon2id hash", async () => {
    await expect(
      db.query(
        `INSERT INTO operators(id, email, password_hash, password_updated_at, status)
         VALUES ($1, $2, 'plaintext', transaction_timestamp(), 'active')`,
        [randomUUID(), "plain@diogenesmodel.com"],
      ),
    ).rejects.toThrow();
  });

  it("tombstones operators instead of deleting them", async () => {
    const id = await insertOperator();

    await expect(db.query(`DELETE FROM operators WHERE id = $1`, [id])).rejects.toThrow(
      /tombstoned, not deleted/,
    );
  });

  it("keeps a challenge salt at exactly 32 bytes and the hash hex encoded", async () => {
    const operatorId = await insertOperator();

    await expect(
      db.query(
        `INSERT INTO operator_email_challenges(id, operator_id, purpose, code_salt, code_hash, expires_at)
         VALUES ($1, $2, 'login_otp', $3, $4, transaction_timestamp() + interval '10 minutes')`,
        [randomUUID(), operatorId, Buffer.alloc(8), "a".repeat(64)],
      ),
    ).rejects.toThrow();
    await expect(
      db.query(
        `INSERT INTO operator_email_challenges(id, operator_id, purpose, code_salt, code_hash, expires_at)
         VALUES ($1, $2, 'login_otp', $3, 'not-a-digest', transaction_timestamp() + interval '10 minutes')`,
        [randomUUID(), operatorId, Buffer.alloc(32)],
      ),
    ).rejects.toThrow();
  });

  it("spends exactly one attempt per claim and refuses a consumed challenge", async () => {
    const operatorId = await insertOperator();
    const challengeId = randomUUID();
    await db.query(
      `INSERT INTO operator_email_challenges(id, operator_id, purpose, code_salt, code_hash, expires_at)
       VALUES ($1, $2, 'login_otp', $3, $4, transaction_timestamp() + interval '10 minutes')`,
      [challengeId, operatorId, Buffer.alloc(32), "b".repeat(64)],
    );

    const claim = `UPDATE operator_email_challenges
                      SET attempt_count = attempt_count + 1
                    WHERE id = $1
                      AND consumed_at IS NULL
                      AND superseded_at IS NULL
                      AND expires_at > transaction_timestamp()
                      AND attempt_count < 5
                    RETURNING attempt_count`;
    const first = await db.query<{ attempt_count: number }>(claim, [challengeId]);
    const second = await db.query<{ attempt_count: number }>(claim, [challengeId]);
    await db.query(
      `UPDATE operator_email_challenges SET consumed_at = transaction_timestamp() WHERE id = $1`,
      [challengeId],
    );
    const afterConsume = await db.query(claim, [challengeId]);

    expect(first.rows[0]?.attempt_count).toBe(1);
    expect(second.rows[0]?.attempt_count).toBe(2);
    expect(afterConsume.rows).toHaveLength(0);
  });

  it("stops claiming once the attempt budget is spent", async () => {
    const operatorId = await insertOperator();
    const challengeId = randomUUID();
    await db.query(
      `INSERT INTO operator_email_challenges(id, operator_id, purpose, code_salt, code_hash, expires_at, attempt_count)
       VALUES ($1, $2, 'login_otp', $3, $4, transaction_timestamp() + interval '10 minutes', 5)`,
      [challengeId, operatorId, Buffer.alloc(32), "c".repeat(64)],
    );

    const claimed = await db.query(
      `UPDATE operator_email_challenges SET attempt_count = attempt_count + 1
        WHERE id = $1 AND attempt_count < 5 RETURNING id`,
      [challengeId],
    );

    expect(claimed.rows).toHaveLength(0);
  });

  it("refuses a session token hash that is not a sha256 digest", async () => {
    const operatorId = await insertOperator();

    await expect(
      db.query(
        `INSERT INTO operator_sessions(id, operator_id, token_hash, expires_at)
         VALUES ($1, $2, 'short', transaction_timestamp() + interval '1 hour')`,
        [randomUUID(), operatorId],
      ),
    ).rejects.toThrow();
  });

  it("refuses two sessions sharing a token hash", async () => {
    const operatorId = await insertOperator();
    const tokenHash = "d".repeat(64);
    await db.query(
      `INSERT INTO operator_sessions(id, operator_id, token_hash, expires_at)
       VALUES ($1, $2, $3, transaction_timestamp() + interval '1 hour')`,
      [randomUUID(), operatorId, tokenHash],
    );

    await expect(
      db.query(
        `INSERT INTO operator_sessions(id, operator_id, token_hash, expires_at)
         VALUES ($1, $2, $3, transaction_timestamp() + interval '1 hour')`,
        [randomUUID(), operatorId, tokenHash],
      ),
    ).rejects.toThrow();
  });

  it("rolls the rate limit window in a single upsert", async () => {
    const bucket = `otp_min:${randomUUID()}`;
    const upsert = `INSERT INTO console_rate_limits(bucket, window_started_at, counter)
                    VALUES ($1, transaction_timestamp(), 1)
                    ON CONFLICT (bucket) DO UPDATE
                       SET window_started_at = CASE
                             WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                                  <= transaction_timestamp()
                             THEN transaction_timestamp()
                             ELSE console_rate_limits.window_started_at
                           END,
                           counter = CASE
                             WHEN console_rate_limits.window_started_at + make_interval(secs => $2)
                                  <= transaction_timestamp()
                             THEN 1
                             ELSE console_rate_limits.counter + 1
                           END
                    RETURNING counter`;

    const first = await db.query<{ counter: number }>(upsert, [bucket, 3_600]);
    const second = await db.query<{ counter: number }>(upsert, [bucket, 3_600]);
    // A window that has already elapsed resets the counter instead of accumulating.
    const rolled = await db.query<{ counter: number }>(upsert, [bucket, 0]);

    expect(first.rows[0]?.counter).toBe(1);
    expect(second.rows[0]?.counter).toBe(2);
    expect(rolled.rows[0]?.counter).toBe(1);
  });

  it("locks on the threshold with a doubling, capped interval", async () => {
    const operatorId = await insertOperator();
    const failure = `UPDATE operators
                        SET failed_login_count = failed_login_count + 1,
                            locked_until = CASE
                              WHEN (failed_login_count + 1) % $2 = 0 THEN transaction_timestamp() + make_interval(
                                mins => LEAST($4::numeric, $3::numeric * power(2, GREATEST((failed_login_count + 1) / $2 - 1, 0)))::int
                              )
                              ELSE locked_until
                            END
                      WHERE id = $1
                      RETURNING failed_login_count,
                                EXTRACT(EPOCH FROM (locked_until - transaction_timestamp())) AS seconds_left`;

    let lastSeconds = 0;
    for (let attempt = 1; attempt <= 30; attempt += 1) {
      const result = await db.query<{ failed_login_count: number; seconds_left: number | null }>(
        failure,
        [operatorId, 10, 15, 240],
      );
      const row = result.rows[0];
      if (attempt === 9) expect(row?.seconds_left).toBeNull();
      if (attempt === 10) lastSeconds = Number(row?.seconds_left);
      if (attempt === 20) {
        expect(Number(row?.seconds_left)).toBeGreaterThan(lastSeconds);
        lastSeconds = Number(row?.seconds_left);
      }
      if (attempt === 30) expect(Number(row?.seconds_left)).toBeGreaterThan(lastSeconds);
    }

    expect(lastSeconds).toBeLessThanOrEqual(240 * 60);
  });

  it("keeps the audit trail append only for operator events", async () => {
    const eventId = randomUUID();
    await db.query(
      `INSERT INTO audit_events(event_id, actor_type, actor_id, action, entity_type, entity_id, details)
       VALUES ($1, 'operator', $2, 'operator.session_created', 'operator', $2, '{}'::jsonb)`,
      [eventId, randomUUID()],
    );

    await expect(
      db.query(`UPDATE audit_events SET action = 'tampered' WHERE event_id = $1`, [eventId]),
    ).rejects.toThrow(/append-only/);
    await expect(
      db.query(`DELETE FROM audit_events WHERE event_id = $1`, [eventId]),
    ).rejects.toThrow(/append-only/);
  });

  it("provides the keyset indexes the console browsers order by", async () => {
    const indexes = await db.query<{ indexname: string }>(
      `SELECT indexname FROM pg_indexes WHERE schemaname = 'public' ORDER BY indexname`,
    );
    const names = indexes.rows.map((row) => row.indexname);

    for (const expected of [
      "sessions_game_created_idx",
      "sessions_device_created_idx",
      "sessions_annotator_created_idx",
      "clips_created_idx",
      "audit_events_actor_idx",
    ]) {
      expect(names).toContain(expected);
    }
  });
});
