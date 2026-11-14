import { Readable } from "node:stream";

import {
  AbortMultipartUploadCommand,
  CompleteMultipartUploadCommand,
  CreateMultipartUploadCommand,
  DeleteObjectCommand,
  GetObjectCommand,
  HeadBucketCommand,
  HeadObjectCommand,
  ListMultipartUploadsCommand,
  ListPartsCommand,
  PutObjectCommand,
  S3Client,
  UploadPartCommand,
  type CompletedPart,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";

import type { AppConfig } from "./config.js";

export type UploadPart = { partNumber: number; etag: string; bytes?: number };
export type ActiveMultipartUpload = {
  objectKey: string;
  providerUploadId: string;
  initiatedAt: string;
};

export interface ObjectStorage {
  ping(): Promise<void>;
  /** `objectClass` is stamped as `x-amz-meta-dataminer_class` and re-checked at completion. */
  createMultipart(objectKey: string, contentType: string, objectClass: string): Promise<string>;
  signUploadPart(input: {
    objectKey: string;
    providerUploadId: string;
    partNumber: number;
    bytes: number;
    contentMd5: string;
  }): Promise<{ url: string; headers: Record<string, string>; expiresInSeconds: number }>;
  /** Single-object presigned PUT for small session artifacts (no multipart). */
  signPutObject(input: {
    objectKey: string;
    contentType: string;
    bytes: number;
    contentMd5: string;
    objectClass: string;
  }): Promise<{ url: string; headers: Record<string, string>; expiresInSeconds: number }>;
  listParts(objectKey: string, providerUploadId: string): Promise<UploadPart[]>;
  completeMultipart(objectKey: string, providerUploadId: string, parts: UploadPart[]): Promise<void>;
  abortMultipart(objectKey: string, providerUploadId: string): Promise<void>;
  listMultipartUploads(prefix: string): Promise<ActiveMultipartUpload[]>;
  deleteObject(objectKey: string): Promise<void>;
  objectExists(objectKey: string): Promise<boolean>;
  headObject(objectKey: string): Promise<{ bytes: number; metadata: Record<string, string> }>;
  getObject(objectKey: string): Promise<Readable>;
}

export class R2ObjectStorage implements ObjectStorage {
  readonly #client: S3Client;
  readonly #bucket: string;
  readonly #presignTtlSeconds: number;
  readonly #requestTimeoutMs: number;

  constructor(config: AppConfig["r2"]) {
    this.#bucket = config.bucket;
    this.#presignTtlSeconds = config.presignTtlSeconds;
    this.#requestTimeoutMs = config.requestTimeoutSeconds * 1_000;
    this.#client = new S3Client({
      region: "auto",
      endpoint: `https://${config.accountId}.r2.cloudflarestorage.com`,
      credentials: {
        accessKeyId: config.accessKeyId,
        secretAccessKey: config.secretAccessKey,
      },
      // Every upload already carries the caller-computed Content-MD5. The SDK's
      // WHEN_SUPPORTED default adds a second CRC32 checksum to presigned URLs;
      // because presigning has no body, that checksum is the empty payload's
      // AAAAAA== and R2 resets the connection once real bytes arrive.
      requestChecksumCalculation: "WHEN_REQUIRED",
    });
  }

  async ping(): Promise<void> {
    await this.#client.send(new HeadBucketCommand({ Bucket: this.#bucket }), {
      abortSignal: AbortSignal.timeout(this.#requestTimeoutMs),
    });
  }

  async createMultipart(objectKey: string, contentType: string, objectClass: string): Promise<string> {
    const response = await this.#client.send(
      new CreateMultipartUploadCommand({
        Bucket: this.#bucket,
        Key: objectKey,
        ContentType: contentType,
        // Per-lane class from the admitted family. This used to be the literal
        // "raw-non-trainable", which happened to match production's objectClass
        // but not staging's ("raw-staging-non-trainable") — a staging completion
        // would have failed its own head_metadata_mismatch check.
        Metadata: { dataminer_class: objectClass },
      }),
      { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
    );
    if (!response.UploadId) {
      throw new Error("R2 did not return an upload id");
    }
    return response.UploadId;
  }

  async signUploadPart(input: {
    objectKey: string;
    providerUploadId: string;
    partNumber: number;
    bytes: number;
    contentMd5: string;
  }): Promise<{ url: string; headers: Record<string, string>; expiresInSeconds: number }> {
    const command = new UploadPartCommand({
      Bucket: this.#bucket,
      Key: input.objectKey,
      UploadId: input.providerUploadId,
      PartNumber: input.partNumber,
      ContentLength: input.bytes,
      ContentMD5: input.contentMd5,
    });
    const url = await getSignedUrl(this.#client, command, { expiresIn: this.#presignTtlSeconds });
    return {
      url,
      headers: { "content-md5": input.contentMd5, "content-length": String(input.bytes) },
      expiresInSeconds: this.#presignTtlSeconds,
    };
  }

  async signPutObject(input: {
    objectKey: string;
    contentType: string;
    bytes: number;
    contentMd5: string;
    objectClass: string;
  }): Promise<{ url: string; headers: Record<string, string>; expiresInSeconds: number }> {
    const command = new PutObjectCommand({
      Bucket: this.#bucket,
      Key: input.objectKey,
      ContentType: input.contentType,
      ContentLength: input.bytes,
      ContentMD5: input.contentMd5,
      Metadata: { dataminer_class: input.objectClass },
    });
    const url = await getSignedUrl(this.#client, command, { expiresIn: this.#presignTtlSeconds });
    return {
      url,
      // Exactly the signed headers; the client must replay them verbatim.
      headers: {
        "content-type": input.contentType,
        "content-md5": input.contentMd5,
        "content-length": String(input.bytes),
        "x-amz-meta-dataminer_class": input.objectClass,
      },
      expiresInSeconds: this.#presignTtlSeconds,
    };
  }

  async listParts(objectKey: string, providerUploadId: string): Promise<UploadPart[]> {
    const parts: UploadPart[] = [];
    let partNumberMarker: string | undefined;
    do {
      const response = await this.#client.send(
        new ListPartsCommand({
          Bucket: this.#bucket,
          Key: objectKey,
          UploadId: providerUploadId,
          PartNumberMarker: partNumberMarker,
        }),
        { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
      );
      for (const part of response.Parts ?? []) {
        if (part.PartNumber === undefined || !part.ETag) {
          throw new Error("R2 returned an incomplete multipart listing");
        }
        parts.push({
          partNumber: part.PartNumber,
          etag: part.ETag,
          ...(part.Size !== undefined ? { bytes: part.Size } : {}),
        });
      }
      partNumberMarker = response.IsTruncated ? response.NextPartNumberMarker : undefined;
    } while (partNumberMarker !== undefined);
    return parts;
  }

  async completeMultipart(objectKey: string, providerUploadId: string, parts: UploadPart[]): Promise<void> {
    const completedParts: CompletedPart[] = parts.map((part) => ({
      PartNumber: part.partNumber,
      ETag: part.etag,
    }));
    await this.#client.send(
      new CompleteMultipartUploadCommand({
        Bucket: this.#bucket,
        Key: objectKey,
        UploadId: providerUploadId,
        MultipartUpload: { Parts: completedParts },
      }),
      { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
    );
  }

  async abortMultipart(objectKey: string, providerUploadId: string): Promise<void> {
    try {
      await this.#client.send(
        new AbortMultipartUploadCommand({
          Bucket: this.#bucket,
          Key: objectKey,
          UploadId: providerUploadId,
        }),
        { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
      );
    } catch (error) {
      if (!isNoSuchUploadError(error)) throw error;
    }
  }

  async listMultipartUploads(prefix: string): Promise<ActiveMultipartUpload[]> {
    const uploads: ActiveMultipartUpload[] = [];
    let keyMarker: string | undefined;
    let uploadIdMarker: string | undefined;
    do {
      const response = await this.#client.send(
        new ListMultipartUploadsCommand({
          Bucket: this.#bucket,
          Prefix: prefix,
          KeyMarker: keyMarker,
          UploadIdMarker: uploadIdMarker,
        }),
        { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
      );
      for (const upload of response.Uploads ?? []) {
        if (!upload.Key || !upload.UploadId || !upload.Initiated) {
          throw new Error("R2 returned an incomplete multipart upload listing");
        }
        uploads.push({
          objectKey: upload.Key,
          providerUploadId: upload.UploadId,
          initiatedAt: upload.Initiated.toISOString(),
        });
      }
      keyMarker = response.IsTruncated ? response.NextKeyMarker : undefined;
      uploadIdMarker = response.IsTruncated ? response.NextUploadIdMarker : undefined;
      if (response.IsTruncated && (!keyMarker || !uploadIdMarker)) {
        throw new Error("R2 truncated multipart listing without continuation markers");
      }
    } while (keyMarker !== undefined && uploadIdMarker !== undefined);
    return uploads;
  }

  async deleteObject(objectKey: string): Promise<void> {
    await this.#client.send(
      new DeleteObjectCommand({ Bucket: this.#bucket, Key: objectKey }),
      { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
    );
  }

  async objectExists(objectKey: string): Promise<boolean> {
    try {
      await this.headObject(objectKey);
      return true;
    } catch (error) {
      if (isNoSuchObject(error)) return false;
      throw error;
    }
  }

  async headObject(objectKey: string): Promise<{ bytes: number; metadata: Record<string, string> }> {
    const response = await this.#client.send(
      new HeadObjectCommand({ Bucket: this.#bucket, Key: objectKey }),
      { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
    );
    if (response.ContentLength === undefined) {
      throw new Error("R2 did not return object length");
    }
    return { bytes: response.ContentLength, metadata: response.Metadata ?? {} };
  }

  async getObject(objectKey: string): Promise<Readable> {
    const response = await this.#client.send(
      new GetObjectCommand({ Bucket: this.#bucket, Key: objectKey }),
      { abortSignal: AbortSignal.timeout(this.#requestTimeoutMs) },
    );
    if (!response.Body) {
      throw new Error("R2 returned an empty object body");
    }
    return response.Body as Readable;
  }
}

export function storageErrorName(error: unknown): string | undefined {
  if (typeof error !== "object" || error === null) return undefined;
  const candidate = error as { name?: unknown; Code?: unknown; code?: unknown };
  if (typeof candidate.name === "string") return candidate.name;
  if (typeof candidate.Code === "string") return candidate.Code;
  return typeof candidate.code === "string" ? candidate.code : undefined;
}

export function isNoSuchUploadError(error: unknown): boolean {
  return storageErrorName(error) === "NoSuchUpload";
}

export function storageErrorDiagnostics(
  error: unknown,
): Readonly<Record<string, string | number>> {
  if (typeof error !== "object" || error === null) return {};
  const candidate = error as {
    $metadata?: { requestId?: unknown; httpStatusCode?: unknown };
  };
  const diagnostics: Record<string, string | number> = {};
  const name = storageErrorName(error);
  if (name) diagnostics.providerErrorName = name;
  if (typeof candidate.$metadata?.requestId === "string") {
    diagnostics.providerRequestId = candidate.$metadata.requestId;
  }
  if (typeof candidate.$metadata?.httpStatusCode === "number") {
    diagnostics.providerHttpStatus = candidate.$metadata.httpStatusCode;
  }
  return diagnostics;
}

function isNoSuchObject(error: unknown): boolean {
  return ["NotFound", "NoSuchKey"].includes(storageErrorName(error) ?? "");
}
