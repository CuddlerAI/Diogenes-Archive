#!/usr/bin/env node
// mock-app-api.mjs — loopback stand-in for the annotator surface (`/app/v1/*`, docs/11 §7).
//
// Why this exists: the account surface now fails closed unless an explicit environment/base-URL
// pair is valid. A UI walkthrough still must never register devices or burn invite codes against
// any real service, so the E2E explicitly selects staging and pins every client HTTP surface at
// this server. It answers the exact wire shapes `HttpAccountApi` deserializes and nothing more.
//
// It also records every request to a JSONL file, which is the evidence that a given UI
// action actually reached the network (and with what payload) rather than failing silently.
//
// Usage: node mock-app-api.mjs --port 43199 --evidence <path.jsonl>

import { createServer } from "node:http";
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";

const args = process.argv.slice(2);
function arg(name, fallback) {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
}
const PORT = Number(arg("port", "43199"));
const HOST = arg("host", "127.0.0.1");
const EVIDENCE = arg("evidence", "");
if (EVIDENCE) mkdirSync(dirname(EVIDENCE), { recursive: true });

// The terms body is deliberately long enough to exercise the scroll/consent panel.
const TERMS_BODY = [
  "DataMiner 采集员数据授权条款（E2E MOCK — 非真实法律文本）",
  "",
  "1. 采集范围：键鼠输入、游戏画面、游戏进程音频、深度与相机参数。",
  "2. 用途：模型训练研究。未经二次授权不做公开发布。",
  "3. 你可以随时登出并停止采集；已上传数据的删除请求走 console 工单。",
  "4. 本文本仅用于本地端到端走查，任何条款均不具法律效力。",
  "",
  ...Array.from({ length: 12 }, (_, i) => `补充条款 ${i + 1}：占位正文，用于验证长文本渲染与滚动。`),
].join("\n");

// sha256 of TERMS_BODY is NOT needed by the client (it echoes what we serve), so a stable
// literal keeps the mock dependency-free and deterministic.
const TERMS = {
  policyVersion: "2026-08-12-e2e",
  purpose: "端到端走查（mock）",
  termsHash: "e2e0000000000000000000000000000000000000000000000000000000000000",
  body: TERMS_BODY,
};

const SESSION_TOKEN = "e2e-session-token";
const VALID_CODE = "000000";
const state = {
  signedUp: false,
  verified: false,
  devices: [],
  consentSigned: null,
  profile: null,
  requests: 0,
};

function record(entry) {
  state.requests += 1;
  if (!EVIDENCE) return;
  try {
    appendFileSync(EVIDENCE, JSON.stringify({ at: new Date().toISOString(), ...entry }) + "\n");
  } catch {
    /* evidence is best-effort; never fail a request over it */
  }
}

function json(res, status, body) {
  const payload = Buffer.from(JSON.stringify(body));
  res.writeHead(status, { "content-type": "application/json", "content-length": payload.length });
  res.end(payload);
}
function noContent(res) {
  res.writeHead(204);
  res.end();
}
function problem(res, status, code, message) {
  // Mirrors the server's error envelope so the Rust mapper produces a real AccountError.
  json(res, status, { code, message, requestId: `e2e-${state.requests}` });
}

async function readJson(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  if (!chunks.length) return {};
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return {};
  }
}

