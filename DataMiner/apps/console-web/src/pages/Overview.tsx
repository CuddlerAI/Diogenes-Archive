import { useState } from "react";

import {
  api,
  type DailyGroupBy,
  type DailyIntake,
  type DailyIntakeGroup,
  type GameDistribution,
  type TimeBasis,
} from "../api.js";
import { EmptyState, ErrorNote, Loading, Panel } from "../components.js";
import { formatBytes, formatDateTime, formatHours, formatHoursShort, formatShare } from "../format.js";
import { useAsync } from "../useAsync.js";

export function Overview() {
  const state = useAsync(() => api.overview(), []);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const overview = state.value;

  if (overview.sessions.total === 0) {
    return (
      <Panel title="总览">
        <EmptyState
          title="控制面尚未收到任何会话"
          explanation="摄取控制面已部署，客户端 uploader 也已实现；缺的是设备 enroll——没有 enroll 就没有 device token，客户端无法向摄取面鉴权，因此还没有任何上传。"
        />
      </Panel>
    );
  }

  return (
    <>
      {/*
        每一格片段类读数都配一行时长。业务目标是按小时下的（「某款游戏 200h」），
        而件数换不出小时——片段长度从几秒到几分钟都有，除不出可用的平均值。
        时长与件数来自同一次聚合的同一组行（browse-repository 的 clip_rollup），
        所以每一格的两个数一定说的是同一批段。
      */}
      <Panel
        title="总览"
        note="片段类读数按服务端登记口径，含已购删的段；时长按 n_frames/fps 求和。要看剔除购删后的存量构成，见下面的游戏分布。"
      >
        <div className="tiles">
          <Tile label="设备" value={String(overview.devices.total)} note={`${overview.devices.active} 台 active`} />
          <Tile label="24h 活跃设备" value={String(overview.devices.seenLast24h)} note={`7 天 ${overview.devices.seenLast7d}`} />
          <Tile label="会话" value={String(overview.sessions.total)} note={`24h 新增 ${overview.sessions.last24h}`} />
          <Tile
            label="片段"
            value={String(overview.clips.total)}
            sub={formatHours(overview.clipSeconds.total)}
            note={`已验证 ${overview.clips.verified} · ${formatHours(overview.clipSeconds.verified)}`}
          />
          <Tile label="已存储" value={formatBytes(overview.storedBytes)} note="R2" />
          {/*
            depth 单列一格，而不是让它躺在「已存储」里。depth 只有装了实验包、
            且该游戏做过 profile 标定的机器才产得出，所以件数就是那条线的产能读数；
            而它的字节又大到能独自主导存储曲线（实测约为 video 的 2 倍），
            混在总量里会让「有人开了 depth」看起来像「多采了三倍」。
            note 里写明是子集，免得有人把两格相加。
          */}
          <Tile
            label="带 depth 的片段"
            value={String(overview.depth.clips)}
            sub={formatHours(overview.depth.seconds)}
            note={`${formatBytes(overview.depth.bytes)}（已含在「已存储」内）`}
          />
          <Tile
            label="可训练片段"
            value={String(overview.trainableClips)}
            sub={formatHours(overview.trainableSeconds)}
            note="由 clip_trainability 视图判定"
            tone={overview.trainableClips > 0 ? "warn" : "mute"}
          />
        </div>
      </Panel>

      <Panel
        title="上传管道"
        note="口径为服务端登记的片段状态，不是客户端本地队列。第二行是该状态下的录制时长——「卡住了多少段」和「卡住了多少小时」是两个决策。"
      >
        <div className="tiles">
          <Tile
            label="pending"
            value={String(overview.clips.pending)}
            sub={formatHours(overview.clipSeconds.pending)}
          />
          <Tile
            label="uploading"
            value={String(overview.clips.uploading)}
            sub={formatHours(overview.clipSeconds.uploading)}
          />
          <Tile
            label="stored"
            value={String(overview.clips.stored)}
            sub={formatHours(overview.clipSeconds.stored)}
          />
          <Tile
            label="retry_wait"
            value={String(overview.clips.retryWait)}
            sub={formatHours(overview.clipSeconds.retryWait)}
            tone={overview.clips.retryWait > 0 ? "warn" : "mute"}
          />
          <Tile
            label="failed"
            value={String(overview.clips.failed)}
            sub={formatHours(overview.clipSeconds.failed)}
            tone={overview.clips.failed > 0 ? "bad" : "mute"}
          />
          <Tile
            label="rejected"
            value={String(overview.clips.rejected)}
            sub={formatHours(overview.clipSeconds.rejected)}
            tone={overview.clips.rejected > 0 ? "bad" : "mute"}
          />
          <Tile
            label="purge 进行中"
            value={String(overview.clips.purgeActive)}
            sub={formatHours(overview.clipSeconds.purgeActive)}
          />
        </div>
      </Panel>

      <GameDistributionPanel />

      <DailyIntakePanel />
    </>
  );
}

