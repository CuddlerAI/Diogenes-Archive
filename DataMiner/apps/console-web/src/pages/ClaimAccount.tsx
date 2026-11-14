import { useState } from "react";

import { ApiError, appApi, describeApiError } from "../api.js";
import { ErrorNote } from "../components.js";
import { useAsync } from "../useAsync.js";
import { PortalDone, PortalShell } from "./portal-shell.js";

/**
 * Landing page for operator-issued claim links (docs/34 §5.2 / §6.5):
 *
 *   /claim-account?challenge=…&token=…
 *
 * One page, two ceremonies, and the server decides which from the credential
 * status — the preflight only tells us what to render. Activation shows the
 * terms and records consent (the person, not the operator, accepts); reset
 * just sets a new password and revokes every session. In both, the password is
 * chosen here by its owner and never existed anywhere else.
 */

const MIN_PASSWORD_LENGTH = 12;

export function ClaimAccount() {
  const params = new URLSearchParams(window.location.search);
  const challengeId = params.get("challenge") ?? "";
  const token = params.get("token") ?? "";
  const linkComplete = challengeId !== "" && token !== "";

  const preflight = useAsync(() => appApi.claimPreflight(challengeId), [challengeId]);

  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [accepted, setAccepted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (preflight.status !== "ready") return;
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
      await appApi.confirmClaim({
        challengeId,
        token,
        password,
        ...(preflight.value.kind === "activate" && preflight.value.terms
          ? { acceptedPolicyVersion: preflight.value.terms.policyVersion }
          : {}),
      });
      setDone(true);
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  const activate = preflight.status === "ready" && preflight.value.kind === "activate";

  return (
    <PortalShell>
      <div className="portal-card">
        <h1>{activate ? "设置密码并激活账号" : "设置新密码"}</h1>
        <p className="card-sub">运营为你发放的一次性链接；密码只由你本人设置，运营看不到它。</p>

        {done ? (
          <PortalDone
            message={
              activate
                ? "密码已设置，账号已激活。现在可以在个人中心或 Windows 客户端用邮箱和新密码登录。"
                : "密码已更新，原有登录已全部退出。现在可以用新密码重新登录。"
            }
            action="去登录"
            onAction={() => (window.location.href = "/account")}
          />
        ) : !linkComplete ? (
          <ErrorNote message="链接不完整，请使用邮件里的完整链接，或联系运营重新发放。" />
        ) : preflight.status === "loading" ? (
          <p className="portal-hint">链接校验中…</p>
        ) : preflight.status === "error" ? (
          <ErrorNote message="链接无效、已过期或已被使用，请联系运营重新发放。" />
        ) : (
          <form onSubmit={submit}>
            <p className="portal-hint">
              账号邮箱：<strong>{preflight.value.email}</strong>。
              {activate ? "设置密码并同意条款后，账号即激活。" : "设置新密码后，所有旧登录会立即退出。"}
            </p>
            <div className="field-grid">
              <label htmlFor="claim-password">
                {activate ? "设置密码" : "新密码"}
                <input
                  id="claim-password"
                  type="password"
                  autoComplete="new-password"
                  minLength={MIN_PASSWORD_LENGTH}
                  value={password}
                  onChange={(event) => setPassword(event.target.value)}
                  required
                  autoFocus
                />
              </label>
              <label htmlFor="claim-confirm">
                再次输入
                <input
                  id="claim-confirm"
                  type="password"
                  autoComplete="new-password"
                  value={confirm}
                  onChange={(event) => setConfirm(event.target.value)}
                  required
                />
              </label>
            </div>
            <p className="portal-hint">至少 {MIN_PASSWORD_LENGTH} 位，不要包含你的邮箱地址。</p>

            {activate && preflight.value.terms ? (
              <>
                <div className="form-sec">
                  <strong>贡献者条款</strong>
                  <small>激活即在系统内签署</small>
                </div>
                <div className="terms-box">
                  <div className="terms-head">
                    <span>DataMiner Contributor Terms</span>
                    <code>{preflight.value.terms.policyVersion}</code>
                  </div>
                  <div className="terms-body">
                    <pre>{preflight.value.terms.body}</pre>
                  </div>
                </div>
                <label className="check-row">
                  <input
                    type="checkbox"
                    checked={accepted}
                    onChange={(event) => setAccepted(event.target.checked)}
                    required
                  />
                  <span>我已阅读并同意上述条款（版本 {preflight.value.terms.policyVersion}）</span>
                </label>
              </>
            ) : null}

            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy || (activate && !accepted)}>
              {busy ? "提交中…" : activate ? "设置密码并激活" : "更新密码"}
            </button>
          </form>
        )}
      </div>
    </PortalShell>
  );
}

function describe(thrown: unknown): string {
  if (!(thrown instanceof ApiError)) return "网络错误，请重试。";
  if (thrown.code === "invalid_credentials") {
    return "链接无效或已过期，请联系运营重新发放。";
  }
  return describeApiError(thrown);
}
