export type DepthLifecycleState =
  | "idle"
  | "starting"
  | "started"
  | "ready"
  | "stopping"
  | "stopped"
  | "flushed"
  | "faulted";

export type DepthStreamIdentity = {
  session_id: string;
  run_id: string;
  generation: number;
};

export type DepthProbeRuntimeStatus = {
  configured: boolean;
  enabled: boolean;
  riskAcknowledged: boolean;
  lifecycle: DepthLifecycleState;
  activeIdentity: DepthStreamIdentity | null;
  lastError: string | null;
  performanceValidationOnly: boolean;
  performanceMetricsEnabled: boolean;
  earlyLaunchRequired: boolean;
  cp2077LocalValidationAvailable: boolean;
  earlyLaunchGameId: string | null;
  earlyLaunchState: "not_required" | "awaiting_launch" | "launching" | "waiting_for_window" | "bound";
  earlyLaunchPid: number | null;
  discoveryAvailable: boolean;
  discoveryState: DiscoverySmokeState;
  discoveryPid: number | null;
  discoveryOutputLocator: string | null;
  discoveryLastError: string | null;
  discoveryRecordingReady: false;
};

export type DiscoverySmokeState =
  | "idle"
  | "preparing"
  | "hooks_armed"
  | "waiting_observations"
  | "sealing"
  | "report_sealed"
  | "faulted";

export type DiscoverySmokePresentation = {
  label:
    | "Idle"
    | "Preparing hooks"
    | "Hooks armed"
    | "Waiting observations"
    | "Sealing reports"
    | "Report sealed"
    | "Discovery faulted";
  canStart: boolean;
  canSeal: boolean;
  recordingReady: false;
};

export function presentDiscoverySmoke(state: DiscoverySmokeState): DiscoverySmokePresentation {
  const labels: Record<DiscoverySmokeState, DiscoverySmokePresentation["label"]> = {
    idle: "Idle",
    preparing: "Preparing hooks",
    hooks_armed: "Hooks armed",
    waiting_observations: "Waiting observations",
    sealing: "Sealing reports",
    report_sealed: "Report sealed",
    faulted: "Discovery faulted",
  };
  return {
    label: labels[state],
    canStart: ["idle", "report_sealed", "faulted"].includes(state),
    canSeal: ["hooks_armed", "waiting_observations"].includes(state),
    recordingReady: false,
  };
}

export type DepthProbeSettings = {
  enabled: boolean;
  riskAcknowledged: boolean;
  unbindFromEnd: number;
  plannedSeconds: number;
  startTimeoutMs: number;
  stopTimeoutMs: number;
  /** Calibration-only bypass for a new performance key; output is permanently non-deliverable. */
  performanceValidationOnly: boolean;
  /** A3 per-Present timing, carried explicitly in StartRun. */
  performanceMetricsEnabled: boolean;
};

export type PerfStage = "baseline" | "watermark_only" | "readback_only" | "full";

export function cp2077LocalValidationAvailable(
  status: DepthProbeRuntimeStatus,
  gameId: string,
): boolean {
  return status.cp2077LocalValidationAvailable === true && gameId === "cyberpunk-2077-steam";
}

/** Explicit short-run intent cannot inherit certification mode or cross games. */
export function depthToggleSettings(
  settings: DepthProbeSettings,
  status: DepthProbeRuntimeStatus,
  gameId: string,
  enabled: boolean,
  riskAcknowledged: boolean,
  localValidationSelected: boolean,
): DepthProbeSettings {
  const localValidation = enabled && localValidationSelected
    && cp2077LocalValidationAvailable(status, gameId);
  return {
    ...settings,
    enabled,
    riskAcknowledged: enabled && riskAcknowledged,
    performanceValidationOnly: localValidation,
    performanceMetricsEnabled: localValidation,
    // Reserve a minute for the approximate 30-second manual run; this is not a timer.
    plannedSeconds: localValidation ? 60 : settings.plannedSeconds,
  };
}

export type PerfStageFlushState = "waiting" | "ready" | "faulted" | "identity_mismatch";

/**
 * Decide whether the just-paused performance run has produced authoritative Flushed evidence.
 * Collector pause is asynchronous, so `collector_pause` returning is not this boundary.
 */
