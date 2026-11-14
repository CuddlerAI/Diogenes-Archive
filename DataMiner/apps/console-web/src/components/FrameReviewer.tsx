import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { ReviewFrameRow } from "../api.js";

/**
 * Frame-by-frame annotation reviewer — the React port of
 * tools/annotation-review/review.html (docs/33 §4). The single-file original
 * stays alive as the local / open-source vehicle; the two share the row
 * contract, not code. Ported fixes over the original:
 *
 *  1. fps comes from the server response (meta-declared), never hardcoded 30 —
 *     an undeclared fps falls back to 30 with a visible warning.
 *  2. `buttons` (frame-end held mouse buttons, authoritative when present)
 *     takes precedence over event-replay reconstruction; a frame where the two
 *     disagree is itself a review finding and is flagged.
 *  3. `actions` (mapping-derived semantic layer) gets a HUD line and its own
 *     timeline track — previously the semantic layer was never eyeballed.
 *  4. RAW mode shows session_frame / duplicate; gframe renders as "—" when the
 *     plane does not define it (docs/02 §3).
 *  5. No-video rows (npz / events-only chunks) render the HUD on a black stage
 *     instead of erroring.
 */

const VKN: Record<number, string> = {
  1: "LMB", 2: "RMB", 4: "MMB", 8: "BS", 9: "Tab", 13: "Enter", 16: "Shift",
  17: "Ctrl", 18: "Alt", 27: "Esc", 32: "Space",
  48: "0", 49: "1", 50: "2", 51: "3", 52: "4", 53: "5", 54: "6", 55: "7", 56: "8", 57: "9",
  65: "A", 66: "B", 67: "C", 68: "D", 69: "E", 70: "F", 71: "G", 72: "H", 73: "I", 74: "J",
  75: "K", 76: "L", 77: "M", 78: "N", 79: "O", 80: "P", 81: "Q", 82: "R", 83: "S", 84: "T",
  85: "U", 86: "V", 87: "W", 88: "X", 89: "Y", 90: "Z",
};
const vkName = (vk: number): string => VKN[vk] ?? `VK${vk}`;

const KEY_LAYOUT = [
  ["Esc", "Tab", "Q", "W", "E", "R"],
  ["Shift", "A", "S", "D", "F", "Space"],
];

type FrameEvent = { type: string; vk?: number; dx?: number; dy?: number };

function frameEvents(row: ReviewFrameRow): FrameEvent[] {
  return Array.isArray(row.events) ? (row.events as FrameEvent[]) : [];
}

function instantEvents(row: ReviewFrameRow): FrameEvent[] {
  return frameEvents(row).filter((event) => event.type !== "mouse_move");
}

/** Mouse-held state per frame, replayed from events within this chunk. A vk
 * whose first action is `mouse_up` was held across the chunk boundary — normal,
 * not a defect (annotation-review/README.md). */
function reconstructMouseHeld(rows: ReviewFrameRow[]): Set<number>[] {
  const held = new Set<number>();
  const firstAction: Record<number, string> = {};
  for (const row of rows) {
    for (const event of frameEvents(row)) {
      if (
        (event.type === "mouse_down" || event.type === "mouse_up") &&
        event.vk !== undefined &&
        !(event.vk in firstAction)
      ) {
        firstAction[event.vk] = event.type;
      }
    }
  }
  for (const [vk, action] of Object.entries(firstAction)) {
    if (action === "mouse_up") held.add(Number(vk));
  }
  return rows.map((row) => {
    for (const event of frameEvents(row)) {
      if (event.type === "mouse_down" && event.vk !== undefined) held.add(event.vk);
      if (event.type === "mouse_up" && event.vk !== undefined) held.delete(event.vk);
    }
    return new Set(held);
  });
}

function sameSet(a: Set<number>, b: Set<number>): boolean {
  if (a.size !== b.size) return false;
  for (const value of a) if (!b.has(value)) return false;
  return true;
}

export type FrameReviewerProps = {
  rows: ReviewFrameRow[];
  /** Presigned playback URL; null renders the HUD over a black stage. */
  videoUrl: string | null;
  schema: "dm1" | "raw-v1";
  fps: number | null;
  muLawScheme: string | null;
  title: string;
};

