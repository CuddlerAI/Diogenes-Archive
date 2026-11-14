/**
 * R2 连通性验收（#57 任务 1）。
 *
 * 这不是单元测试——单元测试跑在 fake storage 上。本脚本用**真实 R2 API token**
 * 打**真实桶**，跑一遍服务端 owned-multipart + 客户端 presigned-PUT 的完整契约：
 *
 *   ping(HeadBucket) → createMultipart → signUploadPart → 用 presigned URL PUT
 *   一个小对象（模拟桌面客户端的角色）→ completeMultipart → headObject 确认
 *   → getObject 读回并核对字节 → deleteObject 清理
 *
 * 复用生产 `R2ObjectStorage`（storage.ts）本身，所以跑通 = 真实上传代码路径通。
 *
 * 用法（凭据只经环境变量传入，绝不写进仓库、绝不打印）：
 *
 *   cd DataMiner/apps/api
 *   R2_ACCOUNT_ID=... R2_BUCKET=diogenes-training-dev \
 *   R2_ACCESS_KEY_ID=... R2_SECRET_ACCESS_KEY=... \
 *   npx tsx scripts/check-r2.ts
 *
 * 退出码 0 = 全部通过；非 0 = 有步骤失败（逐步打印 PASS/FAIL）。
 * 只在你控制的 dev 桶上跑；它会写入并随后删除一个 `_connectivity-check/` 前缀下的临时对象。
 */
import { createHash, randomUUID } from "node:crypto";

import { R2ObjectStorage } from "../src/storage.js";

function requireEnv(name: string): string {
  const value = process.env[name];
  if (!value || value.trim().length === 0) {
    console.error(`✘ 缺少环境变量 ${name}`);
    process.exit(2);
  }
  return value;
}

// 只读 R2_* 子集，不走 loadConfig()——连通性检查不该要求 DATABASE_URL 等无关配置齐全。
const r2Config = {
  accountId: requireEnv("R2_ACCOUNT_ID"),
  bucket: requireEnv("R2_BUCKET"),
  accessKeyId: requireEnv("R2_ACCESS_KEY_ID"),
  secretAccessKey: requireEnv("R2_SECRET_ACCESS_KEY"),
  prefix: process.env.R2_PREFIX ?? "_connectivity-check",
  presignTtlSeconds: Number(process.env.R2_PRESIGN_TTL_SECONDS ?? 900),
  partSizeBytes: Number(process.env.R2_PART_SIZE_BYTES ?? 67_108_864),
  requestTimeoutSeconds: Number(process.env.R2_REQUEST_TIMEOUT_SECONDS ?? 120),
};

// 凭据永不打印。只回显非敏感事实，方便确认打的是哪个桶。
console.log(`R2 连通性检查 → 账号 ${r2Config.accountId.slice(0, 6)}…  桶 ${r2Config.bucket}`);

const storage = new R2ObjectStorage(r2Config);
const objectKey = `_connectivity-check/${new Date().toISOString().slice(0, 10)}/${randomUUID()}.bin`;
const payload = Buffer.from(`dataminer r2 connectivity check ${randomUUID()}\n`, "utf8");
const contentMd5 = createHash("md5").update(payload).digest("base64");

let providerUploadId: string | null = null;
let step = "";

async function run(label: string, fn: () => Promise<void>): Promise<void> {
  step = label;
  process.stdout.write(`  … ${label}`);
  await fn();
  process.stdout.write(`\r  ✔ ${label}\n`);
}

try {
  await run("HeadBucket（ping）", () => storage.ping());

  await run("createMultipart", async () => {
    providerUploadId = await storage.createMultipart(objectKey, "application/octet-stream", "connectivity-probe");
  });

  let etag = "";
  await run("signUploadPart（服务端签名）", async () => {
    const signed = await storage.signUploadPart({
      objectKey,
      providerUploadId: providerUploadId!,
      partNumber: 1,
      bytes: payload.byteLength,
      contentMd5,
    });
    // 客户端的角色：拿 presigned URL 直接 PUT，服务端不经手字节。
    const put = await fetch(signed.url, {
      method: "PUT",
      headers: signed.headers,
      body: payload,
    });
    if (!put.ok) {
      throw new Error(`presigned PUT 返回 ${put.status} ${put.statusText}: ${await put.text()}`);
    }
    const header = put.headers.get("etag");
    if (!header) throw new Error("presigned PUT 未返回 ETag");
    etag = header;
  });

  await run("completeMultipart", () =>
    storage.completeMultipart(objectKey, providerUploadId!, [{ partNumber: 1, etag }]),
  );
  providerUploadId = null; // completed，不再需要 abort 清理

  await run("headObject（确认落盘 + 字节数）", async () => {
    const head = await storage.headObject(objectKey);
    if (head.bytes !== payload.byteLength) {
      throw new Error(`字节数不符：期望 ${payload.byteLength}，实际 ${head.bytes}`);
    }
  });

  await run("getObject（读回核对内容）", async () => {
    const stream = await storage.getObject(objectKey);
    const chunks: Buffer[] = [];
    for await (const chunk of stream) chunks.push(Buffer.from(chunk));
    const readBack = Buffer.concat(chunks);
    if (!readBack.equals(payload)) {
      throw new Error("读回内容与写入不一致");
    }
  });

  await run("deleteObject（清理）", () => storage.deleteObject(objectKey));

  console.log("\n✅ R2 连通性全部通过：HeadBucket + presigned-PUT 全回环成功。#57 任务 1 达标。");
  process.exit(0);
} catch (error) {
  process.stdout.write("\r");
  console.error(`\n✘ 失败于步骤：${step}`);
  console.error(`  ${error instanceof Error ? error.message : String(error)}`);
  // 尽量清理：未 complete 的 multipart 会留下悬挂分片，abort 掉。
  if (providerUploadId) {
    try {
      await storage.abortMultipart(objectKey, providerUploadId);
      console.error("  （已 abort 悬挂的 multipart）");
    } catch {
      console.error(`  ⚠ 未能 abort 悬挂 multipart，手动清理：${objectKey}`);
    }
  }
  console.error("\n常见原因：");
  console.error("  - token 权限不足：需 Object Read & Write，且作用域含该桶；");
  console.error("  - R2_ACCOUNT_ID 或 R2_BUCKET 填错（账号 id 不是桶名）；");
  console.error("  - presigned PUT 403：多为 token 无写权限或桶名与签名不一致。");
  process.exit(1);
}
