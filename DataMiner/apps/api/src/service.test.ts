import { Readable } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import { artifactKinds, requiredArtifactKinds, type ArtifactKind } from "./contracts.js";
import type {
  ArtifactRecord,
  ClipRecord,
  IngestionRepository,
  MultipartRecord,
  UploadIntentRecord,
} from "./domain.js";
import { conflict } from "./errors.js";
import { environmentProfile, primaryFamily } from "./environment.js";
import { IngestionService } from "./service.js";
import type { ObjectStorage } from "./storage.js";

const FIVE_MIB = 5 * 1024 * 1024;
const digest = `sha256:${"a".repeat(64)}`;

function clipWithBytes(bytesByKind: Partial<Record<ArtifactKind, number>> = {}): ClipRecord {
  const kinds: ArtifactKind[] = [
    ...requiredArtifactKinds,
    ...(["audio", "depth"] as const).filter((kind) => bytesByKind[kind] !== undefined),
  ];
  const artifacts: ArtifactRecord[] = kinds.map((kind) => ({
    id: `artifact-${kind}`,
    clipId: "clip-1",
    kind,
    objectKey: `raw/v1/non-trainable/dataminer-api-prod/session/run/clip/${kind}`,
    expectedBytes: bytesByKind[kind] ?? 7,
    expectedSha256: digest,
    contentType: kind === "video" ? "video/mp4" : "application/octet-stream",
    state: "declared",
  }));
  return {
    id: "clip-1",
    sessionId: "session-1",
    runId: "run-1",
    segmentIndex: 0,
    revision: 1,
    state: "declared",
    storageState: "pending",
    verificationState: "unverified",
    retentionState: "retained",
    retentionDeadline: "2026-08-22T00:00:00.000Z",
    artifacts,
    isTrainable: false,
    trainabilityBlockers: ["non_trainable_source"],
    createdAt: "2026-07-23T00:00:00.000Z",
    updatedAt: "2026-07-23T00:00:00.000Z",
  };
}

function kindFromObjectKey(objectKey: string): ArtifactKind {
  const kind = objectKey.split("/").at(-1);
  if (!artifactKinds.includes(kind as ArtifactKind)) throw new Error(`unexpected key ${objectKey}`);
  return kind as ArtifactKind;
}