/**
 * 游戏分布普查（docs/153）：全量存量的单次快照，回答「语料押在哪几款游戏上」。
 * 与下面的每日进线分工——那是滑窗 flow，这是不带时间轴的 stock；要按区间看
 * 游戏拆分去产出报表。
 */
function GameDistributionPanel() {
  const state = useAsync(() => api.statsGameDistribution(), []);

  return (
    <Panel
      title="游戏分布"
      note="全量存量快照：计入全部已登记片段（含未完成上传），已购删的段不计；时长按 n_frames/fps 求和，可训练为现值回看。"
    >
      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" ? <GameDistributionBody census={state.value} /> : null}
    </Panel>
  );
}

function GameDistributionBody({ census }: { census: GameDistribution }) {
  const { rows, totals } = census;
  if (totals.clipCount === 0) {
    return (
      <EmptyState
        title="库里还没有任何片段"
        explanation="有片段登记进来之后，这里会按游戏列出存量时长的构成。"
      />
    );
  }

  return (
    <>
      <p className="muted">
        截至 {formatDateTime(census.asOf)} · {rows.length} 款游戏 · 合计{" "}
        {formatHours(totals.capturedSeconds)} · {totals.sessionCount} 会话 · {totals.clipCount}{" "}
        片段 · 回看可训练 {totals.trainableClipCount} 段（
        {formatHoursShort(totals.trainableSeconds)}）
      </p>
      <table>
        <thead>
          <tr>
            <th>游戏</th>
            <th>时长</th>
            <th>占比</th>
            <th>会话</th>
            <th>片段</th>
            <th>回看可训练</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const percent =
              totals.capturedSeconds > 0 ? (row.capturedSeconds / totals.capturedSeconds) * 100 : 0;
            return (
              <tr key={row.gameId}>
                <td>{row.title}</td>
                <td>{formatHoursShort(row.capturedSeconds)}</td>
                <td>
                  <div className="share-cell">
                    <div className="share-track">
                      {/* 非零至少画 1%：0.01% 的游戏在 90px 的条上不足一像素，
                          画不出来等于把「有一点」渲染成「一点没有」。 */}
                      <div
                        className="share-fill"
                        style={{ width: `${percent > 0 ? Math.max(percent, 1) : 0}%` }}
                      />
                    </div>
                    {formatShare(row.capturedSeconds, totals.capturedSeconds)}
                  </div>
                </td>
                <td>{row.sessionCount}</td>
                <td>{row.clipCount}</td>
                {/* 零时写「—」而不是 0：可训练目前是例外而非常态，一列 0 会把眼睛训练到忽略它。 */}
                <td>
                  {row.trainableClipCount > 0
                    ? `${row.trainableClipCount} 段 · ${formatHoursShort(row.trainableSeconds)}`
                    : "—"}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </>
  );
}

/**
 * 全局每日进线（docs/97）。天界缺省按北京时间——UTC 天界是北京早上 8 点，
 * 会把凌晨录制归到前一天；口径默认按录制墙钟（docs/67 §3.3），排查「今天
 * 到底传上来多少」时切「按上传登记时间」。
 */
function DailyIntakePanel() {
  const [days, setDays] = useState(14);
  const [timeBasis, setTimeBasis] = useState<TimeBasis>("recorded");
  const [groupBy, setGroupBy] = useState<"" | DailyGroupBy>("");
  const state = useAsync(
    () => api.statsDaily(days, timeBasis, groupBy === "" ? undefined : groupBy),
    [days, timeBasis, groupBy],
  );

  return (
    <Panel
      title="每日进线"
      note="天界按北京时间（Asia/Shanghai）；按录制时间统计时，补传会回填历史天的数字。字节数只计已 stored 的对象，验证/健康/可训练列均为现值回看。"
    >
      <div className="filters">
        <label>
          区间
          <select value={days} onChange={(event) => setDays(Number(event.target.value))}>
            <option value={7}>近 7 天</option>
            <option value={14}>近 14 天</option>
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
        <label>
          维度
          <select
            value={groupBy}
            onChange={(event) => setGroupBy(event.target.value as "" | DailyGroupBy)}
          >
            <option value="">整体</option>
            <option value="game">按游戏</option>
            <option value="annotator">按采集员</option>
          </select>
        </label>
      </div>

      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" ? <DailyIntakeBody intake={state.value} /> : null}
    </Panel>
  );
}

function DailyIntakeBody({ intake }: { intake: DailyIntake }) {
  if (intake.totals.clipCount === 0) {
    return <EmptyState title="区间内没有进线" explanation="该区间内没有任何片段落到所选时间口径的天上。" />;
  }

  return (
    <>
      <DailyChart intake={intake} />
      <p className="muted">
        区间合计 {intake.totals.clipCount} 片段 · {formatHours(intake.totals.capturedSeconds)} ·{" "}
        {formatBytes(intake.totals.storedBytes)} · {intake.totals.activeAnnotatorCount} 名采集员 ·{" "}
        {intake.totals.activeDeviceCount} 台设备 · 回看可训练 {intake.totals.trainableClipCount} ·{" "}
        {intake.timeBasis === "recorded" ? "按录制时间统计" : "按上传登记时间统计"}
      </p>
      {intake.groups ? (
        intake.groups.map((group) => (
          <GroupSection key={group.key} group={group} axis={intake.daily.map((day) => day.day)} />
        ))
      ) : (
        <DailyTable intake={intake} />
      )}
    </>
  );
}

/**
 * 拆分维度的一组：与整体同一条日轴对齐（拆分序列稀疏，空天在这里补齐），
 * 图看趋势，合计一行给读数——细读逐日仍回「整体」维度。
 */
function GroupSection({ group, axis }: { group: DailyIntakeGroup; axis: string[] }) {
  const byDay = new Map(group.daily.map((day) => [day.day, day]));
  const peak = Math.max(1, ...group.daily.map((day) => day.clipCount));
  const label = group.key === "none" ? "未挂采集员" : group.label;

  return (
    <div>
      <h3>{label}</h3>
      <div className="chart" role="img" aria-label={`${label} 每日进线片段数量`}>
        {axis.map((day) => {
          const value = byDay.get(day);
          return (
            <div
              key={day}
              className="chart-column"
              title={
                value
                  ? `${day} · ${value.clipCount} 片段 · ${formatHours(value.capturedSeconds)} · ${formatBytes(value.storedBytes)}`
                  : `${day} · 0 片段`
              }
            >
              <div
                className="chart-bar"
                style={{ height: `${((value?.clipCount ?? 0) / peak) * 100}%` }}
              />
              <div
                className="chart-bar chart-bar-verified"
                style={{ height: `${((value?.verifiedClipCount ?? 0) / peak) * 100}%` }}
              />
            </div>
          );
        })}
      </div>
      <p className="muted">
        {group.totals.clipCount} 片段 · {formatHours(group.totals.capturedSeconds)} ·{" "}
        {formatBytes(group.totals.storedBytes)} · 已验证 {group.totals.verifiedClipCount} · 回看可训练{" "}
        {group.totals.trainableClipCount}
        {group.totals.unhealthyClipCount > 0 ? ` · ⚠ ${group.totals.unhealthyClipCount} 段健康异常` : ""}
      </p>
    </div>
  );
}

function DailyChart({ intake }: { intake: DailyIntake }) {
  const peak = Math.max(1, ...intake.daily.map((day) => day.clipCount));

  return (
    <div className="chart" role="img" aria-label="每日进线片段数量">
      {intake.daily.map((day) => (
        <div
          key={day.day}
          className="chart-column"
          title={`${day.day} · ${day.clipCount} 片段 · ${formatHours(day.capturedSeconds)} · ${formatBytes(
            day.storedBytes,
          )} · ${day.activeAnnotatorCount} 名采集员${
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

/** 有数据的那几天单独列出来：图看趋势，「这天到底进了多少」要读数。 */
function DailyTable({ intake }: { intake: DailyIntake }) {
  const active = intake.daily.filter((day) => day.clipCount > 0);
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
          <th>带 depth</th>
          <th>活跃设备</th>
          <th>活跃采集员</th>
          <th>回看可训练</th>
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
            {/* 零条时写「—」而不是 0：depth 目前是例外而非常态，一列 0 会把眼睛训练到忽略它。 */}
            <td>
              {day.depthClipCount > 0
                ? `${day.depthClipCount} · ${formatBytes(day.depthBytes)}`
                : "—"}
            </td>
            <td>{day.activeDeviceCount}</td>
            <td>{day.activeAnnotatorCount}</td>
            <td>{day.trainableClipCount}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/**
 * `sub` 是第二读数，比 note 重、比 value 轻。片段类的格子用它放时长——时长是
 * 业务口径（目标按小时下），件数是管道口径，两个都要能一眼读到，但压成同一
 * 行会分不清主次，塞进 11px 的 note 又等于没有。
 */
function Tile({
  label,
  value,
  sub,
  note,
  tone,
}: {
  label: string;
  value: string;
  sub?: string;
  note?: string;
  tone?: "warn" | "bad" | "mute";
}) {
  return (
    <div className={`tile tile-${tone ?? "mute"}`}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
      {sub ? <span className="tile-sub">{sub}</span> : null}
      {note ? <span className="tile-note">{note}</span> : null}
    </div>
  );
}
