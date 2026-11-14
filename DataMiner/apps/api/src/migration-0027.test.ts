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
  for (const file of files.filter((file) => Number(file.slice(0, 4)) <= 26)) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  legacyDeviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version
     ) VALUES ($1, 'legacy-profile-device', 'legacy-profile', $2, 'active',
               '10.0.26100', '10DE:2684', '566.36', '{}'::jsonb, '[]'::jsonb, '0.7.1')`,
    [legacyDeviceId, "1".repeat(64)],
  );
  await db.query(
    `INSERT INTO device_profile_bindings(
       device_id, profile_id, status, validation_method, validation_evidence
     ) VALUES ($1, 'legacy-profile', 'active', 'test', '{}'::jsonb)`,
    [legacyDeviceId],
  );

  await db.exec(
    await readFile(
      path.join(migrationsDirectory, "0027_decouple_device_profile_enrollment.sql"),
      "utf8",
    ),
  );
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("device enrollment decoupling migration 0027", () => {
  it("records the migration and allows new devices without a legacy profile snapshot", async () => {
    const ledger = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 27`,
    );
    expect(ledger.rows).toEqual([{ name: "decouple_device_profile_enrollment" }]);

    await expect(
      db.query(
        `INSERT INTO devices(
           id, client_device_id, certified_profile_id, credential_fingerprint,
           status, windows_build, gpu_pci_id, gpu_driver,
           display_snapshot, input_devices_snapshot, client_version
         ) VALUES ($1, 'profile-free-device', NULL, $2, 'active',
                   '10.0.26100', '10DE:2684', '566.36', '{}'::jsonb, '[]'::jsonb, '0.7.1')`,
        [randomUUID(), "2".repeat(64)],
      ),
    ).resolves.toBeDefined();
  });

  it("preserves legacy profile snapshots and binding audit rows", async () => {
    const device = await db.query<{ certified_profile_id: string | null }>(
      `SELECT certified_profile_id FROM devices WHERE id = $1`,
      [legacyDeviceId],
    );
    const bindings = await db.query<{ profile_id: string; status: string }>(
      `SELECT profile_id, status FROM device_profile_bindings WHERE device_id = $1`,
      [legacyDeviceId],
    );

    expect(device.rows[0]?.certified_profile_id).toBe("legacy-profile");
    expect(bindings.rows).toEqual([{ profile_id: "legacy-profile", status: "active" }]);
  });
});
