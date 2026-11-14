import { describe, expect, it } from "vitest";

import type { MirrorEvent } from "./contracts.js";
import { MirrorRepository } from "./repository.js";
import { SqliteD1 } from "./testing/sqlite-d1.js";

const DEVICE = "7f3c1a20-9b4e-4d21-8c05-6a1e2f3b4c5d";
const T0 = 1_800_000_000_000;

function sessionEvent(seq: number, overrides: Partial<{ state: string; readySegmentCount: number }> = {}): MirrorEvent {
  return {
    seq,
    entity: "session",
    entityId: "sess-1",
    observedAt: T0 + seq,
    payload: {
      localSessionId: "sess-1",
      state: (overrides.state ?? "recording") as "recording",
      gameSlug: "synthetic",
      startedAt: T0,
      runCount: 1,
      segmentCount: 3,
      readySegmentCount: overrides.readySegmentCount ?? 2,
    },
  };
}

function segmentEvent(seq: number, index: number, localState = "ready", uploadState = "pending"): MirrorEvent {
  return {
    seq,
    entity: "segment",
    entityId: `sess-1:r0000:${index}`,
    observedAt: T0 + seq,
    payload: {
      localSessionId: "sess-1",
      localRunId: "r0000",
      segmentIndex: index,
      localState: localState as "ready",
      nFrames: 300,
      totalBytes: 165_000_000,
      clientUploadState: uploadState as "pending",
    },
  };
}

function heartbeatEvent(seq: number, diskFreeBytes = 54_000_000_000): MirrorEvent {
  return {
    seq,
    entity: "heartbeat",
    entityId: "hb",
    observedAt: T0 + seq,
    payload: {
      clientVersion: "0.1.0",
      recordingState: "recording",
      diskFreeBytes,
      pendingUploadCount: 3,
    },
  };
}

function newRepository() {
  const db = SqliteD1.createWithSchema();
  return { db, repository: new MirrorRepository(db) };
}

