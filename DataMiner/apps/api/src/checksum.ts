import { createHash } from "node:crypto";
import type { Readable } from "node:stream";

export const SHA256_PATTERN = /^sha256:[a-f0-9]{64}$/;

export function sha256Bytes(bytes: Uint8Array | string): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

export function stableJsonSha256(value: unknown): string {
  return sha256Bytes(stableJson(value));
}

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map((item) => stableJson(item)).join(",")}]`;
  }
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${stableJson(record[key])}`)
    .join(",")}}`;
}

export async function sha256Stream(stream: Readable): Promise<{ sha256: string; bytes: bigint }> {
  const hash = createHash("sha256");
  let bytes = 0n;
  for await (const chunk of stream) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk as Uint8Array);
    hash.update(buffer);
    bytes += BigInt(buffer.length);
  }
  return { sha256: `sha256:${hash.digest("hex")}`, bytes };
}