export function FrameReviewer({ rows, videoUrl, schema, fps, muLawScheme, title }: FrameReviewerProps) {
  const effectiveFps = fps ?? 30;
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const hudRef = useRef<HTMLCanvasElement | null>(null);
  const timelineRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [curIdx, setCurIdx] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [rate, setRate] = useState(0.25);

  const reconstructedHeld = useMemo(() => reconstructMouseHeld(rows), [rows]);
  const hasButtons = useMemo(() => rows.some((row) => Array.isArray(row.buttons)), [rows]);
  // Authority order (docs/33 §4 fix 2): buttons when the plane carries them,
  // event replay otherwise. Divergence between the two is surfaced, not hidden.
  const heldAt = useCallback(
    (index: number): Set<number> => {
      const row = rows[index];
      if (row && Array.isArray(row.buttons)) return new Set(row.buttons);
      return reconstructedHeld[index] ?? new Set();
    },
    [rows, reconstructedHeld],
  );
  const buttonsDisagree = useCallback(
    (index: number): boolean => {
      const row = rows[index];
      if (!row || !Array.isArray(row.buttons)) return false;
      return !sameSet(new Set(row.buttons), reconstructedHeld[index] ?? new Set());
    },
    [rows, reconstructedHeld],
  );

  const eventFrames = useMemo(
    () => rows.map((row, index) => ({ index, row })).filter(({ row }) => instantEvents(row).length > 0).map(({ index }) => index),
    [rows],
  );

  const clampIdx = useCallback(
    (index: number) => Math.max(0, Math.min(rows.length - 1, index)),
    [rows.length],
  );

  const seekFrame = useCallback(
    (index: number) => {
      const clamped = clampIdx(index);
      const video = videoRef.current;
      if (video) {
        video.pause();
        video.currentTime = (clamped + 0.5) / effectiveFps;
      }
      setPlaying(false);
      setCurIdx(clamped);
    },
    [clampIdx, effectiveFps],
  );

  const jumpEvent = useCallback(
    (direction: 1 | -1) => {
      const next =
        direction > 0
          ? eventFrames.find((frame) => frame > curIdx)
          : [...eventFrames].reverse().find((frame) => frame < curIdx);
      if (next !== undefined) seekFrame(next);
    },
    [eventFrames, curIdx, seekFrame],
  );

  const togglePlay = useCallback(() => {
    const video = videoRef.current;
    if (!video) return;
    if (playing) {
      video.pause();
      setPlaying(false);
    } else {
      video.playbackRate = rate;
      void video.play();
      setPlaying(true);
    }
  }, [playing, rate]);

  // Frame sync: requestVideoFrameCallback when available, 20ms polling otherwise.
  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    let cancelled = false;
    const sync = (mediaTime: number) => {
      const index = clampIdx(Math.floor(mediaTime * effectiveFps + 1e-4));
      setCurIdx((current) => (current === index ? current : index));
    };
    if (typeof video.requestVideoFrameCallback === "function") {
      const callback = (_now: number, meta: VideoFrameCallbackMetadata) => {
        if (cancelled) return;
        sync(meta.mediaTime);
        video.requestVideoFrameCallback(callback);
      };
      video.requestVideoFrameCallback(callback);
      return () => {
        cancelled = true;
      };
    }
    const timer = setInterval(() => {
      if (!video.paused) sync(video.currentTime);
    }, 20);
    return () => clearInterval(timer);
  }, [clampIdx, effectiveFps, videoUrl]);

  // Keyboard shortcuts, scoped to the reviewer being mounted.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "SELECT" || target.tagName === "TEXTAREA")) return;
      if (event.key === "ArrowLeft") { event.preventDefault(); seekFrame(curIdx - 1); }
      if (event.key === "ArrowRight") { event.preventDefault(); seekFrame(curIdx + 1); }
      if (event.key === " ") { event.preventDefault(); togglePlay(); }
      if (event.key === "e" || event.key === "E") jumpEvent(1);
      if (event.key === "q" || event.key === "Q") jumpEvent(-1);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [curIdx, seekFrame, togglePlay, jumpEvent]);

  // --- HUD ---
  const renderHud = useCallback(() => {
    const hud = hudRef.current;
    const stage = stageRef.current;
    if (!hud || !stage || rows.length === 0) return;
    const rect = stage.getBoundingClientRect();
    const dpr = globalThis.devicePixelRatio || 1;
    hud.width = rect.width * dpr;
    hud.height = rect.height * dpr;
    hud.style.width = `${rect.width}px`;
    hud.style.height = `${rect.height}px`;
    const row = rows[curIdx];
    if (!row) return;
    const held = heldAt(curIdx);
    const ctx = hud.getContext("2d");
    if (!ctx) return;
    const W = rect.width;
    const H = rect.height;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    const S = Math.max(0.7, W / 1280);

    // Keyboard panel (bottom left).
    const kw = 44 * S, kh = 30 * S, gap = 5 * S, x0 = 14 * S;
    const y0 = H - 2 * (kh + gap) - 14 * S;
    const heldKeys = new Set(row.keys.map(vkName));
    ctx.font = `${12 * S}px -apple-system, sans-serif`;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    KEY_LAYOUT.forEach((rowKeys, ri) =>
      rowKeys.forEach((key, ci) => {
        const x = x0 + ci * (kw + gap);
        const y = y0 + ri * (kh + gap);
        const w = key === "Shift" || key === "Space" ? kw * 1.25 : kw;
        ctx.fillStyle = heldKeys.has(key) ? "rgba(61,133,242,0.92)" : "rgba(10,10,14,0.62)";
        ctx.strokeStyle = heldKeys.has(key) ? "#9cc4ff" : "rgba(255,255,255,0.28)";
        ctx.lineWidth = 1.2;
        ctx.beginPath();
        ctx.roundRect(x, y, w, kh, 5 * S);
        ctx.fill();
        ctx.stroke();
        ctx.fillStyle = "#fff";
        ctx.fillText(key, x + w / 2, y + kh / 2);
      }),
    );
    const extra = row.keys.map(vkName).filter((key) => !KEY_LAYOUT.flat().includes(key));
    if (extra.length > 0) {
      ctx.fillStyle = "rgba(61,133,242,0.92)";
      ctx.textAlign = "left";
      ctx.fillText(`其他按住: ${extra.join(" ")}`, x0, y0 - 12 * S);
      ctx.textAlign = "center";
    }

    // Semantic actions line (docs/33 §4 fix 3), above the keyboard panel.
    if (Array.isArray(row.actions) && row.actions.length > 0) {
      ctx.fillStyle = "rgba(155,89,182,0.92)";
      ctx.textAlign = "left";
      ctx.font = `${11 * S}px ui-monospace, monospace`;
      ctx.fillText(`actions: ${row.actions.join(" ")}`, x0, y0 - (extra.length > 0 ? 26 : 12) * S);
      ctx.textAlign = "center";
    }

    // Mouse panel (bottom right).
    const mx = W - 150 * S, my = H - 120 * S;
    ctx.fillStyle = "rgba(10,10,14,0.62)";
    ctx.beginPath();
    ctx.roundRect(mx - 58 * S, my - 58 * S, 200 * S, 112 * S, 8 * S);
    ctx.fill();
    const button = (dx: number, vk: number, label: string) => {
      const on = held.has(vk);
      ctx.fillStyle = on ? "rgba(255,64,64,0.95)" : "rgba(255,255,255,0.14)";
      ctx.strokeStyle = on ? "#ffb0b0" : "rgba(255,255,255,0.3)";
      ctx.beginPath();
      ctx.roundRect(mx + dx, my - 50 * S, 26 * S, 34 * S, 5 * S);
      ctx.fill();
      ctx.stroke();
      ctx.fillStyle = "#fff";
      ctx.font = `${9 * S}px sans-serif`;
      ctx.fillText(label, mx + dx + 13 * S, my - 50 * S + 17 * S);
    };
    button(-50 * S, 1, "L");
    button(-20 * S, 2, "R");
    const cx = mx + 55 * S, cy = my - 10 * S, R = 40 * S;
    ctx.strokeStyle = "rgba(255,255,255,0.3)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.arc(cx, cy, R, 0, 7);
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(cx - R, cy);
    ctx.lineTo(cx + R, cy);
    ctx.moveTo(cx, cy - R);
    ctx.lineTo(cx, cy + R);
    ctx.stroke();
    const dx = row.pointerDx, dy = row.pointerDy;
    if (dx || dy) {
      const len = Math.hypot(dx, dy);
      const sc = (Math.min(1, len / 200) * R) / Math.max(len, 1e-9);
      const ex = cx + dx * sc, ey = cy + dy * sc;
      ctx.strokeStyle = "#37e08b";
      ctx.lineWidth = 2.5 * S;
      ctx.beginPath();
      ctx.moveTo(cx, cy);
      ctx.lineTo(ex, ey);
      ctx.stroke();
      ctx.fillStyle = "#37e08b";
      ctx.beginPath();
      ctx.arc(ex, ey, 3.5 * S, 0, 7);
      ctx.fill();
    }
    ctx.fillStyle = "#cfe9da";
    ctx.font = `${11 * S}px ui-monospace, monospace`;
    ctx.fillText(`dx ${dx}  dy ${dy}`, cx, cy + R + 13 * S);
    ctx.fillText(`bins [${row.pointerBins.join(",")}]`, cx, cy + R + 27 * S);

    // Instant-event banner (top).
    const events = instantEvents(row);
    if (events.length > 0) {
      const text = events
        .map((event) => {
          const arrow = event.type.endsWith("_down") ? "▼" : event.type.endsWith("_up") ? "▲" : "";
          if (event.type.startsWith("mouse_") || event.type.startsWith("key_")) {
            return `${arrow} ${event.vk !== undefined ? vkName(event.vk) : event.type}`;
          }
          return event.type;
        })
        .join("   ");
      ctx.font = `bold ${16 * S}px -apple-system, sans-serif`;
      const tw = ctx.measureText(text).width;
      ctx.fillStyle = "rgba(180,30,30,0.88)";
      ctx.beginPath();
      ctx.roundRect(W / 2 - tw / 2 - 14 * S, 12 * S, tw + 28 * S, 30 * S, 6 * S);
      ctx.fill();
      ctx.fillStyle = "#fff";
      ctx.fillText(text, W / 2, 27 * S);
    }

    // Frame badge (top left). RAW has no gframe by contract (docs/02 §3).
    ctx.textAlign = "left";
    ctx.font = `${12 * S}px ui-monospace, monospace`;
    ctx.fillStyle = "rgba(10,10,14,0.62)";
    ctx.fillRect(10 * S, 10 * S, 250 * S, 22 * S);
    ctx.fillStyle = "#ffd479";
    const globalLabel =
      row.gframe !== null && row.gframe !== undefined
        ? `gframe ${row.gframe}`
        : row.session_frame !== undefined
          ? `sframe ${row.session_frame}`
          : "gframe —";
    const dup = row.duplicate ? "  DUP" : "";
    ctx.fillText(
      `frame ${row.frame}  ${globalLabel}  t=${(row.frame / effectiveFps).toFixed(2)}s${dup}`,
      16 * S,
      21 * S,
    );
  }, [rows, curIdx, heldAt, effectiveFps]);

  // --- timeline (five tracks: mouse events / key events / keys held / pointer motion / actions) ---
  const renderTimeline = useCallback(() => {
    const timeline = timelineRef.current;
    if (!timeline || rows.length === 0) return;
    const dpr = globalThis.devicePixelRatio || 1;
    const width = timeline.clientWidth;
    const height = 56;
    timeline.width = width * dpr;
    timeline.height = height * dpr;
    const ctx = timeline.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const bw = width / rows.length;
    rows.forEach((row, index) => {
      const x = index * bw;
      const events = instantEvents(row);
      if (events.some((event) => event.type.startsWith("mouse_"))) {
        ctx.fillStyle = "#ff5252";
        ctx.fillRect(x, 2, Math.max(bw, 1.5), 9);
      } else if (events.length > 0) {
        ctx.fillStyle = "#ffb300";
        ctx.fillRect(x, 12, Math.max(bw, 1.5), 9);
      }
      if (row.keys.length > 0) {
        ctx.fillStyle = "#3d6ea5";
        ctx.fillRect(x, 23, Math.max(bw, 1), 9);
      }
      if (row.pointerDx || row.pointerDy) {
        ctx.fillStyle = "#37b06b";
        ctx.fillRect(x, 34, Math.max(bw, 1), 9);
      }
      if (Array.isArray(row.actions) && row.actions.length > 0) {
        ctx.fillStyle = "#9b59b6";
        ctx.fillRect(x, 45, Math.max(bw, 1), 9);
      }
    });
    ctx.fillStyle = "#fff";
    ctx.fillRect(curIdx * bw, 0, Math.max(bw, 1.5), height);
  }, [rows, curIdx]);

  useEffect(() => {
    renderHud();
    renderTimeline();
  }, [renderHud, renderTimeline]);

  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const observer = new ResizeObserver(() => {
      renderHud();
      renderTimeline();
    });
    observer.observe(stage);
    return () => observer.disconnect();
  }, [renderHud, renderTimeline]);

  if (rows.length === 0) {
    return <p className="muted">该对象没有可渲染的帧行。</p>;
  }
  const row = rows[curIdx]!;
  const held = heldAt(curIdx);
  const disagree = buttonsDisagree(curIdx);

  return (
    <div className="reviewer">
      <div className="reviewer-main">
        <div className="reviewer-stage" ref={stageRef}>
          {videoUrl ? (
            <video ref={videoRef} className="reviewer-video" src={videoUrl} muted playsInline preload="auto" />
          ) : (
            <div className="reviewer-novideo">无视频帧数据（npz / events-only 对象），仅渲染标注 HUD。</div>
          )}
          <canvas ref={hudRef} className="reviewer-hud" />
        </div>
        <canvas
          ref={timelineRef}
          className="reviewer-timeline"
          onClick={(event) => {
            const rect = event.currentTarget.getBoundingClientRect();
            const x = (event.clientX - rect.left) / rect.width;
            seekFrame(Math.round(x * (rows.length - 1)));
          }}
        />
        <div className="reviewer-legend">
          <span><i style={{ background: "#ff5252" }} />鼠标键 down/up</span>
          <span><i style={{ background: "#ffb300" }} />键盘 down/up</span>
          <span><i style={{ background: "#3d6ea5" }} />keys 按住</span>
          <span><i style={{ background: "#37b06b" }} />鼠标位移</span>
          <span><i style={{ background: "#9b59b6" }} />语义动作</span>
          <span>点击时间轴可跳帧</span>
        </div>
        <div className="reviewer-controls">
          <button onClick={() => jumpEvent(-1)} title="上一个事件">⏮ 事件</button>
          <button onClick={() => seekFrame(curIdx - 10)}>-10</button>
          <button onClick={() => seekFrame(curIdx - 1)}>◀ 1帧</button>
          <button className="primary" onClick={togglePlay} disabled={!videoUrl}>
            {playing ? "暂停" : "播放"}
          </button>
          <button onClick={() => seekFrame(curIdx + 1)}>1帧 ▶</button>
          <button onClick={() => seekFrame(curIdx + 10)}>+10</button>
          <button onClick={() => jumpEvent(1)} title="下一个事件">事件 ⏭</button>
          <select
            value={String(rate)}
            onChange={(event) => {
              const next = Number(event.target.value);
              setRate(next);
              const video = videoRef.current;
              if (video) video.playbackRate = next;
            }}
          >
            <option value="1">1×</option>
            <option value="0.5">0.5×</option>
            <option value="0.25">0.25×</option>
            <option value="0.1">0.1×</option>
          </select>
          <span className="muted">
            快捷键: ←/→ 逐帧 · 空格 播放 · E/Q 事件跳转
          </span>
        </div>
      </div>
      <div className="reviewer-side">
        <div className="reviewer-card">
          <h3>当前帧</h3>
          <p>
            {title}
            <br />
            {schema === "raw-v1" ? "RAW 段" : "dm-1 chunk"} 内帧 <b>{row.frame}</b> / {rows.length - 1}
            {row.gframe !== null && row.gframe !== undefined ? <> · 全局帧 <b>{row.gframe}</b></> : null}
            {row.session_frame !== undefined ? <> · session_frame <b>{row.session_frame}</b></> : null}
            <br />
            clip 内 <b>{(row.frame / effectiveFps).toFixed(3)}s</b>
            {typeof row.video_t_ms === "number" ? <> · video_t_ms <b>{row.video_t_ms}</b></> : null}
            {row.duplicate ? <> · <b className="reviewer-dup">duplicate</b></> : null}
            <br />
            按住键: <b>{row.keys.map(vkName).join(" ") || "—"}</b> · 鼠标键按住:{" "}
            <b>{[...held].map(vkName).join(" ") || "—"}</b>
            {hasButtons ? (
              <span className="muted">（buttons 字段权威{disagree ? "，与 events 重建不一致 ⚠" : ""}）</span>
            ) : (
              <span className="muted">（由 events 重建；chunk 开头先见 up 属跨界持续）</span>
            )}
            {Array.isArray(row.actions) && row.actions.length > 0 ? (
              <>
                <br />
                语义动作: <b>{row.actions.join(" ")}</b>
              </>
            ) : null}
          </p>
          <p className="muted">
            fps: {fps !== null ? fps : "未声明（回退 30，帧对位仅供参考）"} · bins 刻度:{" "}
            {muLawScheme ?? "未声明"}
          </p>
        </div>
        <div className="reviewer-card">
          <h3>原始行</h3>
          <pre className="reviewer-json">{JSON.stringify(row, null, 1)}</pre>
        </div>
        <div className="reviewer-card">
          <h3>审查要点</h3>
          <p className="muted">
            · 左键(LMB)按下 → 后续 1–3 帧内画面应出现开火/交互反馈，这是输入→渲染延迟，属正常；
            反过来（画面先于标注）才是错位。
            <br />· pointerDx&gt;0 → 视角右转；dy&gt;0 → 视角下移。
            <br />· W/A/S/D/Shift 按住期间画面应持续位移/加速。
            <br />· 鼠标键按住状态：有 buttons 字段以它为准；否则由本段 events 重建，段首先出现 up
            说明按下发生在上一段（跨界持续，正常）。
          </p>
        </div>
      </div>
    </div>
  );
}
