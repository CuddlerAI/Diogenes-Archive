type Env = {
  ASSETS: { fetch: (request: Request) => Promise<Response> };
  CONSOLE_API_ORIGIN: string;
  CONSOLE_PROXY_TOKEN?: string;
};

// Console operators and the desktop account client share one stable production
// hostname. Everything outside these API prefixes is a static console asset,
// and unknown paths fall through to the SPA entry point via not_found_handling.
const consoleApiPrefix = "/console/";
const desktopApiPrefix = "/app/";

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    const isApi =
      url.pathname.startsWith(consoleApiPrefix) || url.pathname.startsWith(desktopApiPrefix);
    if (!isApi) {
      const response = await env.ASSETS.fetch(request);
      return withSecurityHeaders(response, url.pathname);
    }

    const target = new URL(url.pathname + url.search, env.CONSOLE_API_ORIGIN);
    const headers = new Headers(request.headers);
    headers.set("host", target.host);
    // A caller-supplied value must never reach the origin: the header is both
    // the console 404-gate credential (docs/11 §4.3) and, on either prefix, the
    // origin's only trustworthy signal that this request traversed our Worker
    // and therefore sits one X-Forwarded-For hop deeper (http.ts
    // clientAddress). It is a Worker↔origin secret; browsers and the desktop
    // client never see it in either direction.
    headers.delete("x-console-proxy-token");
    if (env.CONSOLE_PROXY_TOKEN) headers.set("x-console-proxy-token", env.CONSOLE_PROXY_TOKEN);

    return fetch(
      new Request(target, {
        method: request.method,
        headers,
        body: request.method === "GET" || request.method === "HEAD" ? null : request.body,
        redirect: "manual",
      }),
    );
  },
};

function withSecurityHeaders(response: Response, pathname: string): Response {
  const headers = new Headers(response.headers);
  headers.set("x-content-type-options", "nosniff");
  headers.set("referrer-policy", "no-referrer");
  headers.set("x-frame-options", "DENY");
  headers.set("permissions-policy", "camera=(), microphone=(), geolocation=()");
  // The console is an internal tool and must never be indexed.
  headers.set("x-robots-tag", "noindex, nofollow");
  headers.set(
    "content-security-policy",
    [
      "default-src 'self'",
      "script-src 'self'",
      "style-src 'self' 'unsafe-inline'",
      "img-src 'self' data:",
      // Clip preview plays a presigned R2 object, which is cross-origin by
      // construction (preview.ts signs https://<account>.r2.cloudflarestorage.com).
      // Without this the <video> falls back to default-src 'self' and the player
      // stays black while the API happily signs the URL and writes the audit.
      "media-src 'self' https://*.r2.cloudflarestorage.com",
      "connect-src 'self'",
      "frame-ancestors 'none'",
      "base-uri 'none'",
      "form-action 'none'",
    ].join("; "),
  );
  // Hashed bundles may be cached hard; index.html must not be.
  headers.set(
    "cache-control",
    pathname.startsWith("/assets/") ? "public, max-age=31536000, immutable" : "no-store",
  );
  return new Response(response.body, { status: response.status, headers });
}
