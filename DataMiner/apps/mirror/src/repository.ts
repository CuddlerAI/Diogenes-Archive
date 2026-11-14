import type { D1Database } from "./d1.js";
import type { MirrorEvent } from "./contracts.js";

/**
 * D1 每个查询最多 100 个绑定参数（8 列时只够 12 行），SQL 语句上限 100 KB。
 * 因此整批事件打成**一个** JSON 参数，用 json_each() 在库内展开——
 * 一条语句写完 500 条事件，参数数量与批大小无关。
 */

const INSERT_EVENTS = `
INSERT INTO mirror_events (device_id, seq, entity, entity_id, observed_at, received_at, payload_json)
SELECT
    ?1,
    json_extract(value, '$.seq'),
    json_extract(value, '$.entity'),
    json_extract(value, '$.entityId'),
    json_extract(value, '$.observedAt'),
    ?2,
    json_extract(value, '$.payload')
FROM json_each(?3)
WHERE true
ON CONFLICT (device_id, seq) DO NOTHING
`;

/*
 * 投影 upsert 的 WHERE 守卫是关键：重放旧批次或乱序到达时，
 * `excluded.source_seq > 现有 source_seq` 为假，整行保持不动。
 * 没有它，一次无害的重传就能把状态改回几分钟前。
 */
const UPSERT_SESSIONS = `
INSERT INTO mirror_sessions (
    device_id, local_session_id, source_seq, state, game_slug,
    started_at, ended_at, run_count, segment_count, ready_segment_count, updated_at
)
SELECT
    ?1,
    json_extract(value, '$.payload.localSessionId'),
    json_extract(value, '$.seq'),
    json_extract(value, '$.payload.state'),
    json_extract(value, '$.payload.gameSlug'),
    json_extract(value, '$.payload.startedAt'),
    json_extract(value, '$.payload.endedAt'),
    json_extract(value, '$.payload.runCount'),
    json_extract(value, '$.payload.segmentCount'),
    json_extract(value, '$.payload.readySegmentCount'),
    ?2
FROM json_each(?3)
WHERE json_extract(value, '$.entity') = 'session'
ON CONFLICT (device_id, local_session_id) DO UPDATE SET
    source_seq          = excluded.source_seq,
    state               = excluded.state,
    game_slug           = COALESCE(excluded.game_slug, mirror_sessions.game_slug),
    started_at          = COALESCE(excluded.started_at, mirror_sessions.started_at),
    ended_at            = COALESCE(excluded.ended_at, mirror_sessions.ended_at),
    run_count           = excluded.run_count,
    segment_count       = excluded.segment_count,
    ready_segment_count = excluded.ready_segment_count,
    updated_at          = excluded.updated_at
WHERE excluded.source_seq > mirror_sessions.source_seq
`;

const UPSERT_SEGMENTS = `
INSERT INTO mirror_segments (
    device_id, local_session_id, local_run_id, segment_index, source_seq,
    local_state, n_frames, total_bytes, client_upload_state, updated_at
)
SELECT
    ?1,
    json_extract(value, '$.payload.localSessionId'),
    json_extract(value, '$.payload.localRunId'),
    json_extract(value, '$.payload.segmentIndex'),
    json_extract(value, '$.seq'),
    json_extract(value, '$.payload.localState'),
    json_extract(value, '$.payload.nFrames'),
    json_extract(value, '$.payload.totalBytes'),
    json_extract(value, '$.payload.clientUploadState'),
    ?2
FROM json_each(?3)
WHERE json_extract(value, '$.entity') = 'segment'
ON CONFLICT (device_id, local_session_id, local_run_id, segment_index) DO UPDATE SET
    source_seq          = excluded.source_seq,
    local_state         = excluded.local_state,
    n_frames            = COALESCE(excluded.n_frames, mirror_segments.n_frames),
    total_bytes         = COALESCE(excluded.total_bytes, mirror_segments.total_bytes),
    client_upload_state = excluded.client_upload_state,
    updated_at          = excluded.updated_at
WHERE excluded.source_seq > mirror_segments.source_seq
`;

