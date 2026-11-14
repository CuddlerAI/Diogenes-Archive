import { useMemo, useState } from "react";

import {
  api,
  type OutputBucket,
  type OutputDimension,
  type OutputMetrics,
  type OutputReport,
  type OutputRow,
  type TimeBasis,
} from "../api.js";
import { EmptyState, ErrorNote, Loading, Panel, useAnnotatorOptions } from "../components.js";
import { formatBytes, formatHours, formatHoursShort } from "../format.js";
import { navigate, useRoute } from "../router.js";
import { useAsync } from "../useAsync.js";

/**
 * 产出报表（docs/145）。回答后台此前答不上来的那个问题：每个人每天交了多少，
 * 以及**谁没交**。
 *
 * 页面状态全部住在查询串里（docs/53 的路由纪律）：一个配置就是一条可收藏、可转发的
 * URL——「不同人的不同看法」由各自的书签承载，服务端不存视图。
 */

const DIMENSIONS: { id: OutputDimension; label: string }[] = [
  { id: "annotator", label: "采集员" },
  { id: "game", label: "游戏" },
];

const METRICS = [
  { id: "capturedSeconds", label: "采集时长" },
  { id: "clipCount", label: "片段数" },
  { id: "verifiedClipCount", label: "已验证片段" },
  { id: "storedBytes", label: "存储量" },
  { id: "depthClipCount", label: "带 depth 片段" },
] as const;

type MetricId = (typeof METRICS)[number]["id"];

const isMetric = (value: string): value is MetricId =>
  METRICS.some((metric) => metric.id === value);

/**
 * 表里印什么由这里决定，不由调用点各写一遍。时长走紧凑写法（「5.4 h」）：
 * 一行几十格放不下「5 小时 24 分」。均值会是小数，计数保留一位——四舍五入到
 * 整数会让「日均 0.4 段」显示成 0。
 */
function formatMetric(metric: MetricId, value: number, fractional = false): string {
  if (metric === "capturedSeconds") return formatHoursShort(value);
  if (metric === "storedBytes") return formatBytes(value);
  return fractional && !Number.isInteger(value)
    ? value.toLocaleString(undefined, { maximumFractionDigits: 1 })
    : Math.round(value).toLocaleString();
}

const DAY_MS = 86_400_000;

/**
 * 「今天」按服务端的天界算（Asia/Shanghai），不按浏览器时区——否则在别的时区
 * 打开控制台，默认区间会和表里的天标签差一天。en-CA 的日期格式恰好是 ISO 形状。
 */
const shanghaiToday = (): string =>
  new Intl.DateTimeFormat("en-CA", { timeZone: "Asia/Shanghai" }).format(new Date());

const shiftDay = (day: string, days: number): string =>
  new Date(Date.parse(`${day}T00:00:00Z`) + days * DAY_MS).toISOString().slice(0, 10);

const spanDays = (from: string, to: string): number =>
  Math.round((Date.parse(`${to}T00:00:00Z`) - Date.parse(`${from}T00:00:00Z`)) / DAY_MS) + 1;

/** 与服务端契约同一个数（statsOutputQuerySchema）。超了不静默换粒度，界面明说。 */
const MAX_DAY_SPAN = 92;

const WEEKDAYS = ["日", "一", "二", "三", "四", "五", "六"];

/** 列标签：日粒度带星期几——周末的零格和缺勤的零格意思完全不同。 */
function bucketLabel(bucket: string, granularity: OutputBucket): { head: string; sub: string } {
  if (granularity === "month") return { head: `${Number(bucket.slice(5, 7))} 月`, sub: bucket.slice(0, 4) };
  const weekday = WEEKDAYS[new Date(`${bucket}T00:00:00Z`).getUTCDay()] ?? "";
  return { head: bucket.slice(5), sub: weekday };
}

const isWeekend = (bucket: string, granularity: OutputBucket): boolean =>
  granularity === "day" && [0, 6].includes(new Date(`${bucket}T00:00:00Z`).getUTCDay());