export function perfStageFlushState(
  status: DepthProbeRuntimeStatus,
  expectedRunId: string,
): PerfStageFlushState {
  if (status.activeIdentity && status.activeIdentity.run_id !== expectedRunId) {
    return "identity_mismatch";
  }
  if (status.lifecycle === "faulted" || status.lastError) return "faulted";
  if (status.lifecycle === "flushed" && status.activeIdentity?.run_id === expectedRunId) {
    return "ready";
  }
  return "waiting";
}

export type LatencyQuantiles = {
  p50: number;
  p95: number;
  p99: number;
  max: number;
};

export type WriterTimingSummary = {
    sampleCount: number;
    totalCommitted: number;
    wrapped: boolean;
    overBudgetCount: number;
    durationUs: LatencyQuantiles;
};

export type EarlyLaunchPreflight = {
  eligible: boolean;
  ready: boolean;
  executableHashVerified: boolean;
  launcherPresent: boolean;
  launcherSha256: string | null;
  probePresent: boolean;
  probeSha256: string | null;
  blockers: string[];
};

export function earlyLaunchGameName(gameId: string | null | undefined): string {
  return gameId === "kingdom-come-deliverance-2-steam"
    ? "Kingdom Come: Deliverance II"
    : gameId === "cyberpunk-2077-steam"
      ? "Cyberpunk 2077"
      : "当前游戏";
}

export type Cp2077TrialStatus = {
  available: boolean;
  phase: "idle" | "preparing" | "prepared" | "recording" | "processing" | "completed" | "faulted";
  purpose: "five_stream" | "metric_calibration" | null;
  processId: number | null;
  sessionLocator: string | null;
  profileCandidate: string | null;
  elapsedTargetSeconds: number;
  videoFrames: number | null;
  depthFrames: number | null;
  keyboardInputEvents: number | null;
  mouseInputEvents: number | null;
  metricMarkerCount: number;
  metricCalibrationSamples: number | null;
  metricEvidenceSealed: boolean;
  metricEvidenceMetresPerWorldUnit: number | null;
  metricEvidenceSessionLocator: string | null;
  deliveryLocator: string | null;
  recordingElapsedSeconds: number;
  canStop: boolean;
  lastError: string | null;
  nonTrainable: true;
  recordingReady: false;
};

export type PerfRunOutcome = {
  avgFpsDrop: number;
  low1FpsDrop: number;
  watermarkAvgDrop: number;
  readbackAvgDrop: number;
  writerAvgDrop: number;
  watermarkLow1Drop: number;
  readbackLow1Drop: number;
  writerLow1Drop: number;
  readbackOnlyBypassedFrames: number;
  fullWriterTiming: WriterTimingSummary;
  maxAvgFpsDrop: number;
  maxLow1FpsDrop: number;
  passes: boolean;
};

export type PerfMetricPresentation = {
  valid: boolean;
  passes: boolean;
  message: string;
};

export type PerfRunOutcomePresentation = {
  avgFps: PerfMetricPresentation;
  low1Fps: PerfMetricPresentation;
  valid: boolean;
  passes: boolean;
  conclusion: string;
  attribution: string[];
};

function presentPerfMetric(label: string, rawDrop: unknown, rawLimit: unknown): PerfMetricPresentation {
  const drop = typeof rawDrop === "number" && Number.isFinite(rawDrop) ? rawDrop : null;
  const limit = typeof rawLimit === "number" && Number.isFinite(rawLimit) && rawLimit >= 0
    ? rawLimit
    : null;
  if (drop === null || limit === null) {
    const expected = limit === null ? "有效门限" : `≤ ${(limit * 100).toFixed(2)}%`;
    return {
      valid: false,
      passes: false,
      message: `${label} 数据无效（要求 ${expected}），按未通过处理`,
    };
  }

  const dropPercent = drop * 100;
  const limitPercent = limit * 100;
  const negativeNoiseLimit = limit * 2;
  if (drop < -negativeNoiseLimit) {
    return {
      valid: false,
      passes: false,
      message: `${label} ${dropPercent.toFixed(2)}% < -${(negativeNoiseLimit * 100).toFixed(2)}%，基线与完整档波动过大，本轮无效`,
    };
  }
  if (drop <= limit) {
    return {
      valid: true,
      passes: true,
      message: `${label} ${dropPercent.toFixed(2)}% ≤ ${limitPercent.toFixed(2)}%，通过`,
    };
  }
  return {
    valid: true,
    passes: false,
    message: `${label} ${dropPercent.toFixed(2)}% > ${limitPercent.toFixed(2)}%，超出 ${(dropPercent - limitPercent).toFixed(2)} 个百分点`,
  };
}

