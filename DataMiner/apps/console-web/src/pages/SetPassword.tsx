import { useEffect, useState } from "react";

import { ApiError, api, describeApiError, type ConsoleMeta } from "../api.js";
import { ErrorNote } from "../components.js";

/**
 * Landing page for the one-time links that the invite email and the
 * password-reset email carry (docs/20 §3.2 closes this gap: the SPA had no route
 * for them, so the links were dead ends). The link's `challenge` and `token`
 * query parameters are the credential; there is no session yet.
 *
 *   /accept-invite?challenge=…&token=…   → POST /auth/invite/accept
 *   /reset-password?challenge=…&token=…  → POST /auth/password-reset/confirm
 *
 * Both set a password and, on the reset path, revoke every existing session for
 * that operator server-side. On success we send the operator to the sign-in page
 * rather than auto-authenticating: setting a password is not a login.
 */

// Mirrors the server default (CONSOLE_PASSWORD_MIN_LENGTH). The server remains the
// source of truth and answers `weak_password` if this is ever relaxed below it.
const MIN_PASSWORD_LENGTH = 12;

export function SetPassword({ mode }: { mode: "invite" | "reset" }) {
  const params = new URLSearchParams(window.location.search);
  const challengeId = params.get("challenge") ?? "";
  const token = params.get("token") ?? "";

  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  // The environment label comes from the server, never from text in this file:
  // this page used to hardcode "staging", so the production set-password page
  // announced itself as staging — exactly the mislabel App.tsx warns about.
  const [meta, setMeta] = useState<ConsoleMeta | null>(null);
  useEffect(() => {
    let live = true;
    void api
      .meta()
      .then((value) => live && setMeta(value))
      .catch(() => {
        /* No label is better than a wrong one. */
      });
    return () => {
      live = false;
    };
  }, []);

  const linkComplete = challengeId !== "" && token !== "";
  const heading = mode === "invite" ? "设置管理员密码" : "重置密码";

  async function submit(event: React.FormEvent) {
    event.preventDefault();
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
      if (mode === "invite") {
        await api.acceptInvite(challengeId, token, password);
      } else {
        await api.confirmPasswordReset(challengeId, token, password);
      }
      setDone(true);
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="login">
      <div className="login-card">
        <h1>DataMiner Console</h1>
        <p className="login-sub">
          {heading}
          {meta ? ` · ${meta.environment}` : ""}
        </p>

        {done ? (
          <>
            <p className="login-hint">
              {mode === "invite"
                ? "密码已设置，账号已激活。"
                : "密码已重置，原有会话已全部退出。"}
              现在用邮箱和新密码登录。
            </p>
            <button type="button" onClick={() => (window.location.href = "/")}>
              去登录
            </button>
          </>
        ) : !linkComplete ? (
          <>
            <ErrorNote message="链接不完整或已失效，请使用邮件中的最新链接，或让管理员重新邀请。" />
            <button type="button" className="link" onClick={() => (window.location.href = "/")}>
              返回登录
            </button>
          </>
        ) : (
          <form onSubmit={submit}>
            <p className="login-hint">
              设置一个至少 {MIN_PASSWORD_LENGTH} 位的密码，不要包含你的邮箱地址。
            </p>
            <label htmlFor="password">新密码</label>
            <input
              id="password"
              type="password"
              autoComplete="new-password"
              value={password}
              minLength={MIN_PASSWORD_LENGTH}
              onChange={(event) => setPassword(event.target.value)}
              required
              autoFocus
            />
            <label htmlFor="confirm">再次输入</label>
            <input
              id="confirm"
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(event) => setConfirm(event.target.value)}
              required
            />
            {error ? <ErrorNote message={error} /> : null}
            <button type="submit" disabled={busy}>
              {busy ? "提交中…" : mode === "invite" ? "激活账号" : "重置密码"}
            </button>
          </form>
        )}
      </div>
    </main>
  );
}

function describe(thrown: unknown): string {
  if (!(thrown instanceof ApiError)) return "网络错误，请重试。";
  if (thrown.code === "invalid_credentials") {
    return "链接无效或已过期，请使用最新的邮件链接，或让管理员重新邀请。";
  }
  if (thrown.code === "weak_password") return thrown.message;
  if (thrown.code === "rate_limited") return "尝试过于频繁，请稍后再试。";
  return describeApiError(thrown);
}
