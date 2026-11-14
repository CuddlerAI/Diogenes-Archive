import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// 0031 backfills scopes and reclassifies renewal-superseded consents. Both only
// do anything to rows that already exist, so applying the whole chain to an
// empty database would exercise neither. This suite stops at 0030, writes the
// legacy rows, and then applies 0031 — the migration runs against the data it
// was written for.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

/** 2026-07-25-teaser as it read before docs/105 §4 amended it in place. */
const TEASER_PRE_AMENDMENT_HASH =
  "1ed1fdd3809a6fc713a340e5080be05c9f6721bd6fca5d65441fbf9d4e73b389";
/** The same policy version after the amendment — different bytes, same label. */
const TEASER_AMENDED_HASH = "10b14ab69e36ba9c9bcdd605e9eaa4447b660f961c94dce47ac5448b7229856a";
const AUDIO_HASH = "e048368cfad32fe8641a5562f213ec1de7f680efa7e46efca7ab830c8758e50b";
const UNKNOWN_HASH = "a".repeat(64);

let db: PGlite;
let annotatorId: string;
let teaserConsentId: string;
let audioConsentId: string;
let amendedTeaserConsentId: string;
let unknownConsentId: string;
let withdrawnConsentId: string;

async function insertConsent(input: {
  termsHash: string;
  policyVersion: string;
  status: string;
  acceptedAt: string;
  revokedAt?: string;
  revokeReason?: string;
}) {
  const id = randomUUID();
  await db.query(
    `INSERT INTO consents(
       id, annotator_id, policy_version, purpose, terms_hash, status, accepted_at,
       revoked_at, revoke_reason, allows_trainable_use
     ) VALUES ($1, $2, $3, 'capture', $4, $5::consent_status, $6::timestamptz, $7::timestamptz, $8, true)`,
    [
      id,
      annotatorId,
      input.policyVersion,
      input.termsHash,
      input.status,
      input.acceptedAt,
      input.revokedAt ?? null,
      input.revokeReason ?? null,
    ],
  );
  return id;
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  const upTo0030 = files.filter((file) => Number(file.slice(0, 4)) <= 30);
  const target = files.find((file) => file.startsWith("0031_"));
  expect(target).toBeDefined();
  for (const file of upTo0030) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  annotatorId = randomUUID();
  await db.query(`INSERT INTO annotators(id, alias) VALUES ($1, 'legacy')`, [annotatorId]);

  // The pre-amendment teaser, the amended teaser, the audio document, one whose
  // bytes the repository cannot produce, and a genuine withdrawal that must
  // survive untouched.
  teaserConsentId = await insertConsent({
    termsHash: TEASER_PRE_AMENDMENT_HASH,
    policyVersion: "2026-07-25-teaser",
    status: "revoked",
    acceptedAt: "2026-07-25T00:00:00Z",
    revokedAt: "2026-07-29T00:00:00Z",
    revokeReason: "superseded_by_renewal",
  });
  audioConsentId = await insertConsent({
    termsHash: AUDIO_HASH,
    policyVersion: "2026-07-29-audio-public",
    status: "active",
    acceptedAt: "2026-07-29T00:00:00Z",
  });
  unknownConsentId = await insertConsent({
    termsHash: UNKNOWN_HASH,
    policyVersion: "2099-01-01-not-in-repo",
    status: "active",
    acceptedAt: "2026-07-20T00:00:00Z",
  });
  amendedTeaserConsentId = await insertConsent({
    termsHash: TEASER_AMENDED_HASH,
    policyVersion: "2026-07-25-teaser",
    status: "active",
    acceptedAt: "2026-08-07T00:00:00Z",
  });
  withdrawnConsentId = await insertConsent({
    termsHash: TEASER_PRE_AMENDMENT_HASH,
    policyVersion: "2026-07-25-teaser",
    status: "revoked",
    acceptedAt: "2026-07-25T00:00:00Z",
    revokedAt: "2026-07-30T00:00:00Z",
    revokeReason: "annotator_requested_withdrawal",
  });

  await db.exec(await readFile(path.join(migrationsDirectory, target!), "utf8"));
}, 180_000);

afterAll(async () => {
  await db?.close();
});

async function consent(id: string) {
  const result = await db.query<{
    status: string;
    scopes: string[];
    revoked_at: Date | null;
    revoke_reason: string | null;
    superseded_at: Date | null;
    superseded_by: string | null;
  }>(
    `SELECT status, scopes, revoked_at, revoke_reason, superseded_at, superseded_by
       FROM consents WHERE id = $1`,
    [id],
  );
  return result.rows[0]!;
}

