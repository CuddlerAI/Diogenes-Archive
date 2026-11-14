import { useCallback, useEffect, useState } from "react";

import {
  APP_UNAUTHENTICATED_EVENT,
  ApiError,
  appApi,
  consentScopeLabel,
  describeApiError,
  type AppConsentReceipt,
  type AppConsentState,
  type AppReadiness,
  type AppSessionSummary,
  type AppTerms,
  type MyDevice,
} from "../api.js";
import { EmptyState, ErrorNote, StateBadge } from "../components.js";
import { formatDateTime } from "../format.js";
import { ContributionPanel } from "./Contribution.js";
import { PortalShell } from "./portal-shell.js";
import { ProfileFields, profileFromValues, valuesFromProfile } from "./profile-fields.js";

/**
 * The annotator's self-service page (docs/34 §4.1 /account): sign in, edit
 * contact facts, change the password, see enrolled devices, sign out. The
 * session is the __Host-dma_session cookie — held by the browser, never by
 * this code — and it slides (§6.1a): it ends when its owner signs out, not on
 * a timer.
 *
 * Deliberately absent: anything the desktop client owns. Enrollment, capture,
 * and credential rotation stay in the client; this page manages the account,
 * not the machine.
 */

const MIN_PASSWORD_LENGTH = 12;

type Gate = "checking" | "signed-out" | "signed-in";
type PortalNotice = "registered" | "logged-out" | null;

function noticeFromLocation(): PortalNotice {
  const params = new URLSearchParams(window.location.search);
  if (params.get("registered") === "1") return "registered";
  if (params.get("logged_out") === "1") return "logged-out";
  return null;
}

export function AccountPortal() {
  const [notice, setNotice] = useState<PortalNotice>(noticeFromLocation);
  const [gate, setGate] = useState<Gate>(() =>
    noticeFromLocation() === null ? "checking" : "signed-out",
  );

  const check = useCallback(() => {
    appApi
      .session()
      .then(() => setGate("signed-in"))
      .catch(() => setGate("signed-out"));
  }, []);

  useEffect(() => {
    if (notice === null) check();
  }, [check, notice]);
  useEffect(() => {
    const onExpired = () => setGate("signed-out");
    globalThis.addEventListener?.(APP_UNAUTHENTICATED_EVENT, onExpired);
    return () => globalThis.removeEventListener?.(APP_UNAUTHENTICATED_EVENT, onExpired);
  }, []);

  if (gate === "checking") {
    return (
      <PortalShell>
        <div className="portal-card">
          <h1>个人中心</h1>
          <p className="portal-hint">正在检查登录状态…</p>
        </div>
      </PortalShell>
    );
  }
  if (gate === "signed-out") {
    return (
      <PortalSignIn
        notice={notice}
        onSignedIn={() => {
          window.history.replaceState(null, "", "/account");
          setNotice(null);
          setGate("signed-in");
        }}
      />
    );
  }
  return (
    <PortalHome
      onSignedOut={() => {
        window.history.replaceState(null, "", "/account?logged_out=1");
        setNotice("logged-out");
        setGate("signed-out");
      }}
    />
  );
}

type SignInStage =
  | { name: "password" }
  | { name: "reset-request" }
  | { name: "reset-code"; challengeId: string; expiresInSeconds: number }
  | { name: "reset-done" };

