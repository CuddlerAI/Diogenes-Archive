import { useEffect, useState } from "react";

import {
  ApiError,
  api,
  describeApiError,
  type AcceptanceQcCheck,
  type AcceptanceReport,
  type ClipHealth,
  type ClipSummary,
  type PreviewGrant,
  type SessionDetail,
  type SessionSummary,
} from "../api.js";
import {
  EmptyState,
  ErrorNote,
  Field,
  ingestTone,
  JsonBlock,
  Loading,
  Mono,
  Panel,
  StateBadge,
  verificationTone,
  useAnnotatorOptions,
} from "../components.js";
import {
  describeDataClassification,
  describeTrainabilityBlocker,
  formatBytes,
  formatDateTime,
  formatDuration,
} from "../format.js";
import { Link, navigate, useRoute } from "../router.js";
import { useAsync } from "../useAsync.js";
import { AuditTrail } from "./AuditTrail.js";
import { ConfirmedAction } from "./Governance.js";

export type DataFilters = {
  gameId: string;
  deviceId: string;
  annotatorId: string;
  verificationState: string;
  ingestState: string;
  /** 只看健康异常段：硬故障位非零或补帧占比越线（docs/67 §3.4、docs/146）。 */
  unhealthyOnly: boolean;
};

export const emptyFilters: DataFilters = {
  gameId: "",
  deviceId: "",
  annotatorId: "",
  verificationState: "",
  ingestState: "",
  unhealthyOnly: false,
};

type View = { kind: "sessions" } | { kind: "clips" };

