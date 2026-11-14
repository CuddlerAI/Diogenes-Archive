import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { loadConfig } from "./config.js";
import { HfService, matchKeysOf, subjectMatchKeys, type HfClient } from "./hf.js";
import type { SqlClient } from "./sql.js";

const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../api/migrations",
);

const config = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://console:secret@127.0.0.1:5432/dataminer_dev",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "h".repeat(48),
  CONSOLE_MAIL_MODE: "capture",
  CONSOLE_HF_TOKEN: "hf_read_only_test",
  CONSOLE_HF_REPOS: JSON.stringify(["DiogenesLab/gameplay-teaser"]),
});

class FakeHfClient implements HfClient {
  revision = "abc123def456";
  files: { path: string; sizeBytes: number | null }[] = [];

  async resolveRevision(): Promise<string> {
    return this.revision;
  }

  async listFiles(): Promise<{ path: string; sizeBytes: number | null }[]> {
    return this.files;
  }
}

let db: PGlite;
let client: FakeHfClient;
let service: HfService;
let operatorId: string;

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
    `INSERT INTO operators(id, email, role, status, password_hash, password_updated_at)
     VALUES ($1, 'hf@example.com', 'operator', 'active',
             '$argon2id$v=19$m=65536,t=3,p=4$dGVzdA$dGVzdA', transaction_timestamp())`,
    [operatorId],
  );
  client = new FakeHfClient();
  service = new HfService(db as unknown as SqlClient, client, config);
}, 120_000);

afterAll(async () => {
  await db?.close();
});

describe("teaser path parsing", () => {
  it("extracts session and sample keys from chunk paths", () => {
    expect(matchKeysOf("mini/cod/dev-178-0000/c0006/p0.mp4")).toEqual({
      sessionKey: "dev-178-0000",
      sampleKey: "dev-178-0000_c0006",
    });
  });

  it("extracts a session key from session-level files", () => {
    expect(matchKeysOf("main/cod/dev-178-0000/metadata.json")).toEqual({
      sessionKey: "dev-178-0000",
      sampleKey: null,
    });
  });

  it("carries no keys for repo scaffolding", () => {
    expect(matchKeysOf("README.md")).toEqual({ sessionKey: null, sampleKey: null });
    expect(matchKeysOf("index/sessions.parquet")).toEqual({ sessionKey: null, sampleKey: null });
  });

  it("derives match keys from RAW and dm-1 subject prefixes", () => {
    expect(
      subjectMatchKeys(
        "raw/v1/non-trainable/manual/H/app/non-trainable-sessions/dev-178-0000/runs/r0000/segments/00000/",
      ).sessions,
    ).toContain("dev-178-0000");
    const dm1 = subjectMatchKeys("method-validation/cod-mv/v2026.07/preview/cod/dev-178-0000/c0006/");
    expect(dm1.samples).toContain("dev-178-0000_c0006");
    expect(dm1.sessions).toContain("dev-178-0000");
  });
});

describe("hf reconciliation", () => {
  it("refuses a repo outside the allowlist", async () => {
    await expect(service.reconcile("evil/repo", operatorId)).rejects.toMatchObject({
      code: "hf_repo_not_allowlisted",
    });
  });

  it("snapshots the repo listing with match keys and records the check", async () => {
    client.files = [
      { path: "README.md", sizeBytes: 1000 },
      { path: "mini/cod/dev-178-0000/c0006/p0.mp4", sizeBytes: 11_000_000 },
      { path: "mini/cod/dev-178-0000/c0006/p0.jsonl", sizeBytes: 40_000 },
    ];
    const result = await service.reconcile("DiogenesLab/gameplay-teaser", operatorId);
    expect(result.itemCount).toBe(3);
    expect(result.matchableCount).toBe(2);
    expect(result.revision).toBe("abc123def456");
  });

  it("computes released for a matching subject and not_released otherwise", async () => {
    const released = await service.status(
      "training-dev",
      "method-validation/cod-mv/v2026.07/preview/cod/dev-178-0000/c0006/",
      null,
    );
    expect(released.computed).toBe("released");
    expect(released.matches.length).toBe(2);

    const untouched = await service.status(
      "raw-prod",
      "raw/v1/non-trainable/manual/H/app/non-trainable-sessions/other-session/runs/r0000/segments/00000/",
      null,
    );
    expect(untouched.computed).toBe("not_released");
    expect(untouched.checkedAt).not.toBeNull();
  });

  it("flags a size contradiction as mismatch — the two-sets sentinel", async () => {
    const status = await service.status(
      "training-dev",
      "method-validation/cod-mv/v2026.07/preview/cod/dev-178-0000/c0006/",
      99,
    );
    expect(status.computed).toBe("mismatch");
  });

  it("replaces the snapshot instead of accreting across reconciles", async () => {
    client.files = [{ path: "mini/cod/dev-178-0000/c0006/p0.mp4", sizeBytes: 11_000_000 }];
    client.revision = "fff000";
    await service.reconcile("DiogenesLab/gameplay-teaser", operatorId);
    const rows = await db.query(`SELECT count(*)::int AS n FROM hf_release_items`);
    expect((rows.rows[0] as { n: number }).n).toBe(1);
  });

  it("returns the latest manual mark alongside the computed state", async () => {
    const subjectKey =
      "raw/v1/non-trainable/manual/H/app/non-trainable-sessions/other-session/runs/r0000/segments/00000/";
    await service.recordMark("raw-prod", subjectKey, "manual:pending-release", "准备 v0.2", operatorId);
    const status = await service.status("raw-prod", subjectKey, null);
    expect(status.computed).toBe("not_released");
    expect(status.manualMark?.mark).toBe("manual:pending-release");
  });
});
