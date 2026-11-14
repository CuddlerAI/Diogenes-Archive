import { useState } from "react";

import { api, type PipelineHealth } from "../api.js";
import { EmptyState, ErrorNote, Loading, Mono, Panel, StateBadge } from "../components.js";
import { formatBytes, formatDateTime, formatWaitHours } from "../format.js";
import { useAsync } from "../useAsync.js";

export function Pipeline() {
  const [days, setDays] = useState(14);
  const state = useAsync(() => api.pipelineHealth(days), [days]);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const health = state.value;

  return (
    <>
      <Panel
        title="任务队列"
        note="verification 与 purge 均由 maintenance worker 领取；这里是队列事实，不是 worker 存活状态。"
      >
        <div className="split">
          <JobTable title="验证" counts={health.verification} />
          <JobTable title="删除" counts={health.purge} />
        </div>
      </Panel>

      <QualityBacklogPanel backlog={health.qualityBacklog} />

      <Panel title="上传在途" note="活跃 intent 与 multipart 来自控制面登记，不代表客户端本地队列。">
        <div className="tiles">
          <Tile label="活跃 upload intent" value={String(health.uploads.activeIntents)} />
          <Tile label="活跃 multipart" value={String(health.uploads.activeMultiparts)} />
          <Tile
            label="已过期未收口"
            value={String(health.uploads.expiredMultiparts)}
            tone={health.uploads.expiredMultiparts > 0 ? "warn" : undefined}
          />
        </div>
        {health.uploads.expiredMultiparts > 0 ? (
          <p className="muted">
            过期 multipart 由 orphan sweep 负责中止；持续不降说明 worker 没有在跑。
          </p>
        ) : null}
      </Panel>

      <Panel
        title="存储增长"
        note="按上传登记时间分桶，天界按北京时间（Asia/Shanghai）——与总览每日进线的登记口径逐日可对账。"
      >
        <div className="filters">
          <label>
            区间
            <select value={days} onChange={(event) => setDays(Number(event.target.value))}>
              <option value={7}>近 7 天</option>
              <option value={14}>近 14 天</option>
              <option value={30}>近 30 天</option>
            </select>
          </label>
        </div>
        <StorageChart health={health} />
      </Panel>

      <Panel title="最近失败" note="包含 retry_wait：仍会重试，不是终态。">
        {health.recentFailures.length === 0 ? (
          <EmptyState title="没有失败任务" explanation="当前没有处于重试、失败或死信状态的任务。" />
        ) : (
          <table>
            <thead>
              <tr>
                <th>类型</th>
                <th>片段</th>
                <th>错误</th>
                <th>尝试次数</th>
                <th>更新时间</th>
              </tr>
            </thead>
            <tbody>
              {health.recentFailures.map((failure) => (
                <tr key={`${failure.kind}-${failure.entityId}-${failure.updatedAt}`}>
                  <td>{failure.kind === "verification" ? "验证" : "删除"}</td>
                  <td>
                    <Mono value={failure.entityId} />
                  </td>
                  <td>{failure.errorCode ?? "—"}</td>
                  <td>{failure.attempts}</td>
                  <td>{formatDateTime(failure.updatedAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>
    </>
  );
}

/**
 * QA 积压（issue #855 PR 5）。
 *
 * `quality_state` 按设计是人工裁决点——`clips` 四个状态里只有它带
 * `quality_reviewed_by`，0001 的 CHECK 又要求"不是 pending 就必须有审阅时间"。
 * 既然判定必然由人做，可运营与否就不该靠回放去**预测**吞吐，而应该把真实积压
 * **直接摆出来**。这块面板就是那份替代物。
 *
 * 「待判会话」才是「还要点几次」：判定的粒度是一场录制。
 */
function QualityBacklogPanel({ backlog }: { backlog: PipelineHealth["qualityBacklog"] }) {
  const overdue = backlog.overdueSessionCount > 0 || backlog.overdueClipCount > 0;
  return (
    <Panel
      title="QA 积压"
      note="口径是此刻确实可以被裁决的片段：已通过传输验证、仍是 pending、且没有进入删除流程。等待从 verified_at 起算——在那之前判定根本不被允许。"
    >
      <div className="tiles">
        <Tile label="待判会话" value={String(backlog.pendingSessionCount)} />
        <Tile label="待判片段" value={String(backlog.pendingClipCount)} />
        <Tile label="最老等待" value={formatWaitHours(backlog.oldestWaitHours)} />
        <Tile
          label={`超过 ${backlog.overdueThresholdHours} 小时`}
          value={`${backlog.overdueSessionCount} 场 / ${backlog.overdueClipCount} 段`}
          tone={overdue ? "warn" : undefined}
        />
      </div>
      {backlog.pendingClipCount === 0 ? (
        <p className="muted">队列为空：没有等待裁决的片段。</p>
      ) : (
        <p className="muted">
          最老一条的验证时间 {formatDateTime(backlog.oldestVerifiedAt)}。判定入口在数据浏览 →
          会话详情，按整场做；片段详情里的逐条入口留作例外修正。
        </p>
      )}
      {overdue ? (
        <p className="muted">
          超期不是自动化能清的：QA 是人的裁决点，这里只负责让积压可见，不代人下判定。
        </p>
      ) : null}
    </Panel>
  );
}

function JobTable({
  title,
  counts,
}: {
  title: string;
  counts: PipelineHealth["verification"];
}) {
  const backlog = counts.pending + counts.running + counts.retryWait;
  return (
    <div>
      <h3>
        {title}
        {backlog > 0 ? <StateBadge label={`积压 ${backlog}`} tone="warn" /> : null}
      </h3>
      <table>
        <tbody>
          <tr>
            <td>pending</td>
            <td>{counts.pending}</td>
          </tr>
          <tr>
            <td>running</td>
            <td>{counts.running}</td>
          </tr>
          <tr>
            <td>retry_wait</td>
            <td>{counts.retryWait}</td>
          </tr>
          <tr>
            <td>failed</td>
            <td>{counts.failed}</td>
          </tr>
          <tr>
            <td>dead_letter</td>
            <td>{counts.deadLetter}</td>
          </tr>
        </tbody>
      </table>
    </div>
  );
}

function Tile({ label, value, tone }: { label: string; value: string; tone?: "warn" | undefined }) {
  return (
    <div className={`tile tile-${tone ?? "mute"}`}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
    </div>
  );
}

function StorageChart({ health }: { health: PipelineHealth }) {
  const total = health.storageByDay.reduce((sum, day) => sum + day.storedBytes, 0);
  if (total === 0) {
    return <p className="muted">该区间内没有新增存储。</p>;
  }
  const peak = Math.max(1, ...health.storageByDay.map((day) => day.storedBytes));

  return (
    <>
      <div className="chart" role="img" aria-label="每日新增存储">
        {health.storageByDay.map((day) => (
          <div
            key={day.day}
            className="chart-column"
            title={`${day.day} · ${formatBytes(day.storedBytes)} · ${day.clipCount} 片段`}
          >
            <div className="chart-bar" style={{ height: `${(day.storedBytes / peak) * 100}%` }} />
          </div>
        ))}
      </div>
      <p className="muted">区间内新增 {formatBytes(total)}。</p>
    </>
  );
}