/** Render the Rust IPC outcome without ever leaking undefined/NaN/Infinity into operator copy. */
export function presentPerfRunOutcome(outcome: PerfRunOutcome): PerfRunOutcomePresentation {
  const avgFps = presentPerfMetric("平均 FPS", outcome.avgFpsDrop, outcome.maxAvgFpsDrop);
  const low1Fps = presentPerfMetric("1% low", outcome.low1FpsDrop, outcome.maxLow1FpsDrop);
  const calculatedPasses = avgFps.passes && low1Fps.passes;
  const contributions = [
    outcome.watermarkAvgDrop,
    outcome.readbackAvgDrop,
    outcome.writerAvgDrop,
    outcome.watermarkLow1Drop,
    outcome.readbackLow1Drop,
    outcome.writerLow1Drop,
  ];
  const finiteContributions = contributions.every(
    (value) => typeof value === "number" && Number.isFinite(value),
  );
  const contributionSumsAgree = finiteContributions
    && Math.abs(
      outcome.watermarkAvgDrop + outcome.readbackAvgDrop + outcome.writerAvgDrop
        - outcome.avgFpsDrop,
    ) < 1e-9
    && Math.abs(
      outcome.watermarkLow1Drop + outcome.readbackLow1Drop + outcome.writerLow1Drop
        - outcome.low1FpsDrop,
    ) < 1e-9;
  const timing = outcome.fullWriterTiming;
  const timingValues = timing
    ? [
      timing.sampleCount,
      timing.totalCommitted,
      timing.overBudgetCount,
      timing.durationUs?.p50,
      timing.durationUs?.p95,
      timing.durationUs?.p99,
      timing.durationUs?.max,
    ]
    : [];
  const timingValid = timingValues.length === 7
    && timingValues.every((value) => typeof value === "number" && Number.isFinite(value) && value >= 0)
    && timing.sampleCount > 0
    && timing.sampleCount <= timing.totalCommitted
    && timing.overBudgetCount <= timing.sampleCount
    && timing.durationUs.p50 <= timing.durationUs.p95
    && timing.durationUs.p95 <= timing.durationUs.p99
    && timing.durationUs.p99 <= timing.durationUs.max;
  const bypassValid = typeof outcome.readbackOnlyBypassedFrames === "number"
    && Number.isFinite(outcome.readbackOnlyBypassedFrames)
    && outcome.readbackOnlyBypassedFrames > 0;
  const finiteMetrics = avgFps.valid
    && low1Fps.valid
    && finiteContributions
    && contributionSumsAgree
    && timingValid
    && bypassValid;
  const verdictConsistent = typeof outcome.passes === "boolean" && outcome.passes === calculatedPasses;
  const valid = finiteMetrics && verdictConsistent;
  const passes = valid && calculatedPasses;
  const conclusion = !finiteMetrics
    ? "认证结论：结果数据无效，按未通过处理"
    : !verdictConsistent
      ? "认证结论：结果与门限判定不一致，按未通过处理"
      : passes
        ? "认证结论：通过"
        : "认证结论：未通过";
  const percentPoints = (value: number) => `${value >= 0 ? "+" : ""}${(value * 100).toFixed(2)}`;
  const attribution = finiteContributions && contributionSumsAgree
    ? [
      `平均 FPS 降幅贡献：水印 ${percentPoints(outcome.watermarkAvgDrop)} · GPU 回读 ${percentPoints(outcome.readbackAvgDrop)} · writer ${percentPoints(outcome.writerAvgDrop)} 个百分点`,
      `1% low 降幅贡献：水印 ${percentPoints(outcome.watermarkLow1Drop)} · GPU 回读 ${percentPoints(outcome.readbackLow1Drop)} · writer ${percentPoints(outcome.writerLow1Drop)} 个百分点`,
      bypassValid
        ? `readback-only 已在 writer 前旁路 ${Math.trunc(outcome.readbackOnlyBypassedFrames).toLocaleString("zh-CN")} 帧`
        : "readback-only 旁路证据无效",
      timingValid
        ? `writer 耗时（${Math.trunc(timing.sampleCount).toLocaleString("zh-CN")} 样本）：p50 ${(timing.durationUs.p50 / 1_000).toFixed(2)} ms · p95 ${(timing.durationUs.p95 / 1_000).toFixed(2)} ms · p99 ${(timing.durationUs.p99 / 1_000).toFixed(2)} ms · max ${(timing.durationUs.max / 1_000).toFixed(2)} ms；超 33.3 ms ${Math.trunc(timing.overBudgetCount).toLocaleString("zh-CN")} 次`
        : "writer 耗时证据无效",
    ]
    : ["性能归因数据无效，无法区分水印、GPU 回读与 writer 开销"];
  return { avgFps, low1Fps, valid, passes, conclusion, attribution };
}