type Ranges = { label: string; of: (today: string) => { from: string; to: string } }[];

const RANGES: Ranges = [
  { label: "近 7 天", of: (today) => ({ from: shiftDay(today, -6), to: today }) },
  { label: "近 14 天", of: (today) => ({ from: shiftDay(today, -13), to: today }) },
  { label: "本月", of: (today) => ({ from: `${today.slice(0, 7)}-01`, to: today }) },
  {
    label: "上月",
    of: (today) => {
      const firstOfThis = Date.parse(`${today.slice(0, 7)}-01T00:00:00Z`);
      const lastOfPrev = new Date(firstOfThis - DAY_MS).toISOString().slice(0, 10);
      return { from: `${lastOfPrev.slice(0, 7)}-01`, to: lastOfPrev };
    },
  },
  { label: "近 90 天", of: (today) => ({ from: shiftDay(today, -89), to: today }) },
];

/** 分组的一层：属性 + 该属性的值筛选（不选就是全部）。 */
type Group = { dimension: OutputDimension; values: string[] };

type View = {
  from: string;
  to: string;
  bucket: OutputBucket;
  timeBasis: TimeBasis;
  metric: MetricId;
  /** 合计列的口径。九个读数全可加，所以「合计」就是逐格相加，没有第三种。 */
  aggregate: "sum" | "average";
  groups: Group[];
};

function readView(query: URLSearchParams): View {
  const today = shanghaiToday();
  const groupBy = (query.get("groupBy") ?? "annotator")
    .split(",")
    .map((part) => part.trim())
    .filter((part): part is OutputDimension => DIMENSIONS.some((entry) => entry.id === part));
  const dimensions = groupBy.length > 0 ? [...new Set(groupBy)] : (["annotator"] as OutputDimension[]);
  const metric = query.get("metric") ?? "";
  return {
    from: query.get("from") ?? shiftDay(today, -13),
    to: query.get("to") ?? today,
    bucket: query.get("bucket") === "month" ? "month" : "day",
    timeBasis: query.get("timeBasis") === "registered" ? "registered" : "recorded",
    metric: isMetric(metric) ? metric : "capturedSeconds",
    aggregate: query.get("aggregate") === "average" ? "average" : "sum",
    groups: dimensions.map((dimension) => ({
      dimension,
      values: query.getAll(dimension === "annotator" ? "annotatorId" : "gameId"),
    })),
  };
}

/** 视图写回 URL。多选值走可重复参数，与服务端的 repeatable 对齐。 */
function pushView(view: View): void {
  const query = new URLSearchParams();
  query.set("from", view.from);
  query.set("to", view.to);
  query.set("bucket", view.bucket);
  query.set("timeBasis", view.timeBasis);
  query.set("metric", view.metric);
  query.set("aggregate", view.aggregate);
  query.set("groupBy", view.groups.map((group) => group.dimension).join(","));
  for (const group of view.groups) {
    const key = group.dimension === "annotator" ? "annotatorId" : "gameId";
    for (const value of group.values) query.append(key, value);
  }
  navigate(`/output?${query.toString()}`, { replace: true });
}

