import { describe, expect, it, vi } from "vitest";

import type { IngestionRepository, PurgeJob } from "./domain.js";
import { purgeOne, sweepOrphanMultiparts } from "./purge.js";
import type { ObjectStorage } from "./storage.js";

function job(overrides: Partial<PurgeJob> = {}): PurgeJob {
  return {
    id: "purge-1",
    clipId: "clip-1",
    attempt: 1,
    leaseOwner: "worker-1",
    multiparts: [
      {
        id: "multipart-1",
        objectKey: "raw/v1/non-trainable/object-1",
        providerUploadId: "provider-1",
        aborted: false,
      },
    ],
    artifacts: [
      {
        id: "artifact-1",
        objectKey: "raw/v1/non-trainable/object-1",
        confirmedAbsent: false,
      },
    ],
    ...overrides,
  };
}

describe("clip purge worker", () => {
  it("aborts multipart, confirms object absence, and only then finishes the SQL tombstone", async () => {
    const order: string[] = [];
    const repository = {
      renewPurgeLease: vi.fn(async () => undefined),
      recordPurgeProgress: vi.fn(async (_job, progress) => {
        order.push(progress.kind);
      }),
      finishPurge: vi.fn(async () => {
        order.push("finish");
      }),
      retryPurge: vi.fn(async () => {
        order.push("retry");
      }),
    } as unknown as IngestionRepository;
    const storage = {
      abortMultipart: vi.fn(async () => {
        order.push("abort");
      }),
      deleteObject: vi.fn(async () => {
        order.push("delete");
      }),
      objectExists: vi.fn(async () => {
        order.push("head");
        return false;
      }),
    } as unknown as ObjectStorage;

    await purgeOne(repository, storage, job());

    expect(order).toEqual([
      "abort",
      "multipart_aborted",
      "delete",
      "head",
      "artifact_absent",
      "finish",
    ]);
    expect(repository.renewPurgeLease).toHaveBeenCalledTimes(4);
    expect(repository.retryPurge).not.toHaveBeenCalled();
  });

  it("keeps the job retryable when HEAD still sees an object after delete", async () => {
    const repository = {
      renewPurgeLease: vi.fn(async () => undefined),
      recordPurgeProgress: vi.fn(async () => undefined),
      finishPurge: vi.fn(async () => undefined),
      retryPurge: vi.fn(async () => undefined),
    } as unknown as IngestionRepository;
    const storage = {
      abortMultipart: vi.fn(async () => undefined),
      deleteObject: vi.fn(async () => undefined),
      objectExists: vi.fn(async () => true),
    } as unknown as ObjectStorage;

    await purgeOne(repository, storage, job());

    expect(repository.finishPurge).not.toHaveBeenCalled();
    expect(repository.retryPurge).toHaveBeenCalledWith(
      expect.objectContaining({ id: "purge-1" }),
      "object_still_present_after_delete",
    );
  });

  it("does not repeat provider calls already recorded in durable progress", async () => {
    const repository = {
      renewPurgeLease: vi.fn(async () => undefined),
      recordPurgeProgress: vi.fn(async () => undefined),
      finishPurge: vi.fn(async () => undefined),
      retryPurge: vi.fn(async () => undefined),
    } as unknown as IngestionRepository;
    const storage = {
      abortMultipart: vi.fn(async () => undefined),
      deleteObject: vi.fn(async () => undefined),
      objectExists: vi.fn(async () => false),
    } as unknown as ObjectStorage;

    await purgeOne(
      repository,
      storage,
      job({
        multiparts: job().multiparts.map((multipart) => ({ ...multipart, aborted: true })),
        artifacts: job().artifacts.map((artifact) => ({ ...artifact, confirmedAbsent: true })),
      }),
    );

    expect(storage.abortMultipart).not.toHaveBeenCalled();
    expect(storage.deleteObject).not.toHaveBeenCalled();
    expect(repository.finishPurge).toHaveBeenCalledOnce();
  });
});

describe("provider multipart orphan sweep", () => {
  it("aborts only stale uploads that have no active SQL owner", async () => {
    const repository = {
      isKnownActiveMultipart: vi.fn(async (providerUploadId: string) => providerUploadId === "known"),
      recordOrphanMultipartAborted: vi.fn(async () => undefined),
    } as unknown as IngestionRepository;
    const storage = {
      listMultipartUploads: vi.fn(async () => [
        { objectKey: "prefix/known", providerUploadId: "known", initiatedAt: "2026-07-23T00:00:00Z" },
        { objectKey: "prefix/orphan", providerUploadId: "orphan", initiatedAt: "2026-07-23T00:00:00Z" },
        { objectKey: "prefix/recent", providerUploadId: "recent", initiatedAt: "2026-07-23T00:19:30Z" },
      ]),
      abortMultipart: vi.fn(async () => undefined),
    } as unknown as ObjectStorage;

    const count = await sweepOrphanMultiparts(
      repository,
      storage,
      "prefix/",
      600,
      Date.parse("2026-07-23T00:20:00Z"),
    );

    expect(count).toBe(1);
    expect(storage.listMultipartUploads).toHaveBeenCalledWith("prefix/");
    expect(storage.abortMultipart).toHaveBeenCalledWith("prefix/orphan", "orphan");
    expect(repository.recordOrphanMultipartAborted).toHaveBeenCalledWith("prefix/orphan");
  });
});
