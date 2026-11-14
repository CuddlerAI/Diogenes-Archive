import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
let legacyReleaseId: string;
let currentReleaseId: string;

async function insertPublishedRelease(input: {
  id: string;
  catalogId: string;
  catalogVersion: string;
  schemaVersion: string;
}) {
  const manifest = JSON.stringify({ schema_version: input.schemaVersion });
  const envelope = `${input.catalogId}@${input.catalogVersion}`;
  await db.query(
    `INSERT INTO catalog_releases(
       id, channel, catalog_id, catalog_version, manifest_bytes,
       envelope_bytes, envelope_sha256, signing_key_id, status,
       created_by, published_by, published_at
     ) VALUES (
       $1, 'dev', $2, $3, convert_to($4, 'UTF8'),
       convert_to($5, 'UTF8'),
       ('sha256:' || encode(sha256(convert_to($5, 'UTF8')), 'hex'))::sha256_digest,
       'migration-test-key', 'published',
       'migration-test', 'migration-test', transaction_timestamp()
     )`,
    [input.id, input.catalogId, input.catalogVersion, manifest, envelope],
  );
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files.filter((file) => Number(file.slice(0, 4)) <= 25)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  legacyReleaseId = randomUUID();
  currentReleaseId = randomUUID();
  await insertPublishedRelease({
    id: legacyReleaseId,
    catalogId: "dataminer-dev-fleet",
    catalogVersion: "2026-07-29.0",
    schemaVersion: "dm-catalog-bundle-1",
  });
  await insertPublishedRelease({
    id: currentReleaseId,
    catalogId: "dataminer-halo-infinite-steam-release-test",
    catalogVersion: "2026-08-04.0",
    schemaVersion: "dm-catalog-bundle-2",
  });

  await db.exec(
    await readFile(
      path.join(migrationsDirectory, "0026_revoke_legacy_catalog_bundles.sql"),
      "utf8",
    ),
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("legacy catalog bundle cleanup migration 0026", () => {
  it("records the migration", async () => {
    const ledger = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 26`,
    );
    expect(ledger.rows).toEqual([{ name: "revoke_legacy_catalog_bundles" }]);
  });

  it("revokes a published v1 bundle without changing its immutable identity", async () => {
    const releases = await db.query<{
      id: string;
      status: string;
      revoked_by: string | null;
      revoked_at: Date | null;
    }>(
      `SELECT id, status, revoked_by, revoked_at
         FROM catalog_releases
        WHERE id IN ($1, $2)
        ORDER BY id`,
      [legacyReleaseId, currentReleaseId],
    );
    const legacy = releases.rows.find((row) => row.id === legacyReleaseId);
    const current = releases.rows.find((row) => row.id === currentReleaseId);

    expect(legacy).toMatchObject({
      status: "revoked",
      revoked_by: "migration-0026",
    });
    expect(legacy?.revoked_at).toBeInstanceOf(Date);
    expect(current).toMatchObject({
      status: "published",
      revoked_by: null,
      revoked_at: null,
    });
  });

  it("writes one canonical revocation audit event", async () => {
    const events = await db.query<{
      actor_type: string;
      actor_id: string;
      action: string;
      entity_id: string;
      details: Record<string, unknown>;
    }>(
      `SELECT actor_type, actor_id, action, entity_id, details
         FROM audit_events
        WHERE action = 'catalog.release_revoked'
          AND entity_id = $1`,
      [legacyReleaseId],
    );
    expect(events.rows).toEqual([
      {
        actor_type: "service",
        actor_id: "migration-0026",
        action: "catalog.release_revoked",
        entity_id: legacyReleaseId,
        details: {
          channel: "dev",
          catalogId: "dataminer-dev-fleet",
          catalogVersion: "2026-07-29.0",
          reason: "legacy_bundle_schema_incompatible_with_recording_profiles",
        },
      },
    ]);
  });
});