export type PerfCertificationStatus = {
  active: boolean;
  certificationId: string | null;
  nextStage: PerfStage | null;
  outcome: PerfRunOutcome | null;
  candidatePath: string | null;
  completionError: string | null;
};

export function perfCertificationAction(
  status: PerfCertificationStatus | null,
  collectorState: string,
): "begin" | "stage" | "export" | "finish_session" {
  if (status?.certificationId && !status.nextStage) {
    if (status.outcome && !status.candidatePath) return "export";
    if (collectorState !== "bound") return "finish_session";
  }
  return status?.active && status.nextStage ? "stage" : "begin";
}

export type PerfCertificationGateStatus = {
  state: "passed" | "baseline_matched" | "blocked";
  certificationId: string | null;
  gpuModel: string;
  runtimeVersion: string;
  rendererBackend: "d3d11" | "d3d12";
  backendGeneration: number;
  detail: string;
};

export type PerfCertificationGatePresentation = {
  label: "正在核验" | "尚未核验" | "基准已匹配" | "认证已通过" | "认证未通过" | "检查失败";
  tone: "pending" | "good" | "danger";
  detail: string;
};

export function presentPerfCertificationGate(
  status: PerfCertificationGateStatus | null,
  busy: boolean,
  error: string | null,
): PerfCertificationGatePresentation {
  if (busy) {
    return {
      label: "正在核验",
      tone: "pending",
      detail: "正在核对本机硬件与签名基准认证；不会开始录制。",
    };
  }
  if (error) {
    return { label: "检查失败", tone: "danger", detail: annotatorMessage(error) };
  }
  if (!status) {
    return {
      label: "尚未核验",
      tone: "pending",
      detail: "开启 Depth 并绑定游戏后会自动检查，也可以手动重新检查。",
    };
  }
  if (status.state === "baseline_matched") {
    return { label: "基准已匹配", tone: "good", detail: status.detail };
  }
  if (status.state === "passed") {
    return {
      label: "认证已通过",
      tone: "good",
      detail: `${status.detail} · ${status.gpuModel} · 认证 ${status.certificationId ?? "—"}`,
    };
  }
  return { label: "认证未通过", tone: "danger", detail: status.detail };
}

export type PerfCaptureSample = {
  elapsedMs: number;
  observedFrames: number;
};

export type PerfStageSummary = {
  avgFps: number;
  low1Fps: number;
  seconds: number;
  frameCapped: boolean;
};

/**
 * 一档实测的汇总。
 *
 * `frameCapped` 的判据**按档不同**，这一点不是可有可无的细节：
 *
 * 锁帧是靠「没有超过标称帧率」证明的，不是靠「贴着标称帧率」。
 * 对基线档两端都要判——基线本就该稳在帽子上，如果它已经掉到 0.9×target 以下，
 * 说明这台机器连不注入都跑不到标称帧率，整轮没有意义，拒掉是对的。
 *
 * 但对仅水印与完整档只能判上界。**掉下去正是要测的那件事**：
 * 门限是平均掉帧 ≤2%，而两端都判会把任何掉幅超过 10% 的档报成
 * `NotFrameCapped`——采集员收到的是「你没锁帧」，而他明明锁了，
 * 真因是回读太贵。#443 那次掉了 48.7%，恰恰是三档归因最该派上用场的场景，
 * 两端都判的话那组数据根本采不上来。症状≠真因，130 号 §6.1。
 */
