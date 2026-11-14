import type { D1Database } from "./d1.js";

/**
 * 设备凭据校验——与摄取面 S0 **完全同一套**。
 *
 * 客户端持有 console 签发的一个 opaque per-device 凭据；服务端一律
 * `sha256(bearer)` 匹配 fingerprint。没有共享密钥，令牌本身不自述 device id：
 * device id 来自命中的凭据行。
 *
 * 镜像刻意不发明第二种令牌格式（比如 HMAC 派生的 `<deviceId>.<sig>`）。
 * 那会让一台机器揣两把钥匙、吊销要做两次，而且必然有一次会被忘掉。
 *
 * Worker 够不到 Cloud SQL，因此 D1 里保存 `mirror_device_credentials`：
 * 它是 `devices.credential_fingerprint` 的投影，由 console 在签发/吊销时写入——
 * 与本服务其余部分一样是非权威副本。表为空时 fail closed，每个请求都拒。
 */

const encoder = new TextEncoder();

/**
 * 未知 / 已停用 / 已退役 / annotator 被停用四种情况返回同一 code 与文案，
 * 与摄取面 S0 的口径一致：调用方只知道自己过不去，不知道是哪一种。
 */
export const DEVICE_CREDENTIAL_REJECTION = {
  code: "invalid_device_credential",
  message: "a valid device credential is required",
} as const;

export class AuthError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "AuthError";
    this.code = code;
  }
}

function rejected(): AuthError {
  return new AuthError(DEVICE_CREDENTIAL_REJECTION.code, DEVICE_CREDENTIAL_REJECTION.message);
}

/** WebCrypto 而非 node:crypto：Workers 原生支持，测试不需要 nodejs_compat。 */
export async function credentialFingerprint(credential: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(credential));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/*
 * 活性判定放进查询而不是靠调用方过滤：设备数不设上限，停用一个 annotator
 * 必须一次带走它名下的整个设备队列——包括停用之后才写进来的凭据行。
 */
const RESOLVE_CREDENTIAL = `
SELECT device_id
FROM mirror_device_credentials
WHERE credential_fingerprint = ?1
  AND device_status IN ('pending', 'active')
  AND annotator_status IN ('unbound', 'active')
`;

/**
 * 返回凭据证明的 device id。任何失败都抛 AuthError，不返回布尔值——
 * 调用方没有机会「忘了检查返回值」。
 */
export async function authenticateDevice(
  authorization: string | null,
  db: D1Database,
): Promise<string> {
  // 无凭据时短路，不查库：未认证的扫描器只花一次字符串比较。
  if (!authorization?.startsWith("Bearer ")) throw rejected();
  const credential = authorization.slice(7).trim();
  if (credential.length === 0) throw rejected();

  const row = await db
    .prepare(RESOLVE_CREDENTIAL)
    .bind(await credentialFingerprint(credential))
    .first<{ device_id: string }>();
  if (!row) throw rejected();
  return row.device_id;
}
