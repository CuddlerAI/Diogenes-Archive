import { useState } from "react";

import {
  api,
  consentScopeLabel,
  type AnnotatorDetail,
  type AnnotatorProfile,
  type ClaimIssued,
} from "../api.js";
import {
  EmptyState,
  ErrorNote,
  Field,
  JsonBlock,
  Loading,
  Mono,
  Panel,
  StateBadge,
} from "../components.js";
import { formatBytes, formatDateTime } from "../format.js";
import { useAsync } from "../useAsync.js";
import { AuditTrail } from "./AuditTrail.js";
import { ClientEventsPanel } from "./ClientEvents.js";
import { ContributionPanel } from "./Contribution.js";
import { ConfirmedAction } from "./Governance.js";
import { IM_KINDS, ProfileFields, profileFromValues, valuesFromProfile } from "./profile-fields.js";

/**
 * Shown once after a claim link is issued (docs/34 §5.2): the URL is the only
 * copy the operator will ever see — the server keeps the token's sha256 only.
 * Mail delivery is best-effort; handing the link over by IM is the sanctioned
 * fallback and is what the audit's `delivery` field records.
 */
function ClaimUrlReveal({ issued }: { issued: ClaimIssued }) {
  return (
    <div className="token-reveal">
      <p>
        <strong>设密链接已生成，邮件已尽力发送。</strong>
        如对方收不到邮件，现在复制链接转交本人；关闭后无法再次查看，只能重新签发。
        链接单次有效，密码由本人在链接页设置。
      </p>
      <code className="token">{issued.claimUrl}</code>
      <button
        className="link"
        onClick={() => void navigator.clipboard?.writeText(issued.claimUrl)}
      >
        复制
      </button>
    </div>
  );
}

/**
 * Operator-initiated registration (docs/34 §5.2). The text fields ride the
 * shared ConfirmedAction; the profile block keeps its own state outside it —
 * the same shape IssueGrant uses for its selects. No invite code: the
 * sudo-gated action is itself the authorization decision.
 */
function RegisterAnnotator({ onDone }: { onDone: () => void }) {
  const [profileValues, setProfileValues] = useState<Record<string, string>>({});
  const [note, setNote] = useState("");

  return (
    <div className="register-device">
      <ConfirmedAction
        label="代登记"
        title="代登记采集员"
        confirmation="创建档案与账号，并生成一次性设密链接（邮件尽力送达，也可复制转交本人）。密码与条款同意都由本人在链接页完成，这里不代设密码、不代签条款。"
        fields={[
          { name: "email", label: "邮箱", placeholder: "collector@example.com" },
          { name: "alias", label: "别名", placeholder: "系统内标识，如 alpha" },
        ]}
        run={async (values) => {
          const profile = profileFromValues(profileValues);
          const trimmedNote = note.trim();
          const issued = await api.registerAnnotator({
            email: values.email?.trim() ?? "",
            alias: values.alias?.trim() ?? "",
            ...(trimmedNote ? { note: trimmedNote } : {}),
            ...(profile ? { profile } : {}),
          });
          return <ClaimUrlReveal issued={issued} />;
        }}
        onDone={onDone}
      />
      <label className="inline-field">
        备注（可选，只进审计）
        <input
          value={note}
          maxLength={256}
          placeholder="线下引导 / 某渠道"
          onChange={(event) => setNote(event.target.value)}
        />
      </label>
      <ProfileFields
        values={profileValues}
        onChange={setProfileValues}
        legend="联络资料预填（可选，本人之后可改）"
      />
    </div>
  );
}

