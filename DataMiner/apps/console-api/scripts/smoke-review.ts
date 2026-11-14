/**
 * Real-bucket smoke test for the data-review surface (docs/33 M0/M1 exit
 * criteria). Drives ReviewService directly — no HTTP, no auth, no database —
 * against a real R2 bucket with read-only credentials from the environment:
 *
 *   CONSOLE_REVIEW_R2_ACCOUNT_ID / _ACCESS_KEY_ID / _SECRET_ACCESS_KEY
 *   CONSOLE_REVIEW_SOURCES        (same JSON as production)
 *   SMOKE_SOURCE                  (source id to exercise, default raw-prod)
 *   SMOKE_PREFIX                  (optional: a prefix to walk instead of "")
 *
 * Read-only by construction: the service has no write commands at all.
 * Verifies, in order: tree roots → first walkable directory chain → a segment
 * or chunk directory → frames adapter (decompress + integrity checks) → meta →
 * video presign + a 1-byte Range fetch (does NOT download the video).
 *
 *   pnpm tsx scripts/smoke-review.ts
 */
import { loadConfig } from "../src/config.js";
import { R2ReviewStorage, ReviewService, type ReviewTreePage } from "../src/review.js";
import type { SqlClient } from "../src/sql.js";

const nullSql: SqlClient = {
  async query<T>() {
    return { rows: [] as T[] };
  },
};

const config = loadConfig({
  CONSOLE_ENV: "development",
  DATABASE_URL: "postgres://smoke:smoke@127.0.0.1:5432/smoke",
  CONSOLE_PUBLIC_ORIGIN: "http://localhost:5174",
  CONSOLE_IP_HASH_SALT: "smoke-".padEnd(48, "x"),
  CONSOLE_MAIL_MODE: "capture",
  CONSOLE_REVIEW_R2_ACCOUNT_ID: process.env.CONSOLE_REVIEW_R2_ACCOUNT_ID,
  CONSOLE_REVIEW_R2_ACCESS_KEY_ID: process.env.CONSOLE_REVIEW_R2_ACCESS_KEY_ID,
  CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY: process.env.CONSOLE_REVIEW_R2_SECRET_ACCESS_KEY,
  CONSOLE_REVIEW_SOURCES: process.env.CONSOLE_REVIEW_SOURCES,
});
if (!config.review) {
  console.error("set CONSOLE_REVIEW_R2_* and CONSOLE_REVIEW_SOURCES first");
  process.exit(2);
}

const service = new ReviewService(nullSql, new R2ReviewStorage(config.review), config);
const sourceId = process.env.SMOKE_SOURCE ?? "raw-prod";

let failures = 0;
function check(label: string, ok: boolean, detail = ""): void {
  console.log(`${ok ? "✓" : "✗"} ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

// Walk: bounded depth-first search for a directory that holds an .mp4 — a
// segment (RAW) or chunk (dm-1) directory in either layout. Session roots hold
// objects too (journal, session.json), so "has objects" is not a stop signal.
async function descend(prefix: string): Promise<ReviewTreePage | null> {
  let budget = 60;
  const stack = [prefix];
  while (stack.length > 0 && budget > 0) {
    budget -= 1;
    const page = await service.tree(sourceId, stack.shift()!);
    if (page.objects.some((o) => o.key.endsWith(".mp4"))) return page;
    stack.unshift(...page.directories);
  }
  return null;
}

const roots = await service.tree(sourceId, "");
check("tree('') answers from configuration", roots.directories.length > 0, roots.directories.join(" "));

const start = process.env.SMOKE_PREFIX ?? roots.directories[0]!;
const leaf = await descend(start);
check("descended to an object-bearing directory", leaf !== null, leaf?.prefix ?? "(none)");
if (!leaf) {
  console.error("nothing to smoke below", start);
  process.exit(1);
}
for (const object of leaf.objects) {
  console.log(`   · ${object.key} (${object.sizeBytes} B)`);
}

const framesKey = leaf.objects.find(
  (o) => o.key.endsWith("frames.jsonl.zst") || o.key.endsWith("p0.jsonl"),
)?.key;
if (framesKey) {
  const frames = await service.frames(sourceId, framesKey);
  check(
    `frames adapter (${frames.schema})`,
    frames.rows.length > 0,
    `${frames.rows.length} rows, fps=${frames.fps}, sha256Verified=${frames.provenance.sha256Verified}, frameCountVerified=${frames.provenance.frameCountVerified}`,
  );
  const first = frames.rows[0] as Record<string, unknown>;
  check(
    "row shape has keys/pointer fields",
    Array.isArray(first.keys) && "pointerDx" in first,
    JSON.stringify(Object.keys(first)),
  );
} else {
  check("frames object present in leaf directory", false);
}

const metaKey = leaf.objects.find((o) => o.key.endsWith("meta.json"))?.key;
if (metaKey) {
  const meta = await service.meta(sourceId, metaKey);
  check("meta passthrough", meta.meta !== null, `${meta.sizeBytes} B`);
}

const videoKey = leaf.objects.find((o) => o.key.endsWith(".mp4"))?.key;
if (videoKey) {
  const grant = await service.signVideo(sourceId, videoKey);
  check("video presigned", grant.url.includes("X-Amz-Signature"), `${grant.sizeBytes} B, ttl ${grant.expiresInSeconds}s`);
  // One byte via Range — playback viability without pulling the file (the
  // audit measured single large-object downloads at ~5KB/s on this network).
  const head = await fetch(grant.url, { headers: { range: "bytes=0-0" } });
  check("Range fetch on presigned URL", head.status === 206, `HTTP ${head.status}`);
  // The tail carries the moov box on these non-faststart MP4s; a tail Range
  // succeeding is what <video> seeking depends on.
  const tail = await fetch(grant.url, { headers: { range: "bytes=-512" } });
  check("tail Range fetch (moov reachability)", tail.status === 206, `HTTP ${tail.status}`);
} else {
  check("video object present in leaf directory", false);
}

console.log(failures === 0 ? "\nSMOKE PASS" : `\nSMOKE FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