/* acked_seq 用 MAX 而不是直接赋值：重放一批更早的事件不能让游标倒退。 */
const UPSERT_DEVICE = `
INSERT INTO mirror_devices (
    device_id, first_seen_at, last_seen_at, acked_seq,
    client_version, disk_free_bytes, recording_state
)
VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT (device_id) DO UPDATE SET
    last_seen_at    = excluded.last_seen_at,
    acked_seq       = MAX(mirror_devices.acked_seq, excluded.acked_seq),
    client_version  = COALESCE(excluded.client_version, mirror_devices.client_version),
    disk_free_bytes = COALESCE(excluded.disk_free_bytes, mirror_devices.disk_free_bytes),
    recording_state = COALESCE(excluded.recording_state, mirror_devices.recording_state)
`;

export type IngestResult = { deviceId: string; ackedSeq: number };

type HeartbeatEvent = Extract<MirrorEvent, { entity: "heartbeat" }>;

/** 契约保证 seq 严格递增，因此最后一条 heartbeat 就是本批最新的设备状态。 */
function latestHeartbeat(events: MirrorEvent[]): HeartbeatEvent | undefined {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]!;
    if (event.entity === "heartbeat") return event;
  }
  return undefined;
}

export class MirrorRepository {
  readonly #db: D1Database;

  constructor(db: D1Database) {
    this.#db = db;
  }

  async ingest(deviceId: string, events: MirrorEvent[], receivedAt: number): Promise<IngestResult> {
    const batchJson = JSON.stringify(events);
    const maxSeq = events[events.length - 1]!.seq;
    const heartbeat = latestHeartbeat(events);

    await this.#db.batch([
      this.#db.prepare(INSERT_EVENTS).bind(deviceId, receivedAt, batchJson),
      this.#db.prepare(UPSERT_SESSIONS).bind(deviceId, receivedAt, batchJson),
      this.#db.prepare(UPSERT_SEGMENTS).bind(deviceId, receivedAt, batchJson),
      this.#db
        .prepare(UPSERT_DEVICE)
        .bind(
          deviceId,
          receivedAt,
          maxSeq,
          heartbeat?.payload.clientVersion ?? null,
          heartbeat?.payload.diskFreeBytes ?? null,
          heartbeat?.payload.recordingState ?? null,
        ),
    ]);

    const cursor = await this.getCursor(deviceId);
    return { deviceId, ackedSeq: cursor?.ackedSeq ?? maxSeq };
  }

  async getCursor(deviceId: string): Promise<IngestResult | null> {
    const row = await this.#db
      .prepare("SELECT device_id, acked_seq FROM mirror_devices WHERE device_id = ?1")
      .bind(deviceId)
      .first<{ device_id: string; acked_seq: number }>();
    return row ? { deviceId: row.device_id, ackedSeq: row.acked_seq } : null;
  }

  /**
   * 固定窗口清理。镜像**不承担**留存合规——真正受 consent 约束的数据在 R2 与
   * Cloud SQL，这里没有任何一帧画面或一次按键。这只是防止库无限增长。
   */
  async sweepExpired(cutoff: number): Promise<void> {
    await this.#db.batch([
      this.#db.prepare("DELETE FROM mirror_events WHERE received_at < ?1").bind(cutoff),
      this.#db.prepare("DELETE FROM mirror_segments WHERE updated_at < ?1").bind(cutoff),
      this.#db.prepare("DELETE FROM mirror_sessions WHERE updated_at < ?1").bind(cutoff),
    ]);
  }

  /** 设备退役：删除该设备的全部镜像行。 */
  async forgetDevice(deviceId: string): Promise<void> {
    await this.#db.batch([
      this.#db.prepare("DELETE FROM mirror_events WHERE device_id = ?1").bind(deviceId),
      this.#db.prepare("DELETE FROM mirror_segments WHERE device_id = ?1").bind(deviceId),
      this.#db.prepare("DELETE FROM mirror_sessions WHERE device_id = ?1").bind(deviceId),
      this.#db.prepare("DELETE FROM mirror_devices WHERE device_id = ?1").bind(deviceId),
    ]);
  }
}
