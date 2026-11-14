/**
 * 凭据镜像表的写入契约。
 *
 * 本服务**没有**写这张表的路径——它只读。写入方是 console：签发、轮换、停用、
 * 退役设备，以及停用 annotator 时，把结果投影过来。SQL 定义放在这里而不是 console，
 * 是因为读的一方才知道自己依赖哪些不变量；写的一方 import 这个常量，
 * 两边就不可能各自长歪。
 *
 * `source_seq` 是 console 侧 outbox 的单调序号。发布方会并发、会重试，
 * 乱序到达的旧行必须被守卫挡住——否则一次重投就能把「已停用」改回「活跃」，
 * 而那正是这张表唯一真正要防的事。
 */
export const UPSERT_DEVICE_CREDENTIAL = `
INSERT INTO mirror_device_credentials (
    credential_fingerprint, device_id, source_seq, device_status, annotator_status, updated_at
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT (credential_fingerprint) DO UPDATE SET
    device_id        = excluded.device_id,
    source_seq       = excluded.source_seq,
    device_status    = excluded.device_status,
    annotator_status = excluded.annotator_status,
    updated_at       = excluded.updated_at
WHERE excluded.source_seq > mirror_device_credentials.source_seq
`;

/**
 * 轮换凭据时旧 fingerprint 必须立刻失效。console 在同一次投递里先 upsert 新行、
 * 再把该设备的其余凭据置为 retired——不删行，保留一条可审计的痕迹，
 * 也避免「删了但下一条重试又把它插回来」。
 */
export const RETIRE_OTHER_DEVICE_CREDENTIALS = `
UPDATE mirror_device_credentials
SET device_status = 'retired', source_seq = ?3, updated_at = ?4
WHERE device_id = ?1
  AND credential_fingerprint <> ?2
  AND source_seq < ?3
`;

export type MirrorCredentialRow = {
  credentialFingerprint: string;
  deviceId: string;
  sourceSeq: number;
  deviceStatus: "pending" | "active" | "suspended" | "retired";
  annotatorStatus: "unbound" | "active" | "suspended" | "tombstoned";
  updatedAt: number;
};
