import { useState } from "react";

import { ApiError, appApi, describeApiError, type ProfileInput } from "../api.js";
import { ErrorNote } from "../components.js";
import { useAsync } from "../useAsync.js";
import { PortalDone, PortalShell } from "./portal-shell.js";
import { ProfileFields, profileFromValues } from "./profile-fields.js";

/**
 * Self-service registration in the browser (docs/34 §5.1). The exact same
 * server contract the desktop client uses — invite code, email, alias,
 * password, and the currently served terms — so the two entry paths cannot
 * drift: the terms text always comes from GET /app/v1/terms, and the version
 * the user confirmed travels back for the server to check.
 *
 * Registering is not capturing: the done screen says what still stands between
 * this account and a recording (client sign-in, device enrollment, a capture
 * grant issued by operations).
 */

const MIN_PASSWORD_LENGTH = 12;

type Stage =
  | { name: "form" }
  | { name: "code"; challengeId: string; expiresInSeconds: number }
  | { name: "done" };

function Steps({ current }: { current: 0 | 1 | 2 }) {
  const labels = ["填写资料", "验证邮箱", "完成"];
  return (
    <div className="steps">
      {labels.map((label, index) => (
        <span key={label} className={index < current ? "done" : index === current ? "on" : ""}>
          {label}
        </span>
      ))}
    </div>
  );
}