export function DataBrowser({
  filters,
  onFiltersChange,
  canWrite,
  canPreview,
}: {
  filters: DataFilters;
  onFiltersChange: (filters: DataFilters) => void;
  canWrite: boolean;
  canPreview: boolean;
}) {
  const annotatorOptions = useAnnotatorOptions();
  const [view, setView] = useState<View>({ kind: "sessions" });
  const games = useAsync(() => api.games(), []);

  // Detail views live in the URL so a session or clip can be shared as a link
  // (docs/53 §2): /data/sessions/:id and /data/clips/:id. The URL carries the
  // location only — byte access is re-signed per visit and audited regardless.
  const { path } = useRoute();
  const openClip = /^\/data\/clips\/([^/]+)$/.exec(path)?.[1] ?? null;
  const openSession = /^\/data\/sessions\/([^/]+)$/.exec(path)?.[1] ?? null;

  if (openClip) {
    return (
      <ClipDetailView
        id={openClip}
        canWrite={canWrite}
        canPreview={canPreview}
        onBack={() => navigate("/data")}
      />
    );
  }
  if (openSession) {
    return (
      <SessionDetailView
        id={openSession}
        canWrite={canWrite}
        onBack={() => navigate("/data")}
        onOpenClip={(clipId) => navigate(`/data/clips/${clipId}`)}
      />
    );
  }

  const query = {
    gameId: filters.gameId || undefined,
    deviceId: filters.deviceId || undefined,
    annotatorId: filters.annotatorId || undefined,
    unhealthyOnly: filters.unhealthyOnly ? "true" : undefined,
    pageSize: 50,
  };

  return (
    <Panel title="数据浏览" note="按游戏、设备、标注用户三个维度筛选控制面已登记的数据。">
      <div className="filters">
        <label>
          游戏
          <select
            value={filters.gameId}
            onChange={(event) => onFiltersChange({ ...filters, gameId: event.target.value })}
          >
            <option value="">全部</option>
            {games.status === "ready"
              ? games.value.games.map((game) => (
                  <option key={game.id} value={game.id}>
                    {game.title}（{game.clipCount}）
                  </option>
                ))
              : null}
          </select>
          {/* Without this the list just silently has one option, which reads as
              "no games exist" rather than "the list failed to load". */}
          {games.status === "error" ? (
            <span className="muted">游戏列表加载失败，暂时只能按其他条件筛选。</span>
          ) : null}
        </label>
        <label>
          设备 ID
          <input
            value={filters.deviceId}
            onChange={(event) => onFiltersChange({ ...filters, deviceId: event.target.value })}
            placeholder="uuid"
          />
        </label>
        <label>
          标注用户
          <select
            value={filters.annotatorId}
            onChange={(event) => onFiltersChange({ ...filters, annotatorId: event.target.value })}
          >
            <option value="">全部</option>
            <option value="none">未归属</option>
            {annotatorOptions.map((option) => (
              <option key={option.id} value={option.id}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        {view.kind === "clips" ? (
          <>
            <label>
              传输状态
              <select
                value={filters.ingestState}
                onChange={(event) => onFiltersChange({ ...filters, ingestState: event.target.value })}
              >
                <option value="">全部</option>
                {["pending", "uploading", "stored", "retry_wait", "failed"].map((state) => (
                  <option key={state} value={state}>
                    {state}
                  </option>
                ))}
              </select>
            </label>
            <label>
              验证状态
              <select
                value={filters.verificationState}
                onChange={(event) =>
                  onFiltersChange({ ...filters, verificationState: event.target.value })
                }
              >
                <option value="">全部</option>
                {["unverified", "queued", "verifying", "verified", "rejected"].map((state) => (
                  <option key={state} value={state}>
                    {state}
                  </option>
                ))}
              </select>
            </label>
            <label>
              健康
              <select
                value={filters.unhealthyOnly ? "unhealthy" : ""}
                onChange={(event) =>
                  onFiltersChange({ ...filters, unhealthyOnly: event.target.value === "unhealthy" })
                }
              >
                <option value="">全部</option>
                <option value="unhealthy">只看异常段</option>
              </select>
            </label>
          </>
        ) : null}
        <button className="link" onClick={() => onFiltersChange(emptyFilters)}>
          清除筛选
        </button>
      </div>

      <div className="tabs">
        <button
          className={view.kind === "sessions" ? "tab tab-active" : "tab"}
          onClick={() => setView({ kind: "sessions" })}
        >
          会话
        </button>
        <button
          className={view.kind === "clips" ? "tab tab-active" : "tab"}
          onClick={() => setView({ kind: "clips" })}
        >
          片段
        </button>
      </div>

      {view.kind === "sessions" ? (
        <SessionTable query={query} onOpen={(id) => navigate(`/data/sessions/${id}`)} />
      ) : (
        <ClipTable
          query={{
            ...query,
            ingestState: filters.ingestState || undefined,
            verificationState: filters.verificationState || undefined,
          }}
          onOpen={(id) => navigate(`/data/clips/${id}`)}
        />
      )}
    </Panel>
  );
}

function SessionTable({
  query,
  onOpen,
}: {
  query: Record<string, string | number | undefined>;
  onOpen: (id: string) => void;
}) {
  const [pages, setPages] = useState<SessionSummary[]>([]);
  const [cursor, setCursor] = useState<string | undefined>(undefined);

  // Accumulated pages and the cursor belong to one filter. Without this, changing
  // a filter kept the previous filter's rows on screen and sent its cursor with
  // the new query — the table showed a blend of two result sets.
  const queryKey = JSON.stringify(query);
  useEffect(() => {
    setPages([]);
    setCursor(undefined);
  }, [queryKey]);
  const state = useAsync(() => api.sessions({ ...query, cursor }), [queryKey, cursor]);

  if (state.status === "loading" && pages.length === 0) return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;

  const items = state.status === "ready" ? [...pages, ...state.value.items] : pages;
  if (items.length === 0) {
    return (
      <EmptyState
        title="没有匹配的会话"
        explanation="控制面只登记客户端上传过的会话。设备完成 enroll 并开始上传后，这里才会有数据。"
      />
    );
  }

  return (
    <>
      <table>
        <thead>
          <tr>
            <th>会话</th>
            <th>游戏</th>
            <th>设备</th>
            <th>标注用户</th>
            <th>状态</th>
            <th>分级</th>
            <th>run</th>
            <th>片段</th>
            <th>已验证</th>
            <th>存储量</th>
            <th>开始时间</th>
          </tr>
        </thead>
        <tbody>
          {items.map((session) => (
            <tr key={session.id} className="clickable" onClick={() => onOpen(session.id)}>
              <td>
                <Mono value={session.clientSessionId} />
              </td>
              <td>{session.gameTitle}</td>
              <td>{session.clientDeviceId}</td>
              <td>{session.annotatorAlias ?? <span className="muted">未归属</span>}</td>
              <td>
                <StateBadge label={session.status} tone={session.status === "completed" ? "ok" : "mute"} />
              </td>
              <td>
                <StateBadge
                  label={describeDataClassification(session.serverDataClassification).label}
                  tone="mute"
                />
              </td>
              <td>{session.runCount}</td>
              <td>{session.clipCount}</td>
              <td>{session.verifiedClipCount}</td>
              <td>{formatBytes(session.storedBytes)}</td>
              <td>{formatDateTime(session.startedAt)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {state.status === "ready" && state.value.nextCursor ? (
        <button
          className="link"
          onClick={() => {
            setPages(items);
            setCursor(state.value.nextCursor ?? undefined);
          }}
        >
          加载更多
        </button>
      ) : null}
    </>
  );
}

function ClipTable({
  query,
  onOpen,
}: {
  query: Record<string, string | number | undefined>;
  onOpen: (id: string) => void;
}) {
  const [pages, setPages] = useState<ClipSummary[]>([]);
  const [cursor, setCursor] = useState<string | undefined>(undefined);

  // Accumulated pages and the cursor belong to one filter. Without this, changing
  // a filter kept the previous filter's rows on screen and sent its cursor with
  // the new query — the table showed a blend of two result sets.
  const queryKey = JSON.stringify(query);
  useEffect(() => {
    setPages([]);
    setCursor(undefined);
  }, [queryKey]);
  const state = useAsync(() => api.clips({ ...query, cursor }), [queryKey, cursor]);

  if (state.status === "loading" && pages.length === 0) return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;

  const items = state.status === "ready" ? [...pages, ...state.value.items] : pages;
  if (items.length === 0) {
    return (
      <EmptyState
        title="没有匹配的片段"
        explanation="片段在客户端登记并上传后才会出现。当前筛选条件下没有记录。"
      />
    );
  }

  return (
    <>
      <table>
        <thead>
          <tr>
            <th>片段</th>
            <th>帧数</th>
            <th>时长</th>
            <th>传输</th>
            <th>验证</th>
            <th>质量</th>
            <th>留存</th>
            <th>大小</th>
            <th>可训练</th>
            <th>健康</th>
            <th>录制时间</th>
          </tr>
        </thead>
        <tbody>
          {items.map((clip) => (
            <tr key={clip.id} className="clickable" onClick={() => onOpen(clip.id)}>
              <td>
                r{clip.runSeq}/s{clip.segmentSeq}
              </td>
              <td>{clip.nFrames}</td>
              <td>{formatDuration(clip.durationSeconds)}</td>
              <td>
                <StateBadge label={clip.ingestState} tone={ingestTone(clip.ingestState)} />
              </td>
              <td>
                <StateBadge label={clip.verificationState} tone={verificationTone(clip.verificationState)} />
              </td>
              <td>
                <StateBadge label={clip.qualityState} tone={clip.qualityState === "passed" ? "ok" : "mute"} />
              </td>
              <td>
                <StateBadge label={clip.retentionState} tone={clip.retentionState === "retained" ? "ok" : "warn"} />
              </td>
              <td>{formatBytes(clip.sizeBytes)}</td>
              <td>{clip.isTrainable ? "是" : `否（${clip.blockerCount}）`}</td>
              <td>
                <HealthBadge health={clip.health} nFrames={clip.nFrames} />
              </td>
              <td>{formatDateTime(clip.recordedAt ?? clip.createdAt)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {state.status === "ready" && state.value.nextCursor ? (
        <button
          className="link"
          onClick={() => {
            setPages(items);
            setCursor(state.value.nextCursor ?? undefined);
          }}
        >
          加载更多
        </button>
      ) : null}
    </>
  );
}

function SessionDetailView({
  id,
  canWrite,
  onBack,
  onOpenClip,
}: {
  id: string;
  canWrite: boolean;
  onBack: () => void;
  onOpenClip: (clipId: string) => void;
}) {
  const [revision, setRevision] = useState(0);
  const state = useAsync(() => api.sessionDetail(id), [id, revision]);
  const clips = useAsync(() => api.clips({ sessionId: id, pageSize: 100 }), [id, revision]);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const session = state.value;

  return (
    <>
      <button className="link" onClick={onBack}>
        ← 返回列表
      </button>

      <Panel title={`会话 ${session.clientSessionId}`}>
        <div className="fields">
          <Field label="游戏">
            {session.gameTitle}（{session.gameVersion}）
          </Field>
          <Field label="设备">{session.clientDeviceId}</Field>
          <Field label="标注用户">{session.annotatorAlias ?? "未归属"}</Field>
          <Field label="状态">
            <StateBadge label={session.status} tone={session.status === "completed" ? "ok" : "mute"} />
          </Field>
          <Field label="服务端分级">
            {describeDataClassification(session.serverDataClassification).label}{" "}
            <code>{session.serverDataClassification}</code>
          </Field>
          <Field label="契约版本">{session.schemaVersion}</Field>
          <Field label="catalog 模式">{session.catalogMode ?? "—"}</Field>
          <Field label="采集">
            {session.nativeWidth}×{session.nativeHeight} @ {session.fpsNum}/{session.fpsDen}
          </Field>
          <Field label="客户端版本">{session.clientVersion}</Field>
          <Field label="开始">{formatDateTime(session.startedAt)}</Field>
          <Field label="结束">{formatDateTime(session.endedAt)}</Field>
          <Field label="停止原因">{session.stopReason ?? "—"}</Field>
          <Field label="manifest">
            <Mono value={session.manifestSha256} />
          </Field>
          <Field label="catalog manifest">
            <Mono value={session.catalogManifestSha256} />
          </Field>
          {/* 设备行只钉档案谱系，不钉版本——这次跑的是哪一版，只有会话行答得了。 */}
          <Field label="device profile 版本">
            {session.deviceProfile ? (
              <>
                {session.deviceProfile.version}
                {session.deviceProfile.status === "approved" ? "" : `（${session.deviceProfile.status}）`}{" "}
                <Mono value={session.deviceProfile.versionId} />
              </>
            ) : (
              "—"
            )}
          </Field>
        </div>
        <p className="muted">{describeDataClassification(session.serverDataClassification).note}</p>
        {session.consentId === null ? (
          <p className="muted">
            该会话没有 consent 与 grant 关联，符合 M2a 的 fail-closed 约定：数据固定为 non-trainable。
          </p>
        ) : null}
      </Panel>

      <SessionQualityPanel
        session={session}
        canWrite={canWrite}
        onDone={() => setRevision((current) => current + 1)}
      />

      <SessionAcceptancePanel sessionId={session.id} />

      <Panel
        title="采集 run"
        note="preflight 结果为客户端在启动前的本地审计结论，云端只登记引用；健康为该 run 全部片段的逐段合计。"
      >
        <table>
          <thead>
            <tr>
              <th>#</th>
              <th>状态</th>
              <th>编码器</th>
              <th>分辨率</th>
              <th>帧数</th>
              <th>ready 段</th>
              <th>停止原因</th>
              <th>健康</th>
              <th>preflight</th>
            </tr>
          </thead>
          <tbody>
            {session.runs.map((run) => (
              <tr key={run.id}>
                <td>{run.runSeq}</td>
                <td>
                  <StateBadge label={run.status} tone={run.status === "completed" ? "ok" : "warn"} />
                </td>
                <td>{run.encoderBackend}</td>
                <td>
                  {run.width}×{run.height}
                </td>
                <td>{run.frameCount ?? "—"}</td>
                <td>{run.readyClipCount ?? "—"}</td>
                <td>
                  {run.stopReason ?? "—"}
                  {run.errorCode ? ` · ${run.errorCode}` : ""}
                </td>
                <td>
                  <HealthBadge health={run.health} nFrames={run.frameCount} />
                </td>
                <td>{run.preflightResult ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Panel>

      <SessionArtifactsPanel sessionId={session.id} />

      <Panel title="片段">
        {clips.status === "ready" && clips.value.items.length > 0 ? (
          <table>
            <thead>
              <tr>
                <th>片段</th>
                <th>帧数</th>
                <th>传输</th>
                <th>验证</th>
                <th>大小</th>
              </tr>
            </thead>
            <tbody>
              {clips.value.items.map((clip) => (
                <tr key={clip.id} className="clickable" onClick={() => onOpenClip(clip.id)}>
                  <td>
                    r{clip.runSeq}/s{clip.segmentSeq}
                  </td>
                  <td>{clip.nFrames}</td>
                  <td>
                    <StateBadge label={clip.ingestState} tone={ingestTone(clip.ingestState)} />
                  </td>
                  <td>
                    <StateBadge
                      label={clip.verificationState}
                      tone={verificationTone(clip.verificationState)}
                    />
                  </td>
                  <td>{formatBytes(clip.sizeBytes)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : (
          <EmptyState title="没有片段" explanation="该会话尚未登记任何片段。" />
        )}
      </Panel>

      <AuditTrail title="会话审计" params={{ entityType: "session", entityId: session.id }} />
    </>
  );
}

/**
 * 会话级 QA 判定与可训练性（issue #855 PR 5）。
 *
 * 人实际审的是**一场录制**，不是一条 10 秒片段：按 clip 判是万级点击，按 session
 * 判是百级。所以这里是判定的默认入口，片段详情里那对按钮留作例外修正——「这场
 * 大体没问题但第 37 条废了」。
 *
 * 判据一个字没变：仍然只判通过传输验证的片段，仍然只写 `quality_state` 一列，
 * `quality_reviewed_by` 仍然如实记那位 operator。
 */
function SessionQualityPanel({
  session,
  canWrite,
  onDone,
}: {
  session: SessionDetail;
  canWrite: boolean;
  onDone: () => void;
}) {
  const { trainability } = session;
  const awaiting = trainability.awaitingQualityReviewClipCount;

  return (
    <Panel
      title="可训练性与 QA 判定"
      note="可训练性由数据库只读视图 clip_trainability 判定，console 不复算；QA 判定是这里唯一能写的一列。"
    >
      <div className="tiles">
        <div className="tile tile-mute">
          <span className="tile-label">可训练片段</span>
          <span className="tile-value">
            {trainability.trainableClipCount}/
            {trainability.trainableClipCount + trainability.blockedClipCount}
          </span>
          <span className="tile-note">现在时回看，撤回同意书会改写这个数</span>
        </div>
        <div className={`tile tile-${awaiting > 0 ? "warn" : "mute"}`}>
          <span className="tile-label">等 QA 裁决</span>
          <span className="tile-value">{awaiting}</span>
          <span className="tile-note">已通过传输验证、仍是 pending 的片段</span>
        </div>
      </div>

      {trainability.blockers.length > 0 ? (
        <>
          <h3>卡在哪些闸上</h3>
          <table>
            <thead>
              <tr>
                <th>blocker</th>
                <th>说明</th>
                <th>命中片段</th>
              </tr>
            </thead>
            <tbody>
              {trainability.blockers.map((entry) => (
                <tr key={entry.blocker}>
                  <td>
                    <code>{entry.blocker}</code>
                  </td>
                  <td>{describeTrainabilityBlocker(entry.blocker) || "—"}</td>
                  <td>{entry.clipCount}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      ) : (
        <p className="muted">该会话没有任何 blocker，或尚未登记片段。</p>
      )}

      {canWrite ? (
        <div className="action-row">
          <ConfirmedAction
            label="整场判定合格"
            title="记录质量判定：整场合格"
            confirmation={`这会把该会话下全部已通过传输验证的片段（当前 ${awaiting} 条待判）写成 quality_state=passed，并如实记录你作为审阅人。未通过传输验证的片段照旧留 pending。判定只写这一列，不改变传输、验证或留存状态，也不会让数据变为可训练。`}
            fields={[{ name: "note", label: "备注（可选）", placeholder: "整场画面正常" }]}
            run={async (values, key) => {
              const outcome = await api.reviewSession(
                session.id,
                "passed",
                values.note || undefined,
                key,
              );
              return `已判定 ${outcome.decidedClipCount} 条合格；${outcome.skippedClipCount} 条尚未通过传输验证，照旧留 pending。`;
            }}
            onDone={onDone}
          />
          <ConfirmedAction
            label="整场判定不合格"
            title="记录质量判定：整场不合格"
            confirmation="不合格判定同样只写 quality_state，不会删除任何数据。要删除请走片段详情里的删除请求。"
            fields={[{ name: "note", label: "备注（可选）", placeholder: "全场画面撕裂" }]}
            run={async (values, key) => {
              const outcome = await api.reviewSession(
                session.id,
                "failed",
                values.note || undefined,
                key,
              );
              return `已判定 ${outcome.decidedClipCount} 条不合格；${outcome.skippedClipCount} 条尚未通过传输验证，照旧留 pending。`;
            }}
            onDone={onDone}
          />
        </div>
      ) : null}
      <p className="muted">
        判定按会话做，例外在片段详情里逐条修正。两个入口写的是同一列、走同一条审计路径，
        审阅人永远是发起判定的 operator。
      </p>
    </Panel>
  );
}

function ClipDetailView({
  id,
  canWrite,
  canPreview,
  onBack,
}: {
  id: string;
  canWrite: boolean;
  canPreview: boolean;
  onBack: () => void;
}) {
  const [revision, setRevision] = useState(0);
  const state = useAsync(() => api.clip(id), [id, revision]);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const clip = state.value;

  return (
    <>
      <button className="link" onClick={onBack}>
        ← 返回
      </button>

      <Panel title={`片段 r${clip.runSeq}/s${clip.segmentSeq}`}>
        <div className="fields">
          <Field label="ID">
            <Mono value={clip.id} />
          </Field>
          <Field label="revision">{clip.revision}</Field>
          <Field label="帧数">{clip.nFrames}</Field>
          <Field label="时长">{formatDuration(clip.durationSeconds)}</Field>
          <Field label="事件数">{clip.eventCount}</Field>
          <Field label="会话起始帧">{clip.sessionFrameStart}</Field>
          <Field label="QPC 区间">
            {clip.qpcStart} → {clip.qpcEnd}
          </Field>
          <Field label="留存到期">{formatDateTime(clip.retentionDeadline)}</Field>
        </div>
      </Panel>

      <Panel title="四维状态">
        <div className="tiles">
          <div className="tile tile-mute">
            <span className="tile-label">传输</span>
            <span className="tile-value">
              <StateBadge label={clip.ingestState} tone={ingestTone(clip.ingestState)} />
            </span>
            <span className="tile-note">{formatDateTime(clip.storedAt)}</span>
          </div>
          <div className="tile tile-mute">
            <span className="tile-label">验证</span>
            <span className="tile-value">
              <StateBadge label={clip.verificationState} tone={verificationTone(clip.verificationState)} />
            </span>
            <span className="tile-note">{clip.rejectedReason ?? formatDateTime(clip.verifiedAt)}</span>
          </div>
          <div className="tile tile-mute">
            <span className="tile-label">质量</span>
            <span className="tile-value">
              <StateBadge label={clip.qualityState} tone={clip.qualityState === "passed" ? "ok" : "mute"} />
            </span>
            <span className="tile-note">由 operator 人工裁决</span>
          </div>
          <div className="tile tile-mute">
            <span className="tile-label">留存</span>
            <span className="tile-value">
              <StateBadge label={clip.retentionState} tone={clip.retentionState === "retained" ? "ok" : "warn"} />
            </span>
          </div>
        </div>
        <p className="muted">
          验证通过只代表传输完整性（对象 SHA-256 与大小一致），不代表内容质量合格。
        </p>
        {canWrite ? (
          <div className="action-row">
            <ConfirmedAction
              label="判定合格"
              title="记录质量判定：合格"
              confirmation="逐片段判定是例外入口——日常请在会话详情里按整场判定。它只写 quality_state 一列，不改变传输、验证或留存状态，也不会让数据变为可训练。"
              run={async (_values, key) => {
                await api.reviewClip(id, "passed", undefined, key);
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
            <ConfirmedAction
              label="判定不合格"
              title="记录质量判定：不合格"
              confirmation="逐片段的不合格判定同样只写 quality_state，不会删除任何数据。用它修正整场判定里的个别例外。"
              run={async (_values, key) => {
                await api.reviewClip(id, "failed", undefined, key);
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
            <ConfirmedAction
              label="请求删除"
              title="请求删除该片段"
              confirmation="这会把片段标记为待删除并投递一个 purge 任务。worker 执行后 R2 字节不可恢复；SQL 墓碑与审计记录会保留。"
              destructive
              fields={[{ name: "reason", label: "原因", placeholder: "consent withdrawn" }]}
              run={async (values, key) => {
                const outcome = await api.purgeClip(id, values.reason ?? "", key);
                return outcome.enqueued ? "已投递删除任务。" : "该片段已有进行中的删除任务。";
              }}
              onDone={() => setRevision((current) => current + 1)}
            />
          </div>
        ) : null}
      </Panel>

      <Panel title="可训练性" note="由数据库只读视图 clip_trainability 判定，console 不复算。">
        <p>
          <strong>{clip.isTrainable ? "可训练" : "不可训练"}</strong>
          {clip.blockers.length > 0 ? `（${clip.blockers.length} 项阻塞）` : null}
        </p>
        {clip.blockers.length > 0 ? (
          <ul className="blockers">
            {clip.blockers.map((blocker) => (
              <li key={blocker}>
                <code>{blocker}</code>
                {describeTrainabilityBlocker(blocker) ? (
                  <span className="muted"> · {describeTrainabilityBlocker(blocker)}</span>
                ) : null}
              </li>
            ))}
          </ul>
        ) : null}
      </Panel>

      <Panel
        title="采集健康"
        note="客户端封段时点的事实，随片段一起登记。丢帧/溢出非零、或重复帧超过帧数 1% 就值得看画面；「源被顶替」是 CFR 选帧的正常计数（docs/146），只作诊断展示。"
      >
        <div className="tiles">
          <HealthTile
            label="重复帧"
            value={clip.health.duplicateFrames}
            warn={clip.health.duplicateFrames > DUP_MAX_FRAC * clip.nFrames}
          />
          <HealthTile label="源被顶替" value={clip.health.sourceSuperseded} warn={false} />
          <HealthTile label="采集队列丢帧" value={clip.health.queueDrops} />
          <HealthTile label="输入队列溢出" value={clip.health.inputOverflow} />
        </div>
        <p className="muted">
          录制时间 {formatDateTime(clip.recordedAt)} · 登记时间 {formatDateTime(clip.createdAt)}
        </p>
      </Panel>

      <Panel title="五件套对象">
        <table>
          <thead>
            <tr>
              <th>类型</th>
              <th>对象键</th>
              <th>大小</th>
              <th>SHA-256</th>
              <th>存储</th>
              <th>验证</th>
            </tr>
          </thead>
          <tbody>
            {clip.artifacts.map((artifact) => (
              <tr key={artifact.kind}>
                <td>{artifact.kind}</td>
                <td className="details-cell">
                  <Mono value={artifact.objectKey} />
                </td>
                <td>{formatBytes(artifact.expectedSizeBytes)}</td>
                <td>
                  <Mono value={`${artifact.expectedSha256.slice(0, 20)}…`} />
                </td>
                <td>
                  <StateBadge label={artifact.storageState} tone={ingestTone(artifact.storageState)} />
                </td>
                <td>
                  <StateBadge
                    label={artifact.verificationState}
                    tone={verificationTone(artifact.verificationState)}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <ClipPreview clipId={id} canPreview={canPreview} />
        <ReviewUnitLink
          objectKey={clip.artifacts.find((artifact) => artifact.kind === "video")?.objectKey ?? null}
          bucketName={clip.artifacts.find((artifact) => artifact.kind === "video")?.bucketName ?? null}
        />
      </Panel>

      {clip.verificationJobs.length > 0 ? (
        <Panel title="验证历史">
          <table>
            <thead>
              <tr>
                <th>对象</th>
                <th>状态</th>
                <th>尝试</th>
                <th>错误</th>
                <th>更新时间</th>
              </tr>
            </thead>
            <tbody>
              {clip.verificationJobs.map((job) => (
                <tr key={job.id}>
                  <td>{job.artifactKind}</td>
                  <td>{job.status}</td>
                  <td>{job.attempts}</td>
                  <td>{job.lastErrorCode ?? "—"}</td>
                  <td>{formatDateTime(job.updatedAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Panel>
      ) : null}

      <Panel title="元数据">
        <div className="split">
          <div>
            <h3>meta</h3>
            <JsonBlock value={clip.meta} />
          </div>
          <div>
            <h3>manifest</h3>
            <JsonBlock value={clip.manifest} />
          </div>
        </div>
      </Panel>

      <AuditTrail title="片段审计" params={{ entityType: "clip", entityId: clip.id }} />
    </>
  );
}


/**
 * 会话验收栏（docs/136 §4）。展示 accept.py 落在 R2 session 前缀下的验收报告：
 * PASS/FAIL、game_id、小时数、clip 完整性与 QC 逐判据读数。
 *
 * 三个非 200 状态都是常态而不是故障，逐一区分：404 = 验收 cron（每 2h）还没
 * 扫到这个 session，灰态「未验收」；503 = 部署没配 ACCEPTANCE_R2_*，明确提示
 * 配置缺失而不是伪装成未验收；其余才是真错误。
 *
 * 报告是缓存不是判决（docs/136 §4 末条）：与 clip meta 冲突时以 meta 为准。
 */
type AcceptanceState =
  | { status: "loading" }
  | { status: "ready"; report: AcceptanceReport }
  | { status: "absent" }
  | { status: "unconfigured" }
  | { status: "error"; message: string };

function SessionAcceptancePanel({ sessionId }: { sessionId: string }) {
  const [state, setState] = useState<AcceptanceState>({ status: "loading" });

  // 懒加载：本组件只在会话详情里挂载，进入详情才发这一请求。
  useEffect(() => {
    let live = true;
    setState({ status: "loading" });
    api
      .sessionAcceptance(sessionId)
      .then((report) => {
        if (live) setState({ status: "ready", report });
      })
      .catch((thrown: unknown) => {
        if (!live) return;
        if (thrown instanceof ApiError && thrown.status === 404) {
          setState({ status: "absent" });
        } else if (thrown instanceof ApiError && thrown.status === 503) {
          setState({ status: "unconfigured" });
        } else {
          setState({ status: "error", message: describeApiError(thrown, "验收报告加载失败。") });
        }
      });
    return () => {
      live = false;
    };
  }, [sessionId]);

  return (
    <Panel
      title="验收"
      note="上传完成后由验收管线自动生成（docs/136）：六件套完整性、game_id 铁律、QC floor 档逐判据、小时口径与健康位。报告是缓存不是判决，与 clip meta 冲突时以 meta 为准。"
    >
      {state.status === "loading" ? <Loading /> : null}
      {state.status === "absent" ? (
        <EmptyState
          title="未验收"
          explanation="验收 cron 每 2 小时扫一轮「已完成且未验收」的会话；刚上传完的会话最多要等一轮。没有报告不代表数据有问题。"
        />
      ) : null}
      {state.status === "unconfigured" ? (
        <ErrorNote message="该部署没有配置验收报告存储（ACCEPTANCE_R2_*），验收状态未知——这是配置缺失，不代表该会话未验收。" />
      ) : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" ? <AcceptanceReportView report={state.report} /> : null}
    </Panel>
  );
}

function AcceptanceReportView({ report }: { report: AcceptanceReport }) {
  const health = report.health;
  const healthIssues = [
    health.duplicate_frames > 0 ? `重复帧 ${health.duplicate_frames}` : null,
    health.queue_drops > 0 ? `丢帧 ${health.queue_drops}` : null,
    health.input_overflow > 0 ? `输入溢出 ${health.input_overflow}` : null,
    health.empty_events_clips > 0 ? `空 events 段 ${health.empty_events_clips}` : null,
    health.meta_unreadable > 0 ? `meta 不可读 ${health.meta_unreadable}` : null,
    health.manifest_mismatch > 0 ? `manifest 不符 ${health.manifest_mismatch}` : null,
  ].filter((entry): entry is string => entry !== null);

  return (
    <>
      <div className="fields">
        <Field label="结论">
          <StateBadge label={report.passed ? "PASS" : "FAIL"} tone={report.passed ? "ok" : "bad"} />
        </Field>
        <Field label="game_id">
          <Mono value={report.game_id} />
          {report.mixed_session ? (
            <>
              {" "}
              <StateBadge label="混游戏" tone="warn" />
            </>
          ) : null}
        </Field>
        <Field label="小时数">{report.hours.toFixed(2)} h</Field>
        <Field label="完整片段">
          {report.clips_complete}/{report.clips_total}
        </Field>
        <Field label="含音频片段">{report.audio_present_clips}</Field>
        <Field label="QC">
          {report.qc.ran ? `已跑（${report.qc.tier ?? "?"} 档）` : "未跑"}
        </Field>
      </div>

      {healthIssues.length > 0 ? (
        <p>
          <StateBadge label={healthIssues.join(" · ")} tone="warn" />
        </p>
      ) : null}

      {report.findings.length > 0 ? (
        <>
          <h3>findings</h3>
          <ul className="blockers">
            {report.findings.map((finding) => (
              <li key={finding}>
                <code>{finding}</code>
              </li>
            ))}
          </ul>
        </>
      ) : null}

      {report.qc.ran && report.qc.checks && report.qc.checks.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>判据</th>
              <th>读数</th>
              <th>结果</th>
            </tr>
          </thead>
          <tbody>
            {report.qc.checks.map((check) => (
              // enabled=false 的判据整行置灰：读数留着（可事后换档重判），但
              // 它没有参与本次放行结论。
              <tr key={check.id} className={check.enabled === false ? "muted" : undefined}>
                <td>
                  <Mono value={check.id} />
                </td>
                <td>{formatQcReading(check)}</td>
                <td>
                  {check.enabled === false ? (
                    <StateBadge label="未启用" tone="mute" />
                  ) : (
                    <StateBadge
                      label={check.passed ? "通过" : "未过"}
                      tone={check.passed ? "ok" : "bad"}
                    />
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
      {!report.qc.ran ? (
        <p className="muted">QC 未运行{report.qc.reason ? `：${report.qc.reason}` : ""}。</p>
      ) : null}
    </>
  );
}

/** 读数可能是数字、字符串或结构（以真实报告为准），逐类兜底成可读文本。 */
function formatQcReading(check: AcceptanceQcCheck): string {
  const reading = check.reading;
  if (reading === undefined || reading === null) return "—";
  if (typeof reading === "number") {
    return Number.isInteger(reading) ? String(reading) : reading.toFixed(4);
  }
  if (typeof reading === "string") return reading;
  return JSON.stringify(reading);
}

/**
 * session 根快照（docs/47/67 §3.6）。摄取面从第一天就在写这张表，console 从来
 * 没读过——「这次会话的目录/身份/mapping 快照有没有上来」在界面上是查不到的。
 *
 * 注意 trainable 车道当前只收 `input_calibration` 一件（apps/api service.ts），
 * 所以 certified 会话在这里天然只会有一行；那是摄取侧的准入策略，不是这里漏读。
 */
function SessionArtifactsPanel({ sessionId }: { sessionId: string }) {
  const state = useAsync(() => api.sessionArtifacts(sessionId), [sessionId]);

  return (
    <Panel
      title="session 根快照"
      note="目录/设备/录制/游戏/mapping 身份快照与 markers、标定证据。不参与 clip 可训练性判定，是审计与复现证据。"
    >
      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" && state.value.length === 0 ? (
        <p className="muted">
          该会话没有登记任何根快照。non-trainable 车道要求六件一次性申报齐；trainable
          车道当前只接受 input_calibration 一件。
        </p>
      ) : null}
      {state.status === "ready" && state.value.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>类型</th>
              <th>对象键</th>
              <th>大小</th>
              <th>传输</th>
              <th>验证</th>
            </tr>
          </thead>
          <tbody>
            {state.value.map((artifact) => (
              <tr key={artifact.kind}>
                <td>{artifact.kind}</td>
                <td className="details-cell">
                  <Mono value={artifact.objectKey} />
                </td>
                <td>{formatBytes(artifact.expectedSizeBytes)}</td>
                <td>
                  <StateBadge
                    label={artifact.ingestState}
                    tone={ingestTone(artifact.ingestState)}
                  />
                </td>
                <td>
                  <StateBadge
                    label={artifact.verificationState}
                    tone={verificationTone(artifact.verificationState)}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}

/** 补帧占比的警示线，与判定式同源（browse-repository 的 `DUP_MAX_FRAC`，docs/146）。 */
const DUP_MAX_FRAC = 0.01;

/**
 * 健康计数的行内徽章。警示只给判定式认的三样：丢帧、输入溢出、补帧占比越线
 * （docs/146）。`sourceSuperseded` 是 CFR 选帧的正常计数，生产段几乎恒非零，
 * 亮黄等于全表黄——非零时以静默文本列出，只作诊断。`nFrames` 不可知时
 * （老 run 聚合行）补帧退回「非零即警示」，宁可多看一眼画面。
 */
function HealthBadge({ health, nFrames }: { health: ClipHealth; nFrames?: number | null }) {
  const duplicateExcess =
    nFrames != null ? health.duplicateFrames > DUP_MAX_FRAC * nFrames : health.duplicateFrames > 0;
  const issues = [
    health.queueDrops > 0 ? `丢帧 ${health.queueDrops}` : null,
    duplicateExcess ? `重复 ${health.duplicateFrames}` : null,
    health.inputOverflow > 0 ? `输入溢出 ${health.inputOverflow}` : null,
  ].filter((entry): entry is string => entry !== null);
  const diagnostics = [
    !duplicateExcess && health.duplicateFrames > 0 ? `重复 ${health.duplicateFrames}` : null,
    health.sourceSuperseded > 0 ? `顶替 ${health.sourceSuperseded}` : null,
  ].filter((entry): entry is string => entry !== null);
  if (issues.length === 0 && diagnostics.length === 0) return <span className="muted">—</span>;
  return (
    <>
      {issues.length > 0 ? <StateBadge label={issues.join(" · ")} tone="warn" /> : null}
      {diagnostics.length > 0 ? (
        <span className="muted">{`${issues.length > 0 ? " " : ""}${diagnostics.join(" · ")}`}</span>
      ) : null}
    </>
  );
}

function HealthTile({ label, value, warn }: { label: string; value: number; warn?: boolean }) {
  return (
    <div className={`tile tile-${(warn ?? value > 0) ? "warn" : "mute"}`}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
    </div>
  );
}

/**
 * 从片段跳到数据审查面的同一目录（docs/67 §3.5）。此前两本质量账是断开的：
 * 能看画面的地方（审查面）按对象键浏览，能判定 quality_state 的地方（本页）
 * 只有视频预览。这条链把「看得见的」和「判得了的」接上。
 *
 * 审查源按桶 + 前缀匹配；配不上就不显示——宁可少一条入口，也不给一个点开
 * 必然 403/404 的链接。
 */
function ReviewUnitLink({
  objectKey,
  bucketName,
}: {
  objectKey: string | null;
  bucketName: string | null;
}) {
  const sources = useAsync(() => api.reviewSources(), []);
  if (!objectKey || !bucketName) return null;
  if (sources.status !== "ready") return null;

  const directory = objectKey.slice(0, objectKey.lastIndexOf("/") + 1);
  const source = sources.value.sources.find(
    (candidate) =>
      candidate.bucket === bucketName &&
      candidate.prefixes.some((prefix) => directory.startsWith(prefix)),
  );
  if (!source) return null;

  return (
    <p>
      <Link to={`/review/${source.id}/unit?prefix=${encodeURIComponent(directory)}`}>
        在数据审查中逐帧打开 →
      </Link>
    </p>
  );
}

/**
 * The signed URL is a bearer token for those bytes: it is requested only on an
 * explicit click, held in memory for the life of the page, and never persisted
 * or put in the address bar.
 */
function ClipPreview({ clipId, canPreview }: { clipId: string; canPreview: boolean }) {
  const [grant, setGrant] = useState<PreviewGrant | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (!canPreview) {
    return <p className="muted">当前角色没有预览权限，本页只展示元数据。</p>;
  }

  async function load() {
    setBusy(true);
    setError(null);
    try {
      setGrant(await api.previewClip(clipId));
    } catch (thrown) {
      if (thrown instanceof ApiError && thrown.code === "preview_not_configured") {
        setError("该部署没有配置只读对象凭据，预览不可用。");
      } else if (thrown instanceof ApiError) {
        setError(`${thrown.code}：${thrown.message}`);
      } else {
        setError("预览请求失败。");
      }
    } finally {
      setBusy(false);
    }
  }

  if (grant) {
    return (
      <div className="preview">
        <video className="preview-player" src={grant.url} controls preload="metadata" />
        <p className="muted">
          链接 {formatDateTime(grant.expiresAt)} 过期，本次签发已记入审计。刷新页面后需重新签发。
        </p>
      </div>
    );
  }

  return (
    <div className="preview">
      <button className="link" onClick={() => void load()} disabled={busy}>
        {busy ? "签发中…" : "预览视频（签发短期只读链接，逐次审计）"}
      </button>
      {error ? <ErrorNote message={error} /> : null}
      <p className="muted">
        预览只签发 video 一个对象，有效期数分钟；其余四类产物不提供字节访问。
      </p>
    </div>
  );
}
