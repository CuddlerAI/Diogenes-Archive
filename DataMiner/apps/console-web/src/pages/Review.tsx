import { useEffect, useMemo, useState } from "react";

import {
  ApiError,
  api,
  describeApiError,
  newIdempotencyKey,
  type HfSubjectStatus,
  type ReviewFramesResult,
  type ReviewObjectInfo,
  type ReviewSource,
  type ReviewTreeObject,
  type ReviewTreePage,
  type ReviewVerdict,
  type ReviewVideoGrant,
} from "../api.js";
import { EmptyState, ErrorNote, Loading, Mono, Panel, StateBadge } from "../components.js";
import { DepthViewer } from "../components/DepthViewer.js";
import { FrameReviewer } from "../components/FrameReviewer.js";
import { navigate, useRoute, withQuery } from "../router.js";
import { useAsync } from "../useAsync.js";

/**
 * 数据审查 tab (docs/33 §3.2/§4): browse the allowlisted prefixes of the two
 * R2 buckets, recognise reviewable units (a directory holding an .mp4 — a RAW
 * segment or a dm-1 chunk), and open them in the frame reviewer. Byte access
 * happens object-by-object on explicit action and every exit is audited
 * server-side; this page never persists a signed URL.
 */

function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function baseName(path: string): string {
  const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
  return trimmed.slice(trimmed.lastIndexOf("/") + 1);
}

/** A directory listing is a reviewable unit when it holds a playable video and
 * a frames object — the RAW five-piece, a dm-1 chunk, or a dataminer-api clip
 * whose artifacts carry content-addressed names (`frames-<sha256>.jsonl.zst`,
 * docs/33 §12). */
function detectUnit(objects: ReviewTreeObject[]): { videoKey: string; framesKey: string } | null {
  const videoKey = objects.find((object) => object.key.endsWith(".mp4"))?.key;
  const framesKey = objects.find(
    (object) =>
      object.key.endsWith("frames.jsonl.zst") ||
      object.key.endsWith("/p0.jsonl") ||
      /\/frames-[0-9a-f]{64}\.jsonl\.zst$/.test(object.key),
  )?.key;
  if (!videoKey || !framesKey) return null;
  return { videoKey, framesKey };
}

/** A depth capture unit (docs/77): a directory holding the probe's
 * `depth-index.jsonl`. The `.f32` frames live one level down under `raw/`, so
 * the index is the only marker visible in this listing. */
function detectDepthUnit(objects: ReviewTreeObject[]): boolean {
  return objects.some((object) => object.key.endsWith("/depth-index.jsonl"));
}

/** A viz artifact group (docs/104 §3): a directory under viz/ whose skill
 * upload finished — the manifest is written last, so its presence means every
 * artifact it lists is already in place. */
function detectVizGroup(objects: ReviewTreeObject[]): string | null {
  return objects.find((object) => object.key.endsWith("/viz-manifest.json"))?.key ?? null;
}

type VizManifest = {
  schema: string;
  group: string;
  generated_at: string;
  note?: string;
  artifacts: Array<{
    name: string;
    bytes: number;
    sha256: string;
    content_type: string;
    kind: "viewer" | "video" | "mesh" | "cloud" | "stats" | "other";
  }>;
};

const VIZ_KIND_LABEL: Record<string, string> = {
  viewer: "查看器",
  video: "成片",
  mesh: "网格",
  cloud: "点云",
  stats: "统计",
  other: "文件",
};

