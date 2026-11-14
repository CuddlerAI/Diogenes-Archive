/**
 * api.diogenesmodel.com — the public edge in front of the ingestion API.
 *
 * Why this exists at all. The ingestion API runs on Cloud Run, and its callers
 * are desktop clients holding a per-device bearer token (docs/11 §10), not
 * Google identities. `--no-allow-unauthenticated` would therefore lock out the
 * entire fleet the API exists to serve, so the origin admits `allUsers`. That
 * leaves the bare `*.run.app` URL reachable, which this Worker answers by being
 * the only party that knows `DATAMINER_PROXY_TOKEN`: the API answers `/v1/*`
 * with 404 unless the header matches (apps/api/src/app.ts). A mismatch takes
 * the whole ingestion path down rather than degrading quietly, which is the
 * intended failure mode — the same contract the console uses (docs/11 §4.3).
 *
 * Everything else here is deliberately thin. Bytes never pass through this
 * Worker: clients PUT clip parts straight to R2 with presigned URLs, so the
 * only traffic is small control-plane JSON.
 */
type Env = {
  API_ORIGIN: string;
  DATAMINER_PROXY_TOKEN?: string;
};

// Hop-by-hop headers plus the ones Cloudflare adds that must not be forwarded
// as if the client had sent them. `host` is set from the target below.
const strippedRequestHeaders = [
  "host",
  "connection",
  "keep-alive",
  "transfer-encoding",
  "upgrade",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
];

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const target = new URL(url.pathname + url.search, env.API_ORIGIN);

    const headers = new Headers(request.headers);
    for (const name of strippedRequestHeaders) headers.delete(name);
    headers.set("host", target.host);

    // Unconditional set, never append: a client that supplies its own
    // x-dataminer-proxy-token must not be able to influence what the origin
    // sees, and a deployment with no token configured must not forward a
    // caller-supplied one to an origin that would then trust it.
    headers.delete("x-dataminer-proxy-token");
    if (env.DATAMINER_PROXY_TOKEN) {
      headers.set("x-dataminer-proxy-token", env.DATAMINER_PROXY_TOKEN);
    }

    const response = await fetch(
      new Request(target, {
        method: request.method,
        headers,
        body: request.method === "GET" || request.method === "HEAD" ? null : request.body,
        // The API never redirects. Following one here would replay the request,
        // proxy token and device credential included, at whatever it pointed to.
        redirect: "manual",
      }),
    );

    return withSecurityHeaders(response);
  },
};

function withSecurityHeaders(response: Response): Response {
  const headers = new Headers(response.headers);
  headers.set("x-content-type-options", "nosniff");
  headers.set("referrer-policy", "no-referrer");
  // A JSON API is never a document, never framed, and never indexed.
  headers.set("x-frame-options", "DENY");
  headers.set("x-robots-tag", "noindex, nofollow");
  headers.set("content-security-policy", "default-src 'none'; frame-ancestors 'none'");
  // Idempotency and presigned-URL responses are per-caller and short-lived;
  // nothing this API returns may sit in a shared cache.
  headers.set("cache-control", "no-store");
  return new Response(response.body, { status: response.status, headers });
}