export function summarizePerfStage(
  samples: PerfCaptureSample[],
  targetFps: number,
  stage: PerfStage = "baseline",
): PerfStageSummary | null {
  if (samples.length < 2 || !Number.isFinite(targetFps) || targetFps <= 0) return null;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const seconds = (last.elapsedMs - first.elapsedMs) / 1_000;
  const frames = last.observedFrames - first.observedFrames;
  if (seconds <= 0 || frames <= 0) return null;
  const rates: number[] = [];
  for (let index = 1; index < samples.length; index += 1) {
    const previous = samples[index - 1];
    const current = samples[index];
    const intervalSeconds = (current.elapsedMs - previous.elapsedMs) / 1_000;
    const intervalFrames = current.observedFrames - previous.observedFrames;
    if (intervalSeconds > 0 && intervalFrames >= 0) rates.push(intervalFrames / intervalSeconds);
  }
  if (rates.length === 0) return null;
  rates.sort((left, right) => left - right);
  const lowCount = Math.max(1, Math.ceil(rates.length * 0.01));
  const low1Fps = rates.slice(0, lowCount).reduce((sum, value) => sum + value, 0) / lowCount;
  const avgFps = frames / seconds;
  return {
    avgFps,
    low1Fps,
    seconds,
    frameCapped:
      avgFps <= targetFps * 1.05 && (stage !== "baseline" || avgFps >= targetFps * 0.9),
  };
}
export type DepthUiPhase =
  | "off"
  | "starting"
  | "ready"
  | "active"
  | "stopping"
  | "flushing"
  | "fault";

export type DepthUiPresentation = {
  phase: DepthUiPhase;
  label: "OFF" | "STARTING" | "READY" | "ACTIVE" | "STOPPING" | "FLUSHING" | "FAULT";
  title: string;
  detail: string;
  segmentComplete: boolean;
};

export const DEPTH_START_TIMEOUT_MIN_MS = 15_000;

export const DEFAULT_DEPTH_SETTINGS: DepthProbeSettings = {
  enabled: false,
  riskAcknowledged: false,
  unbindFromEnd: 4,
  plannedSeconds: 600,
  startTimeoutMs: 15_000,
  stopTimeoutMs: 30_000,
  performanceValidationOnly: false,
  performanceMetricsEnabled: false,
};

export const EMPTY_DEPTH_STATUS: DepthProbeRuntimeStatus = {
  configured: false,
  enabled: false,
  riskAcknowledged: false,
  lifecycle: "idle",
  activeIdentity: null,
  lastError: null,
  performanceValidationOnly: false,
  performanceMetricsEnabled: false,
  earlyLaunchRequired: false,
  cp2077LocalValidationAvailable: false,
  earlyLaunchGameId: null,
  earlyLaunchState: "not_required",
  earlyLaunchPid: null,
  discoveryAvailable: false,
  discoveryState: "idle",
  discoveryPid: null,
  discoveryOutputLocator: null,
  discoveryLastError: null,
  discoveryRecordingReady: false,
};

const ACTIVE_COLLECTOR_STATES = new Set(["recording"]);
const LOCKED_COLLECTOR_STATES = new Set([
  "preflighting",
  "starting",
  "recording",
  "pausing",
  "stopping",
]);

/**
 * Depth 面板只认当前 session 契约，不拿 Authenticode 状态充当能力开关。
 *
 * `dm-local-dev` 仍可做本地实验，`dm-session-ingest` 则把 Depth 作为第七件
 * artifact 走正式 uploader。真正有没有编入 feature 由后端命令可用性回答；当前
 * 游戏有没有有效 profile/binding、EXE hash 和性能认证继续由运行时门禁回答。
 */
export function depthUiEnvironmentEligible(
  sessionContract: string | undefined,
): boolean {
  return sessionContract === "dm-local-dev" || sessionContract === "dm-session-ingest";
}

/**
 * Early-launch games need the Depth control plane before a window exists. Once
 * bound, the frozen target contract remains authoritative; before binding, the
 * selected catalog profile supplies the same per-game contract.
 */
export function depthPanelSessionContract(
  boundTargetContract: string | undefined,
  selectedProfileContract: string | undefined,
): string | undefined {
  return boundTargetContract ?? selectedProfileContract;
}

export type DepthPanelRuntimeContext = {
  gameId: string;
  sessionContract: string;
  targetFps: number;
};

/**
 * Catalog activation briefly clears the visible roster while preserving the selected game id.
 * Keep only the already verified route for that same game during the refresh; a different or
 * retired game must never inherit it.
 */
