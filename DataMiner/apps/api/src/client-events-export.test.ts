import { randomUUID } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

import { PGlite } from "@electric-sql/pglite";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";

import {
  clientEventExportBacklog,
  drainClientEventExport,
  UnconfiguredClientEventSink,
  type ClientEventSink,
  type ExportedClientEvent,
} from "./client-events-export.js";

// The drainer's claim/ack/backoff SQL runs against a real PostgreSQL engine
// (PGlite), same discipline as repository.test.ts: the statements are
// exercised, not argued from their text.
const migrationsDirectory = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../migrations",
);

let db: PGlite;
let deviceId: string;

const pool = {
  query: async <T>(text: string, values?: unknown[]) =>
    (await db.query(text, values as unknown[])) as { rows: T[] },
};

/** Inserts a client event plus its export outbox row, as console-api does. */
async function insertEvent(input: {
  kind?: string;
  occurredAt?: string;
  details?: Record<string, unknown>;
  retentionState?: "retained" | "purged";
}): Promise<{ eventId: string; clientEventId: number }> {
  const eventId = randomUUID();
  const inserted = await db.query<{ id: string }>(
    `INSERT INTO client_events(
       event_id, device_id, occurred_at, kind, severity, client_version, details,
       retention_state, purged_at
     ) VALUES ($1, $2, $3, $4, 'info', '0.1.0', $5::jsonb, $6::retention_state,
               CASE WHEN $6::text = 'purged' THEN transaction_timestamp() END)
     RETURNING id`,
    [
      eventId,
      deviceId,
      input.occurredAt ?? "2026-07-20T10:15:00Z",
      input.kind ?? "preflight_failed",
      JSON.stringify(input.details ?? {}),
      input.retentionState ?? "retained",
    ],
  );
  const clientEventId = Number(inserted.rows[0]?.id);
  await db.query(
    `INSERT INTO outbox_jobs(event_id, topic, aggregate_type, aggregate_id, dedupe_key, payload)
     VALUES ($1::uuid, 'client_event.recorded', 'client_event', $2::text, 'client_event:' || $2::text,
             jsonb_build_object('clientEventId', $3::bigint, 'kind', $4::text))`,
    [eventId, eventId, clientEventId, input.kind ?? "preflight_failed"],
  );
  return { eventId, clientEventId };
}

class MemorySink implements ClientEventSink {
  objects = new Map<string, ExportedClientEvent[]>();
  async publish(objectName: string, body: Uint8Array): Promise<void> {
    const lines = gunzipSync(Buffer.from(body)).toString("utf8").trim().split("\n");
    this.objects.set(objectName, lines.map((line) => JSON.parse(line) as ExportedClientEvent));
  }
}

class FailingSink implements ClientEventSink {
  async publish(): Promise<void> {
    throw new Error("gcs_publish_failed status=503");
  }
}

beforeAll(async () => {
  db = new PGlite();
  const files = (await readdir(migrationsDirectory))
    .filter((file) => /^\d+_[a-z0-9_]+\.sql$/.test(file))
    .sort();
  for (const file of files) {
    await db.exec(await readFile(path.join(migrationsDirectory, file), "utf8"));
  }

  const deviceProfileVersionId = randomUUID();
  await db.query(
    `INSERT INTO trusted_catalog_versions(
       id, kind, catalog_id, version, game_id, content_hash, snapshot,
       status, approved_at, approved_by
     ) VALUES ($1, 'device_profile', 'dev-device-profile', '1', NULL, $2, '{}'::jsonb,
               'approved', transaction_timestamp(), 'test')`,
    [deviceProfileVersionId, `sha256:${"a".repeat(64)}`],
  );
  deviceId = randomUUID();
  await db.query(
    `INSERT INTO devices(
       id, client_device_id, certified_profile_id, credential_fingerprint,
       status, windows_build, gpu_pci_id, gpu_driver,
       display_snapshot, input_devices_snapshot, client_version
     ) VALUES ($1, $2, $3, $4, 'active', '10.0.22631', '10DE:2684', '537.13',
               '{}'::jsonb, '[]'::jsonb, '0.1.0')`,
    [deviceId, `client-${deviceId.slice(0, 8)}`, "dev-device-profile", "b".repeat(64)],
  );
}, 120_000);

beforeEach(async () => {
  await db.query(`DELETE FROM outbox_jobs`);
  await db.query(`DELETE FROM client_events`);
});

afterAll(async () => {
  await db?.close();
});

