import { createHash } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, resolve } from "node:path";

const options = parseArgs(process.argv.slice(2));
const runId = `loopback-${Date.now()}`;
const evidencePath = resolve(options.evidence);
const statePath = `${evidencePath}.state.json`;
mkdirSync(dirname(evidencePath), { recursive: true });

let state = loadState();

function parseArgs(args) {
  const parsed = {
    host: "127.0.0.1",
    port: 43199,
    evidence: "upload-loopback-evidence.jsonl",
    partSizeBytes: 256 * 1024,
  };
  for (let index = 0; index < args.length; index += 2) {
    const name = args[index];
    const value = args[index + 1];
    if (value === undefined) throw new Error(`missing value for ${name}`);
    if (name === "--host") parsed.host = value;
    else if (name === "--port") parsed.port = Number(value);
    else if (name === "--evidence") parsed.evidence = value;
    else if (name === "--part-size") parsed.partSizeBytes = Number(value);
    else throw new Error(`unknown option ${name}`);
  }
  if (parsed.host !== "127.0.0.1") {
    throw new Error("the upload lab must bind only to 127.0.0.1");
  }
  if (!Number.isSafeInteger(parsed.port) || parsed.port < 1024 || parsed.port > 65535) {
    throw new Error("invalid port");
  }
  if (!Number.isSafeInteger(parsed.partSizeBytes) || parsed.partSizeBytes < 64 * 1024) {
    throw new Error("invalid part size");
  }
  return parsed;
}

function emptyState() {
  return {
    sessions: {},
    runs: {},
    clips: {},
    intents: {},
    sessionArtifacts: {},
    puts: {},
  };
}

function loadState() {
  if (!existsSync(statePath)) return emptyState();
  try {
    return { ...emptyState(), ...JSON.parse(readFileSync(statePath, "utf8")) };
  } catch {
    return emptyState();
  }
}

function persistState() {
  const temporary = `${statePath}.${process.pid}.tmp`;
  writeFileSync(temporary, `${JSON.stringify(state)}\n`, "utf8");
  renameSync(temporary, statePath);
}

function evidence(kind, data = {}) {
  appendFileSync(
    evidencePath,
    `${JSON.stringify({ timestamp: new Date().toISOString(), runId, kind, ...data })}\n`,
    "utf8",
  );
}

function stableId(prefix, source) {
  return `${prefix}-${createHash("sha256").update(String(source)).digest("hex").slice(0, 20)}`;
}

function json(response, statusCode, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(statusCode, {
    "content-type": "application/json",
    "content-length": body.length,
    connection: "close",
  });
  response.end(body);
}

async function readJson(request) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.length;
    if (bytes > 4 * 1024 * 1024) throw new Error("request body too large");
    chunks.push(chunk);
  }
  if (bytes === 0) return {};
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

async function receivePut(request, response, identity) {
  const md5 = createHash("md5");
  let bytes = 0;
  for await (const chunk of request) {
    bytes += chunk.length;
    md5.update(chunk);
  }
  const digest = md5.digest();
  const md5Base64 = digest.toString("base64");
  const md5Hex = digest.toString("hex");
  const declaredBytes = Number(request.headers["content-length"] ?? -1);
  const declaredMd5 = String(request.headers["content-md5"] ?? "");
  const valid = bytes === declaredBytes && md5Base64 === declaredMd5;
  const etag = `"loopback-${md5Hex}"`;

  state.puts[identity.key] = {
    ...identity,
    bytes,
    declaredBytes,
    md5Matches: md5Base64 === declaredMd5,
    etag,
    runId,
  };
  persistState();
  evidence("r2_put_received", {
    scope: identity.scope,
    artifactKind: identity.artifactKind,
    partNumber: identity.partNumber ?? null,
    bytes,
    declaredBytes,
    md5Matches: md5Base64 === declaredMd5,
    accepted: valid,
  });

  if (!valid) {
    json(response, 400, { retryable: false, code: "LOOPBACK_BODY_MISMATCH" });
    return;
  }
  response.writeHead(200, { etag, "content-length": 0, connection: "close" });
  response.end();
}

function clipIntent(clipId) {
  return Object.values(state.intents).find((intent) => intent.clipId === clipId);
}

function uploadPlan(artifacts) {
  return Object.fromEntries(
    Object.entries(artifacts ?? {}).map(([kind, declaration]) => {
      const byteSize = Number(declaration.byteSize);
      return [
        kind,
        {
          partSizeBytes: options.partSizeBytes,
          partCount: Math.ceil(byteSize / options.partSizeBytes),
        },
      ];
    }),
  );
}

