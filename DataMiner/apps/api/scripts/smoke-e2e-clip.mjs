/**
 * Server-side synthetic-client e2e smoke: drives the FULL production ingestion
 * contract against a deployed API, exactly as the Windows client would, using the
 * pre-provisioned synthetic device + catalog:
 *
 *   POST /v1/sessions -> PUT run -> PUT clip -> POST upload-intents
 *   -> POST parts:sign -> PUT bytes to R2 (presigned) -> POST complete
 *   -> GET clip status
 *
 * Then run the worker to verify (streams from R2, recomputes SHA-256).
 *
 * Env:
 *   API_BASE_URL   deployed ingestion API (e.g. https://dataminer-api-e2e-...run.app)
 *   DEVICE_TOKEN   provisioned device bearer token (provision-e2e-device.mjs)
 *   SYNTHETIC_DIR  path to config/catalogs/dev/synthetic (for catalog/game hashes)
 */
import { createHash, randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";

const API = process.env.API_BASE_URL?.replace(/\/$/, "");
const TOKEN = process.env.DEVICE_TOKEN;
const SYN = process.env.SYNTHETIC_DIR;
if (!API || !TOKEN || !SYN) {
  console.error("API_BASE_URL, DEVICE_TOKEN and SYNTHETIC_DIR are required");
  process.exit(2);
}

const sha = (buf) => `sha256:${createHash("sha256").update(buf).digest("hex")}`;
const md5b64 = (buf) => createHash("md5").update(buf).digest("base64");
const fileSha = (p) => sha(readFileSync(p));

// ---- identities (must match provisioned rows / synthetic catalog) ----
const CATALOG = { id: "dataminer-synthetic-dev", version: "2026-07-23.1", mode: "dev_non_trainable", contentHash: fileSha(path.join(SYN, "catalog.json")) };
const GAME = { id: "synthetic-win32-dev", version: "1", contentHash: fileSha(path.join(SYN, "game-profile.json")), executableSha256: sha(Buffer.from("capture-lab.exe synthetic")) };
const DEVICE_PROFILE = { id: "synthetic-windows-nvidia-dev-v1", version: "1", contentHash: "sha256:208810e4a10d57d25510799539f5fafa2fd924bafec5788bfe40624425fa8460" };
const CAPTURE_PROFILE = { id: "synthetic-win32-dev-v1", version: "1", contentHash: "sha256:66cb5bb2f593e8c4b5ae84f45b7beb08d405bfdda38ec9856054ab371b903954", width: 1920, height: 1080, fpsNum: 30, fpsDen: 1, captureEngine: "native_wgc_window_v1" };
const SETTINGS_PROFILE = { id: "synthetic-native-sdr-v1", version: "1", contentHash: "sha256:a48c67808a20e01fbbd02663cc31fabd77d64d972e5f228e3296f3e2cd3a66f9" };
const MAPPING = { id: "game.synthetic-win32-dev.pc", version: "2026-07-23.1", contentHash: "sha256:9e46b85e8ee63e6518f77d281eca1d1627014429098784a8055d23c7ba8f22dd" };
const CLASSIFICATION = { sourceFamily: "dm-local-dev", contractVersion: "dm-1", artifactSchemaVersion: "dm-local-dev-recording-1", environment: "production", storageNamespace: "raw", nonTrainable: true, isTrainable: false, productionCertified: false };

const localSessionId = `e2e-smoke-${Date.now()}`;
const localRunId = "r0000";
const segmentIndex = 0;
const N_FRAMES = 120, EVENT_COUNT = 5, QPC_START = 1_000_000, QPC_END = 2_000_000;
const W = 1920, H = 1080, FN = 30, FD = 1;
const HEALTH = { duplicateFrames: 0, sourceSuperseded: 0, queueDrops: 0, inputOverflow: 0 };

async function call(method, url, { body, idem } = {}) {
  const headers = { authorization: `Bearer ${TOKEN}` };
  if (body !== undefined) headers["content-type"] = "application/json";
  if (idem) headers["idempotency-key"] = idem;
  const res = await fetch(`${API}${url}`, { method, headers, body: body !== undefined ? JSON.stringify(body) : undefined });
  const text = await res.text();
  let json; try { json = text ? JSON.parse(text) : undefined; } catch { json = text; }
  if (!res.ok) throw new Error(`${method} ${url} -> ${res.status}: ${text}`);
  return json;
}
const idem = () => randomUUID();

// ---- build the RAW five-piece set (interlocking hashes) ----
const video = Buffer.from(`SYNTHETIC-VIDEO-${localSessionId}\n`.repeat(8));
const frames = Buffer.from(`SYNTHETIC-FRAMES-${localSessionId}\n`.repeat(4));
const events = Buffer.from(`SYNTHETIC-EVENTS-${localSessionId}\n`);
const videoSha = sha(video), framesSha = sha(frames), eventsSha = sha(events);

const meta = {
  schema_version: "dm-local-dev-recording-1",
  session_id: localSessionId, run_id: localRunId, segment_index: segmentIndex,
  session_frame_start: 0, n_frames: N_FRAMES,
  state: "non_trainable_ready", non_trainable: true, production_certified: false,
  input_status: "raw_input_v1",
  qpc_start: QPC_START, qpc_end: QPC_END, width: W, height: H, fps_num: FN, fps_den: FD,
  mapping: { id: MAPPING.id, version: MAPPING.version, content_hash: MAPPING.contentHash },
  contract: { version: "dm-1" },
  health: { duplicate_frames: 0, source_superseded: 0, queue_drops: 0, input_overflow: 0 },
  video: { path: "video.mp4", bytes: video.byteLength, sha256: videoSha },
  frames: { path: "frames.jsonl.zst", lines: N_FRAMES, sha256: framesSha },
  events: { path: "events.jsonl.zst", lines: EVENT_COUNT, sha256: eventsSha },
};
const metaBuf = Buffer.from(JSON.stringify(meta));
const metaSha = sha(metaBuf);
const manifestBuf = Buffer.from(JSON.stringify({ "video.mp4": videoSha, "frames.jsonl.zst": framesSha, "events.jsonl.zst": eventsSha, "meta.json": metaSha }));
const manifestSha = sha(manifestBuf);

const BYTES = { video, frames, events, meta: metaBuf, manifest: manifestBuf };
const DECL = {
  video: { byteSize: video.byteLength, sha256: videoSha },
  frames: { byteSize: frames.byteLength, sha256: framesSha },
  events: { byteSize: events.byteLength, sha256: eventsSha },
  meta: { byteSize: metaBuf.byteLength, sha256: metaSha },
  manifest: { byteSize: manifestBuf.byteLength, sha256: manifestSha },
};

console.log(`e2e smoke → ${API}  session=${localSessionId}`);

// 1) session
const session = await call("POST", "/v1/sessions", {
  idem: idem(),
  body: {
    localSessionId, sessionSchemaVersion: "dm-local-dev-session-2",
    startedAt: new Date().toISOString(), localState: "completed",
    classification: CLASSIFICATION, clientVersion: "0.1.0",
    catalog: CATALOG, game: GAME, deviceProfile: DEVICE_PROFILE,
    captureProfile: CAPTURE_PROFILE, settingsProfile: SETTINGS_PROFILE, mapping: MAPPING,
  },
});
console.log(`  ✔ session ${session.sessionId} state=${session.state}`);

// 2) run
const run = await call("PUT", `/v1/sessions/${session.sessionId}/runs/${localRunId}`, {
  idem: idem(),
  body: {
    sequence: 0, localState: "completed",
    startedAt: new Date(Date.now() - 10000).toISOString(), endedAt: new Date().toISOString(),
    qpcFrequency: 10_000_000, sessionFrameStart: 0,
    preflight: { schemaVersion: "dm-preflight-evidence-1", id: "preflight-e2e-0", result: "dev_non_trainable_pass", evidenceHash: sha(Buffer.from("preflight-e2e")) },
    capture: { width: W, height: H, fpsNum: FN, fpsDen: FD, captureEngine: "native_wgc_window_v1", encoderBackend: "mf_nvidia_hardware_v1" },
  },
});
console.log(`  ✔ run ${run.runId} state=${run.state}`);

// 3) clip
const clip = await call("PUT", `/v1/sessions/${session.sessionId}/runs/${localRunId}/clips/${segmentIndex}`, {
  idem: idem(),
  body: {
    classification: CLASSIFICATION, localArtifactState: "ready",
    sessionFrameStart: 0, nFrames: N_FRAMES, qpcStart: QPC_START, qpcEnd: QPC_END,
    eventCount: EVENT_COUNT, health: HEALTH, artifacts: DECL,
  },
});
console.log(`  ✔ clip ${clip.clipId} revision=${clip.revision} storage=${clip.storageState}`);

// 4) upload intent
const intent = await call("POST", `/v1/clips/${clip.clipId}/upload-intents`, {
  idem: idem(),
  body: { clipRevision: clip.revision, transferMode: "server_owned_r2_multipart" },
});
console.log(`  ✔ upload-intent ${intent.uploadIntentId}`);

// 5) sign parts (1 part per artifact — each < 5 MiB, last-part rule waives minimum)
const kinds = ["video", "frames", "events", "meta", "manifest"];
const signBody = { parts: kinds.map((k) => ({ artifactType: k, partNumber: 1, byteSize: BYTES[k].byteLength, contentMd5: md5b64(BYTES[k]) })) };
const signed = await call("POST", `/v1/upload-intents/${intent.uploadIntentId}/parts:sign`, { idem: idem(), body: signBody });
console.log(`  ✔ signed ${signed.parts.length} parts`);

// 6) PUT bytes to R2 via presigned URLs; capture ETags
const etags = {};
for (const p of signed.parts) {
  const put = await fetch(p.url, { method: "PUT", headers: p.headers, body: BYTES[p.artifactType] });
  if (!put.ok) throw new Error(`presigned PUT ${p.artifactType} -> ${put.status}: ${await put.text()}`);
  const etag = put.headers.get("etag");
  if (!etag) throw new Error(`no ETag for ${p.artifactType}`);
  etags[p.artifactType] = etag;
}
console.log(`  ✔ PUT ${Object.keys(etags).length} objects to R2 (${process.env.R2_BUCKET ?? "dev bucket"})`);

// 7) complete
const complete = await call("POST", `/v1/upload-intents/${intent.uploadIntentId}/complete`, {
  idem: idem(),
  body: { artifacts: Object.fromEntries(kinds.map((k) => [k, { parts: [{ partNumber: 1, etag: etags[k] }] }])) },
});
console.log(`  ✔ complete state=${complete.state}`);

// 8) status
const status = await call("GET", `/v1/clips/${clip.clipId}/status`);
console.log(`\n✅ upload path OK. clip status:`);
console.log(JSON.stringify(status, null, 2));
console.log(`\nclipId=${clip.clipId}  (run the worker to drive verification -> verified)`);