export function Annotators({
  onOpenDevice,
  canWrite,
}: {
  onOpenDevice: (id: string) => void;
  canWrite: boolean;
}) {
  const [status, setStatus] = useState("");
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const list = useAsync(
    () => api.annotators({ status: status || undefined, query: query || undefined, pageSize: 50 }),
    [status, query, revision],
  );

  if (selected) {
    return (
      <AnnotatorDetailView
        id={selected}
        canWrite={canWrite}
        onBack={() => setSelected(null)}
        onOpenDevice={onOpenDevice}
      />
    );
  }

  return (
    <Panel title="标注用户" note="身份来自控制面 annotators 表。">
      <div className="filters">
        <label>
          状态
          <select value={status} onChange={(event) => setStatus(event.target.value)}>
            <option value="">全部</option>
            <option value="active">active</option>
            <option value="suspended">suspended</option>
            <option value="tombstoned">tombstoned</option>
          </select>
        </label>
        <label>
          别名
          <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索 alias" />
        </label>
        {canWrite ? (
          <ConfirmedAction
            label="建档"
            title="新建标注用户"
            confirmation="建档只登记身份，不产生任何 consent 或采集授权，也没有可登录的账号；之后可在详情页补发设密链接。要一步到位请用「代登记」。"
            fields={[{ name: "alias", label: "别名", placeholder: "alpha" }]}
            run={async (values, key) => {
              await api.createAnnotator({ alias: values.alias ?? "" }, key);
            }}
            onDone={() => setRevision((current) => current + 1)}
          />
        ) : null}
      </div>
      {canWrite ? <RegisterAnnotator onDone={() => setRevision((current) => current + 1)} /> : null}

      {list.status === "loading" ? <Loading /> : null}
      {list.status === "error" ? <ErrorNote message={list.message} /> : null}
      {list.status === "ready" && list.value.items.length === 0 ? (
        <EmptyState
          title="没有标注用户"
          explanation="目前还没有已注册的标注用户。设备与数据仍可在「标注设备」和「数据浏览」中按未归属查看，这里不会为它们编造归属。"
        />
      ) : null}

      {list.status === "ready" && list.value.items.length > 0 ? (
        <table>
          <thead>
            <tr>
              <th>别名</th>
              <th>状态</th>
              <th>设备</th>
              <th>consent</th>
              <th>grant</th>
              <th>会话</th>
              <th>片段</th>
              <th>已验证</th>
              <th>存储量</th>
              <th>最近活动</th>
            </tr>
          </thead>
          <tbody>
            {list.value.items.map((item) => (
              <tr key={item.id} onClick={() => setSelected(item.id)} className="clickable">
                <td>{item.alias ?? "—"}</td>
                <td>
                  <StateBadge label={item.status} tone={item.status === "active" ? "ok" : "warn"} />
                </td>
                <td>{item.deviceCount}</td>
                <td>{item.activeConsentCount}</td>
                <td>{item.activeGrantCount}</td>
                <td>{item.sessionCount}</td>
                <td>{item.clipCount}</td>
                <td>{item.verifiedClipCount}</td>
                <td>{formatBytes(item.storedBytes)}</td>
                <td>{formatDateTime(item.lastActivityAt)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </Panel>
  );
}

function AnnotatorDetailView({
  id,
  canWrite,
  onBack,
  onOpenDevice,
}: {
  id: string;
  canWrite: boolean;
  onBack: () => void;
  onOpenDevice: (deviceId: string) => void;
}) {
  const [revision, setRevision] = useState(0);
  const state = useAsync(() => api.annotator(id), [id, revision]);

  if (state.status === "loading") return <Loading />;
  if (state.status === "error") return <ErrorNote message={state.message} />;
  const annotator = state.value;

  return (
    <>
      <button className="link" onClick={onBack}>
        ← 返回列表
      </button>

      <Panel title={annotator.alias ?? "未命名标注用户"}>
        <div className="fields">
          <Field label="ID">
            <Mono value={annotator.id} />
          </Field>
          <Field label="状态">
            <StateBadge label={annotator.status} tone={annotator.status === "active" ? "ok" : "warn"} />
          </Field>
          <Field label="外部主体哈希">
            <Mono value={annotator.externalSubjectHash} />
          </Field>
          <Field label="创建时间">{formatDateTime(annotator.createdAt)}</Field>
        </div>
        {canWrite ? (
          <ConfirmedAction
            label={annotator.status === "active" ? "停用" : "恢复"}
            title={annotator.status === "active" ? "停用标注用户" : "恢复标注用户"}
            confirmation={
              annotator.status === "active"
                ? "停用后该用户不再被视为有效身份，已采集的数据不受影响。"
                : "恢复后该用户重新成为有效身份。"
            }
            destructive={annotator.status === "active"}
            run={async (_values, key) => {
              await api.updateAnnotator(
                id,
                { status: annotator.status === "active" ? "suspended" : "active" },
                key,
              );
            }}
            onDone={() => setRevision((current) => current + 1)}
          />
        ) : null}
      </Panel>

      {/* How this user got in. Three entry paths coexist — client self-signup
          (invite code present), operator registration (credential, no code),
          bare governance record (no credential) — and the panel says which. */}
      <Panel
        title="账号与邀请来源"
        note={
          annotator.identity
            ? annotator.identity.inviteCode
              ? "该用户凭邀请码自助注册进入。"
              : "该用户由运营代登记，账号经一次性设密链接激活（或待激活）。"
            : "该用户由运营手工建档，还没有可登录的账号；可在下方补发设密链接。"
        }
      >
        {annotator.identity ? (
          <div className="fields">
            <Field label="邮箱">{annotator.identity.email}</Field>
            <Field label="账号状态">
              <StateBadge
                label={
                  annotator.identity.credentialStatus === "active"
                    ? "已激活"
                    : annotator.identity.credentialStatus === "pending_verification"
                      ? "待本人设密/验证"
                      : "已停用"
                }
                tone={annotator.identity.credentialStatus === "active" ? "ok" : "warn"}
              />
            </Field>
            <Field label="注册时间">{formatDateTime(annotator.identity.registeredAt)}</Field>
            <Field label="邮箱验证时间">{formatDateTime(annotator.identity.emailVerifiedAt)}</Field>
            <Field label="最后登录">{formatDateTime(annotator.identity.lastLoginAt)}</Field>
            <Field label="使用的邀请码">
              <Mono value={annotator.identity.inviteCode} />
            </Field>
            <Field label="邀请码发放人">{annotator.identity.invitedBy ?? "—"}</Field>
            <Field label="兑换时间">{formatDateTime(annotator.identity.redeemedAt)}</Field>
          </div>
        ) : (
          <EmptyState
            title="没有可登录的账号"
            explanation="这条档案只有身份，没有凭据。补发设密链接后，本人即可设置密码、签署条款并激活账号。"
          />
        )}

        {canWrite ? (
          <CredentialActions annotator={annotator} onDone={() => setRevision((current) => current + 1)} />
        ) : null}
      </Panel>

      <Panel
        title="联络资料"
        note="仅用于运营协调采集（docs/34 §5.4）。本人可在个人中心自行维护；注销时载荷自动清空。"
      >
        <AnnotatorProfilePanel
          annotatorId={annotator.id}
          profile={annotator.profile}
          canWrite={canWrite && annotator.status !== "tombstoned"}
          onSaved={() => setRevision((current) => current + 1)}
        />
      </Panel>

      <ContributionPanel load={(days, timeBasis) => api.annotatorContribution(id, days, timeBasis)} />

      <Panel title="Consent">
        {annotator.consents.length === 0 ? (
          <EmptyState
            title="没有 consent 记录"
            explanation="consent 由标注员在客户端注册时对当次 terms 版本签署产生（记录会写下他实际看到的那一版）。console 有录入接口，但这里刻意不提供表单——手工填写等于替他人声明他同意过，而正规路径是他本人在系统内签署。"
          />
        ) : (
          <table>
            <thead>
              <tr>
                <th>policy</th>
                <th>授权范围</th>
                <th>状态</th>
                <th>允许训练</th>
                <th>接受时间</th>
                <th>到期</th>
              </tr>
            </thead>
            <tbody>
              {annotator.consents.map((consent) => (
                <tr key={consent.id}>
                  <td>{consent.policyVersion}</td>
                  {/* 用途那一列对每一行都是同一句部署配置，占着最宽的位置却
                      从不用来判断任何事。换成授权范围——「这个人授权了音频没有」
                      才是运营看这张表时真正在问的（docs/108 §3.7）。 */}
                  <td>
                    {consent.scopes.length > 0
                      ? consent.scopes.map(consentScopeLabel).join("、")
                      : "未声明"}
                  </td>
                  <td>
                    {/* 换版不是撤回（docs/108 D5）：把 superseded 行显示成
                        「已撤回」会让「这个人补过签」读起来像「这个人撤回过授权」，
                        而后者按条款是要触发 purge 的。 */}
                    <StateBadge
                      label={
                        consent.revokedAt
                          ? "已撤回"
                          : consent.supersededAt
                            ? "已换版"
                            : consent.status
                      }
                      tone={consent.status === "active" ? "ok" : "warn"}
                    />
                  </td>
                  <td>{consent.allowsTrainableUse ? "是" : "否"}</td>
                  <td>{formatDateTime(consent.acceptedAt)}</td>
                  <td>{formatDateTime(consent.expiresAt)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      {/* 只读历史（docs/100 §4.4）：签发/撤销入口随 capture grant 车道退役，这张表
          留着是为了让既有的 grant 行仍然看得见——采集不再需要它们，但它们记录了
          一段真实发生过的治理。表与视图的收敛归下一刀。 */}
      <Panel title="采集授权（历史）">
        {annotator.grants.length === 0 ? (
          <EmptyState
            title="没有采集授权"
            explanation="采集授权车道已废除（docs/100）：录制不再需要逐次授权凭证，能录哪些游戏由已验签目录决定。这里只显示历史记录。"
          />
        ) : (
          <table>
            <thead>
              <tr>
                <th>游戏</th>
                <th>状态</th>
                <th>允许训练</th>
                <th>生效</th>
                <th>失效</th>
                <th>签发人</th>
              </tr>
            </thead>
            <tbody>
              {annotator.grants.map((grant) => (
                <tr key={grant.id}>
                  <td>{grant.gameId}</td>
                  <td>
                    <StateBadge label={grant.status} tone={grant.status === "active" ? "ok" : "warn"} />
                  </td>
                  <td>{grant.allowsTrainable ? "是" : "否"}</td>
                  <td>{formatDateTime(grant.validFrom)}</td>
                  <td>{formatDateTime(grant.validUntil)}</td>
                  <td>{grant.issuedBy}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Panel>

      <Panel title="绑定设备">
        {annotator.devices.length === 0 ? (
          <EmptyState title="没有绑定设备" explanation="该标注用户名下没有登记设备。" />
        ) : (
          <div className="cards">
            {annotator.devices.map((device) => (
              <button key={device.id} className="card" onClick={() => onOpenDevice(device.id)}>
                <strong>{device.clientDeviceId}</strong>
                <span>{device.gpuPciId}</span>
                <span>
                  {device.sessionCount} 会话 · {device.clipCount} 片段
                </span>
              </button>
            ))}
          </div>
        )}
      </Panel>

      <AuditTrail
        title="行为日志"
        note="口径为服务端审计事实（API 调用与生命周期事件）。客户端本地诊断日志不上传云端；经白名单的客户端遥测在下方「客户端遥测」面板。"
        params={{ actorType: "annotator", actorId: annotator.id }}
      />

      <ClientEventsPanel title="客户端遥测" params={{ annotatorId: annotator.id }} />

      <Panel title="原始记录">
        <JsonBlock value={annotator} />
      </Panel>
    </>
  );
}

/**
 * The operator's credential levers (docs/34 §5.2 / §6.5), keyed off the
 * credential state so only the sensible one is offered: no credential → issue
 * a claim link (needs an address), pending → re-issue to the stored address
 * (the old link dies), active → trigger a reset. Every one hands the person a
 * link to set their own password; none of them sets or reveals one.
 */
function CredentialActions({
  annotator,
  onDone,
}: {
  annotator: AnnotatorDetail;
  onDone: () => void;
}) {
  const identity = annotator.identity;

  if (!identity) {
    return (
      <ConfirmedAction
        label="补发设密链接"
        title="为该档案创建账号"
        confirmation="创建账号并生成一次性设密链接（邮件尽力送达，也可复制转交本人）。密码与条款同意由本人在链接页完成。"
        fields={[{ name: "email", label: "本人邮箱", placeholder: "collector@example.com" }]}
        run={async (values) => {
          const issued = await api.inviteAnnotatorCredential(
            annotator.id,
            values.email?.trim() ?? "",
          );
          return <ClaimUrlReveal issued={issued} />;
        }}
        onDone={onDone}
      />
    );
  }

  if (identity.credentialStatus === "pending_verification") {
    return (
      <ConfirmedAction
        label="重发设密链接"
        title="重发设密链接"
        confirmation={`向 ${identity.email} 重新签发一次性设密链接；旧链接立即作废。`}
        run={async () => {
          const issued = await api.inviteAnnotatorCredential(annotator.id, identity.email);
          return <ClaimUrlReveal issued={issued} />;
        }}
        onDone={onDone}
      />
    );
  }

  if (identity.credentialStatus === "active") {
    return (
      <ConfirmedAction
        label="触发密码重置"
        title="触发密码重置"
        confirmation={`为 ${identity.email} 签发一次性重置链接。本人用链接设置新密码后，其所有旧登录会立即退出；未使用则一切不变。明文密码任何时候不经过运营。`}
        destructive
        run={async () => {
          const issued = await api.triggerAnnotatorPasswordReset(annotator.id);
          return <ClaimUrlReveal issued={issued} />;
        }}
        onDone={onDone}
      />
    );
  }

  return <p className="muted">该账号已停用，恢复后才能重置密码。</p>;
}

function profileKindLabel(kind: string | null): string {
  return IM_KINDS.find((entry) => entry.value === kind)?.label ?? kind ?? "—";
}

function AnnotatorProfilePanel({
  annotatorId,
  profile,
  canWrite,
  onSaved,
}: {
  annotatorId: string;
  profile: AnnotatorProfile | null;
  canWrite: boolean;
  onSaved: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.updateAnnotatorProfile(annotatorId, profileFromValues(values) ?? {});
      setEditing(false);
      onSaved();
    } catch (thrown) {
      setError(thrown instanceof Error ? thrown.message : "保存失败，请重试。");
    } finally {
      setBusy(false);
    }
  }

  if (editing) {
    return (
      <form className="action-form" onSubmit={save}>
        <ProfileFields values={values} onChange={setValues} legend="" />
        {error ? <ErrorNote message={error} /> : null}
        <div className="action-buttons">
          <button type="submit" disabled={busy}>
            {busy ? "保存中…" : "保存"}
          </button>
          <button type="button" className="link" onClick={() => setEditing(false)}>
            取消
          </button>
        </div>
      </form>
    );
  }

  return (
    <>
      {profile ? (
        <div className="fields">
          <Field label="称呼">{profile.displayName ?? "—"}</Field>
          <Field label="电话">{profile.phone ?? "—"}</Field>
          <Field label="IM">
            {profile.imKind ? `${profileKindLabel(profile.imKind)} · ${profile.imHandle ?? ""}` : "—"}
          </Field>
          <Field label="地区">{profile.region ?? "—"}</Field>
          <Field label="更新时间">{formatDateTime(profile.updatedAt)}</Field>
        </div>
      ) : (
        <EmptyState
          title="没有联络资料"
          explanation="资料是可选的：本人可在个人中心填写，运营也可在此代填。没有资料不影响任何采集功能。"
        />
      )}
      {canWrite ? (
        <button
          className="link"
          onClick={() => {
            setValues(valuesFromProfile(profile));
            setEditing(true);
          }}
        >
          {profile ? "编辑资料" : "代填资料"}
        </button>
      ) : null}
    </>
  );
}
