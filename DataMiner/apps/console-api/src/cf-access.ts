import { createPublicKey, verify as verifySignature, type JsonWebKey, type KeyObject } from "node:crypto";

// Cloudflare Access, once it has authenticated a request at the edge, forwards a
// signed assertion to the origin in the `Cf-Access-Jwt-Assertion` header. It is a
// standard RS256 JWT whose issuer is the team domain and whose audience is the
// Access application's AUD tag. Verifying it — signature, issuer, audience, and
// expiry — lets the console trust the identity without ever seeing a password.
// The signing keys are published as a JWKS at
// `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs` and rotate, so they
// are fetched and cached, and refetched when a token names an unknown key.

export type CfAccessClaims = { email: string };

/** The one method the auth service depends on, so tests can supply a fake. */
export interface CfAccessIdentityVerifier {
  verify(assertion: string): Promise<CfAccessClaims>;
}

type CertsResponse = { keys?: JsonWebKey[] };
export type CertsFetcher = (url: string) => Promise<CertsResponse>;

const defaultFetcher: CertsFetcher = async (url) => {
  const response = await fetch(url, { signal: AbortSignal.timeout(5_000) });
  if (!response.ok) throw new Error(`cf access certs fetch failed with ${response.status}`);
  return (await response.json()) as CertsResponse;
};

export class CfAccessVerifier implements CfAccessIdentityVerifier {
  readonly #issuer: string;
  readonly #certsUrl: string;
  readonly #aud: string;
  readonly #fetcher: CertsFetcher;
  readonly #now: () => number;
  readonly #cacheMs: number;
  readonly #minRefreshMs: number;
  #keys = new Map<string, KeyObject>();
  #fetchedAtMs = 0;

  constructor(options: {
    teamDomain: string;
    aud: string;
    fetcher?: CertsFetcher;
    now?: () => number;
    cacheMs?: number;
    minRefreshMs?: number;
  }) {
    this.#issuer = `https://${options.teamDomain}`;
    this.#certsUrl = `${this.#issuer}/cdn-cgi/access/certs`;
    this.#aud = options.aud;
    this.#fetcher = options.fetcher ?? defaultFetcher;
    this.#now = options.now ?? Date.now;
    this.#cacheMs = options.cacheMs ?? 3_600_000;
    // A bogus `kid` must not let a caller drive an unbounded fetch loop. Only
    // authenticated requests reach here, but this bounds it regardless.
    this.#minRefreshMs = options.minRefreshMs ?? 30_000;
  }

  async verify(assertion: string): Promise<CfAccessClaims> {
    const parts = assertion.split(".");
    if (parts.length !== 3) throw new Error("malformed assertion");
    const [headerB64, payloadB64, signatureB64] = parts;
    const header = decodeSegment(headerB64!);
    if (header.alg !== "RS256") throw new Error("unexpected signing algorithm");
    const kid = typeof header.kid === "string" ? header.kid : "";
    if (!kid) throw new Error("assertion has no key id");

    const key = await this.#keyFor(kid);
    const signed = Buffer.from(`${headerB64}.${payloadB64}`, "utf8");
    const signature = Buffer.from(signatureB64!, "base64url");
    if (!verifySignature("RSA-SHA256", signed, key, signature)) {
      throw new Error("assertion signature did not verify");
    }

    const payload = decodeSegment(payloadB64!);
    const nowSeconds = Math.floor(this.#now() / 1_000);
    if (payload.iss !== this.#issuer) throw new Error("assertion issuer mismatch");
    const audiences = Array.isArray(payload.aud) ? payload.aud : [payload.aud];
    if (!audiences.includes(this.#aud)) throw new Error("assertion audience mismatch");
    if (typeof payload.exp !== "number" || payload.exp <= nowSeconds) throw new Error("assertion expired");
    if (typeof payload.nbf === "number" && payload.nbf > nowSeconds + 60) {
      throw new Error("assertion not yet valid");
    }
    const email = typeof payload.email === "string" ? payload.email.trim().toLowerCase() : "";
    if (!email) throw new Error("assertion carries no email");
    return { email };
  }

  async #keyFor(kid: string): Promise<KeyObject> {
    const fresh = this.#now() - this.#fetchedAtMs <= this.#cacheMs;
    if (this.#keys.has(kid) && fresh) return this.#keys.get(kid)!;
    // Refresh when the cache is empty, stale, or names an unknown key — but not
    // more often than the floor, so an unknown kid cannot hammer the JWKS.
    if (this.#keys.size === 0 || !fresh || this.#now() - this.#fetchedAtMs > this.#minRefreshMs) {
      await this.#refresh();
    }
    const key = this.#keys.get(kid);
    if (!key) throw new Error("assertion signed by an unknown key");
    return key;
  }

  async #refresh(): Promise<void> {
    const certs = await this.#fetcher(this.#certsUrl);
    const next = new Map<string, KeyObject>();
    for (const jwk of certs.keys ?? []) {
      const kid = (jwk as { kid?: unknown }).kid;
      if (jwk.kty !== "RSA" || typeof kid !== "string") continue;
      try {
        next.set(kid, createPublicKey({ key: jwk, format: "jwk" }));
      } catch {
        // A single malformed key must not sink the whole set.
      }
    }
    this.#keys = next;
    this.#fetchedAtMs = this.#now();
  }
}

function decodeSegment(segment: string): Record<string, unknown> {
  const decoded = JSON.parse(Buffer.from(segment, "base64url").toString("utf8")) as unknown;
  if (typeof decoded !== "object" || decoded === null) throw new Error("assertion segment is not an object");
  return decoded as Record<string, unknown>;
}
