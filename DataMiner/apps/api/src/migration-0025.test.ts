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
let legacyDeviceId: string;

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files.filter((file) => Number(file.slice(0, 4)) <= 24)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  legacyDeviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version
     ) VALUES (
       $1, 'legacy-multi-profile-machine', 'legacy-device-profile', $2,
       'active', '10.0.26100', '10DE:2684', '566.36',
       '{}'::jsonb, '[]'::jsonb, '0.7.0'
     )`,
    [legacyDeviceId, "1".repeat(64)],
  );

  await db.exec(await readFile(path.join(migrationsDirectory, "0025_device_profile_bindings.sql"), "utf8"));
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("device profile bindings migration 0025", () => {
  it("records the migration and backfills every legacy single-profile device", async () => {
    const ledger = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 25`,
    );
    expect(ledger.rows).toEqual([{ name: "device_profile_bindings" }]);

    const bindings = await db.query<{
      profile_id: string;
      status: string;
      validation_method: string;
      validation_evidence: Record<string, unknown>;
    }>(
      `SELECT profile_id, status, validation_method, validation_evidence
         FROM device_profile_bindings WHERE device_id = $1`,
      [legacyDeviceId],
    );
    expect(bindings.rows).toEqual([
      {
        profile_id: "legacy-device-profile",
        status: "active",
        validation_method: "legacy_single_profile_backfill",
        validation_evidence: { source: "devices.certified_profile_id" },
      },
    ]);
  });

  it("allows a second active profile while keeping the legacy snapshot unchanged", async () => {
    await db.query(
      `INSERT INTO device_profile_bindings(
         device_id, profile_id, status, validation_method, validation_evidence
       ) VALUES ($1, 'second-device-profile', 'active', 'test', '{}'::jsonb)`,
      [legacyDeviceId],
    );

    const device = await db.query<{ certified_profile_id: string }>(
      `SELECT certified_profile_id FROM devices WHERE id = $1`,
      [legacyDeviceId],
    );
    const profiles = await db.query<{ profile_id: string }>(
      `SELECT profile_id FROM device_profile_bindings WHERE device_id = $1 ORDER BY profile_id`,
      [legacyDeviceId],
    );
    expect(device.rows[0]?.certified_profile_id).toBe("legacy-device-profile");
    expect(profiles.rows.map((row) => row.profile_id)).toEqual([
      "legacy-device-profile",
      "second-device-profile",
    ]);
  });

  it("requires revoked bindings to carry a revocation timestamp", async () => {
    await expect(
      db.query(
        `INSERT INTO device_profile_bindings(
           device_id, profile_id, status, validation_method, validation_evidence
         ) VALUES ($1, 'invalid-revocation', 'revoked', 'test', '{}'::jsonb)`,
        [legacyDeviceId],
      ),
    ).rejects.toThrow();
  });
});
