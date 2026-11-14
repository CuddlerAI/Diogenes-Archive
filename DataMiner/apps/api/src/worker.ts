import { createServer } from "node:http";
import { setTimeout as delay } from "node:timers/promises";

import pg from "pg";

import {
  clientEventExportBacklog,
  drainClientEventExport,
  GcsClientEventSink,
  UnconfiguredClientEventSink,
} from "./client-events-export.js";
import { admittedClassifications, loadConfig } from "./config.js";
import { runPurgeBatch, sweepOrphanMultiparts } from "./purge.js";
import { PostgresRepository } from "./repository.js";
import { R2ObjectStorage } from "./storage.js";
import { runVerificationBatch } from "./verification.js";

const config = loadConfig();
const repository = new PostgresRepository(
  config.databaseUrl,
  config.retentionDays,
  admittedClassifications(config),
);
const storage = new R2ObjectStorage(config.r2);
// The exporter drains outbox_jobs directly; a small dedicated pool keeps its
// claim transactions out of the repository's connection budget.
const exportPool = config.logsExport.enabled
  ? new pg.Pool({ connectionString: config.databaseUrl, max: 2 })
  : null;
const exportSink = config.logsExport.bucket
  ? new GcsClientEventSink({ bucket: config.logsExport.bucket })
  : new UnconfiguredClientEventSink();
let stopping = false;
let nextOrphanSweepAt = 0;
let nextBacklogLogAt = 0;
let lastBatchCompletedAt: string | null = null;
let consecutiveFailures = 0;

process.once("SIGINT", () => {
  stopping = true;
});
process.once("SIGTERM", () => {
  stopping = true;
});

// A Cloud Run *service* deployment (see README step 8) needs something listening
// on $PORT or the startup probe fails; the maintenance loop has no HTTP surface
// of its own. A Cloud Run *job* ignores this. Liveness only — request readiness
// belongs to the API service that owns traffic.
// Since the listener has to exist anyway, it reports something worth reading:
// when the last batch finished and how many have failed back to back. Without
// that the worker is invisible — it either runs or silently does not, and
// nothing distinguishes the two from outside.
const health = createServer((_request, response) => {
  response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
  response.end(
    JSON.stringify({ status: "ok", role: "worker", lastBatchCompletedAt, consecutiveFailures }),
  );
});
health.listen(Number(process.env.PORT ?? 8080), process.env.HOST ?? "0.0.0.0");

while (!stopping) {
  try {
    const verificationCount = await runVerificationBatch(
      repository,
      storage,
      config.worker.batchSize,
      config.worker.leaseSeconds,
    );
    const purge = await runPurgeBatch(
      repository,
      storage,
      config.worker.purgeBatchSize,
      config.worker.purgeLeaseSeconds,
      config.worker.multipartCompletionLeaseSeconds,
    );
    // Telemetry that predates any consent has only this deadline; nothing else
    // in the system will ever reach it.
    const telemetrySwept = await repository.sweepUnconsentedTelemetry(
      config.unconsentedTelemetryTtlDays,
      config.worker.purgeBatchSize,
    );
    let orphanAbortCount = 0;
    if (Date.now() >= nextOrphanSweepAt) {
      // One sweep per admitted lane: each lane has its own exclusive prefix, so
      // an orphan under the first-party prefix is invisible to the dev sweep.
      for (const { prefix } of config.admittedFamilies) {
        orphanAbortCount += await sweepOrphanMultiparts(
          repository,
          storage,
          prefix,
          config.worker.orphanMultipartGraceSeconds,
        );
      }
      nextOrphanSweepAt = Date.now() + 60_000;
    }
    let exportedCount = 0;
    if (exportPool) {
      const exported = await drainClientEventExport(exportPool, exportSink, {
        environment: config.environment,
        batchSize: config.logsExport.batchSize,
        dryRun: config.logsExport.dryRun,
      });
      exportedCount = exported.exported;
      if (exported.failed > 0) {
        console.error("client_events_export batch failed", { failed: exported.failed });
      }
      // 积压深度每分钟一行：导出是否活着的第一观测点（docs/23 §6）。
      if (Date.now() >= nextBacklogLogAt) {
        const backlog = await clientEventExportBacklog(exportPool);
        console.log("client_events_export backlog", {
          pending: backlog.pending,
          oldest: backlog.oldestCreatedAt,
          dryRun: config.logsExport.dryRun,
        });
        nextBacklogLogAt = Date.now() + 60_000;
      }
    }
    lastBatchCompletedAt = new Date().toISOString();
    consecutiveFailures = 0;
    if (
      verificationCount === 0 &&
      purge.enqueued === 0 &&
      purge.leased === 0 &&
      orphanAbortCount === 0 &&
      exportedCount === 0 &&
      telemetrySwept === 0
    ) {
      await delay(2_000);
    }
  } catch (error) {
    consecutiveFailures += 1;
    // Was hardcoded to "staging", so a production worker logged that a staging
    // batch had failed — the one line an operator greps for during an incident.
    console.error(`${config.environment} maintenance batch failed`, safeErrorName(error));
    await delay(5_000);
  }
}

health.close();
await repository.close();
await exportPool?.end();

function safeErrorName(error: unknown): string {
  return error instanceof Error && error.name ? error.name : "unknown_error";
}
