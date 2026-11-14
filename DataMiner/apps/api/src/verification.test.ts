import { Readable } from "node:stream";

import { describe, expect, it, vi } from "vitest";

import { sha256Bytes } from "./checksum.js";
import type { IngestionRepository, VerificationJob } from "./domain.js";
import type { ObjectStorage } from "./storage.js";
import { verifyOne } from "./verification.js";

function verificationHarness(chunks: Buffer[]) {
  const finishVerification = vi.fn();
  const retryVerification = vi.fn();
  const repository = {
    finishVerification,
    retryVerification,
  } as unknown as IngestionRepository;
  const storage = {
    getObject: vi.fn(async () => Readable.from(chunks)),
  } as unknown as ObjectStorage;
  return { repository, storage, finishVerification, retryVerification };
}

function job(
  expectedBytes: number,
  expectedSha256: string,
  extra: Partial<Pick<VerificationJob, "kind" | "semantic">> = {},
): VerificationJob {
  return {
    id: "verification-1",
    artifactId: "artifact-1",
    clipId: "clip-1",
    objectKey: "raw/v1/non-trainable/session/run/clip/video.mp4",
    expectedBytes,
    expectedSha256,
    attempt: 1,
    kind: extra.kind ?? "video",
    ...(extra.semantic ? { semantic: extra.semantic } : {}),
  };
}