describe("0031 scope backfill", () => {
  it("stamps the scopes of documents whose exact bytes are known", async () => {
    // docs/108 §3.2. Keyed on terms_hash, never on policy_version: the same
    // label over different bytes is a different document.
    expect((await consent(teaserConsentId)).scopes.sort()).toEqual([
      "input",
      "public_release",
      "trainable_research",
      "video",
    ]);
    expect((await consent(audioConsentId)).scopes.sort()).toEqual([
      "audio",
      "input",
      "public_release",
      "trainable_research",
      "video",
    ]);
  });

  it("distinguishes the two texts that share the 2026-07-25-teaser label", async () => {
    // docs/105 §4 amended that document in place: same policy_version, different
    // bytes, and it gained clauses for audio and depth. Keying the backfill on
    // policy_version would have recorded people who signed the earlier text as
    // having authorized depth capture they never read about. Keying it on
    // terms_hash records what each of them was actually shown — and the gap
    // between the two is exactly what the re-sign prompt is for.
    const preAmendment = await consent(teaserConsentId);
    const amended = await consent(amendedTeaserConsentId);
    expect(preAmendment.scopes).not.toContain("depth");
    expect(preAmendment.scopes).not.toContain("audio");
    expect(amended.scopes.sort()).toEqual([
      "audio",
      "depth",
      "input",
      "public_release",
      "trainable_research",
      "video",
    ]);
  });

  it("leaves an unrecognised document with no scopes rather than guessing", async () => {
    // Fail-closed: nothing in the repository can say what this text authorized,
    // so the honest record is "nothing declared", not "same as the current
    // deployment".
    expect((await consent(unknownConsentId)).scopes).toEqual([]);
  });

  it("records why each backfilled row got the scopes it did", async () => {
    const audit = await db.query<{ details: { basis: string } }>(
      `SELECT details FROM audit_events
        WHERE action = 'consent.scopes_stamped_from_terms_document' AND consent_id = $1`,
      [teaserConsentId],
    );
    expect(audit.rows).toHaveLength(1);
    expect(audit.rows[0]!.details.basis).toBe("terms_hash_exact_match_docs_106_section_3_2");
  });
});

describe("0031 renewal reclassification", () => {
  it("turns a renewal-superseded consent into an expired, superseded one", async () => {
    const row = await consent(teaserConsentId);
    expect(row.status).toBe("expired");
    expect(row.revoked_at).toBeNull();
    expect(row.revoke_reason).toBeNull();
    // The window is preserved: the moment the old document stopped applying is
    // exactly when the renewal happened.
    expect(row.superseded_at?.toISOString()).toBe("2026-07-29T00:00:00.000Z");
    expect(row.superseded_by).toBe(audioConsentId);
  });

  it("leaves a genuine withdrawal recorded as a withdrawal", async () => {
    // This is the distinction the migration exists to restore. A withdrawal has
    // to keep looking like one, because purge acts on it.
    const row = await consent(withdrawnConsentId);
    expect(row.status).toBe("revoked");
    expect(row.revoke_reason).toBe("annotator_requested_withdrawal");
    expect(row.superseded_at).toBeNull();
  });

  it("leaves no row still recorded as revoked for renewal", async () => {
    const leftover = await db.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM consents WHERE revoke_reason = 'superseded_by_renewal'`,
    );
    expect(leftover.rows[0]!.count).toBe("0");
  });
});

describe("0031 constraints", () => {
  it("refuses a scope word outside the vocabulary", async () => {
    await expect(
      db.query(
        `UPDATE consents SET scopes = ARRAY['input','microphone']::text[] WHERE id = $1`,
        [audioConsentId],
      ),
    ).rejects.toThrow(/consents_scopes_vocabulary_check/);
  });

  it("refuses a NULL element, which would otherwise slip past the vocabulary check", async () => {
    // `scopes <@ ARRAY[...]` evaluates to NULL when the array holds a NULL, and
    // a CHECK passes on NULL — so without its own constraint the vocabulary
    // could be bypassed by a single NULL element.
    await expect(
      db.query(`UPDATE consents SET scopes = ARRAY['input', NULL]::text[] WHERE id = $1`, [
        audioConsentId,
      ]),
    ).rejects.toThrow(/consents_scopes_no_null_element_check/);
  });

  it("refuses to record a renewal as a withdrawal reason", async () => {
    await expect(
      db.query(
        `UPDATE consents
            SET status = 'revoked', revoked_at = transaction_timestamp(),
                revoke_reason = 'superseded_by_renewal'
          WHERE id = $1`,
        [unknownConsentId],
      ),
    ).rejects.toThrow(/consents_renewal_is_not_withdrawal_check/);
  });

  it("still lets an already-superseded consent be withdrawn", async () => {
    // The case the "supersede and revoke are mutually exclusive" version of this
    // constraint would have broken. Withdrawal has to reach historical consents
    // — every consent a purge needs to act on is, by definition, one that has
    // already been superseded.
    await db.query(
      `UPDATE consents
          SET status = 'revoked', revoked_at = transaction_timestamp(),
              revoke_reason = 'annotator_requested_withdrawal'
        WHERE id = $1`,
      [teaserConsentId],
    );
    const row = await consent(teaserConsentId);
    expect(row.status).toBe("revoked");
    expect(row.superseded_at).not.toBeNull();
    expect(row.revoked_at).not.toBeNull();
    // Put it back: later assertions in this file do not depend on it, but a
    // suite that mutates shared fixtures should say so by restoring them.
    await db.query(
      `UPDATE consents SET status = 'expired', revoked_at = NULL, revoke_reason = NULL
        WHERE id = $1`,
      [teaserConsentId],
    );
  });

  it("refuses naming a successor without saying when the supersede happened", async () => {
    await expect(
      db.query(`UPDATE consents SET superseded_by = $2 WHERE id = $1`, [
        unknownConsentId,
        audioConsentId,
      ]),
    ).rejects.toThrow(/consents_superseded_by_needs_time_check/);
  });
});