describe("client events export drain", () => {
  it("exports a batch as hive-partitioned ndjson and marks the jobs published", async () => {
    const first = await insertEvent({
      occurredAt: "2026-07-20T10:15:00Z",
      details: { schema_version: "dm-client-preflight-1", failureCode: "HARDWARE_PATH_PREFLIGHT_FAILED", windowsBuild: "22631" },
    });
    // A second hour bucket proves partitioning splits objects.
    await insertEvent({ kind: "app_started", occurredAt: "2026-07-20T11:05:00Z" });

    const sink = new MemorySink();
    const result = await drainClientEventExport(pool, sink, { environment: "staging" });

    expect(result.exported).toBe(2);
    expect(result.failed).toBe(0);
    expect(result.objects).toHaveLength(2);
    for (const name of result.objects) {
      expect(name).toMatch(
        /^staging\/client_events\/dt=2026-07-20\/hour=(10|11)\/\d{12}-\d{12}\.ndjson\.gz$/,
      );
    }
    const hour10 = result.objects.find((name) => name.includes("hour=10"));
    const rows = sink.objects.get(hour10 ?? "");
    expect(rows).toHaveLength(1);
    expect(rows?.[0]).toMatchObject({
      export_schema_version: "dm-client-event-export-1",
      event_id: first.eventId,
      device_id: deviceId,
      kind: "preflight_failed",
      environment: "staging",
      payload: { failureCode: "HARDWARE_PATH_PREFLIGHT_FAILED", windowsBuild: "22631" },
    });

    const jobs = await db.query<{ status: string; published_at: string | null }>(
      `SELECT status, published_at FROM outbox_jobs`,
    );
    expect(jobs.rows).toHaveLength(2);
    for (const job of jobs.rows) {
      expect(job.status).toBe("succeeded");
      expect(job.published_at).not.toBeNull();
    }
  });

  // docs/23 §7.3: a row purged before export must never reach GCS — its
  // correct export is no export, and the job still completes.
  it("skips purged rows but still completes their jobs", async () => {
    await insertEvent({ retentionState: "purged", details: {} });
    const kept = await insertEvent({ kind: "app_started" });

    const sink = new MemorySink();
    const result = await drainClientEventExport(pool, sink, { environment: "staging" });

    expect(result.exported).toBe(1);
    expect(result.skippedPurged).toBe(1);
    const all = [...sink.objects.values()].flat();
    expect(all).toHaveLength(1);
    expect(all[0]?.event_id).toBe(kept.eventId);

    const pendingAfter = await clientEventExportBacklog(pool);
    expect(pendingAfter.pending).toBe(0);
  });

  it("backs off the whole batch on a sink failure and retries later", async () => {
    await insertEvent({});
    const result = await drainClientEventExport(pool, new FailingSink(), {
      environment: "staging",
    });

    expect(result.failed).toBe(1);
    const job = (
      await db.query<{ status: string; attempt_count: number; available_at: string }>(
        `SELECT status, attempt_count,
                to_char(available_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"') AS available_at
           FROM outbox_jobs`,
      )
    ).rows[0];
    expect(job?.status).toBe("retry_wait");
    expect(job?.attempt_count).toBe(1);

    // Not yet available: the backlog still counts it, the next drain skips it.
    const backlog = await clientEventExportBacklog(pool);
    expect(backlog.pending).toBe(1);
    const retry = await drainClientEventExport(pool, new MemorySink(), {
      environment: "staging",
    });
    expect(retry.exported).toBe(0);

    // Once the backoff elapses the same job drains normally.
    await db.query(`UPDATE outbox_jobs SET available_at = transaction_timestamp()`);
    const drained = await drainClientEventExport(pool, new MemorySink(), {
      environment: "staging",
    });
    expect(drained.exported).toBe(1);
  });

  it("dry-run claims nothing permanently and reports the backlog depth", async () => {
    await insertEvent({});
    await insertEvent({ kind: "app_started" });

    const before = await clientEventExportBacklog(pool);
    expect(before.pending).toBe(2);
    expect(before.oldestCreatedAt).not.toBeNull();

    const sink = new MemorySink();
    const result = await drainClientEventExport(pool, sink, {
      environment: "staging",
      dryRun: true,
    });
    expect(result.exported).toBe(0);
    expect(sink.objects.size).toBe(0);

    const after = await clientEventExportBacklog(pool);
    expect(after.pending).toBe(2);
  });

  it("fails loudly when the sink is unconfigured instead of marking rows published", async () => {
    await insertEvent({});
    const result = await drainClientEventExport(pool, new UnconfiguredClientEventSink(), {
      environment: "staging",
    });

    expect(result.failed).toBe(1);
    const job = (await db.query<{ status: string }>(`SELECT status FROM outbox_jobs`)).rows[0];
    expect(job?.status).toBe("retry_wait");
  });

  it("replaying an already-published batch is a no-op", async () => {
    await insertEvent({});
    const sink = new MemorySink();
    await drainClientEventExport(pool, sink, { environment: "staging" });
    const again = await drainClientEventExport(pool, sink, { environment: "staging" });

    expect(again.exported).toBe(0);
    expect(again.failed).toBe(0);
  });
});
