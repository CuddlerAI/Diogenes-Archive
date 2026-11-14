import { useState } from "react";

import {
  ApiError,
  api,
  type Operator,
  type OperatorRole,
  type OperatorWithAnnotator,
} from "../api.js";
import { EmptyState, ErrorNote, Loading, Panel, StateBadge } from "../components.js";
import { formatDateTime } from "../format.js";
import { StepUpPanel } from "../step-up.js";
import { useAsync } from "../useAsync.js";

/**
 * Console administrator management (docs/20 §6.B). Admin-only: the server refuses
 * every write here without `operator.admin`, so hiding the tab is only UX. The
 * backend already existed — `GET/POST/PATCH /console/v1/operators` — this page is
 * the missing operator surface for it.
 *
 * Two rules are enforced by the server and mirrored here so the UI never dangles
 * a control that will be rejected: an admin cannot change their own role or
 * status (self-lockout guard), and an invited operator becomes active only by
 * accepting the invite, never by an admin flipping status.
 */

const roleLabels: Record<OperatorRole, string> = {
  viewer: "只读 viewer",
  operator: "运营 operator",
  admin: "管理员 admin",
};

const statusLabels: Record<string, string> = {
  invited: "待接受邀请",
  active: "正常",
  suspended: "已停用",
  tombstoned: "已注销",
};

/** The inline controls have no room for a prompt, so they point at the panel. */
function stepUpAware(thrown: unknown, fallback: string): string {
  if (thrown instanceof ApiError && thrown.code === "sudo_required") {
    return "请先用上方的「确认身份」再执行。";
  }
  return thrown instanceof ApiError ? thrown.message : fallback;
}

function statusTone(status: string): "ok" | "warn" | "bad" | "mute" {
  if (status === "active") return "ok";
  if (status === "invited") return "warn";
  if (status === "suspended" || status === "tombstoned") return "bad";
  return "mute";
}

export function Operators({ me, sudoActive, onElevated }: {
  me: Operator;
  /** Whether the session already holds a sudo window. */
  sudoActive: boolean;
  onElevated: () => void;
}) {
  const [revision, setRevision] = useState(0);
  const list = useAsync(() => api.operators(), [revision]);
  const reload = () => setRevision((value) => value + 1);

  return (
    <>
      <Panel
        title="管理员账号"
        note="邀请制，无自助注册。邀请后系统发送一次性设密链接（有效期 24h）；被邀请人设密后账号转为正常。改角色或停用会立即吊销对方全部会话。"
      >
        {/* Creating and revoking governance access is a governance write, so it
            sits behind the same step-up as purge and credential issuance. */}
        <StepUpPanel
          active={sudoActive}
          reason="邀请、改角色、停用都会即时改变他人的治理权限，执行前需要再确认一次身份。"
          onElevated={onElevated}
        />
        <InviteOperator onInvited={reload} />
      </Panel>

      {list.status === "loading" ? <Loading /> : null}
      {list.status === "error" ? <ErrorNote message={list.message} /> : null}
      {list.status === "ready" ? (
        list.value.operators.length === 0 ? (
          <EmptyState
            title="还没有管理员账号"
            explanation="用上方的表单邀请第一位；首个超级管理员由运维用 bootstrap-operator CLI 创建。"
          />
        ) : (
          <table>
            <thead>
              <tr>
                <th>邮箱</th>
                <th>角色</th>
                <th>状态</th>
                <th>标注员身份</th>
                <th>最近登录</th>
                <th>创建时间</th>
                <th>操作</th>
              </tr>
            </thead>
            <tbody>
              {list.value.operators.map((operator) => (
                <OperatorRow
                  key={operator.id}
                  operator={operator}
                  isSelf={operator.id === me.id}
                  onChanged={reload}
                />
              ))}
            </tbody>
          </table>
        )
      ) : null}
    </>
  );
}

function InviteOperator({ onInvited }: { onInvited: () => void }) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<OperatorRole>("viewer");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [invited, setInvited] = useState<string | null>(null);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await api.inviteOperator(email.trim(), role);
      setInvited(result.operator.email);
      setEmail("");
      setRole("viewer");
      onInvited();
    } catch (thrown) {
      setError(
        thrown instanceof ApiError
          ? thrown.code === "sudo_required"
            ? "请先用上方的「确认身份」再执行。"
            : thrown.code === "operator_exists"
              ? "该邮箱已存在管理员账号。"
              : thrown.code === "recipient_not_allowed"
                ? "账号已创建，但该环境只向白名单收件人投递邮件，设密链接未发出。"
                : `${thrown.code}：${thrown.message}`
          : "邀请失败，请重试。",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="action-form" onSubmit={submit}>
      <h3>邀请新管理员</h3>
      <p className="muted">
        填入对方邮箱并选择角色。只读可看，运营可写治理数据，管理员还能管理账号。
      </p>
      <label>
        邮箱
        <input
          type="email"
          autoComplete="off"
          value={email}
          placeholder="teammate@diogenesmodel.com"
          onChange={(event) => setEmail(event.target.value)}
          required
        />
      </label>
      <label>
        角色
        <select value={role} onChange={(event) => setRole(event.target.value as OperatorRole)}>
          <option value="viewer">只读 viewer</option>
          <option value="operator">运营 operator</option>
          <option value="admin">管理员 admin</option>
        </select>
      </label>
      {error ? <ErrorNote message={error} /> : null}
      {invited ? (
        <p className="action-result" style={{ padding: "10px 12px" }}>
          已向 {invited} 发送设密邀请（staging：请在 outbox / 白名单收件箱查看链接）。
        </p>
      ) : null}
      <div className="action-buttons">
        <button type="submit" disabled={busy || email.trim() === ""}>
          {busy ? "发送中…" : "发送邀请"}
        </button>
      </div>
    </form>
  );
}