export function resolveDepthPanelRuntimeContext(
  catalogAvailable: boolean,
  gameId: string,
  sessionContract: string | undefined,
  targetFps: number | null,
  previous: DepthPanelRuntimeContext | null,
): DepthPanelRuntimeContext | null {
  if (!catalogAvailable) return null;
  const normalizedGameId = gameId.trim();
  if (!normalizedGameId) return null;
  const matchingPrevious = previous?.gameId === normalizedGameId ? previous : null;
  const resolvedContract = sessionContract ?? matchingPrevious?.sessionContract;
  const resolvedTargetFps = targetFps ?? matchingPrevious?.targetFps;
  if (
    !resolvedContract
    || resolvedTargetFps === undefined
    || !Number.isFinite(resolvedTargetFps)
    || resolvedTargetFps <= 0
  ) return null;
  return {
    gameId: normalizedGameId,
    sessionContract: resolvedContract,
    targetFps: resolvedTargetFps,
  };
}

/**
 * The probe lifecycle is the source of truth. Collector state is used only for
 * the READY -> ACTIVE distinction: Ready says the probe has acknowledged its
 * boundary; recording says the main capture gate is actually open.
 */
export function presentDepthStatus(
  status: DepthProbeRuntimeStatus,
  collectorState: string,
  statusReadError: string | null = null,
): DepthUiPresentation {
  if (statusReadError) {
    return {
      phase: "fault",
      label: "FAULT",
      title: "无法确认深度采集状态",
      detail: "客户端暂时读不到深度组件状态；恢复前不会沿用上一次的采集中显示。",
      segmentComplete: false,
    };
  }
  if (status.lifecycle === "faulted" || status.lastError) {
    return {
      phase: "fault",
      label: "FAULT",
      title: "深度采集已停止",
      detail: annotatorMessage(status.lastError),
      segmentComplete: false,
    };
  }

  if (status.lifecycle === "starting" || status.lifecycle === "started") {
    return {
      phase: "starting",
      label: "STARTING",
      title: status.lifecycle === "started" ? "组件已启动，等待可用帧" : "正在启动深度采集",
      detail: "主录制尚未开始；收到 Ready 前不会写成正在采集。",
      segmentComplete: false,
    };
  }

  if (status.lifecycle === "ready") {
    if (ACTIVE_COLLECTOR_STATES.has(collectorState)) {
      return {
        phase: "active",
        label: "ACTIVE",
        title: "深度采集进行中",
        detail: "主录制已放行，深度与相机数据正在按当前 generation 写入。",
        segmentComplete: false,
      };
    }
    return {
      phase: "ready",
      label: "READY",
      title: "深度组件已就绪",
      detail: "已收到真实 Ready；主录制放行后才会显示为 Active。",
      segmentComplete: false,
    };
  }

  if (status.lifecycle === "stopping") {
    return {
      phase: "stopping",
      label: "STOPPING",
      title: "正在停止接收新帧",
      detail: "正在等待 Stopped；当前段尚未确认完整。",
      segmentComplete: false,
    };
  }

  if (status.lifecycle === "stopped") {
    return {
      phase: "flushing",
      label: "FLUSHING",
      title: "正在排空并封口",
      detail: "已停止接收新帧，仍在等待待写数据排空和真实 Flushed。",
      segmentComplete: false,
    };
  }

  if (status.lifecycle === "flushed") {
    return {
      phase: "off",
      label: "OFF",
      title: status.enabled ? "当前段已完整封口" : "深度采集已关闭",
      detail: status.enabled
        ? "已确认真实 Flushed；恢复录制时会创建新的 generation。"
        : "本次进程未启用深度采集。",
      segmentComplete: true,
    };
  }

  return {
    phase: "off",
    label: "OFF",
    title: status.enabled ? "已打开，等待录制" : "深度采集已关闭",
    detail: status.enabled
      ? "深度组件尚未启动；开始录制后会先等待 Ready。"
      : "仅在授权的 DEV / NON-TRAINABLE 采集中手动开启。",
    segmentComplete: false,
  };
}

