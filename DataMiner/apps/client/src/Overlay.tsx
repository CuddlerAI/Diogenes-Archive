import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useMemo, useState } from "react";
import type { OverlaySnapshot } from "./collector-contract";

const EMPTY_SNAPSHOT: OverlaySnapshot = {
  recordingState: "idle",
  nonTrainable: true,
  productionCertified: false,
  elapsedMs: 0,
  segmentSeq: 0,
  captureHealth: "healthy",
  storageState: "unknown",
  storageFreeBytes: null,
  storageRemainingSeconds: null,
  storageStopRequested: false,
  pressedContractKeys: [],
  mouseButtons: [],
};

const STATE_LABELS: Record<OverlaySnapshot["recordingState"], string> = {
  idle: "等待录制",
  discovering: "发现窗口",
  bound: "已绑定",
  preflighting: "正在预检",
  starting: "正在启动",
  recording: "正在录制",
  pausing: "正在暂停",
  paused: "已暂停",
  stopping: "正在封段",
  completed: "录制完成",
  faulted: "采集故障",
};

const SPECIAL_KEYS: Record<number, string> = {
  0x10: "Shift",
  0x11: "Ctrl",
  0x12: "Alt",
  0x20: "Space",
  0x25: "←",
  0x26: "↑",
  0x27: "→",
  0x28: "↓",
};

const MOUSE_BUTTONS: Record<number, string> = {
  1: "M1",
  2: "M2",
  4: "M3",
  5: "M4",
  6: "M5",
};

export default function Overlay() {
  const [snapshot, setSnapshot] = useState(EMPTY_SNAPSHOT);

  useEffect(() => {
    document.documentElement.classList.add("overlay-document");
    document.body.classList.add("overlay-document");
    let disposed = false;
    let unlistenSnapshot: (() => void) | undefined;
    let unlistenPing: (() => void) | undefined;

    const reportFault = () => {
      void invoke("collector_overlay_report_fault").catch(() => {});
    };
    window.addEventListener("error", reportFault);
    window.addEventListener("unhandledrejection", reportFault);

    void Promise.all([
      listen<OverlaySnapshot>("overlay://snapshot", (event) => {
        if (!disposed) setSnapshot(event.payload);
      }),
      listen("overlay://ping", () => {
        void invoke("collector_overlay_heartbeat").catch(() => {});
      }),
    ])
      .then(([stopSnapshot, stopPing]) => {
        if (disposed) {
          stopSnapshot();
          stopPing();
          return;
        }
        unlistenSnapshot = stopSnapshot;
        unlistenPing = stopPing;
        return invoke<OverlaySnapshot>("collector_overlay_ready");
      })
      .then((initial) => {
        if (!disposed && initial) setSnapshot(initial);
      })
      .catch(reportFault);

    return () => {
      disposed = true;
      unlistenSnapshot?.();
      unlistenPing?.();
      window.removeEventListener("error", reportFault);
      window.removeEventListener("unhandledrejection", reportFault);
      document.documentElement.classList.remove("overlay-document");
      document.body.classList.remove("overlay-document");
    };
  }, []);

  const pressed = useMemo(
    () => [
      ...snapshot.pressedContractKeys.map(keyLabel),
      ...snapshot.mouseButtons.map((button) => MOUSE_BUTTONS[button] ?? `M${button}`),
    ],
    [snapshot.mouseButtons, snapshot.pressedContractKeys],
  );

  const storageCopy = formatStorage(snapshot);

  return (
    <div className={`overlay-hud overlay-${snapshot.captureHealth}`}>
      <div className="overlay-summary">
        <div className="overlay-status">
          <span className="overlay-dot" aria-hidden="true" />
          <div>
            <strong>{STATE_LABELS[snapshot.recordingState]}</strong>
            <small>SEG {String(snapshot.segmentSeq).padStart(2, "0")}</small>
          </div>
        </div>
        <time>{formatDuration(snapshot.elapsedMs)}</time>
        <div className="overlay-keys" aria-label="当前按下的采集契约按键">
          {pressed.length === 0 ? (
            <span className="overlay-key-empty">
              {snapshot.recordingState === "paused" ? "Ctrl+Alt+F8 继续" : "输入就绪"}
            </span>
          ) : (
            pressed.slice(0, 6).map((key) => <kbd key={key}>{key}</kbd>)
          )}
        </div>
      </div>
      <div className={`overlay-footer overlay-disk-${snapshot.storageState}`}>
        <span>{storageCopy}</span>
        {/* docs/65 J-26：车道徽章随快照派生。只剩一条车道后恒 NON-TRAINABLE（docs/100）。 */}
        <span className="overlay-dev">
          {snapshot.productionCertified ? "CERTIFIED" : "NON-TRAINABLE"}
        </span>
      </div>
    </div>
  );
}

function formatStorage(snapshot: OverlaySnapshot): string {
  if (snapshot.storageStopRequested) return "磁盘水位过低 · 正在安全封口";
  if (snapshot.storageState === "unavailable") return "磁盘状态不可用";
  if (snapshot.storageState === "warning") {
    return `磁盘约剩 ${Math.ceil((snapshot.storageRemainingSeconds ?? 0) / 60)} 分钟`;
  }
  if (snapshot.storageFreeBytes !== null) {
    return `磁盘可用 ${formatBytes(snapshot.storageFreeBytes)}`;
  }
  return "磁盘待检查";
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

function keyLabel(vk: number): string {
  if (SPECIAL_KEYS[vk]) return SPECIAL_KEYS[vk];
  if (vk >= 0x30 && vk <= 0x39) return String.fromCharCode(vk);
  if (vk >= 0x41 && vk <= 0x5a) return String.fromCharCode(vk);
  return `0x${vk.toString(16).toUpperCase().padStart(2, "0")}`;
}

function formatDuration(elapsedMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(elapsedMs / 1_000));
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;
  return [hours, minutes, seconds].map((part) => String(part).padStart(2, "0")).join(":");
}
