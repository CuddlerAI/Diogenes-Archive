/**
 * 校验与封存失败的故障词汇表：产码与分类放在同一处。
 *
 * 起因见 docs/101 §6.5 与 #701。这两半原先拆在两个文件里，靠「存储类错误请加
 * `storage_` 前缀」这条口头约定连接，结果它们从未真正相遇：
 *
 * - 产码的一半（verification.ts）给**任意** `Error.name` 套 `storage_` 前缀。pg 的
 *   `DatabaseError` 把协议消息名直接赋给 `.name`，ErrorResponse 的消息名就是
 *   `'error'`，于是任何数据库异常都落库成 `storage_error`——2026-08-03 那次
 *   42P08 就是这么被记成存储故障的，排查因此先去查了 R2 和跨境链路。
 * - 分类的一半（repository.ts）用 `startsWith("storage_")` 决定要不要销毁数据，
 *   而它唯一调用点上能出现的六个码没有一个以 `storage_` 开头——保留分支不可达。
 *
 * 所以这里的两个导出是成对的：`verificationFaultCode` 是唯一的产码点，
 * `uploadFailureDisposition` 是唯一的分类点，改一边时另一边就在同一屏里。
 */

/** `verifyOne` 的三个阶段。读 R2 / 流式哈希 / 写回 Postgres 是三层不同的故障。 */
export type VerificationStage = "get" | "stream_hash" | "repository";

/**
 * 阶段前缀。**故障码的第一段就说明故障在哪一层**——因为落库的只有这个码：
 * `stage` 本来就在手上，却只进 Cloud Run 日志，日志一过期那些行就再也归不了因
 * （docs/101 §1 说的可见性边界，只不过这一次边界在我们自己的代码里）。
 */
const stagePrefixes: Readonly<Record<VerificationStage, string>> = {
  get: "storage",
  stream_hash: "stream",
  repository: "repository",
};

/** `verification_jobs.last_error_code` 等列是 text，这个上限只为防离谱的 name。 */
const MAX_FAULT_CODE_LENGTH = 80;

/**
 * 内容与治理层面的失败：**数据本身不该留**。逐个列出而不是靠前缀去猜——
 * 「这个码该不该销毁数据」是整个词汇表的问题，就该有一处知道整张表。
 *
 * 前六个是今天能真正走到 `failMultipart` 的全集（`isPostStorageGateFailure` 四个
 * + HEAD 校验两个）；其余是 `verifyOne` 的内容判定码，今天走的是 `finishVerification`
 * 那条路、到不了这里，登记在册是为了万一有人把它们接过来时不必重新想一遍。
 */
const purgeOnUploadFailure: ReadonlySet<string> = new Set([
  "device_not_registered",
  "classification_blocked",
  "retention_blocked",
  "upload_intent_expired",
  "head_size_mismatch",
  "head_metadata_mismatch",
  "size_mismatch",
  "sha256_mismatch",
  "semantic_payload_too_large",
  "manifest_contract_mismatch",
  "manifest_invalid_json",
  "meta_contract_mismatch",
  "meta_invalid_json",
]);

/** 已知的基础设施/服务端缺陷码：数据是好的，出错的是我们，保留待查。 */
const holdOnUploadFailure: ReadonlySet<string> = new Set(["verification_lease_exhausted"]);

/**
 * 把出错阶段编进故障码。
 *
 * `code` 优先于 `name`：pg 把 SQLSTATE 放在 `code`、`name` 恒为无信息的 `'error'`；
 * AWS SDK 恰好相反，`name` 才是 `NoSuchKey` / `TimeoutError` 这类可读的东西。
 * 先看 `code` 再退回 `name`，两边都能拿到有信息的那一半——08-03 那批失败按新口径
 * 会落成 `repository_42p08`，看一眼就知道故障在数据库写回而不在对象存储。
 */
export function verificationFaultCode(stage: VerificationStage, error: unknown): string {
  return `${stagePrefixes[stage]}_${faultDetail(error)}`.slice(0, MAX_FAULT_CODE_LENGTH);
}

/** 该码是否由 `verificationFaultCode` 产出（用于识别未登记的新码）。 */
export function isVerificationFaultCode(code: string): boolean {
  return Object.values(stagePrefixes).some((prefix) => code.startsWith(`${prefix}_`));
}

/**
 * 一次上传封存失败该销毁数据还是保留。
 *
 * **缺省从「销毁」翻成了「保留」**，这是刻意的：认识的码按表走，不认识的码保留。
 * 一个新码落错边的代价因此从「静默删掉采集员的一段录制」降到「多留一段待人看」，
 * 而 `recognized: false` 会跟着审计事件落库，所以「多留的那一段」不会没人知道。
 *
 * 今天六个码的处置与改动前完全一致（仍全部 purge），变的只是未知码的失败模式。
 */
export function uploadFailureDisposition(errorCode: string): {
  retainsData: boolean;
  recognized: boolean;
} {
  if (purgeOnUploadFailure.has(errorCode)) return { retainsData: false, recognized: true };
  const recognized = holdOnUploadFailure.has(errorCode) || isVerificationFaultCode(errorCode);
  return { retainsData: true, recognized };
}

function faultDetail(error: unknown): string {
  const candidate = error as { code?: unknown; name?: unknown } | null | undefined;
  const raw =
    typeof candidate?.code === "string" && candidate.code.trim() !== ""
      ? candidate.code
      : error instanceof Error && error.name
        ? error.name
        : "unknown";
  const normalized = raw.toLowerCase().replace(/[^a-z0-9]+/g, "_");
  return normalized.replace(/^_+|_+$/g, "") || "unknown";
}