export function Review({ canWrite }: { canWrite: boolean }) {
  const sources = useAsync(() => api.reviewSources(), []);
  // Location lives in the URL (docs/53 §2): /review/:source browses with
  // ?prefix=<directory>, /review/:source/unit opens the frame reviewer on the
  // unit directory in ?prefix. A review unit is thereby a shareable link; the
  // bytes behind it are still re-signed and audited per visit.
  const { path, query } = useRoute();
  const segments = path.split("/").filter(Boolean);
  const sourceId = segments[1] ?? null;
  const isUnit = segments[2] === "unit";
  const isDepth = segments[2] === "depth";
  const prefix = query.get("prefix") ?? "";

  if (sources.status === "loading") return <Loading />;
  if (sources.status === "error") return <ErrorNote message={sources.message} />;
  const sourceList = sources.value.sources;
  if (sourceList.length === 0) {
    return (
      <EmptyState
        title="审查面未配置"
        explanation="该部署没有配置 CONSOLE_REVIEW_*（docs/33 M0）。/readyz 的 features.review 为 false。"
      />
    );
  }
  const source = sourceList.find((entry) => entry.id === sourceId) ?? sourceList[0]!;
  const browseUrl = (nextPrefix: string) =>
    withQuery(`/review/${source.id}`, { prefix: nextPrefix || undefined });
  const unitUrl = (unitPrefix: string) =>
    withQuery(`/review/${source.id}/unit`, { prefix: unitPrefix });
  const depthUrl = (unitPrefix: string) =>
    withQuery(`/review/${source.id}/depth`, { prefix: unitPrefix });
  const parentOf = (unitPrefix: string) => {
    const withoutSlash = unitPrefix.slice(0, -1);
    return browseUrl(withoutSlash.slice(0, withoutSlash.lastIndexOf("/") + 1));
  };

  if (isDepth && prefix) {
    return (
      <Panel
        title="深度查看器"
        note="dm-depth-probe 原始深度缓冲（docs/77）。逐帧按需取字节，每次访问都记入审计。"
      >
        <DepthViewer
          sourceId={source.id}
          unitPrefix={prefix}
          onClose={() => navigate(parentOf(prefix))}
        />
      </Panel>
    );
  }

  if (isUnit && prefix) {
    return (
      <ReviewerView
        source={source}
        unitPrefix={prefix}
        canWrite={canWrite}
        onNavigate={(next) => navigate(unitUrl(next))}
        // Back to browsing the unit's parent directory.
        onClose={() => navigate(parentOf(prefix))}
      />
    );
  }

  return (
    <Panel
      title="数据审查"
      note="双桶只读浏览（前缀白名单）。目录含视频与帧数据时可打开逐帧审查器；每一次字节访问都会记入审计。"
    >
      <div className="review-toolbar">
        <label>
          数据源{" "}
          <select
            value={source.id}
            onChange={(event) => navigate(`/review/${event.target.value}`)}
          >
            {sourceList.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.id}（{entry.bucket}）
              </option>
            ))}
          </select>
        </label>
        {source.dbAuthority === "none" ? (
          <StateBadge label="登记于 e2e 环境库（本 console 不接）" tone="mute" />
        ) : null}
        {canWrite ? <HfReconcileButton /> : null}
      </div>
      <TreeView
        source={source}
        prefix={prefix}
        onOpenDirectory={(next) => navigate(browseUrl(next))}
        onOpenUnit={(unit) => navigate(unitUrl(unit))}
        onOpenDepth={(unit) => navigate(depthUrl(unit))}
      />
    </Panel>
  );
}

function Breadcrumbs({ prefix, onNavigate }: { prefix: string; onNavigate: (prefix: string) => void }) {
  const parts = prefix.split("/").filter((part) => part.length > 0);
  return (
    <div className="review-breadcrumbs">
      <button className="link" onClick={() => onNavigate("")}>
        根
      </button>
      {parts.map((part, index) => {
        const target = `${parts.slice(0, index + 1).join("/")}/`;
        return (
          <span key={target}>
            {" / "}
            <button className="link" onClick={() => onNavigate(target)}>
              {part}
            </button>
          </span>
        );
      })}
    </div>
  );
}