export function Output() {
  const { query } = useRoute();
  const view = readView(query);
  const update = (patch: Partial<View>) => pushView({ ...view, ...patch });

  const annotators = useAnnotatorOptions();
  const games = useAsync(() => api.games(), []);
  const gameOptions = useMemo(
    () => (games.status === "ready" ? games.value.games.map((game) => ({ id: game.id, label: game.title })) : []),
    [games],
  );

  const overlong = view.bucket === "day" && spanDays(view.from, view.to) > MAX_DAY_SPAN;
  const annotatorIds = view.groups.find((group) => group.dimension === "annotator")?.values ?? [];
  const gameIds = view.groups.find((group) => group.dimension === "game")?.values ?? [];
  const groupKey = view.groups.map((group) => group.dimension).join(",");

  const report = useAsync(
    () =>
      overlong
        ? Promise.resolve(null)
        : api.statsOutput({
            from: view.from,
            to: view.to,
            bucket: view.bucket,
            groupBy: view.groups.map((group) => group.dimension),
            annotatorId: annotatorIds,
            gameId: gameIds,
            timeBasis: view.timeBasis,
          }),
    [
      view.from,
      view.to,
      view.bucket,
      view.timeBasis,
      groupKey,
      annotatorIds.join(","),
      gameIds.join(","),
      overlong,
    ],
  );

  const used = new Set(view.groups.map((group) => group.dimension));
  const spare = DIMENSIONS.find((entry) => !used.has(entry.id));

  return (
    <Panel
      title="产出报表"
      note={`天界按北京时间（Asia/Shanghai），与总览的每日进线同口径；按录制时间统计时，补传会回填历史区间的数字。空格是这一格确实没有产出，不是没查到；已验证、健康异常与可训练均为现值回看。`}
    >
      <div className="filters">
        <label>
          区间
          <span className="segmented">
            {RANGES.map((range) => {
              const bounds = range.of(shanghaiToday());
              const active = bounds.from === view.from && bounds.to === view.to;
              return (
                <button
                  key={range.label}
                  type="button"
                  aria-pressed={active}
                  onClick={() => update(bounds)}
                >
                  {range.label}
                </button>
              );
            })}
          </span>
        </label>
        <label>
          起
          <input
            type="date"
            value={view.from}
            max={view.to}
            onChange={(event) => event.target.value && update({ from: event.target.value })}
          />
        </label>
        <label>
          止
          <input
            type="date"
            value={view.to}
            min={view.from}
            onChange={(event) => event.target.value && update({ to: event.target.value })}
          />
        </label>
        <label>
          粒度
          <span className="segmented">
            {(["day", "month"] as OutputBucket[]).map((bucket) => (
              <button
                key={bucket}
                type="button"
                aria-pressed={view.bucket === bucket}
                onClick={() => update({ bucket })}
              >
                {bucket === "day" ? "日" : "月"}
              </button>
            ))}
          </span>
        </label>
        <label>
          时间口径
          <select
            value={view.timeBasis}
            onChange={(event) => update({ timeBasis: event.target.value as TimeBasis })}
          >
            <option value="recorded">按录制时间</option>
            <option value="registered">按上传登记时间</option>
          </select>
        </label>
        <label>
          读数
          <select
            value={view.metric}
            onChange={(event) =>
              isMetric(event.target.value) && update({ metric: event.target.value })
            }
          >
            {METRICS.map((metric) => (
              <option key={metric.id} value={metric.id}>
                {metric.label}
              </option>
            ))}
          </select>
        </label>
      </div>

      <div className="group-by">
        <div className="group-by-head">
          {spare ? (
            <button
              type="button"
              className="link"
              onClick={() => update({ groups: [...view.groups, { dimension: spare.id, values: [] }] })}
            >
              ＋ 属性分组
            </button>
          ) : null}
          <span className="muted">顺序即列顺序 · 每层不选值就是全部 · 没产出的采集员照样占一行</span>
        </div>
        {view.groups.map((group, index) => (
          <GroupRow
            key={`${group.dimension}-${index}`}
            group={group}
            index={index}
            options={group.dimension === "annotator" ? annotators : gameOptions}
            taken={view.groups.filter((_, at) => at !== index).map((entry) => entry.dimension)}
            onChange={(next) =>
              update({ groups: view.groups.map((entry, at) => (at === index ? next : entry)) })
            }
            onRemove={
              view.groups.length > 1
                ? () => update({ groups: view.groups.filter((_, at) => at !== index) })
                : undefined
            }
          />
        ))}
      </div>

      {overlong ? (
        <div className="notice">
          <span>
            区间 {spanDays(view.from, view.to)} 天，按日会有同样多的列，超过 {MAX_DAY_SPAN} 列的上限。
          </span>
          <button type="button" className="link" onClick={() => update({ bucket: "month" })}>
            切到按月
          </button>
        </div>
      ) : null}

      {report.status === "loading" ? <Loading /> : null}
      {report.status === "error" ? <ErrorNote message={report.message} /> : null}
      {report.status === "ready" && report.value ? (
        <ReportBody report={report.value} metric={view.metric} aggregate={view.aggregate}
          onAggregate={(aggregate) => update({ aggregate })} />
      ) : null}
    </Panel>
  );
}

