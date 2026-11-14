import { useState } from "react";

import { api, type CatalogRelease, type CatalogVersion, type GameSummary } from "../api.js";
import { EmptyState, ErrorNote, Loading, Mono, Panel, StateBadge } from "../components.js";
import { formatBytes, formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";

const KIND_LABELS: Record<string, string> = {
  device_profile: "设备档",
  capture_profile: "采集档",
  game_profile: "游戏档",
  settings_profile: "设置档",
  game_mapping: "键位映射",
};

function statusTone(status: string): "ok" | "warn" | "bad" | "mute" {
  if (status === "approved") return "ok";
  if (status === "published") return "ok";
  if (status === "draft") return "warn";
  if (status === "revoked") return "bad";
  if (status === "retired") return "mute";
  return "mute";
}

const STATUS_LABELS: Record<string, string> = {
  draft: "草稿",
  approved: "已批准",
  retired: "已退役",
  revoked: "已吊销",
  published: "当前发布",
  superseded: "已取代",
};

// The three consumers read the same row with three different thresholds. This
// asymmetry is the thing operators trip over: a draft device profile fails
// enrollment while a draft capture profile opens sessions perfectly well.
type Gate = { label: string; tone: "ok" | "warn" | "bad" | "mute" };

function sessionGate(v: CatalogVersion): Gate {
  if (v.kind === "game_profile") return { label: "不适用", tone: "mute" };
  if (v.status === "revoked") return { label: "拒绝", tone: "bad" };
  return { label: "放行", tone: v.status === "draft" ? "warn" : "ok" };
}

function enrollGate(v: CatalogVersion): Gate {
  if (v.kind !== "device_profile") return { label: "不适用", tone: "mute" };
  return v.status === "approved" ? { label: "放行", tone: "ok" } : { label: "拒绝", tone: "bad" };
}

function trainableGate(v: CatalogVersion): Gate {
  const statusOk = v.status === "approved" || v.status === "retired";
  if (statusOk && v.approvedForTrainable) return { label: "放行", tone: "ok" };
  return { label: "拒绝", tone: "bad" };
}

export function Catalog() {
  const [gameFilter, setGameFilter] = useState<string>("");
  const games = useAsync(() => api.games(), []);
  const releases = useAsync(() => api.catalogReleases(), []);
  const versions = useAsync(
    () => api.catalogVersions(gameFilter ? { gameId: gameFilter } : {}),
    [gameFilter],
  );

  if (games.status === "loading") return <Loading />;
  if (games.status === "error") return <ErrorNote message={games.message} />;

  return (
    <>
      <GamesPanel games={games.value.games} />
      <ReleasesPanel state={releases} />
      <VersionsPanel
        state={versions}
        games={games.value.games}
        gameFilter={gameFilter}
        onGameFilter={setGameFilter}
      />
    </>
  );
}

function ReleasesPanel({
  state,
}: {
  state: ReturnType<typeof useAsync<{ releases: CatalogRelease[] }>>;
}) {
  return (
    <Panel
      title="Catalog releases"
      note="一个 channel 最多只有一个 current published release。这里展示不可变 envelope 的治理历史；D1 只提供只读视图，不在 console 提前开放发布操作。"
    >
      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" && state.value.releases.length === 0 ? (
        <EmptyState
          title="尚未创建 release"
          explanation="D1 已建立 release 数据模型；首个签名 release 将由后续 D2 的校验与发布流程创建。"
        />
      ) : null}
      {state.status === "ready" && state.value.releases.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>channel / catalog</th>
              <th>状态</th>
              <th>条目 / 游戏</th>
              <th>签名 key</th>
              <th>创建 / 发布</th>
              <th>envelope hash</th>
            </tr>
          </thead>
          <tbody>
            {state.value.releases.map((release) => (
              <tr key={release.id}>
                <td>
                  <strong>{release.channel}</strong>
                  <div>
                    <Mono value={release.catalogId} />
                  </div>
                  <div className="muted">{release.catalogVersion}</div>
                </td>
                <td>
                  <StateBadge
                    label={STATUS_LABELS[release.status] ?? release.status}
                    tone={statusTone(release.status)}
                  />
                </td>
                <td>{release.entryCount} / {release.gameCount}</td>
                <td><Mono value={release.signingKeyId} /></td>
                <td>
                  {release.createdBy}
                  <div className="muted">{formatDateTime(release.createdAt)}</div>
                  {release.publishedBy ? (
                    <div className="muted">
                      发布：{release.publishedBy} · {formatDateTime(release.publishedAt)}
                    </div>
                  ) : null}
                </td>
                <td><Mono value={release.envelopeSha256} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}

function GamesPanel({ games }: { games: GameSummary[] }) {
  return (
    <Panel
      title="游戏"
      note="allowed_for_capture 今天不拦采集——摄取面只检查 game 是否已登记；它与 approved_for_trainable 一起决定 clip 能否训练（任一为否即 game_unapproved）。"
    >
      {games.length === 0 ? (
        <EmptyState
          title="尚未登记任何游戏"
          explanation="游戏行由 register:catalog 注册 bundle 时写入。库里为空说明还没有跑过注册，不是页面故障。"
        />
      ) : (
        <table>
          <thead>
            <tr>
              <th>game_id</th>
              <th>名称</th>
              <th>类型</th>
              <th>可训练闸</th>
              <th>采集登记闸</th>
              <th>会话</th>
              <th>clip</th>
            </tr>
          </thead>
          <tbody>
            {games.map((g) => (
              <tr key={g.id}>
                <td>
                  <Mono value={g.id} />
                </td>
                <td>{g.title}</td>
                <td>{g.contentKind}</td>
                <td>
                  <StateBadge
                    label={g.approvedForTrainable ? "是" : "否"}
                    tone={g.approvedForTrainable ? "ok" : "warn"}
                  />
                </td>
                <td>
                  <StateBadge
                    label={g.allowedForCapture ? "是" : "否"}
                    tone={g.allowedForCapture ? "ok" : "warn"}
                  />
                </td>
                <td>{g.sessionCount}</td>
                <td>{g.clipCount}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

function VersionsPanel({
  state,
  games,
  gameFilter,
  onGameFilter,
}: {
  state: ReturnType<typeof useAsync<{ versions: CatalogVersion[] }>>;
  games: GameSummary[];
  gameFilter: string;
  onGameFilter: (value: string) => void;
}) {
  return (
    <Panel
      title="Catalog 版本"
      note="状态与「可训练」是两件正交的事。字节就绪只表示存在不可变 exact payload 且未 revoked，不代表已经通过 certified 发布审批；仅有 JSONB 的旧行明确不可发布。"
    >
      <div className="filters">
        <label>
          按游戏筛选
          <select value={gameFilter} onChange={(e) => onGameFilter(e.target.value)}>
            <option value="">全部</option>
            {games.map((g) => (
              <option key={g.id} value={g.id}>
                {g.title}
              </option>
            ))}
          </select>
        </label>
      </div>

      {state.status === "loading" ? <Loading /> : null}
      {/* A failed sub-request must not look like an empty list: say which it is. */}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" ? <VersionsTable versions={state.value.versions} /> : null}
    </Panel>
  );
}

function VersionsTable({ versions }: { versions: CatalogVersion[] }) {
  if (versions.length === 0) {
    return (
      <EmptyState
        title="没有已注册的 catalog 版本"
        explanation="catalog 行只能由 pnpm --filter @dataminer/api register:catalog 从 bundle 目录写入（见 docs/31 §2）。空列表说明尚未注册，而不是功能缺失。"
      />
    );
  }

  return (
    <table>
      <thead>
        <tr>
          <th>catalog_id / version</th>
          <th>类别</th>
          <th>game</th>
          <th>状态</th>
          <th>可训练</th>
          <th>开会话</th>
          <th>设备注册</th>
          <th>可训练判定</th>
          <th>批准</th>
          <th>exact payload</th>
          <th>content_hash</th>
        </tr>
      </thead>
      <tbody>
        {versions.map((v) => {
          const session = sessionGate(v);
          const enroll = enrollGate(v);
          const trainable = trainableGate(v);
          return (
            <tr key={v.id}>
              <td>
                <Mono value={v.catalogId} />
                <div className="muted">{v.version}</div>
              </td>
              <td>{KIND_LABELS[v.kind] ?? v.kind}</td>
              <td>
                <Mono value={v.gameId} />
              </td>
              <td>
                <StateBadge label={STATUS_LABELS[v.status] ?? v.status} tone={statusTone(v.status)} />
              </td>
              <td>
                <StateBadge
                  label={v.approvedForTrainable ? "是" : "否"}
                  tone={v.approvedForTrainable ? "ok" : "warn"}
                />
              </td>
              <td>
                <StateBadge label={session.label} tone={session.tone} />
              </td>
              <td>
                <StateBadge label={enroll.label} tone={enroll.tone} />
              </td>
              <td>
                <StateBadge label={trainable.label} tone={trainable.tone} />
              </td>
              <td>
                {v.approvedBy ? (
                  <>
                    {v.approvedBy}
                    <div className="muted">{formatDateTime(v.approvedAt)}</div>
                  </>
                ) : (
                  <span className="muted">未批准</span>
                )}
                {v.revokedAt ? <div className="muted">吊销于 {formatDateTime(v.revokedAt)}</div> : null}
                {v.retiredAt ? <div className="muted">退役于 {formatDateTime(v.retiredAt)}</div> : null}
              </td>
              <td>
                <StateBadge
                  label={v.publishable ? "字节就绪" : "不可发布"}
                  tone={v.publishable ? "ok" : "bad"}
                />
                <div className="muted">
                  {v.payloadByteSize === null ? "仅有 JSONB" : formatBytes(v.payloadByteSize)}
                </div>
              </td>
              <td>
                <Mono value={v.contentHash} />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
