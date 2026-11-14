import type { IngestionRepository, PurgeJob } from "./domain.js";
import type { ObjectStorage } from "./storage.js";

export async function purgeOne(
  repository: IngestionRepository,
  storage: ObjectStorage,
  job: PurgeJob,
): Promise<void> {
  try {
    for (const multipart of job.multiparts) {
      if (multipart.aborted) continue;
      await repository.renewPurgeLease(job);
      await storage.abortMultipart(multipart.objectKey, multipart.providerUploadId);
      await repository.recordPurgeProgress(job, {
        kind: "multipart_aborted",
        multipartId: multipart.id,
      });
    }
    for (const artifact of job.artifacts) {
      if (artifact.confirmedAbsent) continue;
      await repository.renewPurgeLease(job);
      await storage.deleteObject(artifact.objectKey);
      await repository.renewPurgeLease(job);
      if (await storage.objectExists(artifact.objectKey)) {
        throw new Error("object_still_present_after_delete");
      }
      await repository.recordPurgeProgress(job, {
        kind: "artifact_absent",
        artifactId: artifact.id,
      });
    }
    await repository.renewPurgeLease(job);
    await repository.finishPurge(job);
  } catch (error) {
    await repository.retryPurge(job, purgeErrorCode(error));
  }
}

export async function runPurgeBatch(
  repository: IngestionRepository,
  storage: ObjectStorage,
  batchSize: number,
  leaseSeconds: number,
  completionLeaseSeconds: number,
): Promise<{ enqueued: number; leased: number }> {
  const enqueued = await repository.enqueueMaintenancePurges(batchSize);
  const jobs = await repository.leasePurgeJobs(batchSize, leaseSeconds, completionLeaseSeconds);
  await Promise.all(jobs.map((job) => purgeOne(repository, storage, job)));
  return { enqueued, leased: jobs.length };
}

export async function sweepOrphanMultiparts(
  repository: IngestionRepository,
  storage: ObjectStorage,
  objectPrefix: string,
  graceSeconds: number,
  nowMs = Date.now(),
): Promise<number> {
  const prefix = `${objectPrefix.replace(/\/+$/, "")}/`;
  const uploads = await storage.listMultipartUploads(prefix);
  let aborted = 0;
  for (const upload of uploads) {
    if (!upload.objectKey.startsWith(prefix)) {
      throw new Error("multipart_listing_escaped_configured_prefix");
    }
    const initiatedAt = Date.parse(upload.initiatedAt);
    if (!Number.isFinite(initiatedAt) || initiatedAt > nowMs - graceSeconds * 1_000) continue;
    if (await repository.isKnownActiveMultipart(upload.providerUploadId, upload.objectKey)) continue;
    await storage.abortMultipart(upload.objectKey, upload.providerUploadId);
    await repository.recordOrphanMultipartAborted(upload.objectKey);
    aborted += 1;
  }
  return aborted;
}

function purgeErrorCode(error: unknown): string {
  if (error instanceof Error && error.message === "object_still_present_after_delete") {
    return error.message;
  }
  if (error instanceof Error && error.name) {
    return `storage_${error.name.toLowerCase().replace(/[^a-z0-9]+/g, "_")}`.slice(0, 80);
  }
  return "storage_unknown";
}
