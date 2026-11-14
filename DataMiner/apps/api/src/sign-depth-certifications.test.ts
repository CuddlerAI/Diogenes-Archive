import { createPublicKey, verify } from "node:crypto";
import { describe, expect, it } from "vitest";

import { createSignedDepthCertificationEnvelope } from "./sign-depth-certifications.js";
import {
  DEPTH_CERTIFICATION_SIGNATURE_DOMAIN,
  devSigningPublicKeyHex,
} from "./signed-catalog.js";

function manifest(): Buffer {
  return Buffer.from(
    JSON.stringify({
      schema: "dm-depth-performance-certifications-v1",
      approvedRuntimes: [],
      certifications: [],
    }),
  );
}

describe("Depth certification signing", () => {
  it("signs exact manifest bytes with the distinct delivery domain", () => {
    const bytes = manifest();
    const envelope = JSON.parse(createSignedDepthCertificationEnvelope(bytes).toString("utf8")) as {
      schemaVersion: string;
      signingKeyId: string;
      signature: string;
      manifest: string;
    };
    const publicKeyDer = Buffer.concat([
      Buffer.from("302a300506032b6570032100", "hex"),
      Buffer.from(devSigningPublicKeyHex(), "hex"),
    ]);
    const publicKey = createPublicKey({ key: publicKeyDer, format: "der", type: "spki" });
    expect(envelope.schemaVersion).toBe("dm-signed-depth-certifications-1");
    expect(Buffer.from(envelope.manifest, "base64")).toEqual(bytes);
    expect(
      verify(
        null,
        Buffer.concat([DEPTH_CERTIFICATION_SIGNATURE_DOMAIN, bytes]),
        publicKey,
        Buffer.from(envelope.signature, "base64"),
      ),
    ).toBe(true);
  });

  it.each(["v2", "v3"])("signs %s profile grants without changing v1 envelope payloads", (version) => {
    const original = manifest();
    const v1 = createSignedDepthCertificationEnvelope(original);
    const payload = JSON.parse(original.toString("utf8")) as Record<string, unknown>;
    payload.schema = `dm-depth-performance-certifications-${version}`;
    payload.deviceProfileGrants = [];
    const bytes = Buffer.from(JSON.stringify(payload));
    const envelope = JSON.parse(createSignedDepthCertificationEnvelope(bytes).toString("utf8")) as { manifest: string };
    expect(Buffer.from(envelope.manifest, "base64")).toEqual(bytes);
    expect(createSignedDepthCertificationEnvelope(original)).toEqual(v1);
    payload.schema = "dm-depth-performance-certifications-v1";
    expect(() => createSignedDepthCertificationEnvelope(Buffer.from(JSON.stringify(payload)))).toThrow(/unexpected root fields/u);
  });

  it("rejects malformed roots before signing", () => {
    expect(() =>
      createSignedDepthCertificationEnvelope(
        Buffer.from('{"schema":"dm-depth-performance-certifications-v1"}'),
      ),
    ).toThrow(/unexpected root fields/u);
  });
});