/** User-facing copy only. The raw value belongs in diagnostics, not this line. */
export function annotatorMessage(rawError: string | null | undefined): string {
  if (!rawError) return "深度采集未能继续，请导出诊断后联系工程。";
  const error = rawError.toLowerCase();
  if (error.includes("depth_present_rate_mismatch")) {
    return "实际游戏帧率不满足基准要求。请检查限帧和帧生成设置，并在游戏稳定后重试；无需重跑四档。";
  }
  if (error.includes("depth_device_requirements_failed")) {
    return "本机硬件或驱动未通过所选游戏的设备要求，请查看预检中的要求与实际值。";
  }
  if (error.includes("depth_certification_blocked")) {
    return "未匹配当前游戏配置的可用基准认证，请检查认证清单与采集组件版本。";
  }
  if (error.includes("depth_local_validation_failed")) {
    return "本机采集条件校验未完成，请检查实际帧率、游戏启动状态和探针版本。";
  }
  if (error.includes("command") && error.includes("not found") && error.includes("depth_lab_")) {
    return "当前安装版本缺少深度启动接口，请安装修复版本后重新打开应用。";
  }
  if (error.includes("depth_perf_stage_required")) {
    return "性能认证进行中，请从深度认证卡片启动或继续当前档。";
  }
  if (error.includes("风险") || error.includes("risk")) {
    return "请先阅读并确认深度采集的风险说明。";
  }
  // Catalog diagnostics for an exact-build mismatch mention both "profile" and
  // "binding" after the operator-facing explanation. Match the actual cause
  // before the generic configuration bucket or a game update is misreported as
  // a broken local configuration.
  if (
    error.includes("executable")
    || error.includes("exact-build mismatch")
    || error.includes("游戏版本")
  ) {
    return "当前游戏版本尚未完成深度采集适配，请联系工程；重跑性能认证不能解除此限制。";
  }
  if (error.includes("这个游戏暂不支持深度采集")) {
    return "这个游戏暂不支持深度采集。";
  }
  if (error.includes("文件不存在") || error.includes("profile") || error.includes("binding")) {
    return "深度采集配置不可用，请检查所选配置文件是否与当前游戏版本匹配。";
  }
  if (error.includes("bitness") || error.includes("位数")) {
    return "深度采集组件与该游戏不匹配，请联系工程处理。";
  }
  if (error.includes("lane") || error.includes("帧率档")) {
    return "当前录制帧率与深度配置不一致，请更换匹配的配置。";
  }
  if (error.includes("磁盘") || error.includes("disk")) {
    return "本次预计空间不足，请释放采集盘空间或缩短计划时长。";
  }
  if (
    error.includes("性能验收")
    || error.includes("性能认证")
    || error.includes("fps gate")
    || error.includes("certif")
  ) {
    return "本机型尚未完成深度采集性能验收，暂不能开启。";
  }
  if (
    error.includes("stop_before_generation_sealed")
    || error.includes("StopRun arrived before GenerationSealed")
  ) {
    return "录制主链路先异常停止，深度段未能完成封口；请以采集错误详情中的首个故障为准。";
  }
  if (error.includes("feedback_timestamp_invalid")) {
    return "WGC 帧时间与本机时钟出现异常偏移，本次录制已停止。请保留诊断并联系工程。";
  }
  // "feedback" contains "ack", so matching a bare "ack" claimed every
  // feedback_* fault for the identity line below -- a timestamp skew, a full
  // queue and a telemetry cap all read as "identity check failed", which sends
  // the investigation at the wrong half of the pipeline. Match the ACK keyword
  // against a string with that word taken out.
  const outsideFeedback = error.replaceAll("feedback", " ");
  const containsAckToken = /(^|[^a-z])ack([^a-z]|$)/.test(outsideFeedback);
  if (
    error.includes("generation")
    || error.includes("identity")
    || containsAckToken
    || error.includes("身份")
    || error.includes("迟到")
  ) {
    return "本次运行身份校验失败，旧状态没有被接受；请结束后重新开始。";
  }
  if (
    error.includes("flushed")
    || error.includes("writer")
    || error.includes("queue")
    || error.includes("封口")
  ) {
    return "当前段未能确认完整封口，请勿把它当作完整深度数据。";
  }
  if (error.includes("timeout") || error.includes("超时") || error.includes("ready")) {
    return "深度组件未在规定时间内就绪，本次录制没有带深度启动。";
  }
  return "深度采集未能继续，请导出诊断后联系工程。";
}

