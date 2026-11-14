import { DepthGameLaunchCard } from "./DepthGameLaunchCard";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useRef, useState } from "react";
import type { CollectorSnapshot } from "./collector-contract";
import {
  DEFAULT_DEPTH_SETTINGS,
  DEPTH_START_TIMEOUT_MIN_MS,
  EMPTY_DEPTH_STATUS,
  annotatorMessage,
  configureDepthToggle,
  cp2077LocalValidationAvailable,
  cp2077TrialPrepareBlocker,
  depthControlLocked,
  depthEnableIntent,
  depthToggleSettings,
  depthUiEnvironmentEligible,
  formatDepthDiagnostic,
  perfStageFlushState,
  perfCertificationAction,
  presentPerfCertificationGate,
  presentPerfRunOutcome,
  presentDiscoverySmoke,
  presentDepthStatus,
  validateDepthSettings,
} from "./depth-lab-ui";
import type {
  DepthProbeRuntimeStatus,
  DepthProbeSettings,
  EarlyLaunchPreflight,
  Cp2077TrialStatus,
  PerfCertificationGateStatus,
  PerfCertificationStatus,
  PerfStage,
} from "./depth-lab-ui";

const PERF_STAGE_SETTLE_MS = 5_000;
const PERF_STAGE_MEASUREMENT_MS = 300_000;
const PERF_STAGE_TAIL_GUARD_MS = 5_000;
const PERF_STAGE_READY_WINDOW_MS =
  PERF_STAGE_SETTLE_MS + PERF_STAGE_MEASUREMENT_MS + PERF_STAGE_TAIL_GUARD_MS;
const PERF_GATE_CHECK_COLLECTOR_STATES = new Set(["bound", "paused", "completed", "faulted"]);

type DepthLabPanelProps = {
  sessionContract: string | undefined;
  collectorState: string;
  gameId: string;
  capture: {
    runId: string;
    frameCount: number;
    wgcCallbackCount: number;
    duplicateFrames: number;
    sourceSuperseded: number;
    elapsedMs: number;
  } | null;
  targetFps: number | null;
  onCollectorSnapshot?: (snapshot: CollectorSnapshot) => void;
  onPerfCertificationChange?: (active: boolean) => void;
};

function perfStageName(stage: PerfStage | null | undefined): string {
  return stage === "baseline"
    ? "Present 计时基线（无水印/深度）"
    : stage === "watermark_only"
      ? "仅水印"
      : stage === "readback_only"
        ? "GPU 回读（writer 旁路）"
      : stage === "full"
        ? "完整采集"
        : "已完成";
}