function OperatorRow({
  operator,
  isSelf,
  onChanged,
}: {
  operator: OperatorWithAnnotator;
  isSelf: boolean;
  onChanged: () => void;
}) {
  return (
    <tr>
      <td>{operator.email}</td>
      <td>
        <StateBadge label={roleLabels[operator.role]} tone="mute" />
      </td>
      <td>
        <StateBadge
          label={statusLabels[operator.status] ?? operator.status}
          tone={statusTone(operator.status)}
        />
      </td>
      <td>
        <AnnotatorIdentityCell operator={operator} isSelf={isSelf} />
      </td>
      <td>{formatDateTime(operator.lastLoginAt)}</td>
      <td>{formatDateTime(operator.createdAt)}</td>
      <td>
        {isSelf ? (
          <span className="muted">本人账号不可自改</span>
        ) : (
          <div className="action-row">
            <RoleControl operator={operator} onChanged={onChanged} />
            <StatusControl operator={operator} onChanged={onChanged} />
          </div>
        )}
      </td>
    </tr>
  );
}

/** Role change is explicit: pick a different role, then apply. Applying it
 *  revokes the target's sessions server-side, so it is not offered as a silent
 *  on-change side effect. */
function RoleControl({ operator, onChanged }: { operator: Operator; onChanged: () => void }) {
  const [role, setRole] = useState<OperatorRole>(operator.role);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function apply() {
    setBusy(true);
    setError(null);
    try {
      await api.updateOperator(operator.id, { role });
      onChanged();
    } catch (thrown) {
      setError(stepUpAware(thrown, "改角色失败。"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="inline-field" style={{ flexDirection: "row", alignItems: "center", gap: 6 }}>
      <select
        value={role}
        disabled={busy}
        onChange={(event) => setRole(event.target.value as OperatorRole)}
      >
        <option value="viewer">只读 viewer</option>
        <option value="operator">运营 operator</option>
        <option value="admin">管理员 admin</option>
      </select>
      {role !== operator.role ? (
        <button className="link" onClick={() => void apply()} disabled={busy}>
          {busy ? "应用中…" : "应用角色"}
        </button>
      ) : null}
      {error ? <span className="error-note">{error}</span> : null}
    </span>
  );
}

/** Suspend / reactivate. Suspending revokes sessions immediately. An invited
 *  operator can be suspended (revoking the pending invite) but cannot be flipped
 *  to active — that path is the invite link, which the server enforces. */
function StatusControl({ operator, onChanged }: { operator: Operator; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function set(status: "active" | "suspended") {
    setBusy(true);
    setError(null);
    try {
      await api.updateOperator(operator.id, { status });
      onChanged();
    } catch (thrown) {
      setError(stepUpAware(thrown, "操作失败。"));
    } finally {
      setBusy(false);
    }
  }

  if (operator.status === "tombstoned") return <span className="muted">—</span>;

  return (
    <span className="inline-field" style={{ flexDirection: "row", alignItems: "center", gap: 6 }}>
      {operator.status === "suspended" ? (
        <button className="link" onClick={() => void set("active")} disabled={busy}>
          {busy ? "处理中…" : "恢复"}
        </button>
      ) : (
        <button className="danger" onClick={() => void set("suspended")} disabled={busy}>
          {busy ? "处理中…" : "停用"}
        </button>
      )}
      {error ? <span className="error-note">{error}</span> : null}
    </span>
  );
}

/**
 * The other half of this person's identity.
 *
 * An operator who also contributes capture data has a *separate* annotator
 * account — separate table, separate credential, separate database role
 * (docs/11 §4.1 / §4.2). They are linked by the invite the operator issued to
 * themselves, which is why registering is a real registration on the client and
 * not a flag flipped here.
 *
 * Only the row for the signed-in operator offers to mint that invite: an admin
 * cannot register on someone else's behalf, because redeeming the code means
 * choosing a password and verifying an address.
 */
function AnnotatorIdentityCell({
  operator,
  isSelf,
}: {
  operator: OperatorWithAnnotator;
  isSelf: boolean;
}) {
  const [code, setCode] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (operator.annotator) {
    return (
      <span className="inline-field" style={{ gap: 2 }}>
        <StateBadge label="已注册" tone="ok" />
        <span className="muted" style={{ fontSize: 12 }}>
          {operator.annotator.alias ?? operator.annotator.annotatorId.slice(0, 8)}
        </span>
      </span>
    );
  }

  if (!isSelf) return <span className="muted">未注册</span>;

  if (code) {
    return (
      <span className="inline-field" style={{ gap: 4 }}>
        <code className="token">{code}</code>
        <span className="muted" style={{ fontSize: 12 }}>
          在客户端用它注册，注册完成后这里会显示你的标注员身份。
        </span>
      </span>
    );
  }

  async function issue() {
    setBusy(true);
    setError(null);
    try {
      setCode((await api.issueMyAnnotatorInvite()).code);
    } catch (thrown) {
      setError(thrown instanceof ApiError ? thrown.message : "生成失败，请重试。");
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="inline-field" style={{ gap: 4 }}>
      <button className="link" onClick={() => void issue()} disabled={busy}>
        {busy ? "生成中…" : "生成我的邀请码"}
      </button>
      {error ? <span className="error-note">{error}</span> : null}
    </span>
  );
}