describe("verifyOne", () => {
  it("logs provider failures with safe structured context and without the object key", async () => {
    const harness = verificationHarness([]);
    const providerError = Object.assign(new Error("provider refused the read"), {
      name: "NoSuchKey",
      code: "NoSuchKey",
      $metadata: { httpStatusCode: 404, requestId: "r2-request-1" },
    });
    harness.storage.getObject = vi.fn(async () => {
      throw providerError;
    });
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const verificationJob = job(42, `sha256:${"a".repeat(64)}`);

    try {
      await verifyOne(harness.repository, harness.storage, verificationJob);
      expect(harness.retryVerification).toHaveBeenCalledWith(verificationJob, "storage_nosuchkey");
      const line = String(consoleError.mock.calls[0]?.[0]);
      const fields = JSON.parse(line) as Record<string, unknown>;
      expect(fields).toMatchObject({
        verificationJobId: "verification-1",
        clipId: "clip-1",
        artifactKind: "video",
        stage: "get",
        providerErrorCode: "NoSuchKey",
        httpStatusCode: 404,
        requestId: "r2-request-1",
      });
      expect(line).not.toContain(verificationJob.objectKey);
    } finally {
      consoleError.mockRestore();
    }
  });

  it("写回数据库失败时落 repository_* 而不是 storage_*（08-03 的 42P08 复现）", async () => {
    const chunks = [Buffer.from("payload")];
    const bytes = Buffer.concat(chunks);
    const harness = verificationHarness(chunks);
    // 事故当天 finishVerification 里那条 UPDATE 报的就是这个：$2 既做 enum 赋值
    // 又进等值比较，PG 推不出类型。旧口径把它记成 storage_error，指错了排查方向。
    harness.finishVerification.mockRejectedValue(
      Object.assign(new Error("could not determine data type of parameter $2"), {
        name: "error",
        code: "42P08",
      }),
    );
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const verificationJob = job(bytes.length, sha256Bytes(bytes));

    try {
      await verifyOne(harness.repository, harness.storage, verificationJob);

      expect(harness.retryVerification).toHaveBeenCalledWith(verificationJob, "repository_42p08");
      const fields = JSON.parse(String(consoleError.mock.calls[0]?.[0])) as Record<string, unknown>;
      expect(fields).toMatchObject({ stage: "repository", errorCode: "repository_42p08" });
    } finally {
      consoleError.mockRestore();
    }
  });

  it("永久判死时单独出一行，而不是与前 7 次尝试长得一模一样", async () => {
    const harness = verificationHarness([]);
    harness.storage.getObject = vi.fn(async () => {
      throw Object.assign(new Error("gone"), { name: "NoSuchKey" });
    });
    harness.retryVerification.mockResolvedValue("dead_letter");
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);

    try {
      await verifyOne(harness.repository, harness.storage, job(1, `sha256:${"a".repeat(64)}`));

      const messages = consoleError.mock.calls.map(
        (call) => (JSON.parse(String(call[0])) as { message?: string }).message,
      );
      expect(messages).toEqual(["verification attempt failed", "verification permanently failed"]);
    } finally {
      consoleError.mockRestore();
    }
  });

  it("还能重试时不出那一行", async () => {
    const harness = verificationHarness([]);
    harness.storage.getObject = vi.fn(async () => {
      throw Object.assign(new Error("gone"), { name: "NoSuchKey" });
    });
    harness.retryVerification.mockResolvedValue("retry_scheduled");
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);

    try {
      await verifyOne(harness.repository, harness.storage, job(1, `sha256:${"a".repeat(64)}`));

      expect(consoleError).toHaveBeenCalledTimes(1);
    } finally {
      consoleError.mockRestore();
    }
  });

  it.each(["video", "depth"] as const)("streams the full %s object and verifies its whole-object SHA-256", async (kind) => {
    const chunks = [Buffer.from("whole-"), Buffer.from("object-"), Buffer.from("payload")];
    const bytes = Buffer.concat(chunks);
    const harness = verificationHarness(chunks);
    const verificationJob = job(bytes.length, sha256Bytes(bytes), { kind });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(verificationJob, {
      outcome: "verified",
      actualBytes: bytes.length,
      actualSha256: sha256Bytes(bytes),
    });
    expect(harness.retryVerification).not.toHaveBeenCalled();
  });

  it.each(["video", "depth"] as const)("rejects equal-length %s content whose full SHA-256 does not match", async (kind) => {
    const actual = Buffer.from("actual payload");
    const expected = Buffer.from("wanted payload");
    expect(actual.length).toBe(expected.length);
    const harness = verificationHarness([actual.subarray(0, 3), actual.subarray(3)]);
    const verificationJob = job(expected.length, sha256Bytes(expected), { kind });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(verificationJob, {
      outcome: "rejected",
      actualBytes: actual.length,
      actualSha256: sha256Bytes(actual),
      rejectionCode: "sha256_mismatch",
    });
  });

  it.each(["video", "depth"] as const)("reports %s size mismatch before treating multipart ETag as integrity proof", async (kind) => {
    const actual = Buffer.from("short");
    const harness = verificationHarness([actual]);
    const verificationJob = job(actual.length + 1, sha256Bytes(actual), { kind });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(
      verificationJob,
      expect.objectContaining({ outcome: "rejected", rejectionCode: "size_mismatch" }),
    );
  });

  it("rejects a byte-valid manifest whose exact four-file contract is wrong", async () => {
    const payload = Buffer.from(JSON.stringify({ "video.mp4": `sha256:${"b".repeat(64)}` }));
    const harness = verificationHarness([payload]);
    const verificationJob = job(payload.length, sha256Bytes(payload), {
      kind: "manifest",
      semantic: {
        kind: "manifest",
        expectedEntries: {
          "video.mp4": `sha256:${"a".repeat(64)}`,
          "frames.jsonl.zst": `sha256:${"b".repeat(64)}`,
          "events.jsonl.zst": `sha256:${"c".repeat(64)}`,
          "meta.json": `sha256:${"d".repeat(64)}`,
        },
      },
    });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(
      verificationJob,
      expect.objectContaining({ outcome: "rejected", rejectionCode: "manifest_contract_mismatch" }),
    );
  });

  it("accepts a byte-valid meta document only when the locked non-trainable subset matches", async () => {
    const payload = Buffer.from(
      JSON.stringify({
        schema_version: "dm-local-dev-recording-1",
        non_trainable: true,
        production_certified: false,
        audio: {
          health: {
            discontinuities: [],
            silence_filled_spans: [],
          },
        },
        extra_current_field: "allowed",
      }),
    );
    const harness = verificationHarness([payload]);
    const verificationJob = job(payload.length, sha256Bytes(payload), {
      kind: "meta",
      semantic: {
        kind: "meta",
        expectedSubset: {
          schema_version: "dm-local-dev-recording-1",
          non_trainable: true,
          production_certified: false,
          audio: {
            health: {
              discontinuities: [],
              silence_filled_spans: [],
            },
          },
        },
      },
    });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(verificationJob, {
      outcome: "verified",
      actualBytes: payload.length,
      actualSha256: sha256Bytes(payload),
    });
  });

  it("accepts neutral v2 meta only while retired training keys stay absent", async () => {
    const payload = Buffer.from(
      JSON.stringify({
        schema_version: "dm-raw-recording-1",
        state: "ready",
        extra_current_field: "allowed",
      }),
    );
    const harness = verificationHarness([payload]);
    const verificationJob = job(payload.length, sha256Bytes(payload), {
      kind: "meta",
      semantic: {
        kind: "meta",
        expectedSubset: { schema_version: "dm-raw-recording-1", state: "ready" },
        forbiddenTopLevelKeys: ["non_trainable", "production_certified"],
      },
    });

    await verifyOne(harness.repository, harness.storage, verificationJob);

    expect(harness.finishVerification).toHaveBeenCalledWith(verificationJob, {
      outcome: "verified",
      actualBytes: payload.length,
      actualSha256: sha256Bytes(payload),
    });
  });

  it("rejects a neutral v2 meta document that reintroduces retired training keys", async () => {
    const payload = Buffer.from(
      JSON.stringify({
        schema_version: "dm-raw-recording-1",
        state: "ready",
        non_trainable: true,
      }),
    );
    const harness = verificationHarness([payload]);
    const verificationJob = job(payload.length, sha256Bytes(payload), {
      kind: "meta",
      semantic: {
        kind: "meta",
        expectedSubset: { schema_version: "dm-raw-recording-1", state: "ready" },
        forbiddenTopLevelKeys: ["non_trainable", "production_certified"],
      },
    });
    const consoleWarn = vi.spyOn(console, "warn").mockImplementation(() => undefined);

    try {
      await verifyOne(harness.repository, harness.storage, verificationJob);
      expect(harness.finishVerification).toHaveBeenCalledWith(
        verificationJob,
        expect.objectContaining({ outcome: "rejected", rejectionCode: "meta_contract_mismatch" }),
      );
      const fields = JSON.parse(String(consoleWarn.mock.calls[0]?.[0])) as Record<string, unknown>;
      expect(fields.semanticMismatchPaths).toEqual(["/non_trainable"]);
    } finally {
      consoleWarn.mockRestore();
    }
  });

  it("logs the JSON-pointer path when nested meta content differs", async () => {
    const payload = Buffer.from(JSON.stringify({ audio: { health: { queue_overflows: 3 } } }));
    const harness = verificationHarness([payload]);
    const verificationJob = job(payload.length, sha256Bytes(payload), {
      kind: "meta",
      semantic: {
        kind: "meta",
        expectedSubset: { audio: { health: { queue_overflows: 0 } } },
      },
    });
    const consoleWarn = vi.spyOn(console, "warn").mockImplementation(() => undefined);

    try {
      await verifyOne(harness.repository, harness.storage, verificationJob);
      expect(harness.finishVerification).toHaveBeenCalledWith(
        verificationJob,
        expect.objectContaining({ outcome: "rejected", rejectionCode: "meta_contract_mismatch" }),
      );
      const fields = JSON.parse(String(consoleWarn.mock.calls[0]?.[0])) as Record<string, unknown>;
      expect(fields.semanticMismatchPaths).toEqual(["/audio/health/queue_overflows"]);
    } finally {
      consoleWarn.mockRestore();
    }
  });
});