function TreeView({
  source,
  prefix,
  onOpenDirectory,
  onOpenUnit,
  onOpenDepth,
}: {
  source: ReviewSource;
  prefix: string;
  onOpenDirectory: (prefix: string) => void;
  onOpenUnit: (unitPrefix: string) => void;
  onOpenDepth: (unitPrefix: string) => void;
}) {
  // Accumulated pages: nextCursor appends rather than replaces, and the key
  // reset on source/prefix change mirrors DataBrowser's pagination fix.
  const [pages, setPages] = useState<ReviewTreePage[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    setPages([]);
    setError(null);
    setBusy(true);
    api
      .reviewTree(source.id, prefix)
      .then((page) => {
        if (live) setPages([page]);
      })
      .catch((thrown: unknown) => {
        if (live) setError(describeApiError(thrown));
      })
      .finally(() => {
        if (live) setBusy(false);
      });
    return () => {
      live = false;
    };
  }, [source.id, prefix]);

  const directories = useMemo(() => pages.flatMap((page) => page.directories), [pages]);
  const objects = useMemo(() => pages.flatMap((page) => page.objects), [pages]);
  const nextCursor = pages.at(-1)?.nextCursor ?? null;
  const unitHere = detectUnit(objects);
  const depthHere = detectDepthUnit(objects);
  const vizManifestKey = detectVizGroup(objects);

  async function loadMore() {
    if (!nextCursor) return;
    setBusy(true);
    try {
      setPages([...pages, await api.reviewTree(source.id, prefix, nextCursor)]);
    } catch (thrown) {
      setError(describeApiError(thrown));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="review-tree">
      <Breadcrumbs prefix={prefix} onNavigate={onOpenDirectory} />
      {error ? <ErrorNote message={error} /> : null}
      {unitHere ? (
        <p>
          <button className="primary" onClick={() => onOpenUnit(prefix)}>
            打开逐帧审查器
          </button>
        </p>
      ) : null}
      {depthHere ? (
        <p>
          <button className="primary" onClick={() => onOpenDepth(prefix)}>
            打开深度查看器
          </button>
        </p>
      ) : null}
      {vizManifestKey ? <VizGroupCard source={source} manifestKey={vizManifestKey} /> : null}
      <ul className="review-list">
        {directories.map((directory) => (
          <li key={directory}>
            <button className="link" onClick={() => onOpenDirectory(directory)}>
              📁 {baseName(directory)}/
            </button>
          </li>
        ))}
        {objects.map((object) => (
          <li key={object.key}>
            <span className="review-object">
              {baseName(object.key)} <span className="muted">{formatBytes(object.sizeBytes)}</span>
            </span>
          </li>
        ))}
      </ul>
      {busy ? <Loading /> : null}
      {!busy && directories.length === 0 && objects.length === 0 && !error ? (
        <EmptyState title="空目录" explanation="该前缀下没有对象。" />
      ) : null}
      {nextCursor ? (
        <button className="link" onClick={() => void loadMore()} disabled={busy}>
          加载更多…
        </button>
      ) : null}
    </div>
  );
}

/** 空间可视化卡片 (docs/104 §3/§5): renders the artifact rows of a finished viz
 * group. Every open/download presigns on click via viz-url (audited
 * server-side) and hands the URL straight to a new tab — it is never stored.
 * GLB rides to the mesh viewer in the URL fragment because a presigned URL
 * cannot carry extra query params (SigV4 signs the whole query string). */
function VizGroupCard({ source, manifestKey }: { source: ReviewSource; manifestKey: string }) {
  const groupPrefix = manifestKey.slice(0, manifestKey.lastIndexOf("/") + 1);
  const manifest = useAsync(async () => {
    const result = await api.reviewMeta(source.id, manifestKey);
    return result.meta as VizManifest;
  }, [source.id, manifestKey]);
  const [error, setError] = useState<string | null>(null);
  const [busyName, setBusyName] = useState<string | null>(null);

  if (manifest.status === "loading") return <Loading />;
  if (manifest.status === "error") return <ErrorNote message={manifest.message} />;
  const artifacts = Array.isArray(manifest.value.artifacts) ? manifest.value.artifacts : [];
  const meshViewer = artifacts.find((entry) => entry.name === "mesh_viewer.html") ?? null;

  async function open(name: string, kind: string) {
    setBusyName(name);
    setError(null);
    // window.open must run synchronously inside the click gesture: after an
    // await the gesture context is gone and browsers silently block the popup
    // (production smoke caught this — presign 200 but no tab). Open a blank
    // window now, sever opener, and point it at the signed URL once we have
    // it. "noopener" as a feature string is not usable here: it makes
    // window.open return null by spec.
    const pending = window.open("about:blank", "_blank");
    if (pending) pending.opener = null;
    try {
      const grant = await api.reviewVizUrl(source.id, `${groupPrefix}${name}`);
      let target = grant.url;
      if (kind === "mesh" && name.endsWith(".glb") && meshViewer) {
        const viewerGrant = await api.reviewVizUrl(source.id, `${groupPrefix}${meshViewer.name}`);
        target = `${viewerGrant.url}#glb=${encodeURIComponent(grant.url)}`;
      }
      if (pending) {
        pending.location.replace(target);
      } else {
        // popup denied even synchronously — surface it instead of failing mute
        setError("浏览器拦截了新标签页；请允许本站弹出窗口后重试");
      }
    } catch (thrown) {
      if (pending) pending.close();
      setError(describeApiError(thrown));
    } finally {
      setBusyName(null);
    }
  }

  return (
    <Panel title={`空间可视化 · ${manifest.value.group}`}>
      {manifest.value.note ? <p className="muted">{manifest.value.note}</p> : null}
      {error ? <ErrorNote message={error} /> : null}
      <ul className="review-list">
        {artifacts
          .filter((entry) => entry.name !== "mesh_viewer.html" && entry.kind !== "stats")
          .map((entry) => (
            <li key={entry.name}>
              <span className="review-object">
                {VIZ_KIND_LABEL[entry.kind] ?? entry.kind} · {entry.name}{" "}
                <span className="muted">{formatBytes(entry.bytes)}</span>
              </span>{" "}
              <button
                className="link"
                disabled={busyName !== null}
                onClick={() => void open(entry.name, entry.kind)}
              >
                {busyName === entry.name
                  ? "签名中…"
                  : entry.kind === "viewer" || entry.kind === "video"
                    ? "打开"
                    : entry.kind === "mesh" && entry.name.endsWith(".glb") && meshViewer
                      ? "打开网格查看器"
                      : "下载"}
              </button>
            </li>
          ))}
      </ul>
      <p className="muted">
        生成于 {manifest.value.generated_at}；产物由 depth-delivery-viz skill 预生成（docs/104），
        链接为限时签名，不入库。
      </p>
    </Panel>
  );
}