/** 一层分组：换属性、点值筛选、删掉这一层。不点任何值就是「全部」。 */
function GroupRow({
  group,
  index,
  options,
  taken,
  onChange,
  onRemove,
}: {
  group: Group;
  index: number;
  options: { id: string; label: string }[];
  taken: OutputDimension[];
  onChange: (next: Group) => void;
  onRemove: (() => void) | undefined;
}) {
  // 匿名会话是一等筛选值：M2a 的数据没有采集员，必须问得出来（docs/145）。
  const values =
    group.dimension === "annotator" ? [...options, { id: "none", label: "未挂采集员" }] : options;
  const toggle = (id: string) =>
    onChange({
      ...group,
      values: group.values.includes(id)
        ? group.values.filter((value) => value !== id)
        : [...group.values, id],
    });

  return (
    <div className="group-row">
      <span className="group-index">{index + 1}</span>
      <select
        value={group.dimension}
        onChange={(event) =>
          onChange({ dimension: event.target.value as OutputDimension, values: [] })
        }
      >
        {DIMENSIONS.filter((entry) => !taken.includes(entry.id)).map((entry) => (
          <option key={entry.id} value={entry.id}>
            {entry.label}
          </option>
        ))}
      </select>
      <span className="chips">
        <span className="muted">
          {group.values.length > 0 ? `已选 ${group.values.length} 项` : "全部"}
        </span>
        {values.map((option) => (
          <button
            key={option.id}
            type="button"
            className="chip"
            aria-pressed={group.values.includes(option.id)}
            onClick={() => toggle(option.id)}
          >
            {option.label}
          </button>
        ))}
      </span>
      {onRemove ? (
        <button type="button" className="link group-drop" onClick={onRemove} aria-label="移除这一层分组">
          ×
        </button>
      ) : null}
    </div>
  );
}