export function DepthLabPanel({
  sessionContract,
  collectorState,
  gameId,
  capture,
  targetFps,
  onCollectorSnapshot,
  onPerfCertificationChange,
}: DepthLabPanelProps) {
  const eligibleSession = depthUiEnvironmentEligible(sessionContract);
  const [featureAvailable, setFeatureAvailable] = useState(false);
  const [status, setStatus] = useState<DepthProbeRuntimeStatus>(EMPTY_DEPTH_STATUS);
  const [settings, setSettings] = useState<DepthProbeSettings>(DEFAULT_DEPTH_SETTINGS);
  const [commandBusy, setCommandBusy] = useState(false);
  const [commandError, setCommandError] = useState<string | null>(null);
  const [statusReadError, setStatusReadError] = useState<string | null>(null);
  const [localBlock, setLocalBlock] = useState<string | null>(null);
  const [configOpen, setConfigOpen] = useState(false);
  const [localValidationSelected, setLocalValidationSelected] = useState(false);
  const [diagnosticOpen, setDiagnosticOpen] = useState(false);
  const [copyResult, setCopyResult] = useState<string | null>(null);
  const [riskOpen, setRiskOpen] = useState(false);
  const [riskChecked, setRiskChecked] = useState(false);
  const [riskPurpose, setRiskPurpose] = useState<
    "formal" | "discovery" | "certification" | "trial" | "metric_calibration"
  >("formal");
  const [earlyPreflight, setEarlyPreflight] = useState<EarlyLaunchPreflight | null>(null);
  const [earlyPreflightBusy, setEarlyPreflightBusy] = useState(false);
  const [earlyPreflightError, setEarlyPreflightError] = useState<string | null>(null);
  const [discoveryPreflight, setDiscoveryPreflight] = useState<EarlyLaunchPreflight | null>(null);
  const [discoveryPreflightBusy, setDiscoveryPreflightBusy] = useState(false);
  const [trialStatus, setTrialStatus] = useState<Cp2077TrialStatus | null>(null);
  const [trialBusy, setTrialBusy] = useState(false);
  const [perfStatus, setPerfStatus] = useState<PerfCertificationStatus | null>(null);
  const [perfGateStatus, setPerfGateStatus] = useState<PerfCertificationGateStatus | null>(null);
  const [perfGateBusy, setPerfGateBusy] = useState(false);
  const [perfGateError, setPerfGateError] = useState<string | null>(null);
  const [perfStageRunning, setPerfStageRunning] = useState(false);
  const [perfStageRetryRequired, setPerfStageRetryRequired] = useState(false);
  const [perfMessage, setPerfMessage] = useState<string | null>(null);
  const perfStageReadyElapsedMs = useRef<number | null>(null);
  const perfRunId = useRef<string | null>(null);
  const perfRunObserved = useRef(false);
  const perfFinalizing = useRef(false);
  const perfGateAutoCheckKey = useRef<string | null>(null);
  const perfGateRequestVersion = useRef(0);
  const requestVersion = useRef(0);
  const commandInFlight = useRef(false);
  const lastLoggedError = useRef<string | null>(null);

  useEffect(() => {
    setLocalValidationSelected(false);
  }, [gameId]);

  useEffect(() => {
    if (!eligibleSession || !status.discoveryAvailable) {
      setTrialStatus(null);
      return;
    }
    let disposed = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const next = await invoke<Cp2077TrialStatus>("cp2077_trial_get_status", { gameId });
        if (!disposed) setTrialStatus(next);
      } catch (reason) {
        if (!disposed) console.error("[depth-lab] CP2077 trial status failed", reason);
      } finally {
        if (!disposed) timer = window.setTimeout(() => void poll(), 750);
      }
    };
    void poll();
    return () => {
      disposed = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [eligibleSession, gameId, status.discoveryAvailable]);

  function applyStatus(next: DepthProbeRuntimeStatus, version: number) {
    if (version !== requestVersion.current) return;
    setStatus(next);
    if (next.lastError && next.lastError !== lastLoggedError.current) {
      lastLoggedError.current = next.lastError;
      console.error("[depth-lab] backend fault", next.lastError, next.activeIdentity);
    }
  }

  useEffect(() => {
    if (!eligibleSession) {
      setFeatureAvailable(false);
      return;
    }
    let disposed = false;
    let timer: number | undefined;
    const poll = async (initial: boolean) => {
      if (commandInFlight.current) {
        timer = window.setTimeout(() => void poll(false), 250);
        return;
      }
      let keepPolling = true;
      const version = ++requestVersion.current;
      try {
        const next = await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status");
        if (disposed) return;
        setFeatureAvailable(true);
        setStatusReadError(null);
        applyStatus(next, version);
      } catch (reason) {
        if (disposed) return;
        if (initial) {
          // A dev build without the depth-lab Cargo feature has no panel.
          setFeatureAvailable(false);
          keepPolling = false;
        } else {
          const raw = String(reason);
          setStatusReadError(raw);
          console.error("[depth-lab] status query failed", raw);
        }
      } finally {
        if (!disposed && keepPolling) timer = window.setTimeout(() => void poll(false), 750);
      }
    };
    void poll(true);
    return () => {
      disposed = true;
      requestVersion.current += 1;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [eligibleSession]);

  useEffect(() => {
    if (!status.enabled || !status.earlyLaunchRequired || status.earlyLaunchGameId !== gameId) {
      setEarlyPreflightBusy(false);
      setEarlyPreflight(null);
      setEarlyPreflightError(null);
      return;
    }
    if (status.earlyLaunchState !== "awaiting_launch") {
      setEarlyPreflightBusy(false);
      return;
    }
    let disposed = false;
    let timer: number | undefined;
    setEarlyPreflight(null);
    setEarlyPreflightError(null);
    setEarlyPreflightBusy(true);
    const check = async () => {
      let ready = false;
      try {
        const result = await invoke<EarlyLaunchPreflight>("depth_lab_preflight_cp2077");
        ready = result.ready;
        if (!disposed) {
          setEarlyPreflight(result);
          setEarlyPreflightError(null);
        }
      } catch (reason) {
        if (!disposed) setEarlyPreflightError(String(reason));
      } finally {
        if (!disposed) {
          setEarlyPreflightBusy(false);
          if (!ready && status.earlyLaunchState === "awaiting_launch"
            && !depthControlLocked(status, collectorState)) {
            timer = window.setTimeout(() => void check(), 10_000);
          }
        }
      }
    };
    void check();
    return () => {
      disposed = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [gameId, collectorState, status.enabled, status.earlyLaunchRequired, status.earlyLaunchGameId, status.earlyLaunchState]);

  useEffect(() => {
    if (!featureAvailable || !status.discoveryAvailable) {
      setDiscoveryPreflight(null);
      return;
    }
    let disposed = false;
    setDiscoveryPreflightBusy(true);
    void invoke<EarlyLaunchPreflight>("depth_lab_preflight_cp2077_discovery", { gameId })
      .then((result) => {
        if (!disposed) setDiscoveryPreflight(result);
      })
      .catch((reason) => {
        if (!disposed) setCommandError(String(reason));
      })
      .finally(() => {
        if (!disposed) setDiscoveryPreflightBusy(false);
      });
    return () => {
      disposed = true;
    };
  }, [featureAvailable, gameId, status.discoveryAvailable]);

  useEffect(() => {
    if (!featureAvailable) return;
    void invoke<PerfCertificationStatus>("depth_lab_get_perf_certification")
      .then(setPerfStatus)
      .catch((reason) => console.error("[depth-lab] performance certification status failed", reason));
  }, [featureAvailable]);

  useEffect(() => {
    const configured = featureAvailable
      && status.enabled
      && !status.performanceValidationOnly
      && Boolean(gameId);
    if (!configured) {
      perfGateRequestVersion.current += 1;
      perfGateAutoCheckKey.current = null;
      setPerfGateStatus(null);
      setPerfGateError(null);
      setPerfGateBusy(false);
      return;
    }
    const key = gameId;
    if (perfGateAutoCheckKey.current && perfGateAutoCheckKey.current !== key) {
      perfGateRequestVersion.current += 1;
      perfGateAutoCheckKey.current = null;
      setPerfGateStatus(null);
      setPerfGateError(null);
    }
    // 录制状态变化时保留最后一次结论；正式 StartRun 仍会再次执行同源门禁。
    if (!PERF_GATE_CHECK_COLLECTOR_STATES.has(collectorState)) return;
    if (perfGateAutoCheckKey.current === key) return;
    perfGateAutoCheckKey.current = key;
    const version = ++perfGateRequestVersion.current;
    setPerfGateStatus(null);
    setPerfGateError(null);
    setPerfGateBusy(true);
    void invoke<PerfCertificationGateStatus>("depth_lab_check_performance_certification", { gameId })
      .then((result) => {
        if (version === perfGateRequestVersion.current) setPerfGateStatus(result);
      })
      .catch((reason) => {
        if (version === perfGateRequestVersion.current) setPerfGateError(String(reason));
      })
      .finally(() => {
        if (version === perfGateRequestVersion.current) setPerfGateBusy(false);
      });
  }, [collectorState, featureAvailable, gameId, status.enabled, status.performanceValidationOnly]);

  useEffect(() => {
    onPerfCertificationChange?.(
      eligibleSession
        && featureAvailable
        && (Boolean(perfStatus?.active)
          || (Boolean(perfStatus?.certificationId) && !perfStatus?.nextStage && collectorState === "paused")),
    );
  }, [
    collectorState,
    eligibleSession,
    featureAvailable,
    onPerfCertificationChange,
    perfStatus?.active,
    perfStatus?.certificationId,
    perfStatus?.nextStage,
  ]);

  useEffect(() => {
    if (!perfStageRunning || !capture || !perfStatus?.nextStage) return;
    if (perfRunId.current !== capture.runId) return;
    perfRunObserved.current = true;
    if (
      status.lifecycle !== "ready"
      || status.activeIdentity?.run_id !== capture.runId
    ) return;
    perfStageReadyElapsedMs.current ??= capture.elapsedMs;
    const stageElapsedMs = capture.elapsedMs - perfStageReadyElapsedMs.current;
    if (stageElapsedMs >= PERF_STAGE_READY_WINDOW_MS && !perfFinalizing.current) {
      void finishPerfStage(perfStatus.nextStage);
    }
  }, [capture, perfStageRunning, perfStatus, status.activeIdentity?.run_id, status.lifecycle]);

  useEffect(() => {
    if (
      !perfStageRunning
      || !perfRunObserved.current
      || perfFinalizing.current
      || collectorState !== "paused"
    ) return;
    perfRunId.current = null;
    perfRunObserved.current = false;
    perfStageReadyElapsedMs.current = null;
    setPerfStageRunning(false);
    setPerfMessage("本档提前暂停，未写入认证结果。请从认证卡片重跑当前档。");
  }, [collectorState, perfStageRunning]);

  const presentation = presentDepthStatus(status, collectorState, statusReadError);
  const perfOutcomePresentation = perfStatus?.outcome
    ? presentPerfRunOutcome(perfStatus.outcome)
    : null;
  const perfGatePresentation = presentPerfCertificationGate(
    perfGateStatus,
    perfGateBusy,
    perfGateError,
  );
  const discoveryPresentation = presentDiscoverySmoke(status.discoveryState);
  const locked = commandBusy || depthControlLocked(status, collectorState);
  const operatorMessage = localBlock
    ?? (commandError || statusReadError
      ? annotatorMessage(commandError ?? statusReadError)
      : presentation.detail);
  const diagnostic = useMemo(
    () => formatDepthDiagnostic(
      status,
      collectorState,
      commandError ?? statusReadError,
      undefined,
      perfGateError,
      perfStatus,
    ),
    [collectorState, commandError, perfGateError, perfStatus, status, statusReadError],
  );
  const perfStageLabel = perfStageName(perfStatus?.nextStage);
  const perfAction = perfCertificationAction(perfStatus, collectorState);
  const canStartPerfStage = perfAction === "export"
    ? PERF_GATE_CHECK_COLLECTOR_STATES.has(collectorState)
    : perfStatus?.active
      ? collectorState === "paused"
        || (perfStatus.nextStage === "baseline" && collectorState === "bound")
      : collectorState === "bound";
  const trialPrepareBlocker = trialStatus
    ? cp2077TrialPrepareBlocker(collectorState, trialStatus.phase)
    : "CP2077 五流实验状态尚未就绪。";
  const currentMetricEvidenceSealed = Boolean(
    trialStatus?.metricEvidenceSealed
      && trialStatus.sessionLocator
      && trialStatus.sessionLocator === trialStatus.metricEvidenceSessionLocator,
  );

  if (!eligibleSession || (!featureAvailable && !trialStatus?.available)) return null;

  function updateSetting<K extends keyof DepthProbeSettings>(key: K, value: DepthProbeSettings[K]) {
    setSettings((current) => ({ ...current, [key]: value }));
    setLocalBlock(null);
  }

  async function submitToggle(enabled: boolean, riskAcknowledged: boolean) {
    const request = depthToggleSettings(
      settings, status, gameId, enabled, riskAcknowledged, localValidationSelected,
    );
    const validationError = validateDepthSettings(request);
    if (validationError) {
      setLocalBlock(validationError);
      setConfigOpen(true);
      return;
    }

    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    setLocalBlock(null);
    const version = ++requestVersion.current;
    const result = await configureDepthToggle(
      status,
      request,
      (next) => invoke<DepthProbeRuntimeStatus>("depth_lab_configure", { request: next, gameId }),
      () => invoke<DepthProbeRuntimeStatus>("depth_lab_get_status"),
    );
    applyStatus(result.status, version);
    if (result.rawError) {
      setCommandError(result.rawError);
      console.error("[depth-lab] configure failed", result.rawError);
    } else {
      setSettings(request);
      if (!enabled) setRiskChecked(false);
    }
    commandInFlight.current = false;
    setCommandBusy(false);
  }

  async function refreshPerfGate() {
    if (
      !status.enabled
      || status.performanceValidationOnly
      || !PERF_GATE_CHECK_COLLECTOR_STATES.has(collectorState)
    ) return;
    const version = ++perfGateRequestVersion.current;
    setPerfGateBusy(true);
    setPerfGateError(null);
    try {
      const result = await invoke<PerfCertificationGateStatus>(
        "depth_lab_check_performance_certification",
        { gameId },
      );
      if (version !== perfGateRequestVersion.current) return;
      setPerfGateStatus(result);
      perfGateAutoCheckKey.current = gameId;
    } catch (reason) {
      if (version === perfGateRequestVersion.current) setPerfGateError(String(reason));
    } finally {
      if (version === perfGateRequestVersion.current) setPerfGateBusy(false);
    }
  }

  function requestToggle() {
    if (status.enabled) {
      void submitToggle(false, false);
      return;
    }
    if (depthEnableIntent(status.riskAcknowledged) === "confirm_risk") {
      setRiskChecked(false);
      setRiskPurpose("formal");
      setRiskOpen(true);
      return;
    }
    void submitToggle(true, true);
  }

  function confirmRisk() {
    if (!riskChecked) return;
    setRiskOpen(false);
    if (riskPurpose === "discovery") {
      void startDiscovery(true);
    } else if (riskPurpose === "certification") {
      void beginPerfCertification();
    } else if (riskPurpose === "trial" || riskPurpose === "metric_calibration") {
      void prepareCp2077Trial(
        true,
        riskPurpose === "metric_calibration" ? "metric_calibration" : "five_stream",
      );
    } else {
      void submitToggle(true, true);
    }
  }

  function requestPerfCertification() {
    if (perfAction === "export") {
      void retryPerfExport();
      return;
    }
    if (perfAction === "stage" && perfStatus) {
      void startPerfStage(perfStatus);
      return;
    }
    setRiskChecked(false);
    setRiskPurpose("certification");
    setRiskOpen(true);
  }

  async function beginPerfCertification() {
    if (collectorState !== "bound" || !gameId || !targetFps) {
      setLocalBlock("请先选择游戏并绑定窗口，再开始性能认证。");
      return;
    }
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    setLocalBlock(null);
    try {
      const next = await invoke<PerfCertificationStatus>("depth_lab_begin_perf_certification");
      setPerfStatus(next);
      setPerfStageRetryRequired(false);
      await startPerfStage(next);
    } catch (reason) {
      setCommandError(String(reason));
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function startPerfStage(current: PerfCertificationStatus) {
    const stage = current.nextStage;
    if (!stage || !current.certificationId) return;
    if (stage === "baseline" && !["bound", "paused"].includes(collectorState)) {
      setLocalBlock("基线档只能从已绑定状态启动，或在提前暂停后重跑。");
      return;
    }
    if (stage !== "baseline" && collectorState !== "paused") {
      setLocalBlock("请等待上一档完全暂停，再继续下一档。");
      return;
    }
    const request: DepthProbeSettings = {
      ...settings,
      enabled: true,
      riskAcknowledged: true,
      plannedSeconds: Math.max(settings.plannedSeconds, 315),
      performanceValidationOnly: true,
      performanceMetricsEnabled: true,
    };
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    try {
      const nextDepth = await invoke<DepthProbeRuntimeStatus>("depth_lab_configure", {
        request,
        gameId,
      });
      applyStatus(nextDepth, ++requestVersion.current);
      setSettings(request);
      const requestId = `depth-perf-${stage}-${crypto.randomUUID()}`;
      const started = await invoke<CollectorSnapshot>("depth_lab_start_perf_stage", {
        request: { requestId },
      });
      // This command starts the collector outside main.tsx's normal start handler. Feed the
      // authoritative returned snapshot back to the owner immediately instead of waiting for
      // an event/poll to eventually replace the stale Bound view.
      onCollectorSnapshot?.(started);
      if (!started.capture) throw new Error("性能认证档启动后没有取得新 run 身份。");
      if (!["preflighting", "starting", "recording"].includes(started.state)) {
        throw new Error(`性能认证档没有进入启动链路，collector 返回 ${started.state}。`);
      }
      perfRunId.current = started.capture.runId;
      perfRunObserved.current = false;
      perfStageReadyElapsedMs.current = null;
      setPerfStageRetryRequired(false);
      setPerfStageRunning(true);
      setPerfMessage(`${perfStageName(stage)}档正在等待 probe Ready；Ready 后预热 5 秒、测量 5 分钟，再留 5 秒收尾保护区后自动暂停。`);
    } catch (reason) {
      perfRunId.current = null;
      perfRunObserved.current = false;
      perfStageReadyElapsedMs.current = null;
      setPerfStageRunning(false);
      setPerfMessage(`${perfStageName(stage)}档未能启动，请修复下方错误后重跑本档。`);
      setCommandError(String(reason));
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function waitForPerfStageFlush(expectedRunId: string): Promise<void> {
    const deadline = Date.now() + Math.max(settings.stopTimeoutMs, 500) + 5_000;
    while (Date.now() <= deadline) {
      const next = await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status");
      applyStatus(next, ++requestVersion.current);
      const flushState = perfStageFlushState(next, expectedRunId);
      if (flushState === "ready") return;
      if (flushState === "identity_mismatch") {
        throw new Error(`性能认证 run 身份变化：等待 ${expectedRunId}，实际 ${next.activeIdentity?.run_id ?? "无"}`);
      }
      if (flushState === "faulted") {
        throw new Error(next.lastError ?? "深度探针在收口时进入 Faulted");
      }
      await new Promise<void>((resolve) => window.setTimeout(resolve, 250));
    }
    throw new Error(`等待 ${expectedRunId} 的 probe Flushed 超时`);
  }

  async function finishPerfStage(stage: PerfStage) {
    if (perfFinalizing.current || !perfStatus?.certificationId || !targetFps) return;
    perfFinalizing.current = true;
    setCommandBusy(true);
    commandInFlight.current = true;
    try {
      const completingRunId = perfRunId.current;
      const pausing = await invoke<CollectorSnapshot>("collector_pause", {
        request: { reason: `depth_perf_${stage}` },
      });
      onCollectorSnapshot?.(pausing);
      if (!completingRunId) throw new Error("性能认证档缺少当前 run 身份，不能登记结果。");
      setPerfMessage(`${perfStageName(stage)}档已停止，正在等待深度探针完整封口…`);
      await waitForPerfStageFlush(completingRunId);
      const next = await invoke<PerfCertificationStatus>("depth_lab_record_perf_stage", {
        request: {
          certificationId: perfStatus.certificationId,
          stage,
          targetFps,
        },
      });
      perfRunId.current = null;
      perfRunObserved.current = false;
      perfStageReadyElapsedMs.current = null;
      setPerfStageRetryRequired(false);
      setPerfStatus(next);
      if (next.outcome) {
        setSettings((current) => ({
          ...current,
          enabled: false,
          riskAcknowledged: false,
          performanceValidationOnly: false,
          performanceMetricsEnabled: false,
        }));
        applyStatus(
          await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status"),
          ++requestVersion.current,
        );
      }
      setPerfStageRunning(false);
      setPerfMessage(next.outcome
        ? next.outcome.passes
          ? `四档测量已达标并导出候选证据${next.candidatePath ? `：${next.candidatePath}` : ""}。性能门仍关闭；需审核并发布签名中央清单后才会生效。`
          : `四档测量未达标；A/B 归因和候选证据已保留${next.candidatePath ? `：${next.candidatePath}` : ""}，性能门保持关闭。请按归因结果调整后重新测量。`
        : "本档已完成。准备好后继续下一档。");
    } catch (reason) {
      perfRunId.current = null;
      perfRunObserved.current = false;
      perfStageReadyElapsedMs.current = null;
      setPerfStageRunning(false);
      setCommandError(String(reason));
      // The fourth measurement may already be committed. Never infer retry-stage
      // from a failed finalization response; fetch the backend-owned next action.
      try {
        const current = await invoke<PerfCertificationStatus>("depth_lab_get_perf_certification");
        setPerfStatus(current);
        setPerfStageRetryRequired(Boolean(current.nextStage));
        setPerfMessage(current.nextStage
          ? `${perfStageName(stage)}档未登记，请查看错误后重跑本档。`
          : current.candidatePath
            ? `本轮结果已导出：${current.candidatePath}。请使用主界面“结束会话”。`
            : current.outcome
              ? "四档测量结果已保留，候选文件尚未导出。修复错误后可重试导出，无需重录。"
              : "四档采集已结束，但本轮结果无效。请保留诊断，使用主界面“结束会话”后重新绑定，从 baseline 开始新一轮。");
      } catch (refreshError) {
        setPerfStatus(null);
        setPerfStageRetryRequired(false);
        setPerfMessage(`无法确认认证阶段，请保留诊断后结束会话，勿重复启动第四档：${String(refreshError)}`);
      }
    } finally {
      perfFinalizing.current = false;
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function retryPerfExport() {
    if (!perfStatus?.certificationId || !targetFps || commandInFlight.current) return;
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    try {
      const next = await invoke<PerfCertificationStatus>("depth_lab_record_perf_stage", {
        request: { certificationId: perfStatus.certificationId, stage: "full", targetFps },
      });
      setPerfStatus(next);
      setSettings((current) => ({
        ...current,
        enabled: false,
        riskAcknowledged: false,
        performanceValidationOnly: false,
        performanceMetricsEnabled: false,
      }));
      applyStatus(await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status"), ++requestVersion.current);
      setPerfMessage(`候选证据已导出：${next.candidatePath}。请使用主界面“结束会话”。`);
    } catch (reason) {
      setCommandError(String(reason));
      setPerfMessage("候选导出仍未成功，已有四档测量保留；请检查错误后重试导出。");
      try {
        setPerfStatus(await invoke<PerfCertificationStatus>("depth_lab_get_perf_certification"));
      } catch { /* Keep the last known completed measurement; never start another run here. */ }
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function cancelPerfCertification() {
    setCommandBusy(true);
    try {
      setPerfStatus(await invoke<PerfCertificationStatus>("depth_lab_cancel_perf_certification"));
      perfRunId.current = null;
      perfRunObserved.current = false;
      perfStageReadyElapsedMs.current = null;
      setPerfStageRetryRequired(false);
      setPerfStageRunning(false);
      setPerfMessage("本轮性能候选采集已取消，没有写入候选证据。");
    } catch (reason) {
      setCommandError(String(reason));
    } finally {
      setCommandBusy(false);
    }
  }

  async function copyDiagnostic() {
    try {
      await navigator.clipboard.writeText(diagnostic);
      setCopyResult("诊断已复制");
    } catch (reason) {
      setCopyResult("复制失败，请展开后手动复制");
      console.error("[depth-lab] diagnostic copy failed", reason);
    }
  }

  async function launchConfiguredGame() {
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    try {
      await invoke("depth_lab_launch_cp2077");
      const next = await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status");
      applyStatus(next, ++requestVersion.current);
    } catch (reason) {
      const raw = String(reason);
      setCommandError(raw);
      console.error("[depth-lab] configured game early launch failed", raw);
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function refreshDiscoveryPreflight() {
    setDiscoveryPreflightBusy(true);
    setCommandError(null);
    try {
      setDiscoveryPreflight(await invoke<EarlyLaunchPreflight>("depth_lab_preflight_cp2077_discovery"));
    } catch (reason) {
      setCommandError(String(reason));
    } finally {
      setDiscoveryPreflightBusy(false);
    }
  }

  function requestDiscoveryStart() {
    setRiskChecked(false);
    setRiskPurpose("discovery");
    setRiskOpen(true);
  }

  async function startDiscovery(riskAcknowledged: boolean) {
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    try {
      await invoke("depth_lab_start_cp2077_discovery", {
        request: { riskAcknowledged, gameId },
      });
      applyStatus(
        await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status"),
        ++requestVersion.current,
      );
    } catch (reason) {
      const raw = String(reason);
      setCommandError(raw);
      console.error("[depth-lab] CP2077 discovery start failed", raw);
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  async function stopDiscovery() {
    setCommandBusy(true);
    commandInFlight.current = true;
    setCommandError(null);
    try {
      await invoke("depth_lab_stop_cp2077_discovery");
      applyStatus(
        await invoke<DepthProbeRuntimeStatus>("depth_lab_get_status"),
        ++requestVersion.current,
      );
    } catch (reason) {
      const raw = String(reason);
      setCommandError(raw);
      console.error("[depth-lab] CP2077 discovery seal failed", raw);
    } finally {
      commandInFlight.current = false;
      setCommandBusy(false);
    }
  }

  function requestCp2077TrialPrepare(purpose: "five_stream" | "metric_calibration") {
    if (trialPrepareBlocker) {
      setLocalBlock(trialPrepareBlocker);
      return;
    }
    setRiskChecked(false);
    setRiskPurpose(purpose === "metric_calibration" ? "metric_calibration" : "trial");
    setRiskOpen(true);
  }

  async function prepareCp2077Trial(
    riskAcknowledged: boolean,
    purpose: "five_stream" | "metric_calibration",
  ) {
    setTrialBusy(true);
    setCommandError(null);
    setLocalBlock(null);
    try {
      setTrialStatus(await invoke<Cp2077TrialStatus>("cp2077_trial_prepare_and_launch", {
        request: { riskAcknowledged, purpose },
      }));
    } catch (reason) {
      const raw = String(reason);
      setCommandError(raw);
      console.error("[depth-lab] CP2077 trial preparation failed", raw);
    } finally {
      setTrialBusy(false);
    }
  }

  async function openCp2077TrialOutput() {
    try {
      await invoke("cp2077_trial_open_output");
    } catch (reason) {
      setCommandError(String(reason));
    }
  }

  const identity = status.activeIdentity;
  const inputsLocked = locked || status.enabled;

  return (
    <>
      {trialStatus?.available && (
        <section className="depth-lab cp2077-trial-launcher" aria-label="CP2077 本地深度采集">
          <div className="depth-block cp2077-trial">
            <strong>CP2077 主采集器 · {trialStatus.purpose === "metric_calibration" ? "米制标定" : trialStatus.purpose === "five_stream" ? "五流模式" : trialStatus.metricEvidenceSealed ? "米制证据已封存" : "待准备"}</strong>
            <span>
              阶段：{trialStatus.phase.toUpperCase()}
              {trialStatus.processId ? ` · PID ${trialStatus.processId}` : ""}
              {trialStatus.profileCandidate ? ` · ${trialStatus.profileCandidate}` : ""}
            </span>
            <span>RGB + 真实 D3D12 Depth + Raw Input + Player Pose + Render Camera；本地不可训练。</span>
            {trialStatus.metricEvidenceSealed && (
              <span className="text-good">米制证据已封存并通过完整性校验：1 世界单位 = {trialStatus.metricEvidenceMetresPerWorldUnit ?? "—"} 米。</span>
            )}
            {trialStatus.purpose === "metric_calibration" && (
              <span>沿已选直线步行，看到清楚的整数米距离时按 Ctrl+Alt+F9，共打 5 个点；首尾 HUD 距离至少差 20 米。</span>
            )}
            {trialStatus.phase === "preparing" && <span>正在核对权限、运行保护证据和本机工具，并启动唯一的早期注入进程。</span>}
            {trialStatus.phase === "prepared" && <span>Depth 探针已随游戏早期启动。请扫描并绑定这个 CP2077 窗口，然后使用页面底部主采集器的“开始录制 / 结束会话”。{trialStatus.purpose === "metric_calibration" ? "开录后先在起点按一次 Ctrl+Alt+F9。" : ""}</span>}
            {trialStatus.phase === "recording" && (
              <span>主采集器正在同步采集 {trialStatus.recordingElapsedSeconds} 秒：RGB、Raw Input、Depth、Player Pose 与 Render Camera 共用同一个 session/run。{trialStatus.purpose === "metric_calibration" ? `每到一个新的整数米读数按一次 Ctrl+Alt+F9；已打点 ${trialStatus.metricMarkerCount}/5。` : ""}</span>
            )}
            {trialStatus.phase === "processing" && <span>正在排空视频与 GPU 队列，并生成 QC 和{trialStatus.purpose === "metric_calibration" ? "标定截图/位姿草稿" : "本地五件套"}，请勿关闭客户端。</span>}
            {trialStatus.phase === "completed" && (
              <span className="text-good">已完成：RGB {trialStatus.videoFrames ?? "—"} 帧，Depth {trialStatus.depthFrames ?? "—"} 帧{trialStatus.purpose === "metric_calibration" ? `，标定点 ${trialStatus.metricCalibrationSamples ?? "—"} 个（${currentMetricEvidenceSealed ? "米制证据已封存" : "待第二人读 HUD"}）` : `，键盘 ${trialStatus.keyboardInputEvents ?? "—"} 条，鼠标 ${trialStatus.mouseInputEvents ?? "—"} 条`}。</span>
            )}
            {trialStatus.sessionLocator && <small>本机产物：{trialStatus.sessionLocator}</small>}
            {!trialStatus.sessionLocator && trialStatus.metricEvidenceSessionLocator && <small>米制证据：{trialStatus.metricEvidenceSessionLocator}</small>}
            {trialStatus.deliveryLocator && <small>五件套：{trialStatus.deliveryLocator}</small>}
            {trialStatus.lastError && <span>{trialStatus.lastError}</span>}
            {trialPrepareBlocker && !["preparing", "prepared", "recording", "processing"].includes(trialStatus.phase) && (
              <small>{trialPrepareBlocker}</small>
            )}
            <div className="depth-lab-actions">
              <button type="button" className="button primary" disabled={trialBusy || trialPrepareBlocker !== null} onClick={() => requestCp2077TrialPrepare("metric_calibration")}>
                {trialBusy ? "正在准备…" : "准备米制标定"}
              </button>
              <button type="button" className="button secondary" disabled={trialBusy || trialPrepareBlocker !== null} onClick={() => requestCp2077TrialPrepare("five_stream")}>
                准备五流采集
              </button>
              <button type="button" className="button secondary" disabled={(!trialStatus.sessionLocator && !trialStatus.metricEvidenceSessionLocator) || trialStatus.phase === "recording" || trialStatus.phase === "processing"} onClick={() => void openCp2077TrialOutput()}>
                {trialStatus.deliveryLocator ? "打开五件套目录" : !trialStatus.sessionLocator && trialStatus.metricEvidenceSessionLocator ? "打开证据目录" : "打开产物目录"}
              </button>
            </div>
          </div>
        </section>
      )}
      <section className={`depth-lab depth-${presentation.phase}`} aria-labelledby="depth-lab-title">
        <header className="depth-lab-head">
          <div className="depth-lab-title">
            <span className="section-kicker cyan">DEPTH · EXPERIMENTAL</span>
            <div>
              <h2 id="depth-lab-title">深度采集</h2>
              <span className={`depth-state depth-state-${presentation.phase}`}>{presentation.label}</span>
            </div>
            <p>{presentation.title}。{operatorMessage}</p>
          </div>
          <button
            type="button"
            role="switch"
            aria-checked={status.enabled}
            aria-label="深度采集手动开关"
            className={`depth-switch${status.enabled ? " enabled" : ""}`}
            disabled={locked}
            onClick={requestToggle}
          >
            <span />
            <strong>{commandBusy ? "提交中" : status.enabled ? "已打开" : "已关闭"}</strong>
          </button>
        </header>

        {cp2077LocalValidationAvailable(status, gameId) && (
          <div className="depth-block" aria-label="CP2077 常规客户端短测">
            <label>
              <input
                type="checkbox"
                checked={localValidationSelected}
                disabled={inputsLocked || Boolean(perfStatus?.active)}
                onChange={(event) => setLocalValidationSelected(event.target.checked)}
              />
              <span>本地短测（约 30 秒）</span>
            </label>
            <span>勾选后开启深度采集，从下方启动游戏并绑定窗口，再使用主界面的“开始录制”。</span>
            <span>空手、FOV 75，简单前进后退即可。约 30 秒后按 Ctrl+Alt+F10 结束；不会自动停止。</span>
            <small>仅检查本地采集链路，数据不可交付、不可训练，不生成性能认证。</small>
          </div>
        )}

        <div className="depth-lab-facts" aria-label="深度采集运行状态">
          <div><span>生命周期</span><strong>{status.lifecycle.toUpperCase()}</strong></div>
          <div><span>段尾完整</span><strong className={presentation.segmentComplete ? "text-good" : ""}>{presentation.segmentComplete ? "已确认" : "未确认"}</strong></div>
          <div>
            <span>{status.enabled ? "运行用途" : "下次运行用途"}</span>
            <strong>{status.performanceValidationOnly ? perfStatus?.active ? "性能测量" : "本地验证" : "正式门控"}</strong>
            {status.performanceMetricsEnabled && <span>A3 分项计时已启用</span>}
          </div>
          <div><span>Generation</span><strong>{identity ? identity.generation : "—"}</strong></div>
          <div><span>Run</span><strong title={identity?.run_id}>{identity?.run_id ?? "尚未创建"}</strong></div>
        </div>

        <DepthGameLaunchCard gameId={gameId} status={status} preflight={earlyPreflight} error={earlyPreflightError}
          checking={earlyPreflightBusy} locked={locked} onLaunch={() => void launchConfiguredGame()} />

        {!status.performanceValidationOnly && (
          <div
            className={`depth-block depth-certification-gate ${perfGatePresentation.tone}`}
            role={perfGatePresentation.tone === "danger" ? "alert" : "status"}
            aria-label="当前机器 Depth 性能认证状态"
          >
            <strong>性能认证</strong>
            <span className="depth-certification-verdict">{perfGatePresentation.label}</span>
            <span>{perfGatePresentation.detail}</span>
            {perfGateStatus?.state === "passed" && <small>{perfGateStatus.detail}</small>}
            <button
              type="button"
              className="button secondary"
              disabled={
                !status.enabled
                || perfGateBusy
                || !PERF_GATE_CHECK_COLLECTOR_STATES.has(collectorState)
              }
              onClick={() => void refreshPerfGate()}
            >
              {perfGateBusy ? "正在检查…" : perfGateStatus ? "重新检查" : "检查认证"}
            </button>
          </div>
        )}

        {status.discoveryAvailable && discoveryPreflight?.eligible && (
          <div className="depth-block" role="status" aria-label="CP2077 discovery-only smoke">
            <strong>CP2077 Discovery-only Smoke</strong>
            <span>
              {discoveryPresentation.label}
              {status.discoveryPid ? ` · PID ${status.discoveryPid}` : ""}
            </span>
            <span><strong>Recording Ready: NO</strong>。发现报告永远不可直接用于录制或上传。</span>
            {status.discoveryOutputLocator && <small>本机证据：{status.discoveryOutputLocator}</small>}
            {status.discoveryLastError && <span>{status.discoveryLastError}</span>}
            <div className="depth-lab-actions">
              <button
                type="button"
                className="button primary"
                disabled={
                  commandBusy
                  || !discoveryPreflight.ready
                  || !discoveryPresentation.canStart
                }
                onClick={requestDiscoveryStart}
              >
                启动发现模式
              </button>
              <button
                type="button"
                className="button secondary"
                disabled={
                  commandBusy
                  || (!discoveryPresentation.canSeal
                    && !(status.discoveryState === "faulted" && status.discoveryPid !== null))
                }
                onClick={() => void stopDiscovery()}
              >
                停止并封存报告
              </button>
              <button
                type="button"
                className="button secondary"
                disabled={commandBusy || discoveryPreflightBusy}
                onClick={() => void refreshDiscoveryPreflight()}
              >
                {discoveryPreflightBusy ? "正在检查…" : "重新检查发现环境"}
              </button>
            </div>
            {!discoveryPreflight.ready && <span>{discoveryPreflight.blockers.join("；")}</span>}
          </div>
        )}

        <div className="depth-block" role="status" aria-label="深度性能认证候选采集">
          <strong>性能认证候选采集</strong>
          <span>在指定低配基准机上四档各运行至少 5 分钟；第三档保留 GPU 回读但在 writer 前丢弃，用来拆分编码/落盘开销。这里只导出待审核证据，不会直接放行本机。</span>
          {perfStatus?.active && perfStatus.nextStage && <span>下一档：{perfStageLabel}</span>}
          {perfStatus?.completionError && <span className="text-danger">{perfStatus.completionError}</span>}
          {perfMessage && <span>{perfMessage}</span>}
          {perfOutcomePresentation && (
            <div className="depth-perf-results" aria-label="性能认证结果">
              <span className={perfOutcomePresentation.avgFps.passes ? "text-good" : "text-danger"}>
                {perfOutcomePresentation.avgFps.message}
              </span>
              <span className={perfOutcomePresentation.low1Fps.passes ? "text-good" : "text-danger"}>
                {perfOutcomePresentation.low1Fps.message}
              </span>
              <span className={perfOutcomePresentation.passes ? "text-good" : "text-danger"}>
                {perfOutcomePresentation.conclusion}
              </span>
              {perfOutcomePresentation.attribution.map((message) => (
                <span key={message}>{message}</span>
              ))}
            </div>
          )}
          <div className="depth-lab-actions">
            {perfAction === "finish_session" ? (
              <span>本轮采集已结束。请使用主界面“结束会话”；需要重测时，重新绑定窗口后从 baseline 开始新一轮。</span>
            ) : <button
              type="button"
              className="button primary"
              disabled={commandBusy || perfStageRunning || !canStartPerfStage}
              onClick={requestPerfCertification}
            >
              {perfAction === "export" ? "重试导出本轮结果" : perfStatus?.active
                ? perfStageRunning
                  ? `正在运行：${perfStageLabel}`
                  : `${perfStageRetryRequired ? "重跑" : "继续"}：${perfStageLabel}`
                : "开始四档性能测量与 writer A/B（约 25 分钟）"}
            </button>}
            {perfStatus?.active && !perfStageRunning && (
              <button
                type="button"
                className="button secondary"
                disabled={commandBusy}
                onClick={() => void cancelPerfCertification()}
              >
                取消本轮
              </button>
            )}
          </div>
        </div>

        {(localBlock || commandError || statusReadError || status.lastError) && (
          <div className="depth-block" role="alert">
            <strong>当前未放行</strong>
            <span>{operatorMessage}</span>
          </div>
        )}

        <div className="depth-lab-actions">
          <button
            type="button"
            className="button secondary"
            aria-expanded={configOpen}
            onClick={() => setConfigOpen((open) => !open)}
          >
            {configOpen ? "收起配置" : "配置与门控"}
          </button>
          <button
            type="button"
            className="link-button quiet"
            aria-expanded={diagnosticOpen}
            onClick={() => setDiagnosticOpen((open) => !open)}
          >
            {diagnosticOpen ? "收起诊断" : "查看诊断"}
          </button>
          {locked && <small>当前 run 未结束，等待 Flushed 后才能修改开关。</small>}
        </div>

        {configOpen && (
          <div className="depth-config">
            <p>Depth Profile 与 Camera Binding 由当前游戏的已校验 catalog 自动加载。</p>
            <label className="depth-field">
              <span>计划时长（秒）</span>
              <input
                type="number"
                min={1}
                step={1}
                disabled={inputsLocked}
                value={settings.plannedSeconds}
                onChange={(event) => updateSetting("plannedSeconds", Number(event.target.value))}
              />
            </label>
            <label className="depth-field">
              <span>采样回退位置</span>
              <input
                type="number"
                min={0}
                step={1}
                disabled={inputsLocked}
                value={settings.unbindFromEnd}
                onChange={(event) => updateSetting("unbindFromEnd", Number(event.target.value))}
              />
            </label>
            <label className="depth-field">
              <span>启动等待（毫秒）</span>
              <input
                type="number"
                min={DEPTH_START_TIMEOUT_MIN_MS}
                max={60_000}
                step={100}
                disabled={inputsLocked}
                value={settings.startTimeoutMs}
                onChange={(event) => updateSetting("startTimeoutMs", Number(event.target.value))}
              />
            </label>
            <label className="depth-field">
              <span>停止等待（毫秒）</span>
              <input
                type="number"
                min={500}
                max={60_000}
                step={100}
                disabled={inputsLocked}
                value={settings.stopTimeoutMs}
                onChange={(event) => updateSetting("stopTimeoutMs", Number(event.target.value))}
              />
            </label>
          </div>
        )}

        {diagnosticOpen && (
          <div className="depth-diagnostic">
            <div>
              <strong>工程诊断</strong>
              <button type="button" className="button secondary" onClick={() => void copyDiagnostic()}>
                复制诊断
              </button>
            </div>
            <pre>{diagnostic}</pre>
            {(earlyPreflight || earlyPreflightError) && <pre>{JSON.stringify({ startup: earlyPreflight, error: earlyPreflightError }, null, 2)}</pre>}
            {copyResult && <small role="status">{copyResult}</small>}
          </div>
        )}
      </section>

      {riskOpen && (
        <div className="dialog-backdrop" role="presentation">
          <section className="depth-risk-dialog" role="dialog" aria-modal="true" aria-labelledby="depth-risk-title">
            <span className="section-kicker">EXPERIMENTAL RISK</span>
            <h2 id="depth-risk-title">
              {riskPurpose === "discovery"
                ? "确认启动 CP2077 发现模式？"
                : riskPurpose === "metric_calibration"
                  ? "确认准备 CP2077 米制标定？"
                  : riskPurpose === "trial"
                  ? "确认准备 CP2077 五流实验？"
                  : "确认开启实验性深度采集？"}
            </h2>
            <p>
              {riskPurpose === "trial" || riskPurpose === "metric_calibration"
                ? "该功能会在权利闸与运行保护硬门通过后向游戏进程注入本地采集组件，可能造成游戏异常、性能波动或触发账号风控。"
                : "该功能会向已授权测试的游戏进程注入本地采集组件，可能造成游戏异常、性能波动或触发账号风控。"}
            </p>
            <p>
              {riskPurpose === "discovery"
                ? "本次只收集 D3D12 / pose 发现证据，Recording Ready 始终为 NO；请勿开始主录制。"
                : riskPurpose === "trial" || riskPurpose === "metric_calibration"
                  ? "启动前会再次核对仓库内置的权利批准记录与运行保护证据；任一门未通过都不会注入。"
                  : "只用于 DEV / NON-TRAINABLE 采集；打开开关不代表已就绪，必须等到真实 Ready。"}
            </p>
            <label>
              <input
                type="checkbox"
                checked={riskChecked}
                onChange={(event) => setRiskChecked(event.target.checked)}
              />
              <span>
                {riskPurpose === "trial" || riskPurpose === "metric_calibration"
                  ? "我已理解注入与账号风险，并确认本次仅为单人、本地私有、不可训练且不交付的实验；游戏自身联网不授权 DataMiner 上传。"
                  : "我已理解风险，并确认当前游戏和账号允许本次实验采集。"}
              </span>
            </label>
            <div>
              <button type="button" className="button secondary" onClick={() => setRiskOpen(false)}>取消</button>
              <button type="button" className="button danger" disabled={!riskChecked} onClick={confirmRisk}>确认并继续</button>
            </div>
          </section>
        </div>
      )}
    </>
  );
}
