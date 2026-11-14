import { describe, expect, it } from "vitest";

import { R2ObjectStorage } from "./storage.js";

const config = {
  accountId: "00000000000000000000000000000000",
  bucket: "test-bucket",
  accessKeyId: "test-access-key",
  secretAccessKey: "test-secret-key",
  prefix: "test-prefix",
  presignTtlSeconds: 900,
  partSizeBytes: 8 * 1024 * 1024,
  requestTimeoutSeconds: 120,
};

describe("R2ObjectStorage presigning", () => {
  it("keeps the Content-MD5 contract free of SDK-generated zero CRC32 checksums", async () => {
    const storage = new R2ObjectStorage(config);
    const contentMd5 = "1B2M2Y8AsgTpgAmY7PhCfg==";
    const signed = await storage.signUploadPart({
      objectKey: "raw/test.bin",
      providerUploadId: "upload-id",
      partNumber: 1,
      bytes: 123,
      contentMd5,
    });
    const url = new URL(signed.url);

    expect(url.searchParams.has("x-amz-checksum-crc32")).toBe(false);
    expect(url.searchParams.has("x-amz-sdk-checksum-algorithm")).toBe(false);
    expect(url.searchParams.get("X-Amz-SignedHeaders")).toBe(
      "content-length;content-md5;host",
    );
    expect(signed.headers).toEqual({
      "content-md5": contentMd5,
      "content-length": "123",
    });
  });
});
