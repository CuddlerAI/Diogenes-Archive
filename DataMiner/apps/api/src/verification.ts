import { createHash } from "node:crypto";
import type { Readable } from "node:stream";

import type { IngestionRepository, VerificationJob } from "./domain.js";
import type { ObjectStorage } from "./storage.js";
import { type VerificationStage, verificationFaultCode } from "./verification-faults.js";

const MAX_SEMANTIC_JSON_BYTES = 2 * 1024 * 1024;
const MAX_SEMANTIC_MISMATCH_PATHS = 8;

type SemanticRejection = {
  code: string;
  mismatchPaths?: string[];
};

export async function verifyOne(
  repository: IngestionRepository,
  storage: ObjectStorage,
  job: VerificationJob,
): Promise<void> {
  let stage: VerificationStage = "get";
  try {
    const stream = await storage.getObject(job.objectKey);
    stage = "stream_hash";
    const actual = await inspectStream(stream, job.semantic !== undefined);
    const actualBytes = Number(actual.bytes);
    const rejection: SemanticRejection | undefined =
      actualBytes !== job.expectedBytes
        ? { code: "size_mismatch" }
        : actual.sha256 !== job.expectedSha256
          ? { code: "sha256_mismatch" }
          : semanticRejection(job, actual.payload, actual.semanticOverflow);
    const rejectionCode = rejection?.code;
    stage = "repository";
    await repository.finishVerification(
      job,
      rejectionCode
        ? {
            outcome: "rejected",
            actualBytes,
            actualSha256: actual.sha256,
            rejectionCode,
          }
        : { outcome: "verified", actualBytes, actualSha256: actual.sha256 },
    );
    if (rejectionCode) {
      emitVerificationLog("WARNING", "verification content rejected", {
        ...verificationContext(job),
        stage: "stream_hash",
        rejectionCode,
        ...(rejection.mismatchPaths ? { semanticMismatchPaths: rejection.mismatchPaths } : {}),
        actualBytes,
        actualSha256: actual.sha256,
      });
    }
  } catch (error) {
    const retryErrorCode = verificationFaultCode(stage, error);
    emitVerificationLog("ERROR", "verification attempt failed", {
      ...verificationContext(job),
      stage,
      errorCode: retryErrorCode,
      ...faultProviderDetails(error),
    });
    const outcome = await repository.retryVerification(job, retryErrorCode);
    // 第 8 次尝试与前 7 次原先打的是同一行日志，没有任何一行说「这件产物已经永久判死」。
    // 而同一个故障横扫每一件产物、8 次全败，那不是抖动是服务端出事了——值得单独一行。
    if (outcome === "dead_letter") {
      emitVerificationLog("ERROR", "verification permanently failed", {
        ...verificationContext(job),
        stage,
        errorCode: retryErrorCode,
        ...faultProviderDetails(error),
      });
    }
  }
}

export async function runVerificationBatch(
  repository: IngestionRepository,
  storage: ObjectStorage,
  batchSize: number,
  leaseSeconds: number,
): Promise<number> {
  const jobs = await repository.leaseVerificationJobs(batchSize, leaseSeconds);
  await Promise.all(jobs.map((job) => verifyOne(repository, storage, job)));
  return jobs.length;
}

async function inspectStream(
  stream: Readable,
  captureSemanticPayload: boolean,
): Promise<{ sha256: string; bytes: bigint; payload?: Buffer; semanticOverflow: boolean }> {
  const hash = createHash("sha256");
  const chunks: Buffer[] = [];
  let bytes = 0n;
  let capturedBytes = 0;
  let semanticOverflow = false;
  for await (const chunk of stream) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk as Uint8Array);
    hash.update(buffer);
    bytes += BigInt(buffer.length);
    if (captureSemanticPayload && !semanticOverflow) {
      capturedBytes += buffer.length;
      if (capturedBytes > MAX_SEMANTIC_JSON_BYTES) {
        semanticOverflow = true;
        chunks.length = 0;
      } else {
        chunks.push(buffer);
      }
    }
  }
  return {
    sha256: `sha256:${hash.digest("hex")}`,
    bytes,
    ...(captureSemanticPayload && !semanticOverflow ? { payload: Buffer.concat(chunks) } : {}),
    semanticOverflow,
  };
}

