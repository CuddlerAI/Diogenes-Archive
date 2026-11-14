import { generateKeyPairSync, sign as signData, type JsonWebKey } from "node:crypto";

import { describe, expect, it } from "vitest";

import { CfAccessVerifier, type CertsFetcher } from "./cf-access.js";

const TEAM = "hakkoai.cloudflareaccess.com";
const ISSUER = `https://${TEAM}`;
const AUD = "a".repeat(64);
const KID = "test-key-1";
const NOW_MS = Date.parse("2026-07-25T00:00:00.000Z");
const now = () => NOW_MS;

const { publicKey, privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const jwk: JsonWebKey = { ...publicKey.export({ format: "jwk" }), kid: KID, alg: "RS256", use: "sig" };
const certs: CertsFetcher = async () => ({ keys: [jwk] });

function b64url(value: unknown): string {
  return Buffer.from(JSON.stringify(value)).toString("base64url");
}

function makeToken(
  payload: Record<string, unknown>,
  options: { kid?: string; alg?: string; key?: typeof privateKey } = {},
): string {
  const header = { alg: options.alg ?? "RS256", kid: options.kid ?? KID, typ: "JWT" };
  const signingInput = `${b64url(header)}.${b64url(payload)}`;
  const signature = signData("RSA-SHA256", Buffer.from(signingInput, "utf8"), options.key ?? privateKey).toString(
    "base64url",
  );
  return `${signingInput}.${signature}`;
}

function goodPayload(overrides: Record<string, unknown> = {}) {
  return {
    iss: ISSUER,
    aud: AUD,
    email: "Ops@Diogenesmodel.com",
    exp: Math.floor(NOW_MS / 1_000) + 600,
    ...overrides,
  };
}

function verifier(fetcher: CertsFetcher = certs) {
  return new CfAccessVerifier({ teamDomain: TEAM, aud: AUD, fetcher, now });
}

describe("cf access verifier", () => {
  it("accepts a well-formed assertion and returns the lowercased email", async () => {
    const claims = await verifier().verify(makeToken(goodPayload()));
    expect(claims.email).toBe("ops@diogenesmodel.com");
  });

  it("accepts an audience array that contains the configured aud", async () => {
    const claims = await verifier().verify(makeToken(goodPayload({ aud: ["x".repeat(64), AUD] })));
    expect(claims.email).toBe("ops@diogenesmodel.com");
  });

  it("rejects a wrong audience", async () => {
    await expect(verifier().verify(makeToken(goodPayload({ aud: "b".repeat(64) })))).rejects.toThrow();
  });

  it("rejects a wrong issuer", async () => {
    await expect(
      verifier().verify(makeToken(goodPayload({ iss: "https://evil.cloudflareaccess.com" }))),
    ).rejects.toThrow();
  });

  it("rejects an expired assertion", async () => {
    await expect(
      verifier().verify(makeToken(goodPayload({ exp: Math.floor(NOW_MS / 1_000) - 1 }))),
    ).rejects.toThrow();
  });

  it("rejects a tampered payload carrying the original signature", async () => {
    const [header, , signature] = makeToken(goodPayload()).split(".");
    const forged = b64url(goodPayload({ email: "attacker@evil.com" }));
    await expect(verifier().verify(`${header}.${forged}.${signature}`)).rejects.toThrow();
  });

  it("rejects an assertion signed by a different key", async () => {
    const other = generateKeyPairSync("rsa", { modulusLength: 2048 }).privateKey;
    await expect(verifier().verify(makeToken(goodPayload(), { key: other }))).rejects.toThrow();
  });

  it("rejects an unknown key id", async () => {
    await expect(verifier().verify(makeToken(goodPayload(), { kid: "unknown" }))).rejects.toThrow();
  });

  it("rejects a non-RS256 algorithm", async () => {
    await expect(verifier().verify(makeToken(goodPayload(), { alg: "none" }))).rejects.toThrow();
  });

  it("rejects a malformed assertion", async () => {
    await expect(verifier().verify("garbage")).rejects.toThrow();
    await expect(verifier().verify("only.two")).rejects.toThrow();
  });

  it("refetches the key set when a token names a key seen after rotation", async () => {
    // First fetch returns no usable key, the second (after the min-refresh floor)
    // returns the real one — the verifier must pick it up rather than cache the miss.
    let call = 0;
    const rotating: CertsFetcher = async () => (++call === 1 ? { keys: [] } : { keys: [jwk] });
    const rotated = new CfAccessVerifier({
      teamDomain: TEAM,
      aud: AUD,
      fetcher: rotating,
      now,
      minRefreshMs: 0,
    });
    await expect(rotated.verify(makeToken(goodPayload()))).rejects.toThrow(); // first fetch: empty
    const claims = await rotated.verify(makeToken(goodPayload())); // second fetch: key present
    expect(claims.email).toBe("ops@diogenesmodel.com");
  });
});