function ReportBody({
  report,
  metric,
  aggregate,
  onAggregate,
}: {
  report: OutputReport;
  metric: MetricId;
  aggregate: "sum" | "average";
  onAggregate: (next: "sum" | "average") => void;
}) {
  // 排序不进 URL：它是读表时的临时动作，不是要收藏的那个视图。
  const [sort, setSort] = useState<{ key: string; descending: boolean }>({
    key: "total",
    descending: true,
  });

  const valueOf = (row: OutputRow, key: string): number => {
    if (key === "total") return aggregateOf(row.totals, metric, aggregate, report.buckets.length);
    const at = report.buckets.indexOf(key);
    return at < 0 ? 0 : (row.cells[at]?.[metric] ?? 0);
  };
  const rows = [...report.rows].sort((left, right) => {
    const delta = valueOf(right, sort.key) - valueOf(left, sort.key);
    const ordered = sort.descending ? delta : -delta;
    return ordered || left.labels.join().localeCompare(right.labels.join());
  });

  const peak = Math.max(1, ...report.rows.flatMap((row) => row.cells.map((cell) => cell[metric])));
  // 按人数去重，不是按行数：叠了「游戏」这一层之后，同一个人有几个游戏就有几行，
  // 数行会把「5 个人」读成「10 个人」。
  const activeAnnotators =
    report.groupBy[0] === "annotator"
      ? new Set(
          report.rows
            .filter((row) => row.keys[0] !== "none" && row.totals.clipCount > 0)
            .map((row) => row.keys[0]),
        ).size
      : null;
  const toggleSort = (key: string) =>
    setSort((current) =>
      current.key === key ? { key, descending: !current.descending } : { key, descending: true },
    );

  if (report.rows.length === 0) {
    return (
      <EmptyState
        title="这个区间内没有产出"
        explanation="所选区间、分组与筛选下没有任何片段。换个区间，或把某一层分组的值筛选清空再看。"
      />
    );
  }

  return (
    <>
      <div className="tiles">
        <Tile
          label="区间合计"
          value={formatHours(report.totals.capturedSeconds)}
          note={`${report.totals.clipCount.toLocaleString()} 片段 · ${formatBytes(report.totals.storedBytes)}`}
        />
        {activeAnnotators !== null && report.enrolledAnnotatorCount !== null ? (
          <Tile
            label="参与采集员"
            value={`${activeAnnotators} / ${report.enrolledAnnotatorCount}`}
            note="在册中区间内有产出"
            tone={activeAnnotators < report.enrolledAnnotatorCount ? "warn" : "mute"}
          />
        ) : null}
        <Tile
          label="已验证"
          value={
            report.totals.clipCount > 0
              ? `${Math.round((report.totals.verifiedClipCount / report.totals.clipCount) * 100)}%`
              : "—"
          }
          note={`${report.totals.verifiedClipCount.toLocaleString()} 片段`}
        />
        <Tile
          label="健康异常"
          value={report.totals.unhealthyClipCount.toLocaleString()}
          note="片段，现值回看"
          tone={report.totals.unhealthyClipCount > 0 ? "warn" : "mute"}
        />
        {/* depth 单列：只有装了实验包、且该游戏做过标定的机器才产得出，件数就是那条线的产能读数。 */}
        <Tile
          label="带 depth"
          value={report.totals.depthClipCount.toLocaleString()}
          note={`${formatBytes(report.totals.depthBytes)}（已含在合计内）`}
        />
        <Tile
          label="行数"
          value={String(report.rows.length)}
          note={report.groupBy.map((dimension) => dimensionLabel(dimension)).join(" × ")}
        />
      </div>

      <div className="grid-wrap">
        <table className="output-grid">
          <thead>
            <tr>
              {report.groupBy.map((dimension, index) => (
                <th
                  key={dimension}
                  className={`dim pin${index === report.groupBy.length - 1 ? " pin-last" : ""}`}
                  style={{ left: index * 160 }}
                >
                  {dimensionLabel(dimension)}
                </th>
              ))}
              <th className="pin total-col" style={{ left: report.groupBy.length * 160 }}>
                <select
                  value={aggregate}
                  aria-label="合计口径"
                  onChange={(event) => onAggregate(event.target.value === "average" ? "average" : "sum")}
                >
                  <option value="sum">合计</option>
                  <option value="average">{report.bucket === "day" ? "日均" : "月均"}</option>
                </select>
                <button type="button" className="sort" onClick={() => toggleSort("total")}>
                  {sortMark(sort, "total")}
                </button>
              </th>
              {report.buckets.map((bucket) => {
                const label = bucketLabel(bucket, report.bucket);
                return (
                  <th
                    key={bucket}
                    className={isWeekend(bucket, report.bucket) ? "rest" : undefined}
                    onClick={() => toggleSort(bucket)}
                  >
                    <span className="bucket-head">
                      {label.head}
                      {sortMark(sort, bucket)}
                    </span>
                    <span className="bucket-sub">{label.sub}</span>
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.keys.join("/")}>
                {row.keys.map((key, index) => (
                  <td
                    key={`${key}-${index}`}
                    className={`dim pin${index === row.keys.length - 1 ? " pin-last" : ""}`}
                    style={{ left: index * 160 }}
                  >
                    <span title={rowLabel(report.groupBy[index], key, row.labels[index])}>
                      {rowLabel(report.groupBy[index], key, row.labels[index])}
                    </span>
                    {index === 0 && report.groupBy[0] === "annotator" && row.totals.clipCount === 0 ? (
                      <span className="dim-note">区间内无产出</span>
                    ) : null}
                  </td>
                ))}
                <td className="pin total-col numeric" style={{ left: row.keys.length * 160 }}>
                  {formatMetric(
                    metric,
                    aggregateOf(row.totals, metric, aggregate, report.buckets.length),
                    aggregate === "average",
                  )}
                </td>
                {row.cells.map((cell, index) => (
                  <Cell key={report.buckets[index] ?? index} cell={cell} metric={metric} peak={peak} />
                ))}
              </tr>
            ))}
            <tr className="grand-total">
              {report.groupBy.map((dimension, index) => (
                <td
                  key={dimension}
                  className={`dim pin${index === report.groupBy.length - 1 ? " pin-last" : ""}`}
                  style={{ left: index * 160 }}
                >
                  {index === 0 ? "全员合计" : ""}
                </td>
              ))}
              <td className="pin total-col numeric" style={{ left: report.groupBy.length * 160 }}>
                {formatMetric(
                  metric,
                  aggregateOf(report.totals, metric, aggregate, report.buckets.length),
                  aggregate === "average",
                )}
              </td>
              {report.columnTotals.map((cell, index) => (
                <td key={report.buckets[index] ?? index} className="numeric">
                  {cell[metric] > 0 ? formatMetric(metric, cell[metric]) : "·"}
                </td>
              ))}
            </tr>
          </tbody>
        </table>
      </div>

      <p className="muted">
        {report.from} 至 {report.to} ·{" "}
        {report.timeBasis === "recorded" ? "按录制时间统计" : "按上传登记时间统计"} · 天界{" "}
        {report.timeZone} · 深浅只是重复编码，数字始终印在格子里 · ⚠ 表示该格有健康异常片段
      </p>
    </>
  );
}