function semanticRejection(
  job: VerificationJob,
  payload: Buffer | undefined,
  overflow: boolean,
): SemanticRejection | undefined {
  const semantic = job.semantic;
  if (!semantic) return undefined;
  if (overflow || !payload) return { code: "semantic_payload_too_large" };
  let parsed: unknown;
  try {
    parsed = JSON.parse(payload.toString("utf8"));
  } catch {
    return { code: `${semantic.kind}_invalid_json` };
  }
  if (semantic.kind === "manifest") {
    if (!isRecord(parsed)) return { code: "manifest_contract_mismatch" };
    const actualKeys = Object.keys(parsed).sort();
    const expectedKeys = Object.keys(semantic.expectedEntries).sort();
    if (
      actualKeys.length !== expectedKeys.length ||
      actualKeys.some((key, index) => key !== expectedKeys[index]) ||
      expectedKeys.some((key) => parsed[key] !== semantic.expectedEntries[key])
    ) {
      return { code: "manifest_contract_mismatch" };
    }
    return undefined;
  }
  const forbiddenPaths = isRecord(parsed)
    ? (semantic.forbiddenTopLevelKeys ?? [])
        .filter((key) => Object.hasOwn(parsed, key))
        .map((key) => jsonPointerPath("", key))
    : [];
  const mismatchPaths = [
    ...new Set([...subsetMismatchPaths(parsed, semantic.expectedSubset), ...forbiddenPaths]),
  ].slice(0, MAX_SEMANTIC_MISMATCH_PATHS);
  return mismatchPaths.length === 0
    ? undefined
    : { code: "meta_contract_mismatch", mismatchPaths };
}

function subsetMismatchPaths(actual: unknown, expected: unknown, path = ""): string[] {
  const currentPath = path || "/";
  if (Array.isArray(expected)) {
    // JSON parsing always creates fresh arrays, so reference equality would
    // reject valid audio health arrays even when every element matches.
    if (!Array.isArray(actual)) return [currentPath];
    const mismatches = actual.length === expected.length ? [] : [`${currentPath}/length`];
    for (
      let index = 0;
      index < expected.length && mismatches.length < MAX_SEMANTIC_MISMATCH_PATHS;
      index += 1
    ) {
      mismatches.push(
        ...subsetMismatchPaths(actual[index], expected[index], jsonPointerPath(path, String(index))),
      );
    }
    return mismatches.slice(0, MAX_SEMANTIC_MISMATCH_PATHS);
  }
  if (isRecord(expected)) {
    if (!isRecord(actual)) return [currentPath];
    const mismatches: string[] = [];
    for (const [key, value] of Object.entries(expected)) {
      mismatches.push(...subsetMismatchPaths(actual[key], value, jsonPointerPath(path, key)));
      if (mismatches.length >= MAX_SEMANTIC_MISMATCH_PATHS) break;
    }
    return mismatches.slice(0, MAX_SEMANTIC_MISMATCH_PATHS);
  }
  return Object.is(actual, expected) ? [] : [currentPath];
}

function jsonPointerPath(parent: string, key: string): string {
  const escaped = key.replaceAll("~", "~0").replaceAll("/", "~1");
  return `${parent}/${escaped}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function verificationContext(job: VerificationJob) {
  return {
    verificationJobId: job.id,
    clipId: job.clipId,
    artifactId: job.artifactId,
    artifactKind: job.kind,
    objectKeyFingerprint: createHash("sha256").update(job.objectKey).digest("hex").slice(0, 24),
    expectedBytes: job.expectedBytes,
    expectedSha256: job.expectedSha256,
    attempt: job.attempt,
    workerRevision: process.env.K_REVISION ?? "unknown",
    workerCommit: process.env.DATAMINER_BUILD_COMMIT ?? "unknown",
  };
}

/**
 * 三个阶段共用：字段名保持 `providerErrorCode`（既有日志查询与告警按它写的），
 * 但它现在既可能是 R2 的 `NoSuchKey`，也可能是 Postgres 的 SQLSTATE `42P08`——
 * 要判在哪一层看 `stage` 与 `errorCode` 的前缀，不要看这个字段。
 */
function faultProviderDetails(error: unknown) {
  const candidate = error as {
    name?: unknown;
    code?: unknown;
    $metadata?: { httpStatusCode?: unknown; requestId?: unknown; extendedRequestId?: unknown };
  };
  const metadata = candidate && typeof candidate === "object" ? candidate.$metadata : undefined;
  return {
    providerErrorCode:
      typeof candidate?.code === "string"
        ? candidate.code
        : typeof candidate?.name === "string"
          ? candidate.name
          : "unknown_error",
    ...(typeof metadata?.httpStatusCode === "number" ? { httpStatusCode: metadata.httpStatusCode } : {}),
    ...(typeof metadata?.requestId === "string" ? { requestId: metadata.requestId } : {}),
    ...(typeof metadata?.extendedRequestId === "string"
      ? { extendedRequestId: metadata.extendedRequestId }
      : {}),
  };
}

function emitVerificationLog(
  severity: "WARNING" | "ERROR",
  message: string,
  fields: Record<string, unknown>,
): void {
  const line = JSON.stringify({ severity, message, ...fields });
  if (severity === "ERROR") console.error(line);
  else console.warn(line);
}
