import { api } from "../api.js";
import { EmptyState, ErrorNote, Loading, Mono, Panel } from "../components.js";
import { formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";

export function AuditTrail({
  title,
  note,
  params,
}: {
  title: string;
  note?: string;
  params: Record<string, string | number | undefined>;
}) {
  const state = useAsync(() => api.auditEvents({ ...params, pageSize: 50 }), [JSON.stringify(params)]);

  return (
    <Panel title={title} {...(note ? { note } : {})}>
      {state.status === "loading" ? <Loading /> : null}
      {state.status === "error" ? <ErrorNote message={state.message} /> : null}
      {state.status === "ready" && state.value.items.length === 0 ? (
        <EmptyState
          title="没有审计事件"
          explanation="审计只记录服务端观察到的事实。若客户端从未调用过控制面，这里就是空的。"
        />
      ) : null}
      {state.status === "ready" && state.value.items.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>时间</th>
              <th>动作</th>
              <th>主体</th>
              <th>对象</th>
              <th>详情</th>
            </tr>
          </thead>
          <tbody>
            {state.value.items.map((entry) => (
              <tr key={entry.id}>
                <td>{formatDateTime(entry.occurredAt)}</td>
                <td>
                  <code>{entry.action}</code>
                </td>
                <td>
                  {entry.actorType} · <Mono value={entry.actorId} />
                </td>
                <td>
                  {entry.entityType} · <Mono value={entry.entityId} />
                </td>
                <td className="details-cell">
                  <code>{JSON.stringify(entry.details)}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}
