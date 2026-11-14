import { AuthError, authenticateDevice } from "./auth.js";
import { deviceParamsSchema, ingestEventsSchema } from "./contracts.js";
import type { D1Database } from "./d1.js";
import { MirrorRepository } from "./repository.js";

/**
 * dataminer-mirror — 客户端本地 journal 的单向观测镜像。
 *
 * 这个 Worker 只做一件事：把采集机报上来的脱敏投影写进 D1。它**不是**权威事实源。
 * 任何会改变数据能否被训练、能否被删除、能否被上传的判断，都只能读 Cloud SQL。
 * 详见 docs/13-local-journal-cloud-mirror.md。
 *
 * 因此这里刻意**没有**：任何写回客户端的路径、任何 PATCH/PUT 语义、
 * 任何返回给 dataminer-api 或影响上传授权的字段，以及任何自己签发凭据的能力。
 */

export type Env = {
  MIRROR_DB: D1Database;
  /** 事件保留天数；镜像不承担留存合规，只防止库无限增长。 */
  MIRROR_RETENTION_DAYS?: string;
};

const MAX_BODY_BYTES = 1024 * 1024;
const DEFAULT_RETENTION_DAYS = 90;

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}

function errorResponse(code: string, message: string, status: number, details?: unknown): Response {
  return json(details === undefined ? { code, message } : { code, message, details }, status);
}

function retentionCutoff(env: Env, now: number): number {
  const days = Number.parseInt(env.MIRROR_RETENTION_DAYS ?? "", 10);
  const effective = Number.isFinite(days) && days > 0 ? days : DEFAULT_RETENTION_DAYS;
  return now - effective * 24 * 60 * 60 * 1000;
}

async function readJsonBody(request: Request): Promise<unknown> {
  const declared = request.headers.get("content-length");
  if (declared !== null && Number(declared) > MAX_BODY_BYTES) {
    throw new PayloadTooLarge();
  }
  const text = await request.text();
  // Content-Length 可以撒谎（或缺席），因此实际字节数再核一次。
  if (new TextEncoder().encode(text).length > MAX_BODY_BYTES) {
    throw new PayloadTooLarge();
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new MalformedJson();
  }
}

class PayloadTooLarge extends Error {}
class MalformedJson extends Error {}

export async function handleRequest(request: Request, env: Env, now: number): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname === "/healthz" && request.method === "GET") {
    return json({ status: "ok", service: "dataminer-mirror", authoritative: false });
  }

  if (!url.pathname.startsWith("/v1/mirror/")) {
    return errorResponse("not_found", "unknown route", 404);
  }

  let principal: string;
  try {
    principal = await authenticateDevice(request.headers.get("authorization"), env.MIRROR_DB);
  } catch (error) {
    if (error instanceof AuthError) return errorResponse(error.code, error.message, 401);
    throw error;
  }

  const repository = new MirrorRepository(env.MIRROR_DB);

  if (url.pathname === "/v1/mirror/events" && request.method === "POST") {
    let body: unknown;
    try {
      body = await readJsonBody(request);
    } catch (error) {
      if (error instanceof PayloadTooLarge) {
        return errorResponse("payload_too_large", `body must not exceed ${MAX_BODY_BYTES} bytes`, 413);
      }
      if (error instanceof MalformedJson) {
        return errorResponse("invalid_request", "body must be valid JSON", 400);
      }
      throw error;
    }

    const parsed = ingestEventsSchema.safeParse(body);
    if (!parsed.success) {
      return errorResponse(
        "invalid_request",
        "request did not match the mirror event contract",
        400,
        parsed.error.issues.map((issue) => ({ path: issue.path.join("."), message: issue.message })),
      );
    }
    // 凭据证明的身份必须与请求体一致，否则一台机器就能替别人写行。
    if (parsed.data.deviceId !== principal) {
      return errorResponse("device_scope_mismatch", "device credential does not match the request body", 403);
    }

    const result = await repository.ingest(principal, parsed.data.events, now);
    return json({ ...result, authoritative: false });
  }

  const deviceMatch = /^\/v1\/mirror\/devices\/([^/]+)(\/cursor)?$/.exec(url.pathname);
  if (deviceMatch) {
    const params = deviceParamsSchema.safeParse({ deviceId: decodeURIComponent(deviceMatch[1]!) });
    if (!params.success) {
      return errorResponse("invalid_request", "malformed device id", 400);
    }
    if (params.data.deviceId !== principal) {
      return errorResponse("device_scope_mismatch", "device credential does not match the requested device", 403);
    }
    const isCursor = deviceMatch[2] !== undefined;
    if (isCursor && request.method === "GET") {
      const cursor = await repository.getCursor(principal);
      return json({ deviceId: principal, ackedSeq: cursor?.ackedSeq ?? 0, authoritative: false });
    }
    // 设备退役：删除该设备的全部镜像行。这不是 consent purge——
    // 受 consent 约束的数据在 R2 与 Cloud SQL，删这里不构成合规动作。
    if (!isCursor && request.method === "DELETE") {
      await repository.forgetDevice(principal);
      return json({ deviceId: principal, forgotten: true });
    }
    return errorResponse("method_not_allowed", "unsupported method for this route", 405);
  }

  return errorResponse("not_found", "unknown route", 404);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      return await handleRequest(request, env, Date.now());
    } catch (error) {
      // 不回显内部错误细节：调用方拿不到栈，运维在 Workers 日志里看。
      console.error("mirror_request_failed", error instanceof Error ? error.message : "unknown");
      return errorResponse("internal_error", "request failed", 500);
    }
  },

  async scheduled(_event: unknown, env: Env): Promise<void> {
    const now = Date.now();
    await new MirrorRepository(env.MIRROR_DB).sweepExpired(retentionCutoff(env, now));
  },
};

export const __testing = { retentionCutoff, MAX_BODY_BYTES, DEFAULT_RETENTION_DAYS };
