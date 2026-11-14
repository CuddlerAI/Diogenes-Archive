import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// Same discipline as migration-0006.test.ts: the enum rebuild, the profile
// constraints, and the tombstone wipe trigger run against a real PostgreSQL
// engine, not argued from the SQL text.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

let db: PGlite;

async function insertAnnotator() {
  const id = randomUUID();
  await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, $2)`, [
    id,
    `annotator-${id.slice(0, 8)}`,
  ]);
  return id;
}

async function insertProfile(annotatorId: string) {
  await db.query(
    `INSERT INTO annotator_profiles(annotator_id, display_name, phone, im_kind, im_handle, region)
     VALUES ($1, '阿甲', '13800000000', 'wechat', 'wx_handle', '上海')`,
    [annotatorId],
  );
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
}, 180_000);

afterAll(async () => {
  await db?.close();
});

describe("0014 challenge purpose rebuild", () => {
  it("carries account_claim and no longer carries login_otp", async () => {
    const labels = await db.query<{ enumlabel: string }>(
      `SELECT e.enumlabel
         FROM pg_enum e
         JOIN pg_type t ON t.oid = e.enumtypid
        WHERE t.typname = 'annotator_challenge_purpose'
        ORDER BY e.enumsortorder`,
    );
    expect(labels.rows.map((row) => row.enumlabel)).toEqual([
      "signup_verify",
      "password_reset",
      "account_claim",
    ]);
  });

  it("accepts an account_claim challenge row", async () => {
    const annotatorId = await insertAnnotator();
    await db.query(
      `INSERT INTO annotator_credentials(annotator_id, email, password_hash)
       VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=1$c2FsdA$aGFzaA')`,
      [annotatorId, `claim-${annotatorId.slice(0, 8)}@example.com`],
    );
    await db.query(
      `INSERT INTO annotator_email_challenges(id, annotator_id, purpose, code_salt, code_hash, expires_at)
       VALUES ($1, $2, 'account_claim', $3, $4, transaction_timestamp() + interval '1 hour')`,
      [randomUUID(), annotatorId, Buffer.alloc(32, 1), "a".repeat(64)],
    );
  });

  it("refuses the retired login_otp value", async () => {
    const annotatorId = await insertAnnotator();
    await expect(
      db.query(
        `INSERT INTO annotator_email_challenges(id, annotator_id, purpose, code_salt, code_hash, expires_at)
         VALUES ($1, $2, 'login_otp', $3, $4, transaction_timestamp() + interval '1 hour')`,
        [randomUUID(), annotatorId, Buffer.alloc(32, 1), "b".repeat(64)],
      ),
    ).rejects.toThrow(/invalid input value for enum/);
  });
});

describe("0014 annotator_profiles", () => {
  it("stores a full profile and every field is optional", async () => {
    const full = await insertAnnotator();
    await insertProfile(full);

    const empty = await insertAnnotator();
    await db.query(`INSERT INTO annotator_profiles(annotator_id) VALUES ($1)`, [empty]);
  });

  it("requires im_kind and im_handle together", async () => {
    const annotatorId = await insertAnnotator();
    await expect(
      db.query(
        `INSERT INTO annotator_profiles(annotator_id, im_kind) VALUES ($1, 'wechat')`,
        [annotatorId],
      ),
    ).rejects.toThrow(/check constraint/);
    await expect(
      db.query(
        `INSERT INTO annotator_profiles(annotator_id, im_handle) VALUES ($1, 'orphan-handle')`,
        [annotatorId],
      ),
    ).rejects.toThrow(/check constraint/);
  });

  it("refuses an unknown im_kind and a blank display name", async () => {
    const annotatorId = await insertAnnotator();
    await expect(
      db.query(
        `INSERT INTO annotator_profiles(annotator_id, im_kind, im_handle) VALUES ($1, 'icq', 'x')`,
        [annotatorId],
      ),
    ).rejects.toThrow(/check constraint/);
    await expect(
      db.query(`INSERT INTO annotator_profiles(annotator_id, display_name) VALUES ($1, '  ')`, [
        annotatorId,
      ]),
    ).rejects.toThrow(/check constraint/);
  });

  it("refuses DELETE like every other governance table", async () => {
    const annotatorId = await insertAnnotator();
    await insertProfile(annotatorId);
    await expect(
      db.query(`DELETE FROM annotator_profiles WHERE annotator_id = $1`, [annotatorId]),
    ).rejects.toThrow(/tombstone/i);
  });
});

describe("0014 tombstone wipe trigger", () => {
  it("empties the profile payload when the annotator is tombstoned, keeping the row", async () => {
    const annotatorId = await insertAnnotator();
    await insertProfile(annotatorId);

    await db.query(
      `UPDATE annotators SET status = 'tombstoned', tombstoned_at = transaction_timestamp()
        WHERE id = $1`,
      [annotatorId],
    );

    const wiped = await db.query<{
      display_name: string | null;
      phone: string | null;
      im_kind: string | null;
      im_handle: string | null;
      region: string | null;
    }>(
      `SELECT display_name, phone, im_kind, im_handle, region
         FROM annotator_profiles WHERE annotator_id = $1`,
      [annotatorId],
    );
    expect(wiped.rows).toHaveLength(1);
    expect(wiped.rows[0]).toEqual({
      display_name: null,
      phone: null,
      im_kind: null,
      im_handle: null,
      region: null,
    });
  });

  it("does not fire for a suspension", async () => {
    const annotatorId = await insertAnnotator();
    await insertProfile(annotatorId);

    await db.query(`UPDATE annotators SET status = 'suspended' WHERE id = $1`, [annotatorId]);

    const kept = await db.query<{ display_name: string | null }>(
      `SELECT display_name FROM annotator_profiles WHERE annotator_id = $1`,
      [annotatorId],
    );
    expect(kept.rows[0]?.display_name).toBe("阿甲");
  });
});
