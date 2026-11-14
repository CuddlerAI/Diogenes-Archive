import { describe, expect, it } from "vitest";

import { clientAddress, readCookie, sessionCookieName } from "./http.js";

describe("cookie parsing", () => {
  it("finds the session cookie among others", () => {
    const header = `theme=dark; ${sessionCookieName}=abc123; locale=zh-CN`;

    expect(readCookie(header, sessionCookieName)).toBe("abc123");
  });

  it("does not match a cookie whose name merely ends with the target", () => {
    const header = `not-${sessionCookieName}=decoy`;

    expect(readCookie(header, sessionCookieName)).toBeNull();
  });

  it("returns null when the header is absent or the cookie is missing", () => {
    expect(readCookie(undefined, sessionCookieName)).toBeNull();
    expect(readCookie("theme=dark", sessionCookieName)).toBeNull();
  });

  it("decodes a percent-encoded value", () => {
    expect(readCookie(`${sessionCookieName}=a%2Bb`, sessionCookieName)).toBe("a+b");
  });
});

/**
 * Requests that traversed the Cloudflare Worker sit one hop further out than
 * direct hits, and `trustProxy` is process-wide. The extra hop is attributed
 * by evidence — the Worker attaches the `x-console-proxy-token` shared secret
 * to everything it forwards (both `/console/` and `/app/`) and strips any
 * caller-supplied value — never by path: a path rule could be aimed by the
 * caller at whichever depth reads their forged entry. These pin the properties
 * that matter: proof-carrying requests read past the Worker's own egress
 * address, everything else stays on `request.ip`, and no arrangement of
 * caller-controlled headers ever reads a prepended entry.
 */
const workerSecret = "w".repeat(48);

function fakeRequest(
  url: string,
  forwardedFor: string | undefined,
  ip: string,
  token?: string,
) {
  return {
    url,
    ip,
    headers: {
      ...(forwardedFor === undefined ? {} : { "x-forwarded-for": forwardedFor }),
      ...(token === undefined ? {} : { "x-console-proxy-token": token }),
    },
  } as unknown as Parameters<typeof clientAddress>[0];
}

describe("clientAddress", () => {
  // Browser → Cloudflare (appends browser) → Worker → Google front end
  // (appends the Worker's egress). request.ip with 1 trusted hop is that
  // egress address, which varies per request across Cloudflare's fleet.
  const consoleChain = "203.0.113.7, 172.71.90.1";

  it("reads past the Worker egress when the request carries the Worker's proof", () => {
    const console_ = fakeRequest("/console/v1/auth/login", consoleChain, "172.71.90.1", workerSecret);
    expect(clientAddress(console_, 1, workerSecret)).toBe("203.0.113.7");

    // Same for the desktop surface: reached via the console hostname it is
    // exactly as deep, and keying its rate limit on the per-request Worker
    // egress would hand out a fresh bucket every attempt.
    const app = fakeRequest("/app/v1/login", consoleChain, "172.71.90.1", workerSecret);
    expect(clientAddress(app, 1, workerSecret)).toBe("203.0.113.7");
  });

  it("leaves direct hits on request.ip, whatever the path says", () => {
    // Google's front end appends the real client; 1 hop already resolves it.
    expect(clientAddress(fakeRequest("/app/v1/login", "203.0.113.7", "203.0.113.7"), 1, workerSecret)).toBe(
      "203.0.113.7",
    );
    expect(clientAddress(fakeRequest("/console/v1/x", "203.0.113.7", "203.0.113.7"), 1, workerSecret)).toBe(
      "203.0.113.7",
    );
  });

  it("does not let a self-attached token buy the deeper read", () => {
    // Direct hit on the Cloud Run URL, prepended entry, wrong token: stepping
    // left would read the forgery. The Worker strips inbound tokens, so a
    // matching value can only have come from the Worker itself.
    const forgedDirect = fakeRequest("/app/v1/login", "evil.example, 203.0.113.7", "203.0.113.7", "guess");
    expect(clientAddress(forgedDirect, 1, workerSecret)).toBe("203.0.113.7");
  });

  it("ignores entries a caller prepended ahead of the Worker", () => {
    const forged = `evil.example, ${consoleChain}`;
    expect(
      clientAddress(fakeRequest("/console/v1/auth/login", forged, "172.71.90.1", workerSecret), 1, workerSecret),
    ).toBe("203.0.113.7");
  });

  it("tracks the configured hop count rather than assuming one", () => {
    const deeper = `203.0.113.7, 172.71.90.1, 10.0.0.1`;
    expect(
      clientAddress(fakeRequest("/console/v1/x", deeper, "10.0.0.1", workerSecret), 2, workerSecret),
    ).toBe("203.0.113.7");
  });

  it("falls back to request.ip when the chain is shorter than the trusted depth", () => {
    expect(
      clientAddress(fakeRequest("/console/v1/x", undefined, "10.0.0.1", workerSecret), 1, workerSecret),
    ).toBe("10.0.0.1");
    expect(
      clientAddress(fakeRequest("/console/v1/x", "203.0.113.7", "203.0.113.7", workerSecret), 1, workerSecret),
    ).toBe("203.0.113.7");
  });

  it("never steps left when no Worker secret is configured", () => {
    // Local development runs without a Worker; a token header is meaningless.
    expect(
      clientAddress(fakeRequest("/console/v1/x", consoleChain, "172.71.90.1", workerSecret), 1, null),
    ).toBe("172.71.90.1");
  });
});
