import type { FastifyReply, FastifyRequest } from "fastify";

import { badRequest, forbidden } from "./errors.js";
import { secretEquals } from "./secrets.js";

export const sessionCookieName = "__Host-dmc_session";
const csrfHeaderName = "x-dmc-csrf";

// The annotator web surface keeps its own cookie and its own CSRF header
// (docs/34 §6.1). Two names, not one: an operator and an annotator session in
// the same browser must never be mistaken for each other, and "the admin is
// also an annotator" makes that browser a normal case, not an edge case.
export const appSessionCookieName = "__Host-dma_session";
export const appCsrfHeaderName = "x-dma-csrf";

export function readCookie(header: string | undefined, name: string): string | null {
  if (!header) return null;
  for (const part of header.split(";")) {
    const separator = part.indexOf("=");
    if (separator < 0) continue;
    if (part.slice(0, separator).trim() !== name) continue;
    return decodeURIComponent(part.slice(separator + 1).trim());
  }
  return null;
}

export function setSessionCookie(reply: FastifyReply, token: string, maxAgeSeconds: number): void {
  reply.header(
    "set-cookie",
    `${sessionCookieName}=${encodeURIComponent(token)}; Max-Age=${maxAgeSeconds}; Path=/; Secure; HttpOnly; SameSite=Strict`,
  );
}

export function clearSessionCookie(reply: FastifyReply): void {
  reply.header(
    "set-cookie",
    `${sessionCookieName}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict`,
  );
}

export function setAppSessionCookie(reply: FastifyReply, token: string, maxAgeSeconds: number): void {
  reply.header(
    "set-cookie",
    `${appSessionCookieName}=${encodeURIComponent(token)}; Max-Age=${maxAgeSeconds}; Path=/; Secure; HttpOnly; SameSite=Strict`,
  );
}

export function clearAppSessionCookie(reply: FastifyReply): void {
  reply.header(
    "set-cookie",
    `${appSessionCookieName}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict`,
  );
}

// SameSite=Strict already blocks cross-site cookie delivery. The header and
// origin checks are the second and third layers: a custom header cannot be set
// by a simple cross-origin form post, and a mismatched Origin is rejected
// outright rather than trusted.
export function assertSameOriginWrite(
  request: FastifyRequest,
  publicOrigin: string,
  headerName: string = csrfHeaderName,
): void {
  if (request.method === "GET" || request.method === "HEAD" || request.method === "OPTIONS") return;

  if (request.headers[headerName] !== "1") {
    throw forbidden("csrf_header_missing", "state changing requests must send the console CSRF header");
  }

  const fetchSite = request.headers["sec-fetch-site"];
  if (typeof fetchSite === "string" && fetchSite !== "same-origin" && fetchSite !== "none") {
    throw forbidden("cross_site_request", "state changing requests must originate from the console");
  }

  const origin = request.headers.origin;
  if (typeof origin === "string" && origin !== publicOrigin) {
    throw forbidden("cross_site_request", "state changing requests must originate from the console");
  }
}

/**
 * The public `/app/v1` writes are shared by two very different callers: the
 * desktop client (no cookie jar, no browser headers) and the annotator web
 * pages (a browser with ambient state worth protecting). A mandatory CSRF
 * header would break every deployed desktop client, so the discriminator is
 * evidence the caller is a browser: `Sec-Fetch-Site` / `Origin` are attached
 * by browsers and not by reqwest. When they are present they must agree with
 * our origin; when absent the caller is not a browser and there is no ambient
 * credential to protect (docs/34 §6.1).
 */
export function assertBrowserSameOriginWrite(request: FastifyRequest, publicOrigin: string): void {
  if (request.method === "GET" || request.method === "HEAD" || request.method === "OPTIONS") return;

  const fetchSite = request.headers["sec-fetch-site"];
  if (typeof fetchSite === "string" && fetchSite !== "same-origin" && fetchSite !== "none") {
    throw forbidden("cross_site_request", "state changing requests must originate from the console");
  }

  const origin = request.headers.origin;
  if (typeof origin === "string" && origin !== publicOrigin) {
    throw forbidden("cross_site_request", "state changing requests must originate from the console");
  }
}