function PortalSignIn({
  notice,
  onSignedIn,
}: {
  notice: PortalNotice;
  onSignedIn: () => void;
}) {
  const [stage, setStage] = useState<SignInStage>({ name: "password" });
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submitPassword(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await appApi.login(email, password);
      onSignedIn();
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function submitResetRequest(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      // Always 202; the wording below stays neutral so this cannot probe which
      // addresses are registered.
      const result = await appApi.requestPasswordReset(email);
      setStage({ name: "reset-code", ...result });
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function submitResetConfirm(event: React.FormEvent) {
    event.preventDefault();
    if (stage.name !== "reset-code") return;
    if (newPassword !== confirm) {
      setError("两次输入的密码不一致。");
      return;
    }
    if (newPassword.length < MIN_PASSWORD_LENGTH) {
      setError(`密码至少 ${MIN_PASSWORD_LENGTH} 位。`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await appApi.confirmPasswordReset(stage.challengeId, code, newPassword);
      setStage({ name: "reset-done" });
    } catch (thrown) {
      setError(describe(thrown));
      setCode("");
    } finally {
      setBusy(false);
    }
  }

  return (
    <PortalShell>
      <div className="portal-card">
        <h1>个人中心</h1>
        <p className="card-sub">管理你的采集账号：联络资料、密码与设备。</p>
        {notice === "registered" ? (
          <p className="portal-note" role="status">
            注册成功，账号已激活。请使用刚注册的邮箱和密码登录；无需再次填写注册表。
          </p>
        ) : null}
        {notice === "logged-out" ? (
          <p className="portal-note" role="status">
            已安全退出登录。
          </p>
        ) : null}

        {stage.name === "password" ? (
          <form onSubmit={submitPassword}>
            <label htmlFor="portal-email">
              邮箱
              <input
                id="portal-email"
                type="email"
                autoComplete="username"
                value={email}
                onChange={(event) => setEmail(event.target.value)}
                required
                autoFocus
              />
            </label>
            <label htmlFor="portal-password">
              密码
              <input
                id="portal-password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                required
              />
            </label>
            <p className="portal-hint">除非你主动退出，登录会一直保持。</p>
            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy}>
              {busy ? "登录中…" : "登录"}
            </button>
            <div className="portal-foot">
              <button
                type="button"
                className="link"
                onClick={() => {
                  setError(null);
                  setStage({ name: "reset-request" });
                }}
                disabled={busy}
              >
                忘记密码？
              </button>
              <button
                type="button"
                className="link"
                onClick={() => (window.location.href = "/join")}
                disabled={busy}
              >
                凭邀请码登记新账号
              </button>
            </div>
          </form>
        ) : null}

        {stage.name === "reset-request" ? (
          <form onSubmit={submitResetRequest}>
            <p className="portal-hint">输入账号邮箱。如果它已注册，会收到一个 6 位验证码。</p>
            <label htmlFor="portal-reset-email">
              邮箱
              <input
                id="portal-reset-email"
                type="email"
                autoComplete="username"
                value={email}
                onChange={(event) => setEmail(event.target.value)}
                required
                autoFocus
              />
            </label>
            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy || email.trim() === ""}>
              {busy ? "发送中…" : "发送验证码"}
            </button>
            <div className="portal-foot">
              <button
                type="button"
                className="link"
                onClick={() => {
                  setError(null);
                  setStage({ name: "password" });
                }}
                disabled={busy}
              >
                返回登录
              </button>
            </div>
          </form>
        ) : null}

        {stage.name === "reset-code" ? (
          <form onSubmit={submitResetConfirm}>
            <p className="portal-hint">
              如果该邮箱已注册，验证码已发出，{Math.round(stage.expiresInSeconds / 60)}{" "}
              分钟内有效。输入验证码并设置新密码；完成后所有旧登录都会退出。
            </p>
            <label htmlFor="portal-code">
              6 位验证码
              <input
                id="portal-code"
                inputMode="numeric"
                autoComplete="one-time-code"
                pattern="[0-9]{6}"
                maxLength={6}
                value={code}
                onChange={(event) => setCode(event.target.value.replace(/\D/g, ""))}
                required
                autoFocus
              />
            </label>
            <div className="field-grid">
              <label htmlFor="portal-new-password">
                新密码
                <input
                  id="portal-new-password"
                  type="password"
                  autoComplete="new-password"
                  minLength={MIN_PASSWORD_LENGTH}
                  value={newPassword}
                  onChange={(event) => setNewPassword(event.target.value)}
                  required
                />
              </label>
              <label htmlFor="portal-new-confirm">
                再次输入
                <input
                  id="portal-new-confirm"
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  onChange={(event) => setConfirm(event.target.value)}
                  required
                />
              </label>
            </div>
            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy || code.length !== 6}>
              {busy ? "提交中…" : "重置密码"}
            </button>
          </form>
        ) : null}

        {stage.name === "reset-done" ? (
          <>
            <p className="portal-note">密码已重置，原有登录已全部退出。现在用新密码登录。</p>
            <div className="portal-foot">
              <button type="button" className="link" onClick={() => setStage({ name: "password" })}>
                返回登录
              </button>
            </div>
          </>
        ) : null}
      </div>
    </PortalShell>
  );
}