describe("MirrorRepository.ingest", () => {
  it("writes the append-only log and both projections in one batch", async () => {
    const { db, repository } = newRepository();

    const result = await repository.ingest(
      DEVICE,
      [sessionEvent(1), segmentEvent(2, 0), segmentEvent(3, 1), heartbeatEvent(4)],
      T0,
    );

    expect(result).toEqual({ deviceId: DEVICE, ackedSeq: 4 });
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_events")[0]).toEqual({ n: 4 });
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_segments")[0]).toEqual({ n: 2 });

    const [session] = db.query<{ state: string; ready_segment_count: number; game_slug: string }>(
      "SELECT state, ready_segment_count, game_slug FROM mirror_sessions",
    );
    expect(session).toEqual({ state: "recording", ready_segment_count: 2, game_slug: "synthetic" });

    const [device] = db.query<{ acked_seq: number; disk_free_bytes: number; client_version: string }>(
      "SELECT acked_seq, disk_free_bytes, client_version FROM mirror_devices",
    );
    expect(device).toEqual({ acked_seq: 4, disk_free_bytes: 54_000_000_000, client_version: "0.1.0" });
  });

  it("stores the payload as JSON that survives a round trip", async () => {
    const { db, repository } = newRepository();
    await repository.ingest(DEVICE, [segmentEvent(1, 0)], T0);

    const [row] = db.query<{ payload_json: string }>("SELECT payload_json FROM mirror_events");
    expect(JSON.parse(row!.payload_json)).toMatchObject({
      localSessionId: "sess-1",
      segmentIndex: 0,
      clientUploadState: "pending",
    });
  });

  it("is idempotent — replaying the same batch changes nothing", async () => {
    const { db, repository } = newRepository();
    const batch = [sessionEvent(1), segmentEvent(2, 0), heartbeatEvent(3)];

    const first = await repository.ingest(DEVICE, batch, T0);
    const second = await repository.ingest(DEVICE, batch, T0 + 60_000);

    expect(second).toEqual(first);
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_events")[0]).toEqual({ n: 3 });
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_sessions")[0]).toEqual({ n: 1 });
  });

  it("never regresses a projection when an older batch is replayed", async () => {
    const { db, repository } = newRepository();

    await repository.ingest(DEVICE, [sessionEvent(1, { state: "recording", readySegmentCount: 2 })], T0);
    await repository.ingest(DEVICE, [sessionEvent(9, { state: "completed", readySegmentCount: 7 })], T0 + 1_000);
    // 迟到的旧事件：seq 更小，必须被 upsert 守卫挡住。
    await repository.ingest(DEVICE, [sessionEvent(4, { state: "paused", readySegmentCount: 3 })], T0 + 2_000);

    const [session] = db.query<{ state: string; source_seq: number; ready_segment_count: number }>(
      "SELECT state, source_seq, ready_segment_count FROM mirror_sessions",
    );
    expect(session).toEqual({ state: "completed", source_seq: 9, ready_segment_count: 7 });
  });

  it("never regresses the device cursor", async () => {
    const { repository } = newRepository();

    await repository.ingest(DEVICE, [heartbeatEvent(300)], T0);
    const replayed = await repository.ingest(DEVICE, [heartbeatEvent(100)], T0 + 1_000);

    expect(replayed.ackedSeq).toBe(300);
  });

  it("applies the newest of several same-entity events inside one batch", async () => {
    const { db, repository } = newRepository();

    await repository.ingest(
      DEVICE,
      [
        segmentEvent(1, 0, "active", "unknown"),
        segmentEvent(2, 0, "finalizing", "unknown"),
        segmentEvent(3, 0, "ready", "pending"),
      ],
      T0,
    );

    const rows = db.query<{ local_state: string; client_upload_state: string; source_seq: number }>(
      "SELECT local_state, client_upload_state, source_seq FROM mirror_segments",
    );
    expect(rows).toEqual([{ local_state: "ready", client_upload_state: "pending", source_seq: 3 }]);
  });

  it("handles a batch with no projectable entities", async () => {
    const { db, repository } = newRepository();

    const result = await repository.ingest(
      DEVICE,
      [
        {
          seq: 1,
          entity: "fault",
          entityId: "sess-1",
          observedAt: T0,
          payload: { localSessionId: "sess-1", code: "JOURNAL_WRITE_FAILED", detailsId: "det-9" },
        },
      ],
      T0,
    );

    expect(result.ackedSeq).toBe(1);
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_events")[0]).toEqual({ n: 1 });
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_sessions")[0]).toEqual({ n: 0 });
    expect(db.query("SELECT COUNT(*) AS n FROM mirror_segments")[0]).toEqual({ n: 0 });
    // 没有 heartbeat 时设备行仍然建立，只是状态字段留空。
    expect(db.query("SELECT client_version FROM mirror_devices")[0]).toEqual({ client_version: null });
  });

  it("keeps optional payload fields nullable end to end", async () => {
    const { db, repository } = newRepository();

    await repository.ingest(
      DEVICE,
      [
        {
          seq: 1,
          entity: "segment",
          entityId: "sess-1:r0000:0",
          observedAt: T0,
          payload: {
            localSessionId: "sess-1",
            localRunId: "r0000",
            segmentIndex: 0,
            localState: "active",
            clientUploadState: "unknown",
          },
        },
      ],
      T0,
    );

    expect(db.query("SELECT n_frames, total_bytes FROM mirror_segments")[0]).toEqual({
      n_frames: null,
      total_bytes: null,
    });
  });

  it("keeps devices isolated from each other", async () => {
    const { db, repository } = newRepository();
    const other = "c81d4e2a-0f77-4b93-9a16-3d5e7c8b1042";

    await repository.ingest(DEVICE, [segmentEvent(1, 0)], T0);
    await repository.ingest(other, [segmentEvent(1, 0)], T0);

    expect(db.query("SELECT COUNT(*) AS n FROM mirror_segments")[0]).toEqual({ n: 2 });
    expect((await repository.getCursor(other))?.ackedSeq).toBe(1);
  });

  it("answers the pre-upload census: locally ready but not yet registered", async () => {
    const { db, repository } = newRepository();

    await repository.ingest(
      DEVICE,
      [
        segmentEvent(1, 0, "ready", "registered"),
        segmentEvent(2, 1, "ready", "pending"),
        segmentEvent(3, 2, "ready", "failed"),
        segmentEvent(4, 3, "corrupt", "unknown"),
      ],
      T0,
    );

    const [census] = db.query<{ n: number }>(
      `SELECT COUNT(*) AS n FROM mirror_segments
       WHERE local_state = 'ready' AND client_upload_state <> 'registered'`,
    );
    expect(census).toEqual({ n: 2 });
  });
});

describe("MirrorRepository maintenance", () => {
  it("sweeps rows older than the cutoff and keeps newer ones", async () => {
    const { db, repository } = newRepository();

    await repository.ingest(DEVICE, [sessionEvent(1), segmentEvent(2, 0)], T0);
    await repository.ingest(DEVICE, [sessionEvent(3), segmentEvent(4, 1)], T0 + 100_000);

    await repository.sweepExpired(T0 + 50_000);

    expect(db.query("SELECT COUNT(*) AS n FROM mirror_events")[0]).toEqual({ n: 2 });
    // segment 0 的最后观察值早于 cutoff，segment 1 更新，只应留下后者。
    expect(db.query<{ segment_index: number }>("SELECT segment_index FROM mirror_segments")).toEqual([
      { segment_index: 1 },
    ]);
  });

  it("forgets a single device without touching the others", async () => {
    const { db, repository } = newRepository();
    const other = "c81d4e2a-0f77-4b93-9a16-3d5e7c8b1042";

    await repository.ingest(DEVICE, [sessionEvent(1), segmentEvent(2, 0), heartbeatEvent(3)], T0);
    await repository.ingest(other, [sessionEvent(1), segmentEvent(2, 0), heartbeatEvent(3)], T0);

    await repository.forgetDevice(DEVICE);

    for (const table of ["mirror_events", "mirror_sessions", "mirror_segments", "mirror_devices"]) {
      const rows = db.query<{ device_id: string }>(`SELECT device_id FROM ${table}`);
      expect(rows.every((row) => row.device_id === other)).toBe(true);
      expect(rows.length).toBeGreaterThan(0);
    }
  });
});
