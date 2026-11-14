import { useState } from "react";

import type { Contribution, TimeBasis } from "../api.js";
import { ErrorNote, Loading, Panel } from "../components.js";
import { formatBytes, formatDateTime, formatHours } from "../format.js";
import { useAsync } from "../useAsync.js";

export function ContributionPanel({
  load,
}: {
  load: (days: number, timeBasis: TimeBasis) => Promise<Contribution>;
}) {
  const [days, setDays] = useState(30);
  // 默认按录制时间（docs/67 §3.3）。登记口径保留为显式选项——排查上传积压时，
  // 「哪天传上来的」正是要问的那个问题。
  const [timeBasis, setTimeBasis] = useState<TimeBasis>("recorded");
  const state = useAsync(() => load(days, timeBasis), [days, timeBasis]);

  return (
    <Panel
      title="数据贡献"
      note="统计口径为服务端登记的会话与片段，字节数只计已 stored 的对象；时长按片段帧数与帧率派生。天界按北京时间（Asia/Shanghai），与总览的每日进线同口径。"
    >
      <div className="filters">
        <label>
          区间
          <select value={days} onChange={(event) => setDays(Number(event.target.value))}>
            <option value={7}>近 7 天</option>
            <option value={30}>近 30 天</option>
            <option value={90}>近 90 天</option>
          </select>
        </label>
        <label>
          时间口径
          <select
            value={timeBasis}
            onChange={(event) => setTimeBasis(event.target.value as TimeBasis)}
          >
            <option value="recorded">按录制时间</option>
            <option value="registered">按上传登记时间</option>
          </select>
        </label>
      </div>

      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" ? (
        <>
          <div className="tiles">
            <Stat label="采集时长" value={formatHours(state.value.totals.capturedSeconds)} />
            <Stat label="会话" value={String(state.value.totals.sessionCount)} />
            <Stat label="片段" value={String(state.value.totals.clipCount)} />
            <Stat label="已存储片段" value={String(state.value.totals.storedClipCount)} />
            <Stat label="已验证片段" value={String(state.value.totals.verifiedClipCount)} />
            <Stat label="被拒片段" value={String(state.value.totals.rejectedClipCount)} />
            <Stat
              label="健康异常片段"
              value={String(state.value.totals.unhealthyClipCount)}
              tone={state.value.totals.unhealthyClipCount > 0 ? "warn" : "mute"}
            />
            <Stat label="存储量" value={formatBytes(state.value.totals.storedBytes)} />
          </div>
          <p className="muted">
            首次活动 {formatDateTime(state.value.totals.firstActivityAt)} · 最近活动{" "}
            {formatDateTime(state.value.totals.lastActivityAt)} ·{" "}
            {state.value.timeBasis === "recorded" ? "按录制时间统计" : "按上传登记时间统计"}
          </p>
          <DailyChart contribution={state.value} />
          <DailyTable contribution={state.value} />
        </>
      ) : null}
    </Panel>
  );
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: "warn" | "mute" }) {
  return (
    <div className={`tile tile-${tone ?? "mute"}`}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
    </div>
  );
}

function DailyChart({ contribution }: { contribution: Contribution }) {
  const peak = Math.max(1, ...contribution.daily.map((day) => day.clipCount));
  if (contribution.totals.clipCount === 0) {
    return <p className="muted">该区间内没有片段。</p>;
  }

  return (
    <div className="chart" role="img" aria-label="每日片段数量">
      {contribution.daily.map((day) => (
        <div
          key={day.day}
          className="chart-column"
          title={`${day.day} · ${day.clipCount} 片段 · ${formatHours(day.capturedSeconds)}${
            day.unhealthyClipCount > 0 ? ` · ${day.unhealthyClipCount} 段健康异常` : ""
          }`}
        >
          <div className="chart-bar" style={{ height: `${(day.clipCount / peak) * 100}%` }} />
          <div
            className="chart-bar chart-bar-verified"
            style={{ height: `${(day.verifiedClipCount / peak) * 100}%` }}
          />
        </div>
      ))}
    </div>
  );
}

/**
 * 有数据的那几天单独列出来。图能看出趋势，但「这个人这天录了几小时、有没有异常段」
 * 需要读数——这正是 docs/67 §3.3/§3.4 里 console 之前答不上来的问题。
 */
function DailyTable({ contribution }: { contribution: Contribution }) {
  const active = contribution.daily.filter((day) => day.clipCount > 0);
  if (active.length === 0) return null;

  return (
    <table>
      <thead>
        <tr>
          <th>日期</th>
          <th>时长</th>
          <th>会话</th>
          <th>片段</th>
          <th>已验证</th>
          <th>健康异常</th>
          <th>存储量</th>
        </tr>
      </thead>
      <tbody>
        {[...active].reverse().map((day) => (
          <tr key={day.day}>
            <td>{day.day}</td>
            <td>{formatHours(day.capturedSeconds)}</td>
            <td>{day.sessionCount}</td>
            <td>{day.clipCount}</td>
            <td>{day.verifiedClipCount}</td>
            <td>{day.unhealthyClipCount > 0 ? `⚠ ${day.unhealthyClipCount}` : "—"}</td>
            <td>{formatBytes(day.storedBytes)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