/**
 * The client address for this request.
 *
 * Counting from the *right* is the invariant here. `/app/v1` is reached
 * directly by the desktop client and Google's front end appends to
 * X-Forwarded-For rather than replacing it, so a caller can always prepend
 * entries of its choosing. An earlier cut took the leftmost entry and therefore
 * read exactly those: every request got a fresh per-IP rate limit bucket, and
 * the forged value landed in `annotator_invite_uses.ip_hash` and the audit
 * trail. Walking from the right skips only hops we own, so prepended entries
 * are never read.
 *
 * What `request.ip` alone cannot express is that requests sit at different
 * depths. `trustProxy` is process-wide, and it is set for the direct surface
 * (`CONSOLE_TRUSTED_PROXY_HOPS`, default 1 = Cloud Run reached directly).
 * Anything that arrives through the Cloudflare Worker — `/console/v1` always,
 * `/app/v1` whenever the desktop client uses the console hostname — is one hop
 * further out, so `request.ip` resolves to the Worker's egress address, which
 * belongs to Cloudflare's fleet and differs per request. Observed in
 * production: two console logins two seconds apart recorded two different
 * `ipHash` values, meaning per-IP limits were keyed on a value that never
 * repeats (each attempt got a fresh bucket) and the audit ipHash identified
 * nobody.
 *
 * The extra hop is attributed by *evidence*, not by path: the Worker attaches
 * the `x-console-proxy-token` shared secret to every request it forwards (both
 * prefixes) and strips any caller-supplied value, so a matching token is proof
 * the request traversed our Worker. Deciding by path instead would let a
 * caller pick whichever depth suits them — a direct `/app/v1` hit on the Cloud
 * Run URL with one prepended X-Forwarded-For entry would satisfy a path rule
 * that expects Worker depth and read the forged entry; conversely a
 * through-Worker `/app/v1` request under a path rule pinned to `/console/v1`
 * would key its rate limit on the Worker egress and get a fresh bucket per
 * request. With the token as the discriminator, requests with proof step one
 * entry further left (still counted from the right), and everything else keeps
 * `request.ip`.
 */
export function clientAddress(
  request: FastifyRequest,
  trustedProxyHops: number,
  proxyToken: string | null,
): string {
  const presented = request.headers["x-console-proxy-token"];
  const throughWorker =
    proxyToken !== null && typeof presented === "string" && secretEquals(presented, proxyToken);
  if (!throughWorker) return request.ip;
  const chain = forwardedChain(request);
  // With n hops trusted from the right, entry `length - n` is the address the
  // outermost trusted proxy reported. The Worker owns one hop more than the
  // process-wide setting, so its client is one position further left.
  const index = chain.length - trustedProxyHops - 1;
  // Short chain means the request did not traverse the proxy depth we expect.
  // Fall back rather than reach past the left edge and read an entry the
  // caller could have prepended.
  return index >= 0 ? chain[index]! : request.ip;
}

/** X-Forwarded-For as a left-to-right list; each proxy appends its own client. */
function forwardedChain(request: FastifyRequest): string[] {
  const header = request.headers["x-forwarded-for"];
  const raw = Array.isArray(header) ? header.join(",") : header;
  if (typeof raw !== "string") return [];
  return raw
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

export function truncatedUserAgent(request: FastifyRequest): string | null {
  const agent = request.headers["user-agent"];
  if (typeof agent !== "string" || agent.length === 0) return null;
  return agent.slice(0, 256);
}

export function requireJsonBody(request: FastifyRequest): void {
  const contentType = request.headers["content-type"];
  if (typeof contentType !== "string" || !contentType.toLowerCase().startsWith("application/json")) {
    throw badRequest("unsupported_media_type", "request body must be application/json");
  }
}