function serviceHarness(clip: ClipRecord) {
  let intent: UploadIntentRecord | null = null;
  const providerAttempts = new Map<ArtifactKind, number>();
  const createMultipart = vi.fn(async (objectKey: string) => {
    const kind = kindFromObjectKey(objectKey);
    const attempt = (providerAttempts.get(kind) ?? 0) + 1;
    providerAttempts.set(kind, attempt);
    return `provider-${kind}-${attempt}`;
  });
  const signUploadPart = vi.fn(async (input: { partNumber: number; contentMd5: string }) => ({
    url: `https://r2.example/part-${input.partNumber}`,
    headers: { "content-md5": input.contentMd5 },
    expiresInSeconds: 300,
  }));
  const listParts = vi.fn(async (objectKey: string) => {
    const kind = kindFromObjectKey(objectKey);
    const expectedBytes = clip.artifacts.find((artifact) => artifact.kind === kind)!.expectedBytes;
    const count = Math.ceil(expectedBytes / FIVE_MIB);
    return Array.from({ length: count }, (_, index) => ({
      partNumber: index + 1,
      etag: `"etag-${kind}-${index + 1}"`,
      bytes: index + 1 === count ? expectedBytes - FIVE_MIB * (count - 1) : FIVE_MIB,
    }));
  });
  const completedKeys = new Set<string>();
  const completeMultipart = vi.fn(async (objectKey: string) => {
    completedKeys.add(objectKey);
  });
  const abortMultipart = vi.fn(async () => undefined);
  const headObject = vi.fn(async (objectKey: string) => ({
    bytes: completedKeys.has(objectKey)
      ? clip.artifacts.find((artifact) => artifact.kind === kindFromObjectKey(objectKey))!
          .expectedBytes
      : (() => {
          throw new Error("not found");
        })(),
    metadata: { dataminer_class: "raw-non-trainable" },
  }));
  const storage = {
    createMultipart,
    signUploadPart,
    listParts,
    completeMultipart,
    abortMultipart,
    objectExists: vi.fn(async (objectKey: string) => completedKeys.has(objectKey)),
    deleteObject: vi.fn(async () => undefined),
    listMultipartUploads: vi.fn(async () => []),
    headObject,
    getObject: vi.fn(async () => Readable.from([])),
  } as unknown as ObjectStorage;

  const beginUploadIntent = vi.fn(
    async (
      _deviceId: string,
      clipId: string,
      uploads: ReadonlyArray<{
        artifactId: string;
        kind: ArtifactKind;
        partSizeBytes: number;
      }>,
    ) => {
      const records: MultipartRecord[] = uploads.map((upload) => {
        const artifact = clip.artifacts.find((candidate) => candidate.kind === upload.kind)!;
        return {
          id: `multipart-${upload.kind}`,
          uploadIntentId: "intent-1",
          artifactId: upload.artifactId,
          clipId,
          kind: upload.kind,
          objectKey: artifact.objectKey,
          contentType: artifact.contentType,
          state: "initiated",
          attemptNo: 1,
          partSizeBytes: upload.partSizeBytes,
          expectedBytes: artifact.expectedBytes,
          createdAt: "2026-07-23T00:00:00.000Z",
        };
      });
      intent = {
        id: "intent-1",
        clipId,
        state: "initiated",
        sessionSchemaVersion: "dm-local-dev-session-2",
        uploads: records,
        createdAt: "2026-07-23T00:00:00.000Z",
      };
      return intent;
    },
  );
  const completeMultipartRecord = vi.fn(
    async (_deviceId: string, multipartId: string, _actualBytes: number) => {
      if (!intent) throw new Error("intent missing");
      const upload = intent.uploads.find((candidate) => candidate.id === multipartId);
      if (!upload) throw new Error("multipart missing");
      upload.state = "completed";
      if (intent.uploads.every((candidate) => candidate.state === "completed")) {
        intent.state = "completed";
      }
      return upload;
    },
  );
  const assertUploadAllowed = vi.fn(async () => undefined);
  const recordSignedParts = vi.fn(async () => undefined);
  const failMultipart = vi.fn(async () => undefined);
  const releaseMultipartCompletion = vi.fn(async () => true);
  const bindMultipartProviderUpload = vi.fn(
    async (_deviceId: string, multipartId: string, providerUploadId: string) => {
      if (!intent) throw new Error("intent missing");
      const upload = intent.uploads.find((candidate) => candidate.id === multipartId);
      if (!upload) throw new Error("multipart missing");
      upload.providerUploadId = providerUploadId;
    },
  );
  const beginMultipartCompletion = vi.fn(async (_deviceId: string, multipartId: string) => {
    if (!intent) throw new Error("intent missing");
    const upload = intent.uploads.find((candidate) => candidate.id === multipartId);
    if (!upload) throw new Error("multipart missing");
    return { ...upload, completionLeaseId: `lease-${multipartId}` };
  });
  const restartMultipartCompletion = vi.fn(
    async (
      _deviceId: string,
      multipartId: string,
      _completionLeaseId: string,
      providerUploadId: string,
    ) => {
      if (!intent) throw new Error("intent missing");
      const upload = intent.uploads.find((candidate) => candidate.id === multipartId);
      if (!upload) throw new Error("multipart missing");
      upload.providerUploadId = providerUploadId;
      upload.attemptNo += 1;
      return upload;
    },
  );
  const repository = {
    getClip: vi.fn(async () => clip),
    assertUploadAllowed,
    beginUploadIntent,
    findUploadIntentForClip: vi.fn(async () => intent),
    getUploadIntent: vi.fn(async () => intent),
    recordSignedParts,
    bindMultipartProviderUpload,
    beginMultipartCompletion,
    releaseMultipartCompletion,
    restartMultipartCompletion,
    completeMultipart: completeMultipartRecord,
    failMultipart,
  } as unknown as IngestionRepository;
  const service = new IngestionService(
    repository,
    storage,
    "dataminer-production",
    [
      {
        family: primaryFamily(environmentProfile("production")),
        prefix: "raw/v1/non-trainable/dataminer-api-prod",
      },
    ],
    FIVE_MIB,
    300,
    300,
  );
  return {
    service,
    storage,
    repository,
    createMultipart,
    beginUploadIntent,
    signUploadPart,
    listParts,
    completeMultipart,
    completeMultipartRecord,
    completedKeys,
    assertUploadAllowed,
    recordSignedParts,
    beginMultipartCompletion,
    releaseMultipartCompletion,
    restartMultipartCompletion,
    failMultipart,
  };
}

function completeEvidence(clip: ClipRecord) {
  return {
    artifacts: Object.fromEntries(
      clip.artifacts.map((artifact) => {
        const kind = artifact.kind;
        const bytes = artifact.expectedBytes;
        const count = Math.ceil(bytes / FIVE_MIB);
        return [
          kind,
          {
            parts: Array.from({ length: count }, (_, index) => ({
              partNumber: index + 1,
              etag: `etag-${kind}-${index + 1}`,
            })),
          },
        ];
      }),
    ),
  } as Parameters<IngestionService["completeUploadIntent"]>[2];
}