function PortalHome({ onSignedOut }: { onSignedOut: () => void }) {
  const [profileValues, setProfileValues] = useState<Record<string, string>>({});
  const [profileLoaded, setProfileLoaded] = useState(false);
  const [profileNote, setProfileNote] = useState<string | null>(null);
  const [devices, setDevices] = useState<MyDevice[] | null>(null);
  const [summary, setSummary] = useState<AppSessionSummary | null>(null);
  const [readiness, setReadiness] = useState<AppReadiness | null>(null);
  const [consents, setConsents] = useState<AppConsentReceipt[] | null>(null);
  const [consentState, setConsentState] = useState<AppConsentState | null>(null);
  const [terms, setTerms] = useState<AppTerms | null>(null);
  const [consentAccepted, setConsentAccepted] = useState(false);
  const [consentNote, setConsentNote] = useState<string | null>(null);
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [passwordNote, setPasswordNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void appApi
      .profile()
      .then((result) => {
        setProfileValues(valuesFromProfile(result.profile));
        setProfileLoaded(true);
      })
      .catch(() => setProfileLoaded(true));
    void appApi
      .devices()
      .then((result) => setDevices(result.devices))
      .catch(() => setDevices([]));
    // 自视图三份数据（docs/66 批次 C）各自失败各自留空：任何一份挂掉都不该
    // 拖垮资料与密码这两个已有功能。
    void appApi
      .session()
      .then(setSummary)
      .catch(() => setSummary(null));
    void appApi
      .readiness()
      .then(setReadiness)
      .catch(() => setReadiness(null));
    void appApi
      .consents()
      .then((result) => setConsents(result.consents))
      .catch(() => setConsents([]));
    void appApi
      .consentState()
      .then(setConsentState)
      .catch(() => setConsentState(null));
  }, []);

  // 条款正文只在真的要补签时才拉：这一页多数时候没有待办，不该为此多打一个请求。
  // 拉不到就不给可勾选的对象——对一份读不到的文书表示同意，不算同意。
  useEffect(() => {
    if (!consentState?.resignRequired || terms !== null) return;
    void appApi
      .terms()
      .then(setTerms)
      .catch(() => setTerms(null));
  }, [consentState, terms]);

  async function renewConsent() {
    if (!terms) return;
    setBusy(true);
    setError(null);
    setConsentNote(null);
    try {
      await appApi.renewConsent(terms.policyVersion);
      setConsentAccepted(false);
      const [nextState, nextReceipts] = await Promise.all([
        appApi.consentState(),
        appApi.consents(),
      ]);
      setConsentState(nextState);
      setConsents(nextReceipts.consents);
      setConsentNote("已签署当前条款。");
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function saveProfile(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setProfileNote(null);
    try {
      const result = await appApi.updateProfile(profileFromValues(profileValues) ?? {});
      setProfileValues(valuesFromProfile(result.profile));
      setProfileNote("资料已保存。");
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function changePassword(event: React.FormEvent) {
    event.preventDefault();
    if (newPassword !== confirm) {
      setError("两次输入的新密码不一致。");
      return;
    }
    if (newPassword.length < MIN_PASSWORD_LENGTH) {
      setError(`新密码至少 ${MIN_PASSWORD_LENGTH} 位。`);
      return;
    }
    setBusy(true);
    setError(null);
    setPasswordNote(null);
    try {
      await appApi.changePassword(currentPassword, newPassword);
      setCurrentPassword("");
      setNewPassword("");
      setConfirm("");
      setPasswordNote("密码已修改。其他设备上的登录已退出，当前登录保持有效。");
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function signOut() {
    setBusy(true);
    setError(null);
    try {
      await appApi.logout();
    } catch (thrown) {
      if (thrown instanceof ApiError && thrown.status === 401) {
        // The server already considers this session invalid, so signed-out is
        // the only truthful state even though the logout write returned 401.
        onSignedOut();
        return;
      }
      setError("退出登录失败，当前登录仍然保留。请检查网络后重试。");
      return;
    } finally {
      setBusy(false);
    }
    onSignedOut();
  }

  return (
    <PortalShell>
      <div className="portal-card">
        <div className="portal-session">
          <div className="who">
            <span className="dot" />
            <div>
              <h1>个人中心</h1>
              <p className="card-sub">
                {summary
                  ? `${summary.alias ?? "（未设别名）"} · ${summary.email ?? ""}`
                  : "已登录。"}
                除非你主动退出，这个登录会一直保持。
              </p>
            </div>
          </div>
          <button type="button" className="link" onClick={signOut} disabled={busy}>
            {busy ? "退出中…" : "退出登录"}
          </button>
        </div>
        {error ? <ErrorNote message={error} /> : null}
      </div>

      <div className="portal-card">
        <h1>采集就绪清单</h1>
        <p className="card-sub">
          五步全部亮起即可在客户端开始采集。授权由运营签发；如长期未亮，请联系运营。
        </p>
        {readiness === null ? (
          <p className="portal-hint">加载中…</p>
        ) : (
          <ul className="device-rows">
            {readiness.steps.map((step) => (
              <li key={step.key}>
                <StateBadge label={step.ok ? "✓" : "…"} tone={step.ok ? "ok" : "warn"} />
                <span>{READINESS_LABELS[step.key] ?? step.key}</span>
                <span className="meta">{readinessDetail(step)}</span>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="portal-card">
        <h1>数据贡献</h1>
        <p className="card-sub">
          你上传的会话与片段在服务端的登记与校验状态。「已验证」指字节校验通过；
          质量与可训练判定另有流程。
        </p>
        <ContributionPanel load={(days) => appApi.contribution(days)} />
      </div>

      {consentState?.resignRequired ? (
        // 此前这一页只有回执、没有动作：条款变了，本人在网页上无路可走
        // （docs/108 §1.3 缺陷 A）。差集在最前，因为「补签能多授权什么」才是
        // 用来做决定的信息，两个版本号的对比不是。
        <div className="portal-card">
          <h1>需要补充签署</h1>
          <p className="card-sub">
            服务端在服务的条款与你已签的那份不是同一份文书。
            {consentState.pendingScopes.length > 0 ? (
              <>
                {" "}补签后新增授权：
                <strong>{consentState.pendingScopes.map(consentScopeLabel).join("、")}</strong>。
              </>
            ) : (
              " 本次是措辞修订，授权范围不变，但仍需要你签署当前版本。"
            )}
          </p>
          <p className="card-sub">
            已采集的数据仍按你当时签署的那一版归属，不受影响——换版不是撤回。
          </p>
          {terms ? (
            <>
              <textarea
                className="terms-body"
                readOnly
                value={terms.body}
                aria-label="贡献者条款正文"
                rows={16}
              />
              <label className="consent-check">
                <input
                  type="checkbox"
                  checked={consentAccepted}
                  disabled={busy}
                  onChange={(event) => setConsentAccepted(event.target.checked)}
                />
                <span>我已阅读并同意上述条款（版本 {terms.policyVersion}）</span>
              </label>
              <button
                type="button"
                className="primary"
                disabled={busy || !consentAccepted}
                onClick={() => void renewConsent()}
              >
                {busy ? "签署中…" : "签署当前条款"}
              </button>
            </>
          ) : (
            <p className="portal-hint">条款正文没有取到，在读到之前无法签署。刷新后重试。</p>
          )}
          {consentNote ? <p className="portal-note">{consentNote}</p> : null}
        </div>
      ) : null}

      <div className="portal-card">
        <h1>条款回执</h1>
        <p className="card-sub">你在系统内签署过的贡献者条款，版本与时间以服务端记录为准。</p>
        {consents === null ? (
          <p className="portal-hint">加载中…</p>
        ) : consents.length === 0 ? (
          <EmptyState
            title="还没有签署记录"
            explanation="注册时的条款签署会在这里留有回执；如你认为这里应该有记录，请联系运营。"
          />
        ) : (
          <ul className="device-rows">
            {consents.map((receipt) => (
              <li key={receipt.consentId}>
                <code className="mono">{receipt.policyVersion}</code>
                <StateBadge
                  label={
                    receipt.revokedAt ? "已撤回" : receipt.supersededAt ? "已换版" : receipt.status
                  }
                  tone={receipt.revokedAt ? "warn" : receipt.status === "active" ? "ok" : "warn"}
                />
                <span className="meta">
                  签署于 {formatDateTime(receipt.acceptedAt ?? receipt.createdAt)}
                  {receipt.scopes.length > 0
                    ? ` · 授权范围：${receipt.scopes.map(consentScopeLabel).join("、")}`
                    : ""}
                  {receipt.revokedAt ? ` · 撤回于 ${formatDateTime(receipt.revokedAt)}` : ""}
                  {/* 换版不是撤回：显示成「你补签后由新的一份接替」，而不是 status
                      的原文 "expired"（读起来像授权自己过期了）。 */}
                  {!receipt.revokedAt && receipt.supersededAt
                    ? ` · ${formatDateTime(receipt.supersededAt)} 起由新签署的版本接替`
                    : ""}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="portal-card">
        <h1>联络资料</h1>
        <p className="card-sub">仅用于运营协调采集，全部可选。修改密码不影响资料。</p>
        {profileLoaded ? (
          <form onSubmit={saveProfile}>
            <ProfileFields values={profileValues} onChange={setProfileValues} legend="" />
            {profileNote ? <p className="portal-note">{profileNote}</p> : null}
            <button type="submit" disabled={busy}>
              {busy ? "保存中…" : "保存资料"}
            </button>
          </form>
        ) : (
          <p className="portal-hint">加载中…</p>
        )}
      </div>

      <div className="portal-card">
        <h1>修改密码</h1>
        <p className="card-sub">需要输入当前密码；完成后其他设备的登录会退出，本页保持登录。</p>
        <form onSubmit={changePassword}>
          <label htmlFor="acct-current">
            当前密码
            <input
              id="acct-current"
              type="password"
              autoComplete="current-password"
              value={currentPassword}
              onChange={(event) => setCurrentPassword(event.target.value)}
              required
            />
          </label>
          <div className="field-grid">
            <label htmlFor="acct-new">
              新密码
              <input
                id="acct-new"
                type="password"
                autoComplete="new-password"
                minLength={MIN_PASSWORD_LENGTH}
                value={newPassword}
                onChange={(event) => setNewPassword(event.target.value)}
                required
              />
            </label>
            <label htmlFor="acct-confirm">
              再次输入新密码
              <input
                id="acct-confirm"
                type="password"
                autoComplete="new-password"
                value={confirm}
                onChange={(event) => setConfirm(event.target.value)}
                required
              />
            </label>
          </div>
          <p className="portal-hint">至少 {MIN_PASSWORD_LENGTH} 位，不要包含你的邮箱地址。</p>
          {passwordNote ? <p className="portal-note">{passwordNote}</p> : null}
          <button type="submit" disabled={busy}>
            {busy ? "提交中…" : "修改密码"}
          </button>
        </form>
      </div>

      <div className="portal-card">
        <h1>我的设备</h1>
        <p className="card-sub">设备在 Windows 客户端里登记与管理；这里只读。</p>
        {devices === null ? (
          <p className="portal-hint">加载中…</p>
        ) : devices.length === 0 ? (
          <EmptyState
            title="还没有登记设备"
            explanation="在 Windows 客户端用本账号登录并完成设备登记后，这里会列出你的采集机。"
          />
        ) : (
          <ul className="device-rows">
            {devices.map((device) => (
              <li key={device.deviceId}>
                <code className="mono">{device.clientDeviceId}</code>
                <StateBadge
                  label={device.status}
                  tone={device.status === "active" ? "ok" : "warn"}
                />
                <span className="meta">
                  客户端 {device.clientVersion} · 最近活跃 {formatDateTime(device.lastSeenAt)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </PortalShell>
  );
}

function describe(thrown: unknown): string {
  if (!(thrown instanceof ApiError)) return "网络错误，请重试。";
  if (thrown.code === "invalid_credentials") return "无法验证，请检查后重试。";
  return describeApiError(thrown);
}

/** 就绪灯的中文标签（键来自 GET /app/v1/readiness）。 */
const READINESS_LABELS: Record<string, string> = {
  account: "账号已激活",
  consent: "条款已签署",
  device: "设备已登记",
  client_seen: "客户端近 7 天在线过",
};

function readinessDetail(step: { key: string; ok: boolean; detail: string | null }): string {
  if (step.detail === null) return step.ok ? "" : "待完成";
  if (step.key === "device") return step.ok ? `${step.detail} 台可用` : "在客户端登录并登记本机";
  return step.detail;
}