function muLawSchemeOf(meta: unknown): string | null {
  if (typeof meta !== "object" || meta === null) return null;
  const record = meta as Record<string, unknown>;
  if (typeof record.mu_law_scheme === "string") return record.mu_law_scheme;
  // dm-1 chunks declare the key schema instead; show it rather than nothing.
  if (typeof record.key_schema === "string") return "dm1-v2（据 key_schema）";
  return null;
}

function ReviewerView({
  source,
  unitPrefix,
  canWrite,
  onNavigate,
  onClose,
}: {
  source: ReviewSource;
  unitPrefix: string;
  canWrite: boolean;
  onNavigate: (prefix: string) => void;
  onClose: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [frames, setFrames] = useState<ReviewFramesResult | null>(null);
  const [grant, setGrant] = useState<ReviewVideoGrant | null>(null);
  const [objectInfo, setObjectInfo] = useState<ReviewObjectInfo | null>(null);
  // Same-parent directories, for 上一段/下一段 across segments or chunks.
  const [siblings, setSiblings] = useState<string[]>([]);

  useEffect(() => {
    let live = true;
    setError(null);
    setFrames(null);
    setGrant(null);
    setObjectInfo(null);
    (async () => {
      const listing = await api.reviewTree(source.id, unitPrefix);
      const unit = detectUnit(listing.objects);
      if (!unit) throw new Error("该目录不含可审查的视频+帧数据组合。");
      const [framesResult, grantResult] = await Promise.all([
        api.reviewFrames(source.id, unit.framesKey),
        api.reviewVideoUrl(source.id, unit.videoKey),
      ]);
      if (!live) return;
      setFrames(framesResult);
      setGrant(grantResult);
      if (source.dbAuthority === "production") {
        // Best-effort registration cross-check; failure here must not block review.
        api
          .reviewObject(source.id, unit.videoKey)
          .then((info) => {
            if (live) setObjectInfo(info);
          })
          .catch(() => {});
      }
    })().catch((thrown: unknown) => {
      if (live) setError(thrown instanceof Error && !("code" in thrown) ? thrown.message : describeApiError(thrown));
    });
    return () => {
      live = false;
    };
  }, [source.id, source.dbAuthority, unitPrefix]);

  useEffect(() => {
    let live = true;
    const withoutSlash = unitPrefix.slice(0, -1);
    const parent = withoutSlash.slice(0, withoutSlash.lastIndexOf("/") + 1);
    if (!parent) {
      setSiblings([]);
      return;
    }
    api
      .reviewTree(source.id, parent)
      .then((page) => {
        if (live) setSiblings(page.directories);
      })
      .catch(() => {
        if (live) setSiblings([]);
      });
    return () => {
      live = false;
    };
  }, [source.id, unitPrefix]);

  const siblingIndex = siblings.indexOf(unitPrefix);
  const previous = siblingIndex > 0 ? siblings[siblingIndex - 1] : null;
  const next = siblingIndex >= 0 && siblingIndex < siblings.length - 1 ? siblings[siblingIndex + 1] : null;

  return (
    <Panel title="逐帧审查" note={unitPrefix}>
      <div className="review-toolbar">
        <button className="link" onClick={onClose}>
          ← 返回浏览
        </button>
        {previous ? (
          <button className="link" onClick={() => onNavigate(previous)}>
            ⏮ 上一段
          </button>
        ) : null}
        {next ? (
          <button className="link" onClick={() => onNavigate(next)}>
            下一段 ⏭
          </button>
        ) : null}
        {frames ? (
          <>
            <StateBadge
              label={frames.provenance.sha256Verified ? "sha256 对账通过" : "无 sha256 声明"}
              tone={frames.provenance.sha256Verified ? "ok" : "mute"}
            />
            <StateBadge
              label={frames.provenance.frameCountVerified ? "帧数对账通过" : "无帧数声明"}
              tone={frames.provenance.frameCountVerified ? "ok" : "mute"}
            />
          </>
        ) : null}
        {source.dbAuthority === "none" ? (
          <StateBadge label="登记于 e2e 环境库（本 console 不接）" tone="mute" />
        ) : objectInfo ? (
          <StateBadge
            label={objectInfo.dbRegistered ? "已登记（artifact_objects）" : "未登记（带外进桶）"}
            tone={objectInfo.dbRegistered ? "ok" : "warn"}
          />
        ) : null}
        {grant ? (
          <HfStatusBadge source={source.id} subjectKey={unitPrefix} videoSizeBytes={grant.sizeBytes} />
        ) : null}
      </div>
      {error ? <ErrorNote message={error} /> : null}
      {!error && (!frames || !grant) ? <Loading /> : null}
      {frames && grant && frames.schema !== "raw-events-v1" ? (
        <>
          <FrameReviewer
            rows={frames.rows}
            videoUrl={grant.url}
            schema={frames.schema}
            fps={frames.fps}
            muLawScheme={muLawSchemeOf(frames.meta)}
            title={baseName(unitPrefix)}
          />
          <p className="muted">
            视频链接 {grant.expiresAt} 过期（签发已记审计）；帧数据 sha256 <Mono value={frames.provenance.sha256.slice(0, 16)} />…
            刷新或切换后需重新签发。
          </p>
          <VerdictPanel
            source={source.id}
            subjectKind={frames.schema === "raw-v1" ? "raw_segment" : "dataset_chunk"}
            subjectKey={unitPrefix}
            canWrite={canWrite}
          />
        </>
      ) : null}
    </Panel>
  );
}

/**
 * HF release status for the reviewed unit (docs/33 §6). R2 and HF are disjoint
 * sets by design, so "无对应" is the expected state; a match — and above all a
 * size mismatch — is a signal for a human to check whether something was
 * released in error. A 404 means the deployment has no HF config: hide.
 */
function HfStatusBadge({
  source,
  subjectKey,
  videoSizeBytes,
}: {
  source: string;
  subjectKey: string;
  videoSizeBytes: number;
}) {
  const [status, setStatus] = useState<HfSubjectStatus | null | "off">(null);

  useEffect(() => {
    let live = true;
    setStatus(null);
    api
      .hfStatus(source, subjectKey, videoSizeBytes)
      .then((result) => {
        if (live) setStatus(result);
      })
      .catch((thrown: unknown) => {
        if (!live) return;
        setStatus(thrown instanceof ApiError && thrown.status === 404 ? "off" : "off");
      });
    return () => {
      live = false;
    };
  }, [source, subjectKey, videoSizeBytes]);

  if (status === null || status === "off") return null;
  const label =
    status.computed === "released"
      ? "HF: 已发布（请核对是否应当）"
      : status.computed === "mismatch"
        ? "HF: 同名但字节不符 ⚠"
        : status.checkedAt
          ? "HF: 无对应"
          : "HF: 未对账";
  const tone =
    status.computed === "mismatch" ? "bad" : status.computed === "released" ? "warn" : "mute";
  return (
    <>
      <StateBadge label={label} tone={tone} />
      {status.manualMark ? (
        <StateBadge label={`人工: ${status.manualMark.mark}`} tone="warn" />
      ) : null}
    </>
  );
}

/** Operator-triggered reconcile over the configured repo allowlist — no cron
 * (docs/33 辩论 §8.5): release is a low-frequency human event. Hidden when the
 * deployment has no HF config (the repos call 404s). */
function HfReconcileButton() {
  const [repos, setRepos] = useState<string[]>([]);
  const [message, setMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    api
      .hfRepos()
      .then((result) => {
        if (live) setRepos(result.repos);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  if (repos.length === 0) return null;

  async function reconcile() {
    setBusy(true);
    setMessage(null);
    try {
      const results = [];
      for (const repoId of repos) {
        results.push(await api.hfReconcile(repoId));
      }
      setMessage(
        results
          .map((r) => `${r.repoId}@${r.revision.slice(0, 8)}: ${r.itemCount} 文件（可匹配 ${r.matchableCount}）`)
          .join("；"),
      );
    } catch (thrown) {
      setMessage(describeApiError(thrown));
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="review-hf-reconcile">
      <button className="link" onClick={() => void reconcile()} disabled={busy}>
        {busy ? "HF 对账中…" : "HF 对账"}
      </button>
      {message ? <span className="muted"> {message}</span> : null}
    </span>
  );
}

/**
 * The structured replacement for the manual QA ledger (docs/33 §5): append-only
 * verdicts over the reviewed unit, latest row effective. Deliberately does NOT
 * touch clips.quality_state or trainability.
 */
function VerdictPanel({
  source,
  subjectKind,
  subjectKey,
  canWrite,
}: {
  source: string;
  subjectKind: ReviewVerdict["subjectKind"];
  subjectKey: string;
  canWrite: boolean;
}) {
  const [revision, setRevision] = useState(0);
  const history = useAsync(() => api.reviewVerdicts(source, subjectKey), [source, subjectKey, revision]);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function record(result: "passed" | "failed") {
    setBusy(true);
    setError(null);
    try {
      await api.recordReviewVerdict(
        { source, subjectKind, subjectKey, result, ...(note.trim() ? { note: note.trim() } : {}) },
        newIdempotencyKey(),
      );
      setNote("");
      setRevision((n) => n + 1);
    } catch (thrown) {
      setError(describeApiError(thrown));
    } finally {
      setBusy(false);
    }
  }

  const latest = history.status === "ready" ? history.value.verdicts[0] : undefined;

  return (
    <div className="reviewer-card review-verdicts">
      <h3>审查判定</h3>
      {latest ? (
        <p>
          最新判定：
          <StateBadge label={latest.result === "passed" ? "通过" : "不通过"} tone={latest.result === "passed" ? "ok" : "bad"} />{" "}
          <span className="muted">
            {latest.operatorEmail ?? latest.operatorId.slice(0, 8)} · {latest.createdAt}
            {latest.note ? ` · ${latest.note}` : ""}
          </span>
        </p>
      ) : history.status === "ready" ? (
        <p className="muted">尚无判定记录。</p>
      ) : null}
      {history.status === "error" ? <ErrorNote message={history.message} /> : null}
      {canWrite ? (
        <div className="review-verdict-form">
          <input
            type="text"
            placeholder="备注（≤280 字，选填）"
            value={note}
            maxLength={280}
            onChange={(event) => setNote(event.target.value)}
          />
          <button className="primary" disabled={busy} onClick={() => void record("passed")}>
            判定通过
          </button>
          <button disabled={busy} onClick={() => void record("failed")}>
            判定不通过
          </button>
        </div>
      ) : (
        <p className="muted">当前角色无判定写入权限。</p>
      )}
      {error ? <ErrorNote message={error} /> : null}
      {history.status === "ready" && history.value.verdicts.length > 1 ? (
        <details>
          <summary className="muted">历史判定（追加式，最新生效）</summary>
          <ul className="review-list">
            {history.value.verdicts.map((verdict) => (
              <li key={verdict.id}>
                {verdict.result === "passed" ? "✓ 通过" : "✗ 不通过"} ·{" "}
                {verdict.operatorEmail ?? verdict.operatorId.slice(0, 8)} · {verdict.createdAt}
                {verdict.note ? ` · ${verdict.note}` : ""}
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </div>
  );
}
