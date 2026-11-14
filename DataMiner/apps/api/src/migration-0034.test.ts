import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { sessionArtifactKinds } from "./contracts.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }
});

afterAll(async () => {
  await db.close();
});

describe("0034 · session artifact recording profile", () => {
  it("keeps the database enum aligned with every API contract kind", async () => {
    const { rows } = await db.query<{ label: string }>(
      `SELECT unnest(enum_range(NULL::session_artifact_kind))::text AS label`,
    );
    const labels = rows.map((row) => row.label);

    for (const kind of sessionArtifactKinds) expect(labels).toContain(kind);
    expect(labels).toContain("capture_profile");
    expect(labels).toContain("settings_profile");
  });

  it("records the migration in the shared ledger", async () => {
    const { rows } = await db.query<{ name: string }>(
      `SELECT name FROM schema_migrations WHERE version = 34`,
    );
    expect(rows).toEqual([{ name: "session_artifact_recording_profile" }]);
  });
});