describe("IngestionService multipart bundle", () => {
  it("fails closed before creating provider uploads when retention blocks the clip", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    harness.assertUploadAllowed.mockRejectedValueOnce(
      conflict("retention_blocked", "clip retention no longer permits upload"),
    );

    await expect(
      harness.service.createUploadIntent("device-1", clip.id, {
        clipRevision: 1,
        transferMode: "server_owned_r2_multipart",
      }),
    ).rejects.toMatchObject({ code: "retention_blocked", statusCode: 409 });
    expect(harness.createMultipart).not.toHaveBeenCalled();
  });

  it("creates one server-owned multipart upload for every artifact and computes part counts", async () => {
    const clip = clipWithBytes({ video: FIVE_MIB + 7 });
    const harness = serviceHarness(clip);

    const result = await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    expect(harness.createMultipart).toHaveBeenCalledTimes(5);
    expect(harness.beginUploadIntent.mock.invocationCallOrder[0]).toBeLessThan(
      harness.createMultipart.mock.invocationCallOrder[0]!,
    );
    expect(Object.keys(result.artifacts).sort()).toEqual(
      clip.artifacts.map((artifact) => artifact.kind).sort(),
    );
    expect(result.artifacts.video).toMatchObject({ partSizeBytes: FIVE_MIB, partCount: 2 });
    expect(result.artifacts.manifest).toMatchObject({ partSizeBytes: FIVE_MIB, partCount: 1 });
    for (const [objectKey] of harness.createMultipart.mock.calls) {
      expect(objectKey).toMatch(/^raw\/v1\/non-trainable\//);
    }
  });

  it("creates and completes the optional sixth multipart upload for present audio", async () => {
    const clip = clipWithBytes({ audio: FIVE_MIB + 3 });
    const harness = serviceHarness(clip);

    const created = await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    expect(harness.createMultipart).toHaveBeenCalledTimes(6);
    expect(created.artifacts.audio).toMatchObject({ partSizeBytes: FIVE_MIB, partCount: 2 });

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).resolves.toEqual({ uploadIntentId: "intent-1", state: "stored" });
    expect(harness.completeMultipart).toHaveBeenCalledTimes(6);
  });

  it("creates and completes video plus depth in the same seven-artifact upload intent", async () => {
    const clip = clipWithBytes({ audio: 17, depth: FIVE_MIB + 11 });
    const harness = serviceHarness(clip);

    const created = await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    expect(harness.createMultipart).toHaveBeenCalledTimes(7);
    expect(created.artifacts.depth).toMatchObject({ partSizeBytes: FIVE_MIB, partCount: 2 });
    expect(created.artifacts.video).toBeDefined();

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).resolves.toEqual({ uploadIntentId: "intent-1", state: "stored" });
    expect(harness.completeMultipart).toHaveBeenCalledTimes(7);
    expect(harness.completeMultipartRecord).toHaveBeenCalledTimes(7);
  });

  it("reuses the durable SQL creation claim after a provider create failure", async () => {
    const clip = clipWithBytes({ depth: 17 });
    const harness = serviceHarness(clip);
    harness.createMultipart.mockRejectedValueOnce(new Error("provider unavailable"));

    await expect(
      harness.service.createUploadIntent("device-1", clip.id, {
        clipRevision: 1,
        transferMode: "server_owned_r2_multipart",
      }),
    ).rejects.toThrow("provider unavailable");

    await expect(
      harness.service.createUploadIntent("device-1", clip.id, {
        clipRevision: 1,
        transferMode: "server_owned_r2_multipart",
      }),
    ).resolves.toMatchObject({
      uploadIntentId: "intent-1",
      artifacts: { video: expect.any(Object), depth: expect.any(Object) },
    });

    expect(harness.beginUploadIntent).toHaveBeenCalledOnce();
    expect(harness.createMultipart).toHaveBeenCalledTimes(7);
  });

  it("requires every non-final part to match the clip-level part size exactly", async () => {
    const clip = clipWithBytes({ video: FIVE_MIB + 7 });
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    await expect(
      harness.service.signParts("device-1", "intent-1", {
        parts: [
          {
            artifactType: "video",
            partNumber: 1,
            byteSize: FIVE_MIB - 1,
            contentMd5: "AAAAAAAAAAAAAAAAAAAAAA==",
          },
        ],
      }),
    ).rejects.toMatchObject({ code: "invalid_part_size", statusCode: 400 });
    expect(harness.signUploadPart).not.toHaveBeenCalled();

    await harness.service.signParts("device-1", "intent-1", {
      parts: [
        {
          artifactType: "video",
          partNumber: 1,
          byteSize: FIVE_MIB,
          contentMd5: "AAAAAAAAAAAAAAAAAAAAAA==",
        },
        {
          artifactType: "video",
          partNumber: 2,
          byteSize: 7,
          contentMd5: "BBBBBBBBBBBBBBBBBBBBBB==",
        },
      ],
    });
    expect(harness.signUploadPart).toHaveBeenCalledTimes(2);
    expect(harness.recordSignedParts).toHaveBeenCalledWith(
      "device-1",
      "intent-1",
      expect.any(Array),
      305,
    );
  });

  it("accepts quoted provider ETags but completes only after all five artifacts reconcile", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).resolves.toEqual({ uploadIntentId: "intent-1", state: "stored" });
    expect(harness.listParts).toHaveBeenCalledTimes(5);
    expect(harness.completeMultipart).toHaveBeenCalledTimes(5);
    expect(harness.completeMultipartRecord).toHaveBeenCalledTimes(5);
  });

  it("rejects a client ETag that does not match the provider's authoritative listing", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });
    const evidence = completeEvidence(clip);
    evidence.artifacts.video.parts[0]!.etag = "invented-etag";

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", evidence),
    ).rejects.toMatchObject({ code: "multipart_etag_mismatch", statusCode: 409 });
    expect(harness.completeMultipart).not.toHaveBeenCalled();
  });

  it("queues cleanup when governance closes after R2 completion but before SQL completion", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });
    harness.completeMultipartRecord.mockRejectedValueOnce(
      conflict("retention_blocked", "clip retention no longer permits upload"),
    );

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).rejects.toMatchObject({ code: "retention_blocked", statusCode: 409 });
    expect(harness.completeMultipart).toHaveBeenCalledTimes(1);
    expect(harness.failMultipart).toHaveBeenCalledWith(
      "device-1",
      "multipart-video",
      "lease-multipart-video",
      "retention_blocked",
    );
  });

  it("restarts only the missing provider attempt after R2 loses an events completion", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });

    const defaultListParts = harness.listParts.getMockImplementation()!;
    let eventsListCalls = 0;
    harness.listParts.mockImplementation(async (objectKey: string) => {
      if (kindFromObjectKey(objectKey) === "events" && ++eventsListCalls === 2) {
        throw Object.assign(new Error("provider upload disappeared"), { name: "NoSuchUpload" });
      }
      return defaultListParts(objectKey);
    });
    harness.completeMultipart.mockImplementation(async (objectKey: string) => {
      if (kindFromObjectKey(objectKey) === "events") {
        throw Object.assign(new Error("provider failed completion"), {
          name: "InternalError",
          $metadata: { requestId: "provider-request-1", httpStatusCode: 500 },
        });
      }
      harness.completedKeys.add(objectKey);
    });

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).rejects.toMatchObject({
      code: "multipart_upload_restarted",
      statusCode: 409,
      retryable: true,
    });

    expect(harness.completeMultipartRecord).toHaveBeenCalledTimes(2);
    expect(harness.restartMultipartCompletion).toHaveBeenCalledWith(
      "device-1",
      "multipart-events",
      "lease-multipart-events",
      "provider-events-2",
    );
    expect(harness.releaseMultipartCompletion).not.toHaveBeenCalled();
  });

  it("restarts a stale completing attempt when its provider upload is already gone", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });
    const defaultListParts = harness.listParts.getMockImplementation()!;
    harness.listParts.mockImplementation(async (objectKey: string) => {
      if (kindFromObjectKey(objectKey) === "events") {
        throw Object.assign(new Error("provider upload disappeared"), { name: "NoSuchUpload" });
      }
      return defaultListParts(objectKey);
    });

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).rejects.toMatchObject({ code: "multipart_upload_restarted", retryable: true });
    expect(harness.completeMultipart).toHaveBeenCalledTimes(2);
    expect(harness.restartMultipartCompletion).toHaveBeenCalledWith(
      "device-1",
      "multipart-events",
      "lease-multipart-events",
      "provider-events-2",
    );
  });

  it("adopts the object when R2 reports an error after actually completing it", async () => {
    const clip = clipWithBytes();
    const harness = serviceHarness(clip);
    await harness.service.createUploadIntent("device-1", clip.id, {
      clipRevision: 1,
      transferMode: "server_owned_r2_multipart",
    });
    harness.completeMultipart.mockImplementation(async (objectKey: string) => {
      harness.completedKeys.add(objectKey);
      if (kindFromObjectKey(objectKey) === "events") {
        throw Object.assign(new Error("response lost"), { name: "InternalError" });
      }
    });

    await expect(
      harness.service.completeUploadIntent("device-1", "intent-1", completeEvidence(clip)),
    ).resolves.toEqual({ uploadIntentId: "intent-1", state: "stored" });
    expect(harness.restartMultipartCompletion).not.toHaveBeenCalled();
    expect(harness.completeMultipartRecord).toHaveBeenCalledTimes(5);
  });
});