/**
 * 合计列。九个读数全部可加，所以「合计」就是逐格相加——不存在「按区间重算」与
 * 「逐列相加」不等的第三种口径（那只会在混进 distinct 计数时出现）。
 */
function aggregateOf(
  metrics: OutputMetrics,
  metric: MetricId,
  aggregate: "sum" | "average",
  buckets: number,
): number {
  const total = metrics[metric];
  return aggregate === "average" ? total / Math.max(1, buckets) : total;
}

function Cell({ cell, metric, peak }: { cell: OutputMetrics; metric: MetricId; peak: number }) {
  const value = cell[metric];
  if (value <= 0) return <td className="numeric zero">·</td>;
  // 色阶用 color-mix 叠在表面色上：单色渐深，数字仍然是主编码。
  const weight = Math.min(1, value / peak);
  return (
    <td
      className="numeric"
      style={{ background: `color-mix(in srgb, var(--accent) ${Math.round(10 + weight * 34)}%, transparent)` }}
    >
      {formatMetric(metric, value)}
      {cell.unhealthyClipCount > 0 ? " ⚠" : ""}
    </td>
  );
}

function dimensionLabel(dimension: OutputDimension | undefined): string {
  return DIMENSIONS.find((entry) => entry.id === dimension)?.label ?? "";
}

/**
 * 行名。服务端的 label 不替界面措辞：匿名会话回的是 `none`，中文在这里给；
 * 零产出行的后续维度是空串，渲染成「—」。
 */
function rowLabel(
  dimension: OutputDimension | undefined,
  key: string,
  label: string | undefined,
): string {
  if (dimension === "annotator" && key === "none") return "未挂采集员";
  if (key === "") return "—";
  return label && label !== "" ? label : key;
}

function sortMark(sort: { key: string; descending: boolean }, key: string): string {
  if (sort.key !== key) return "";
  return sort.descending ? " ↓" : " ↑";
}

function Tile({
  label,
  value,
  note,
  tone,
}: {
  label: string;
  value: string;
  note?: string;
  tone?: "warn" | "mute";
}) {
  return (
    <div className={`tile tile-${tone ?? "mute"}`}>
      <span className="tile-label">{label}</span>
      <span className="tile-value">{value}</span>
      {note ? <span className="tile-note">{note}</span> : null}
    </div>
  );
}
