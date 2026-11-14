import { useState } from "react";

import { api } from "../api.js";
import { EmptyState, ErrorNote, JsonBlock, Loading, Panel, StateBadge } from "../components.js";
import { formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";

/**
 * 白名单客户端遥测（docs/25，docs/67 §3.6）。`client_events` 与
 * `POST /app/v1/client-events` 早就在线，但 console 一直没有展示入口——
 * 「多少台设备、哪些 build 在 preflight 上失败」只能查库。
 *
 * 与审计事件的分工不变：审计是服务端观察到的事实且不随 purge 消失，这里是
 * 客户端自报的行为遥测，可随 consent 撤销一并清除。
 */
export function ClientEventsPanel({
  title,
  params,
}: {
  title: string;
  params: { deviceId?: string; annotatorId?: string; sessionId?: string };
}) {
  const [severity, setSeverity] = useState("");
  const key = JSON.stringify({ ...params, severity });
  const state = useAsync(
    () => api.clientEvents({ ...params, severity: severity || undefined, pageSize: 50 }),
    [key],
  );

  return (
    <Panel
      title={title}
      note="客户端自报的白名单遥测（docs/25）。本地 JSONL 诊断日志仍不上传；这里也不含任何绝对路径或设备句柄。"
    >
      <div className="filters">
        <label>
          严重级
          <select value={severity} onChange={(event) => setSeverity(event.target.value)}>
            <option value="">全部</option>
            {["error", "warn", "info", "debug"].map((level) => (
              <option key={level} value={level}>
                {level}
              </option>
            ))}
          </select>
        </label>
      </div>

      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" && state.value.items.length === 0 ? (
        <EmptyState
          title="没有遥测事件"
          explanation="客户端只上报白名单事件，且需要设备凭据。没有真实客户端调用过控制面时，这里就是空的。"
        />
      ) : null}
      {state.status === "ready" && state.value.items.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>发生时间</th>
              <th>事件</th>
              <th>严重级</th>
              <th>客户端版本</th>
              <th>详情</th>
            </tr>
          </thead>
          <tbody>
            {state.value.items.map((event) => (
              <tr key={event.eventId}>
                <td>{formatDateTime(event.occurredAt)}</td>
                <td>{event.kind}</td>
                <td>
                  <StateBadge
                    label={event.severity}
                    tone={event.severity === "error" ? "bad" : event.severity === "warn" ? "warn" : "mute"}
                  />
                </td>
                <td>{event.clientVersion}</td>
                <td className="details-cell">
                  <JsonBlock value={event.details} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}
