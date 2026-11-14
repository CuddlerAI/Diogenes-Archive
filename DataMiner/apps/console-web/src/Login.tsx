import { useEffect, useState } from "react";

import { ApiError, api, describeApiError, type ConsoleMeta, type Operator } from "./api.js";
import { ErrorNote } from "./components.js";

type Stage =
  | { name: "password" }
  | { name: "code"; challengeId: string; expiresInSeconds: number }
  | { name: "reset-request" }
  | { name: "reset-sent" };

export function Login({ onSignedIn }: { onSignedIn: (operator: Operator) => void }) {
  const [stage, setStage] = useState<Stage>({ name: "password" });
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  // Which sign-in to render, and which environment this is. Both are unknown
  // before a session exists, so they come from the public meta endpoint.
  const [meta, setMeta] = useState<ConsoleMeta | null>(null);
  useEffect(() => {
    let live = true;
    void api
      .meta()
      .then((value) => live && setMeta(value))
      .catch(() => {
        /* Falls back to the password form, which is the pre-existing behaviour. */
      });
    return () => {
      live = false;
    };
  }, []);
  const [busy, setBusy] = useState(false);

  async function submitPassword(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await api.login(email, password);
      if ("operator" in result) {
        // password_only: the session cookie is already set, so we are signed in.
        onSignedIn(result.operator);
      } else {
        setStage({ name: "code", challengeId: result.challengeId, expiresInSeconds: result.expiresInSeconds });
      }
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
      const result = await api.verifyOtp(stage.challengeId, code);
      onSignedIn(result.operator);
    } catch (thrown) {
      setError(describe(thrown));
      setCode("");
    } finally {
      setBusy(false);
    }
  }

  async function submitResetRequest(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      // Always 202 on the server; the confirmation below must stay neutral so it
      // cannot be used to probe whether an address is a real operator.
      await api.requestPasswordReset(email);
      setStage({ name: "reset-sent" });
    } catch (thrown) {
      setError(describe(thrown));
    } finally {
      setBusy(false);
    }
  }

  async function resend() {
    if (stage.name !== "code") return;
    setBusy(true);
    setError(null);
    try {
      const result = await api.resendOtp(stage.challengeId);
      setStage({ name: "code", challengeId: result.challengeId, expiresInSeconds: result.expiresInSeconds });
      setCode("");
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
        <p className="login-sub">运营 / QA 控制台{meta ? ` · ${meta.environment}` : ""}</p>
        {/* 受邀标注员常会直达根域名（docs/65 J-03）：这里是运营登录，给他们一个
            正确的去处。不放注册链接——注册入口仍随邀请码线下发放（docs/66 裁-4）。 */}
        <p className="login-hint">
          采集员请前往 <a href="/account">采集员个人中心</a>。
        </p>

        {/* Under cf_access there is no console password: /auth/login answers 404
            password_login_disabled, so drawing the password form here left the
            operator with a control that could never work. */}
        {meta?.loginMode === "cf_access" ? (
          <div>
            <p>本控制台通过 Cloudflare Access 登录，没有独立密码。</p>
            <button
              type="button"
              disabled={busy}
              onClick={() => {
                setBusy(true);
                setError(null);
                void api
                  .cfAccessLogin()
                  .then((result) => onSignedIn(result.operator))
                  .catch((thrown) => setError(describe(thrown)))
                  .finally(() => setBusy(false));
              }}
            >
              重新通过 Cloudflare Access 登录
            </button>
          </div>
        ) : null}

        {meta?.loginMode !== "cf_access" && stage.name === "password" ? (
          <form onSubmit={submitPassword}>
            <label htmlFor="email">邮箱</label>
            <input
              id="email"
              type="email"
              autoComplete="username"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
              required
            />
            <label htmlFor="password">密码</label>
            <input
              id="password"
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              required
            />
            <button type="submit" disabled={busy}>
              {busy ? "提交中…" : "下一步"}
            </button>
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
          </form>
        ) : null}

        {stage.name === "code" ? (
          <form onSubmit={submitCode}>
            <p className="login-hint">
              验证码已发往你的邮箱，{Math.round(stage.expiresInSeconds / 60)} 分钟内有效，仅可使用一次。
            </p>
            <label htmlFor="code">6 位验证码</label>
            <input
              id="code"
              inputMode="numeric"
              autoComplete="one-time-code"
              pattern="[0-9]{6}"
              maxLength={6}
              value={code}
              onChange={(event) => setCode(event.target.value.replace(/\D/g, ""))}
              required
              autoFocus
            />
            <button type="submit" disabled={busy || code.length !== 6}>
              {busy ? "校验中…" : "登录"}
            </button>
            <button type="button" className="link" onClick={resend} disabled={busy}>
              重新发送（旧验证码作废）
            </button>
          </form>
        ) : null}

        {stage.name === "reset-request" ? (
          <form onSubmit={submitResetRequest}>
            <p className="login-hint">输入账号邮箱，我们会发送一个一次性的重置链接。</p>
            <label htmlFor="reset-email">邮箱</label>
            <input
              id="reset-email"
              type="email"
              autoComplete="username"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
              required
              autoFocus
            />
            <button type="submit" disabled={busy || email.trim() === ""}>
              {busy ? "发送中…" : "发送重置链接"}
            </button>
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
          </form>
        ) : null}

        {stage.name === "reset-sent" ? (
          <>
            <p className="login-hint">
              如果该邮箱对应一个有效的管理员账号，重置链接已经发出。请查收邮件并按链接设置新密码。
            </p>
            <button
              type="button"
              className="link"
              onClick={() => setStage({ name: "password" })}
            >
              返回登录
            </button>
          </>
        ) : null}

        {error ? <ErrorNote message={error} /> : null}
      </div>
    </main>
  );
}

// The server answers an unknown address, a wrong password, and a wrong code with
// the same code on purpose. The wording here must not undo that by guessing which
// one it was.
function describe(thrown: unknown): string {
  if (!(thrown instanceof ApiError)) return "网络错误，请重试。";
  if (thrown.code === "invalid_credentials") return "无法验证，请检查后重试。";
  if (thrown.code === "rate_limited") {
    const wait = thrown.retryAfterSeconds ? `，请在 ${thrown.retryAfterSeconds} 秒后重试` : "";
    return `尝试过于频繁${wait}。`;
  }
  if (thrown.code === "recipient_not_allowed") return "该环境只向白名单收件人投递邮件。";
  return describeApiError(thrown);
}