export function Join() {
  const terms = useAsync(() => appApi.terms(), []);

  const [stage, setStage] = useState<Stage>({ name: "form" });
  const [inviteCode, setInviteCode] = useState("");
  const [email, setEmail] = useState("");
  const [alias, setAlias] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [profileValues, setProfileValues] = useState<Record<string, string>>({});
  const [accepted, setAccepted] = useState(false);
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submitForm(event: React.FormEvent) {
    event.preventDefault();
    if (terms.status !== "ready") return;
    if (password !== confirm) {
      setError("两次输入的密码不一致。");
      return;
    }
    if (password.length < MIN_PASSWORD_LENGTH) {
      setError(`密码至少 ${MIN_PASSWORD_LENGTH} 位。`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const profile: ProfileInput | undefined = profileFromValues(profileValues);
      const result = await appApi.signup({
        inviteCode,
        email,
        password,
        alias,
        acceptedPolicyVersion: terms.value.policyVersion,
        ...(profile ? { profile } : {}),
      });
      setStage({ name: "code", ...result });
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function submitCode(event: React.FormEvent) {
    event.preventDefault();
    if (stage.name !== "code") return;
    setBusy(true);
    setError(null);
    try {
      await appApi.verifySignup(stage.challengeId, code);
      setStage({ name: "done" });
    } catch (thrown) {
      setError(describe(thrown));
      setCode("");
    } finally {
      setBusy(false);
    }
  }

  async function resend() {
    if (stage.name !== "code") return;
    setBusy(true);
    setError(null);
    try {
      const result = await appApi.resendSignup(stage.challengeId);
      setStage({ name: "code", ...result });
      setCode("");
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  return (
    <PortalShell>
      <div className="portal-card">
        <h1>采集员登记</h1>
        <p className="card-sub">凭邀请码创建采集账号；密码与条款都由你本人完成。</p>
        <Steps current={stage.name === "form" ? 0 : stage.name === "code" ? 1 : 2} />

        {stage.name === "done" ? (
          <PortalDone
            title="注册成功"
            message={
              <>
                邮箱已验证，账号已经创建并激活，<strong>不需要再次注册</strong>。下一步请登录刚注册的账号；
                然后在 Windows 客户端使用同一账号登录、完成设备登记，并等待运营签发采集授权。
                如果还没有安装客户端，请联系运营获取当前受支持的安装方式。
              </>
            }
            action="登录刚注册的账号"
            onAction={() => window.location.replace("/account?registered=1")}
          />
        ) : null}

        {stage.name === "code" ? (
          <form onSubmit={submitCode}>
            <p className="portal-hint">
              验证码已发往 <strong>{email}</strong>，{Math.round(stage.expiresInSeconds / 60)}{" "}
              分钟内有效，仅可使用一次。
            </p>
            <label htmlFor="join-code">
              6 位验证码
              <input
                id="join-code"
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
            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy || code.length !== 6}>
              {busy ? "校验中…" : "验证并激活"}
            </button>
            <div className="portal-foot">
              <button type="button" className="link" onClick={resend} disabled={busy}>
                重新发送（旧验证码作废）
              </button>
            </div>
          </form>
        ) : null}

        {stage.name === "form" ? (
          <form onSubmit={submitForm}>
            <div className="form-sec">
              <strong>账号信息</strong>
              <small>邀请码由运营发放</small>
            </div>
            <label htmlFor="join-invite">
              邀请码
              <input
                id="join-invite"
                value={inviteCode}
                placeholder="DM-XXXX-XXXX-XXXX"
                onChange={(event) => setInviteCode(event.target.value)}
                required
                autoFocus
              />
            </label>
            <div className="field-grid">
              <label htmlFor="join-email">
                邮箱
                <input
                  id="join-email"
                  type="email"
                  autoComplete="username"
                  value={email}
                  onChange={(event) => setEmail(event.target.value)}
                  required
                />
              </label>
              <label htmlFor="join-alias">
                别名（系统内标识你）
                <input
                  id="join-alias"
                  value={alias}
                  maxLength={80}
                  onChange={(event) => setAlias(event.target.value)}
                  required
                />
              </label>
              <label htmlFor="join-password">
                密码
                <input
                  id="join-password"
                  type="password"
                  autoComplete="new-password"
                  minLength={MIN_PASSWORD_LENGTH}
                  value={password}
                  onChange={(event) => setPassword(event.target.value)}
                  required
                />
              </label>
              <label htmlFor="join-confirm">
                再次输入密码
                <input
                  id="join-confirm"
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  onChange={(event) => setConfirm(event.target.value)}
                  required
                />
              </label>
            </div>
            <p className="portal-hint">密码至少 {MIN_PASSWORD_LENGTH} 位，不要包含你的邮箱地址。</p>

            <ProfileFields values={profileValues} onChange={setProfileValues} />

            <div className="form-sec">
              <strong>贡献者条款</strong>
              <small>注册即在系统内签署，版本由服务端记录</small>
            </div>
            {terms.status === "loading" ? <p className="portal-hint">条款加载中…</p> : null}
            {terms.status === "error" ? (
              <ErrorNote message="条款加载失败，刷新页面重试。未读到条款前无法注册。" />
            ) : null}
            {terms.status === "ready" ? (
              <>
                <div className="terms-box">
                  <div className="terms-head">
                    <span>DataMiner Contributor Terms</span>
                    <code>{terms.value.policyVersion}</code>
                  </div>
                  <div className="terms-body">
                    <pre>{terms.value.body}</pre>
                  </div>
                </div>
                <label className="check-row">
                  <input
                    type="checkbox"
                    checked={accepted}
                    onChange={(event) => setAccepted(event.target.checked)}
                    required
                  />
                  <span>我已阅读并同意上述条款（版本 {terms.value.policyVersion}）</span>
                </label>
              </>
            ) : null}

            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy || terms.status !== "ready" || !accepted}>
              {busy ? "提交中…" : "注册"}
            </button>
            <div className="portal-foot">
              <button
                type="button"
                className="link"
                onClick={() => (window.location.href = "/account")}
                disabled={busy}
              >
                已有账号？去登录
              </button>
            </div>
          </form>
        ) : null}
      </div>
    </PortalShell>
  );
}

// The server answers an unknown code, a spent code, and a taken address without
// telling them apart; the wording here must not pretend to know more.
function describe(thrown: unknown): string {
  if (!(thrown instanceof ApiError)) return "网络错误，请重试。";
  if (thrown.code === "invalid_credentials") return "验证码不正确或已失效，请重试或重新发送。";
  return describeApiError(thrown);
}
