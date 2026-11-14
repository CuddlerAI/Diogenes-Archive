import { useState } from "react";

import { ApiError, api, currentStepUpFactor } from "./api.js";
import { ErrorNote } from "./components.js";

/**
 * Elevates the session for destructive writes.
 *
 * sudo is a session-wide window, not a per-action prompt, so this is offered
 * once at the top of a page rather than bolted onto every button — the inline
 * controls below it (role, status) have nowhere sensible to put a password
 * field, and asking three times for one window would be noise.
 *
 * Which factor is asked for follows the login mode. Under `cf_access` there is
 * no console password at all: the server re-verifies the Access assertion the
 * edge attaches, so this is a single click. Presentation only — the server
 * decides what it actually verifies, and refuses the write either way.
 */
export function StepUpPanel({
  active,
  reason,
  onElevated,
}: {
  /** Whether the session already has a live sudo window. */
  active: boolean;
  reason: string;
  onElevated: () => void;
}) {
  const [password, setPassword] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const needsPassword = currentStepUpFactor() === "password";

  async function elevate(secret?: string) {
    setBusy(true);
    setError(null);
    try {
      await api.sudo(secret);
      setPassword(null);
      onElevated();
    } catch (thrown) {
      setError(
        thrown instanceof ApiError && thrown.status === 401
          ? needsPassword
            ? "密码不正确。"
            : "身份确认失败，请重新登录后再试。"
          : "确认失败，请重试。",
      );
    } finally {
      setBusy(false);
    }
  }

  if (active) {
    return <p className="muted">已确认身份，可以执行管理操作。</p>;
  }

  if (password !== null) {
    return (
      <form
        className="action-form"
        onSubmit={(event) => {
          event.preventDefault();
          void elevate(password);
        }}
      >
        <label>
          当前密码
          <input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            required
            autoFocus
          />
        </label>
        {error ? <ErrorNote message={error} /> : null}
        <div className="action-buttons">
          <button type="submit" disabled={busy}>
            {busy ? "确认中…" : "确认"}
          </button>
          <button
            type="button"
            className="link"
            onClick={() => {
              setPassword(null);
              setError(null);
            }}
          >
            取消
          </button>
        </div>
      </form>
    );
  }

  return (
    <div className="action-form">
      <p className="muted">{reason}</p>
      {error ? <ErrorNote message={error} /> : null}
      <button
        type="button"
        disabled={busy}
        onClick={() => (needsPassword ? setPassword("") : void elevate())}
      >
        {busy ? "确认中…" : "确认身份"}
      </button>
    </div>
  );
}
