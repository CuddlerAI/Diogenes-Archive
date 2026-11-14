import { useState } from "react";

import { api, newIdempotencyKey } from "../api.js";
import { EmptyState, ErrorNote, Loading, Mono, Panel, StateBadge } from "../components.js";
import { formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";
import { StepUpPanel } from "../step-up.js";
import { ConfirmedAction } from "./Governance.js";

/**
 * Invite administration. Issuing a code is an authorization decision, so both
 * creating and revoking go through ConfirmedAction — the same confirmation,
 * idempotency key, and sudo escalation the governance writes use.
 *
 * The plaintext codes come back only in the create response. They are not
 * secrets (11 §12.2) and the list keeps showing them, but the moment of issuing
 * is still the moment to copy them out.
 */
export function Invites({
  canWrite,
  sudoActive,
  onElevated,
}: {
  canWrite: boolean;
  sudoActive: boolean;
  onElevated: () => void;
}) {
  const [revision, setRevision] = useState(0);
  const [filter, setFilter] = useState<"all" | "active" | "revoked">("all");
  const list = useAsync(
    () => api.invites(filter === "all" ? undefined : filter),
    [filter, revision],
  );
  const reload = () => setRevision((value) => value + 1);

  return (
    <>
      <Panel
        title="邀请码"
        note="一码一用户。生成后把明文交给要加入的标注用户，他们在客户端注册时输入；用过的码不能再次使用。"
      >
        <div className="filters">
          <label>
            状态
            <select
              value={filter}
              onChange={(event) => setFilter(event.target.value as typeof filter)}
            >
              <option value="all">全部</option>
              <option value="active">有效</option>
              <option value="revoked">已吊销</option>
            </select>
          </label>
        </div>

        {canWrite ? (
          <div className="action-row">
            {/* Issuing and revoking invite codes is sudo-gated server side
                (invite-admin). Without this the first click always failed and
                then asked for the password, which reads as an error rather than
                as a deliberate confirmation step. */}
            <StepUpPanel
              active={sudoActive}
              reason="邀请码决定谁能进入系统，生成与吊销前需要再确认一次身份。"
              onElevated={onElevated}
            />
            <ConfirmedAction
              label="生成邀请码"
              title="生成邀请码"
              confirmation="生成后明文只显示这一次。数量默认 1，可填备注记录发放对象。"
              fields={[
                { name: "count", label: "数量（1–100）", placeholder: "1" },
                { name: "note", label: "备注（可选）", placeholder: "例如：2026Q3 外包批次" },
              ]}
              run={async (values, key) => {
                const count = Number(values.count ?? "1");
                const note = (values.note ?? "").trim();
                const created = await api.createInvites(
                  {
                    count: Number.isFinite(count) && count >= 1 ? Math.floor(count) : 1,
                    ...(note === "" ? {} : { note }),
                  },
                  key,
                );
                return <IssuedInvites codes={created.codes} />;
              }}
              onDone={reload}
            />
          </div>
        ) : null}
      </Panel>

      {list.status === "loading" ? <Loading /> : null}
      {list.status === "error" ? <ErrorNote message={list.message} /> : null}
      {list.status === "ready" ? (
        list.value.invites.length === 0 ? (
          <EmptyState
            title="还没有邀请码"
            explanation="生成一批，交给要加入的标注用户；他们注册后这里会显示兑换人。"
          />
        ) : (
          <table>
            <thead>
              <tr>
                <th>邀请码</th>
                <th>状态</th>
                <th>使用</th>
                <th>备注</th>
                <th>创建人</th>
                <th>兑换用户</th>
                <th>创建时间</th>
                {canWrite ? <th /> : null}
              </tr>
            </thead>
            <tbody>
              {list.value.invites.map((invite) => (
                <tr key={invite.code}>
                  <td>
                    <Mono value={invite.displayCode} />
                  </td>
                  <td>
                    <StateBadge
                      label={invite.status === "active" ? "有效" : "已吊销"}
                      tone={invite.status === "active" ? "ok" : "mute"}
                    />
                  </td>
                  <td>
                    {invite.usedCount}/{invite.maxUses}
                  </td>
                  <td>{invite.note ?? <span className="muted">—</span>}</td>
                  <td>{invite.createdBy ?? <span className="muted">—</span>}</td>
                  <td>
                    {invite.usedByAnnotatorId ? (
                      <Mono value={invite.usedByAnnotatorId} />
                    ) : (
                      <span className="muted">未使用</span>
                    )}
                  </td>
                  <td>{formatDateTime(invite.createdAt)}</td>
                  {canWrite ? (
                    <td>
                      {/* Revocation stops further redemption, so it is offered
                          only while there is something left to stop. */}
                      {invite.status === "active" && invite.usedCount < invite.maxUses ? (
                        <ConfirmedAction
                          label="吊销"
                          title="吊销邀请码"
                          confirmation="吊销后这个码无法再用于注册。已经用它注册的用户不受影响。"
                          destructive
                          run={async () => {
                            await api.revokeInvite(invite.code, newIdempotencyKey());
                            return "已吊销。";
                          }}
                          onDone={reload}
                        />
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                  ) : null}
                </tr>
              ))}
            </tbody>
          </table>
        )
      ) : null}
    </>
  );
}

/** Shown once after creation, mirroring how a device credential is handed over. */
function IssuedInvites({ codes }: { codes: string[] }) {
  return (
    <div className="token-reveal">
      <p>
        <strong>请立即复制并发给对应用户。</strong>
        列表里之后仍能查到这些码，但发放动作就是现在。
      </p>
      {codes.map((code) => (
        <code className="token" key={code}>
          {code}
        </code>
      ))}
      <button className="link" onClick={() => void navigator.clipboard?.writeText(codes.join("\n"))}>
        复制全部
      </button>
    </div>
  );
}
