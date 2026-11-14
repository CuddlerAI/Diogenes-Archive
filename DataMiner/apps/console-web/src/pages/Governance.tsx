import { useState, type ReactNode } from "react";

import {
  ApiError,
  api,
  currentStepUpFactor,
  describeApiError,
  newIdempotencyKey,
  type StepUpFactor,
} from "../api.js";
import { ErrorNote } from "../components.js";

/**
 * Wraps any destructive or credential-issuing action. It asks for confirmation,
 * mints one idempotency key per attempt so a retry replays instead of writing
 * twice, and escalates when the server asks for sudo — to the password prompt,
 * or under cf_access to re-verifying the Access assertion, which needs no input.
 */
export function ConfirmedAction({
  label,
  title,
  confirmation,
  destructive,
  fields,
  stepUp = currentStepUpFactor(),
  run,
  onDone,
}: {
  label: string;
  title: string;
  confirmation: string;
  destructive?: boolean;
  fields?: { name: string; label: string; placeholder?: string }[];
  /** Defaults to the session's factor; only tests pass this explicitly. */
  stepUp?: StepUpFactor;
  run: (values: Record<string, string>, idempotencyKey: string) => Promise<ReactNode | void>;
  onDone?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sudoPassword, setSudoPassword] = useState<string | null>(null);
  const [result, setResult] = useState<ReactNode | null>(null);
  // When the run returned content — often a one-time secret like a claim URL
  // or device token — onDone is deferred until the operator dismisses it.
  // Firing it immediately lets a parent refetch swap in a loading state and
  // unmount this component, destroying the only copy of the secret (found
  // live on the annotator detail page: the reset claimUrl vanished).
  const [pendingDone, setPendingDone] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    const key = newIdempotencyKey();
    try {
      if (sudoPassword !== null) {
        await api.sudo(sudoPassword);
        setSudoPassword(null);
      }
      const outcome = await run(values, key);
      setResult(outcome ?? "已完成。");
      setOpen(false);
      if (outcome) {
        setPendingDone(true);
      } else {
        onDone?.();
      }
    } catch (thrown) {
      if (thrown instanceof ApiError && thrown.code === "sudo_required") {
        if (stepUp === "cf_access_assertion") {
          // No console password exists in this mode — the step-up re-verifies
          // the Access assertion the edge already attached. Prompting would be
          // a dead end, so elevate and let the caller press confirm again.
          try {
            await api.sudo();
            setError("已确认身份，请再次点击确认以执行。");
          } catch {
            setError("身份确认失败，请重新登录后再试。");
          }
        } else {
          setSudoPassword("");
          setError("这是高危操作，请重新输入密码确认。");
        }
      } else if (thrown instanceof ApiError) {
        setError(describeApiError(thrown, "请求失败，请重试。"));
      } else {
        setError("请求失败，请重试。");
      }
    } finally {
      setBusy(false);
    }
  }

  if (result) {
    return (
      <div className="action-result">
        {result}
        <button
          className="link"
          onClick={() => {
            setResult(null);
            if (pendingDone) {
              setPendingDone(false);
              onDone?.();
            }
          }}
        >
          知道了
        </button>
      </div>
    );
  }

  if (!open) {
    return (
      <button className={destructive ? "danger" : "link"} onClick={() => setOpen(true)}>
        {label}
      </button>
    );
  }

  return (
    <form className="action-form" onSubmit={submit}>
      <h3>{title}</h3>
      <p className="muted">{confirmation}</p>
      {fields?.map((field) => (
        <label key={field.name}>
          {field.label}
          <input
            value={values[field.name] ?? ""}
            placeholder={field.placeholder ?? ""}
            onChange={(event) =>
              setValues((current) => ({ ...current, [field.name]: event.target.value }))
            }
            required
          />
        </label>
      ))}
      {sudoPassword !== null ? (
        <label>
          当前密码
          <input
            type="password"
            autoComplete="current-password"
            value={sudoPassword}
            onChange={(event) => setSudoPassword(event.target.value)}
            required
            autoFocus
          />
        </label>
      ) : null}
      {error ? <ErrorNote message={error} /> : null}
      <div className="action-buttons">
        <button type="submit" className={destructive ? "danger" : ""} disabled={busy}>
          {busy ? "执行中…" : "确认"}
        </button>
        <button
          type="button"
          className="link"
          onClick={() => {
            setOpen(false);
            setError(null);
            setSudoPassword(null);
          }}
        >
          取消
        </button>
      </div>
    </form>
  );
}

/** Shown once after registration or rotation: the plaintext is never retrievable. */
export function IssuedToken({ token }: { token: string | null }) {
  if (!token) {
    return (
      <p className="muted">
        这是一次重放的响应，token 只在首次签发时返回。如需新凭据请执行轮换。
      </p>
    );
  }
  return (
    <div className="token-reveal">
      <p>
        <strong>请立即复制并交给设备持有者。</strong>
        关闭后无法再次查看：服务端只保存它的 sha256。
      </p>
      <code className="token">{token}</code>
      <button className="link" onClick={() => void navigator.clipboard?.writeText(token)}>
        复制
      </button>
    </div>
  );
}

export function RegisterDevice({ onRegistered }: { onRegistered: () => void }) {
  return (
    <div className="register-device">
      <ConfirmedAction
        label="登记设备"
        title="登记新设备"
        confirmation="登记会生成一次性设备 token。服务端只保存其 sha256，明文关闭后无法找回。"
        fields={[
          { name: "clientDeviceId", label: "客户端设备 ID", placeholder: "dev-0001" },
          { name: "windowsBuild", label: "系统版本", placeholder: "10.0.26100" },
          { name: "gpuPciId", label: "GPU PCI ID", placeholder: "PCI\\VEN_10DE&DEV_2684" },
          { name: "gpuDriver", label: "GPU 驱动", placeholder: "566.36" },
          { name: "clientVersion", label: "客户端版本", placeholder: "0.1.0" },
        ]}
        run={async (values, key) => {
          const issued = await api.createDevice(
            {
              clientDeviceId: values.clientDeviceId,
              windowsBuild: values.windowsBuild,
              gpuPciId: values.gpuPciId,
              gpuDriver: values.gpuDriver,
              clientVersion: values.clientVersion,
              displaySnapshot: {},
              inputDevicesSnapshot: [],
            },
            key,
          );
          return <IssuedToken token={issued.token} />;
        }}
        onDone={onRegistered}
      />
    </div>
  );
}