export function depthControlLocked(status: DepthProbeRuntimeStatus, collectorState: string): boolean {
  return LOCKED_COLLECTOR_STATES.has(collectorState)
    || ["starting", "started", "ready", "stopping", "stopped"].includes(status.lifecycle);
}

export function depthEnableIntent(riskAcknowledged: boolean): "confirm_risk" | "submit" {
  return riskAcknowledged ? "submit" : "confirm_risk";
}

export function cp2077TrialPrepareBlocker(
  collectorState: string,
  trialPhase: Cp2077TrialStatus["phase"],
): string | null {
  if (["preparing", "prepared", "recording", "processing"].includes(trialPhase)) {
    return "CP2077 五流实验正在准备或运行，请先完成当前流程。";
  }
  if (!["idle", "completed", "faulted"].includes(collectorState)) {
    return "主采集器当前不是空闲终态，请先结束或解除现有窗口流程。";
  }
  return null;
}

export function validateDepthSettings(settings: DepthProbeSettings): string | null {
  if (!settings.enabled) return null;
  if (settings.performanceMetricsEnabled && !settings.performanceValidationOnly) {
    return "A3 分项计时只能在性能校准模式中启用。";
  }
  if (!settings.riskAcknowledged) return "请先阅读并确认深度采集的风险说明。";
  if (!Number.isInteger(settings.unbindFromEnd) || settings.unbindFromEnd < 0) {
    return "采样回退位置必须是大于等于 0 的整数。";
  }
  if (!Number.isInteger(settings.plannedSeconds) || settings.plannedSeconds <= 0) {
    return "计划时长必须是大于 0 的整数秒。";
  }
  if (
    !Number.isInteger(settings.startTimeoutMs)
    || !Number.isInteger(settings.stopTimeoutMs)
    || settings.startTimeoutMs < DEPTH_START_TIMEOUT_MIN_MS
    || settings.startTimeoutMs > 60_000
    || settings.stopTimeoutMs < 500
    || settings.stopTimeoutMs > 60_000
  ) {
    return "启动等待时间必须在 15000 到 60000 毫秒之间，停止等待时间必须在 500 到 60000 毫秒之间。";
  }
  return null;
}

export type DepthToggleResult = {
  status: DepthProbeRuntimeStatus;
  rawError: string | null;
};

/** Configure failures must publish a freshly read backend status, never optimism. */
export async function configureDepthToggle(
  previous: DepthProbeRuntimeStatus,
  settings: DepthProbeSettings,
  configure: (request: DepthProbeSettings) => Promise<DepthProbeRuntimeStatus>,
  refresh: () => Promise<DepthProbeRuntimeStatus>,
): Promise<DepthToggleResult> {
  try {
    return { status: await configure(settings), rawError: null };
  } catch (reason) {
    let rolledBack = previous;
    try {
      rolledBack = await refresh();
    } catch {
      // The previous confirmed snapshot is safer than inventing a new state.
    }
    return { status: rolledBack, rawError: String(reason) };
  }
}

export function formatDepthDiagnostic(
  status: DepthProbeRuntimeStatus,
  collectorState: string,
  commandError: string | null,
  capturedAt = new Date().toISOString(),
  performanceCertificationError: string | null = null,
  performanceCertification: PerfCertificationStatus | null = null,
): string {
  return JSON.stringify({
    schema: "depth-lab-ui-diagnostic/v1",
    capturedAt,
    collectorState,
    configured: status.configured,
    enabled: status.enabled,
    riskAcknowledged: status.riskAcknowledged,
    lifecycle: status.lifecycle,
    activeIdentity: status.activeIdentity,
    lastError: status.lastError,
    performanceValidationOnly: status.performanceValidationOnly,
    performanceMetricsEnabled: status.performanceMetricsEnabled,
    earlyLaunchRequired: status.earlyLaunchRequired,
    earlyLaunchGameId: status.earlyLaunchGameId,
    earlyLaunchState: status.earlyLaunchState,
    earlyLaunchPid: status.earlyLaunchPid,
    discoveryAvailable: status.discoveryAvailable,
    discoveryState: status.discoveryState,
    discoveryPid: status.discoveryPid,
    discoveryOutputLocator: status.discoveryOutputLocator,
    discoveryLastError: status.discoveryLastError,
    discoveryRecordingReady: status.discoveryRecordingReady,
    commandError,
    performanceCertificationError,
    performanceCertification,
  }, null, 2);
}