function bearer(req) {
  const h = req.headers.authorization ?? "";
  return h.startsWith("Bearer ") ? h.slice(7) : null;
}
function requireSession(req, res) {
  if (bearer(req) !== SESSION_TOKEN) {
    problem(res, 401, "unauthorized", "会话已失效，请重新登录。");
    return false;
  }
  return true;
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://${HOST}:${PORT}`);
  const path = url.pathname;
  const method = req.method ?? "GET";
  let body = {};
  if (method === "POST" || method === "PUT") body = await readJson(req);
  record({ method, path, body: redact(body), authenticated: Boolean(bearer(req)) });

  try {
    if (path === "/lab/ready") return json(res, 200, { ready: true });
    if (path === "/lab/evidence") return json(res, 200, { requests: state.requests, ...state });

    switch (`${method} ${path}`) {
      case "GET /app/v1/terms":
        return json(res, 200, TERMS);

      case "POST /app/v1/signup":
        state.signedUp = true;
        return json(res, 200, { challengeId: "e2e-challenge", expiresInSeconds: 600 });

      case "POST /app/v1/signup/resend":
        return json(res, 200, { challengeId: "e2e-challenge-2", expiresInSeconds: 600 });

      case "POST /app/v1/signup/verify":
        if (String(body.code ?? "") !== VALID_CODE) {
          return problem(res, 400, "invalid_code", "验证码不正确或已过期。");
        }
        state.verified = true;
        return noContent(res);

      case "POST /app/v1/password-reset/request":
        return json(res, 200, { challengeId: "e2e-reset", expiresInSeconds: 600 });

      case "POST /app/v1/password-reset/confirm":
        if (String(body.code ?? "") !== VALID_CODE) {
          return problem(res, 400, "invalid_code", "验证码不正确或已过期。");
        }
        return noContent(res);

      case "POST /app/v1/login":
        return json(res, 200, { token: SESSION_TOKEN });

      case "GET /app/v1/session":
        if (!requireSession(req, res)) return;
        return json(res, 200, { annotatorId: "e2e-annotator-0001" });

      case "POST /app/v1/logout":
        if (!requireSession(req, res)) return;
        return noContent(res);

      case "POST /app/v1/devices/enroll": {
        if (!requireSession(req, res)) return;
        const clientDeviceId = body.clientDeviceId ?? "unknown";
        const device = {
          deviceId: `e2e-device-${state.devices.length + 1}`,
          clientDeviceId,
          status: "active",
          clientVersion: body.clientVersion ?? "unknown",
          lastSeenAt: new Date().toISOString(),
          createdAt: new Date().toISOString(),
        };
        state.devices = [device];
        return json(res, 200, {
          deviceId: device.deviceId,
          clientDeviceId,
          token: "e2e-device-token",
        });
      }

      case "GET /app/v1/devices":
        if (!requireSession(req, res)) return;
        return json(res, 200, { devices: state.devices });

      case "POST /app/v1/password":
        if (!requireSession(req, res)) return;
        return noContent(res);

      case "GET /app/v1/consent":
        if (!requireSession(req, res)) return;
        return json(res, 200, {
          consent: state.consentSigned,
          currentPolicyVersion: TERMS.policyVersion,
          currentAllowsTrainableUse: true,
          matchesCurrentTerms: Boolean(state.consentSigned),
          // An unsigned scope makes the re-sign banner render, which is the interesting state.
          pendingScopes: state.consentSigned ? [] : ["audio", "depth"],
          currentScopes: ["input", "video", "audio", "depth", "trainable_research"],
          resignRequired: !state.consentSigned,
        });

      case "POST /app/v1/consent":
        if (!requireSession(req, res)) return;
        state.consentSigned = {
          policyVersion: TERMS.policyVersion,
          allowsTrainableUse: true,
          acceptedAt: new Date().toISOString(),
          scopes: ["input", "video", "audio", "depth", "trainable_research"],
        };
        return noContent(res);

      case "GET /app/v1/profile":
        if (!requireSession(req, res)) return;
        return json(res, 200, { profile: state.profile });

      case "PUT /app/v1/profile": {
        if (!requireSession(req, res)) return;
        state.profile = {
          displayName: body.displayName ?? null,
          phone: body.phone ?? null,
          imKind: body.imKind ?? null,
          imHandle: body.imHandle ?? null,
          region: body.region ?? null,
          updatedAt: new Date().toISOString(),
        };
        return json(res, 200, { profile: state.profile });
      }
      default:
        break;
    }

    // Anything the client asks for that we did not model is itself a finding — make it loud.
    record({ method, path, unmodelled: true });
    return problem(res, 404, "not_found", `mock 未实现的端点：${method} ${path}`);
  } catch (err) {
    return problem(res, 500, "mock_error", String(err));
  }
});

/** Never write a password or code into the evidence file. */
function redact(body) {
  if (!body || typeof body !== "object") return body;
  const out = { ...body };
  for (const k of ["password", "newPassword", "currentPassword"]) {
    if (k in out) out[k] = "<redacted>";
  }
  return out;
}

server.listen(PORT, HOST, () => {
  console.log(`mock-app-api listening on http://${HOST}:${PORT}`);
});