const server = createServer(async (request, response) => {
  const url = new URL(request.url ?? "/", `http://${options.host}:${options.port}`);
  const path = url.pathname;
  try {
    if (request.method === "GET" && path === "/lab/ready") {
      json(response, 200, { ready: true, runId });
      return;
    }
    if (request.method === "GET" && path === "/lab/evidence") {
      json(response, 200, {
        runId,
        putCount: Object.values(state.puts).filter((item) => item.runId === runId).length,
        segmentPutCount: Object.values(state.puts).filter(
          (item) => item.runId === runId && item.scope === "segment",
        ).length,
        sessionArtifactPutCount: Object.values(state.puts).filter(
          (item) => item.runId === runId && item.scope === "session_artifact",
        ).length,
        completedIntents: Object.values(state.intents).filter((intent) => intent.completed).length,
      });
      return;
    }

    let match = path.match(/^\/lab-r2\/segment\/([^/]+)\/([^/]+)\/(\d+)$/);
    if (request.method === "PUT" && match) {
      const intentId = decodeURIComponent(match[1]);
      const artifactKind = decodeURIComponent(match[2]);
      const partNumber = Number(match[3]);
      await receivePut(request, response, {
        key: `segment:${intentId}:${artifactKind}:${partNumber}`,
        scope: "segment",
        intentId,
        artifactKind,
        partNumber,
      });
      return;
    }

    match = path.match(/^\/lab-r2\/session-artifact\/([^/]+)\/([^/]+)$/);
    if (request.method === "PUT" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const artifactKind = decodeURIComponent(match[2]);
      await receivePut(request, response, {
        key: `session:${sessionId}:${artifactKind}`,
        scope: "session_artifact",
        sessionId,
        artifactKind,
      });
      return;
    }

    if (request.method === "POST" && path === "/v1/sessions") {
      const body = await readJson(request);
      const sessionId = stableId("lab-session", body.localSessionId ?? "unknown");
      state.sessions[sessionId] = {
        localSessionId: body.localSessionId ?? null,
        sealed: state.sessions[sessionId]?.sealed ?? false,
      };
      persistState();
      evidence("session_registered");
      json(response, 200, { sessionId });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+):seal$/);
    if (request.method === "POST" && match) {
      const sessionId = decodeURIComponent(match[1]);
      await readJson(request);
      state.sessions[sessionId] = { ...(state.sessions[sessionId] ?? {}), sealed: true };
      persistState();
      evidence("session_sealed");
      json(response, 200, { state: "sealed" });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+)\/runs\/([^/]+)$/);
    if (request.method === "PUT" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const localRunId = decodeURIComponent(match[2]);
      await readJson(request);
      const runIdValue = stableId("lab-run", `${sessionId}:${localRunId}`);
      state.runs[`${sessionId}:${localRunId}`] = { runId: runIdValue };
      persistState();
      evidence("run_registered");
      json(response, 200, { runId: runIdValue });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+)\/runs\/([^/]+)\/clips\/(\d+)$/);
    if (request.method === "PUT" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const localRunId = decodeURIComponent(match[2]);
      const segmentIndex = Number(match[3]);
      const body = await readJson(request);
      const runIdValue = state.runs[`${sessionId}:${localRunId}`]?.runId
        ?? stableId("lab-run", `${sessionId}:${localRunId}`);
      const clipId = stableId("lab-clip", `${sessionId}:${localRunId}:${segmentIndex}`);
      state.clips[clipId] = {
        sessionId,
        runId: runIdValue,
        segmentIndex,
        artifacts: body.artifacts ?? {},
      };
      persistState();
      evidence("clip_registered", {
        segmentIndex,
        artifactCount: Object.keys(body.artifacts ?? {}).length,
      });
      json(response, 200, { clipId, runId: runIdValue, revision: 1 });
      return;
    }

    match = path.match(/^\/v1\/clips\/([^/]+)\/upload-intents$/);
    if (request.method === "POST" && match) {
      const clipId = decodeURIComponent(match[1]);
      await readJson(request);
      const clip = state.clips[clipId];
      if (!clip) {
        json(response, 409, { retryable: false, code: "LOOPBACK_CLIP_UNKNOWN" });
        return;
      }
      const uploadIntentId = stableId("lab-intent", clipId);
      const artifacts = uploadPlan(clip.artifacts);
      state.intents[uploadIntentId] = {
        ...(state.intents[uploadIntentId] ?? {}),
        clipId,
        artifacts,
      };
      persistState();
      evidence("upload_intent_created", {
        artifactCount: Object.keys(artifacts).length,
        partCount: Object.values(artifacts).reduce((sum, plan) => sum + plan.partCount, 0),
      });
      json(response, 200, { uploadIntentId, artifacts });
      return;
    }

    match = path.match(/^\/v1\/upload-intents\/([^/]+)\/parts:sign$/);
    if (request.method === "POST" && match) {
      const uploadIntentId = decodeURIComponent(match[1]);
      const body = await readJson(request);
      const requested = body.parts?.[0];
      const plan = state.intents[uploadIntentId]?.artifacts?.[requested?.artifactType];
      if (
        body.parts?.length !== 1
        || !plan
        || !Number.isInteger(requested.partNumber)
        || requested.partNumber < 1
        || requested.partNumber > plan.partCount
      ) {
        json(response, 409, { retryable: false, code: "LOOPBACK_PART_INVALID" });
        return;
      }
      const signedUrl = `http://${options.host}:${options.port}/lab-r2/segment/${encodeURIComponent(uploadIntentId)}/${encodeURIComponent(requested.artifactType)}/${requested.partNumber}`;
      evidence("part_signed", {
        artifactKind: requested.artifactType,
        partNumber: requested.partNumber,
        bytes: requested.byteSize,
      });
      json(response, 200, {
        parts: [{
          artifactType: requested.artifactType,
          partNumber: requested.partNumber,
          url: signedUrl,
          headers: {
            "content-md5": requested.contentMd5,
            "content-length": String(requested.byteSize),
          },
        }],
      });
      return;
    }

    match = path.match(/^\/v1\/upload-intents\/([^/]+)\/complete$/);
    if (request.method === "POST" && match) {
      const uploadIntentId = decodeURIComponent(match[1]);
      await readJson(request);
      const intent = state.intents[uploadIntentId];
      const expectedParts = Object.entries(intent?.artifacts ?? {}).flatMap(([kind, plan]) =>
        Array.from({ length: plan.partCount }, (_, index) => `segment:${uploadIntentId}:${kind}:${index + 1}`),
      );
      const missingParts = expectedParts.filter((key) => !state.puts[key]);
      if (!intent || missingParts.length > 0) {
        evidence("upload_complete_rejected", { missingPartCount: missingParts.length });
        json(response, 409, { retryable: true, code: "LOOPBACK_PARTS_INCOMPLETE" });
        return;
      }
      intent.completed = true;
      persistState();
      evidence("upload_completed", { partCount: expectedParts.length });
      json(response, 200, { state: "stored" });
      return;
    }

    match = path.match(/^\/v1\/clips\/([^/]+)\/status$/);
    if (request.method === "GET" && match) {
      const clipId = decodeURIComponent(match[1]);
      const intent = clipIntent(clipId);
      json(response, 200, { verificationState: intent?.completed ? "verified" : "pending" });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+)\/artifacts$/);
    if (request.method === "PUT" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const body = await readJson(request);
      const declarations = Object.entries(body.artifacts ?? {}).map(([kind, item]) => ({
        kind,
        objectKey: `loopback/${sessionId}/${kind}`,
        verificationState: "pending",
        expectedBytes: item.byteSize,
        expectedSha256: item.sha256,
      }));
      state.sessionArtifacts[sessionId] = Object.fromEntries(
        declarations.map((item) => [item.kind, item]),
      );
      persistState();
      evidence("session_artifacts_declared", { artifactCount: declarations.length });
      json(response, 200, { sessionId, artifacts: declarations });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+)\/artifacts\/([^/]+)\/sign$/);
    if (request.method === "POST" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const kind = decodeURIComponent(match[2]);
      const body = await readJson(request);
      const declaration = state.sessionArtifacts[sessionId]?.[kind];
      if (!declaration) {
        json(response, 409, { retryable: false, code: "LOOPBACK_ARTIFACT_UNKNOWN" });
        return;
      }
      const contentType = kind === "markers" ? "application/x-ndjson" : "application/json";
      json(response, 200, {
        kind,
        objectKey: declaration.objectKey,
        url: `http://${options.host}:${options.port}/lab-r2/session-artifact/${encodeURIComponent(sessionId)}/${encodeURIComponent(kind)}`,
        headers: {
          "content-type": contentType,
          "content-md5": body.contentMd5,
          "content-length": String(declaration.expectedBytes),
          "x-amz-meta-dataminer_class": "raw-non-trainable",
        },
        expiresInSeconds: 600,
      });
      return;
    }

    match = path.match(/^\/v1\/sessions\/([^/]+)\/artifacts\/([^/]+)\/complete$/);
    if (request.method === "POST" && match) {
      const sessionId = decodeURIComponent(match[1]);
      const kind = decodeURIComponent(match[2]);
      if (!state.puts[`session:${sessionId}:${kind}`]) {
        json(response, 409, { retryable: true, code: "LOOPBACK_ARTIFACT_INCOMPLETE" });
        return;
      }
      evidence("session_artifact_completed", { artifactKind: kind });
      json(response, 200, { kind, verificationState: "verified" });
      return;
    }

    evidence("request_ignored", { method: request.method, path });
    json(response, 404, { retryable: false, code: "LOOPBACK_ROUTE_UNKNOWN" });
  } catch (error) {
    evidence("server_error", { method: request.method, path, error: error.message });
    json(response, 500, { retryable: true, code: "LOOPBACK_SERVER_ERROR" });
  }
});

server.listen(options.port, options.host, () => {
  evidence("server_started", {
    host: options.host,
    port: options.port,
    partSizeBytes: options.partSizeBytes,
  });
  process.stdout.write(`upload loopback lab listening on http://${options.host}:${options.port}\n`);
});

function shutdown(signal) {
  evidence("server_stopping", { signal });
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(1), 2_000).unref();
}

process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));
