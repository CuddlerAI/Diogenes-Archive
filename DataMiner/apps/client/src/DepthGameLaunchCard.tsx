import type { DepthProbeRuntimeStatus, EarlyLaunchPreflight } from "./depth-lab-ui";
import { annotatorMessage, earlyLaunchGameName } from "./depth-lab-ui";

export function DepthGameLaunchCard({ gameId, status, preflight, error, checking, locked, onLaunch }: {
  gameId: string;
  status: DepthProbeRuntimeStatus;
  preflight: EarlyLaunchPreflight | null;
  error?: string | null;
  checking: boolean;
  locked: boolean;
  onLaunch: () => void;
}) {
  const configured = status.earlyLaunchRequired && status.earlyLaunchGameId === gameId;
  // These are the current launch targets in services/early_launch.rs. Visibility
  // before configuration does not authorize launch; the configured runtime does.
  if (!configured && gameId !== "kingdom-come-deliverance-2-steam" && gameId !== "cyberpunk-2077-steam") return null;
  const enabled = configured && status.enabled;
  const state = enabled ? status.earlyLaunchState : "awaiting_launch";
  const bound = state === "bound";
  const starting = state === "launching" || state === "waiting_for_window";
  const ready = enabled && !checking && preflight?.ready === true;
  const label = !enabled ? "待开启" : bound ? "已绑定" : starting ? "启动中" : checking ? "检查中" : ready ? "可启动" : "暂不可启动";
  const detail = !enabled ? "开启深度采集后，从这里启动游戏。"
    : bound ? "游戏窗口已绑定，可使用主界面的录制操作。"
    : state === "launching" ? "正在启动游戏，请稍候。"
    : state === "waiting_for_window" ? "进入游戏后，扫描并绑定游戏窗口。"
    : checking ? "正在检查游戏版本和启动组件。"
    : ready ? "从这里启动游戏，以准备深度采集。"
    : "启动条件未满足，请查看下方原因。";
  return (
    <section className={`depth-block depth-certification-gate depth-game-launch${bound ? " good" : ""}`} aria-label="游戏启动" role="status">
      <strong>游戏启动</strong>
      <span className="depth-certification-verdict">{label}</span>
      <div className="depth-game-launch-copy">
        <span>{earlyLaunchGameName(gameId)}</span>
        <span>{detail}</span>
        {enabled && !checking && !bound && !starting && !preflight?.ready && (
          <span className="text-danger">{error ? annotatorMessage(error) : preflight?.blockers.join("；") || "启动检查未完成，请查看诊断。"}</span>
        )}
      </div>
      {!bound && !starting && (
        <button type="button" className="button primary" disabled={locked || !ready || state !== "awaiting_launch"} onClick={onLaunch}>
          启动游戏
        </button>
      )}
    </section>
  );
}
