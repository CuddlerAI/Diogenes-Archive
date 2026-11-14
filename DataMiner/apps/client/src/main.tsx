import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { getCurrentWindow } from "@tauri-apps/api/window";
import React, { useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import Overlay from "./Overlay";
import type {
  CaptureStorageStatus,
  CatalogProvenance,
  CollectorFault,
  CollectorNotification,
  CollectorSnapshot,
  CollectorState,
  DataModelJobStatus,
  DiagnosticExportResult,
  GameSummary,
  SessionDeletionResult,
  SessionHistoryDetail,
  SessionHistorySummary,
  WindowCandidate,
} from "./collector-contract";
import { AccountStatusReadPlaceholder, AccountWorkspace } from "./Account";
import { DepthLabPanel } from "./DepthLabPanel";
import {
  depthPanelSessionContract,
  resolveDepthPanelRuntimeContext,
} from "./depth-lab-ui";
import type { DepthPanelRuntimeContext } from "./depth-lab-ui";
import { LogoMark, LogoMarkSmall } from "./brand";
import { BUILD_BADGE_TITLE, BUILD_BADGE_UPPER } from "./build-flavor";
import { admissionSuffix, isGameSelectable, onboardingReason } from "./game-admission";
import { catalogRosterKey, startCatalogGameRefresh } from "./catalog-games";
import {
  segmentUploadDisplayState,
  uploadActionLabel,
  uploadActionTitle,
  uploadSummaryState,
} from "./session-history-ui";
import {
  AccountIcon,
  CaptureIcon,
  HistoryIcon,
  NoTargetIcon,
  RefreshIcon,
  WindowIcon,
} from "./icons.generated";
import type { AccountStatus, ConsentNotice } from "./account-contract";
import { CONSENT_STATUS_EVENT, consentScopeLabel } from "./account-contract";
import {
  VIEW_SCALE_STATUS_EVENT,
  calibrationKindLabel,
  directionLabel,
  failReasonLabel,
  speedLabel,
} from "./view-scale-contract";
import type {
  ViewCalibrationKindName,
  ViewScaleMeasurement,
  ViewScaleStatus,
} from "./view-scale-contract";
import { UPDATE_STATUS_EVENT } from "./update-contract";
import type { UpdateStatusSnapshot } from "./update-contract";
import "./styles.css";

const ACTIVE_STATES = new Set<CollectorState>(["preflighting", "starting", "recording", "pausing", "stopping"]);

const STATE_COPY: Record<CollectorState, { label: string; detail: string }> = {
  idle: { label: "等待目标", detail: "选择一个受支持的游戏窗口" },
  discovering: { label: "正在扫描", detail: "正在核验可录制的窗口" },
  bound: { label: "采集就绪", detail: "窗口身份与采集规格已锁定" },
  preflighting: { label: "正在预检", detail: "正在核验目录、窗口、设备、编码器与存储事实" },
  starting: { label: "正在启动", detail: "正在建立 GPU 与输入时间轴" },
  recording: { label: "正在录制", detail: "画面与键鼠事件正在同步写入" },
  pausing: { label: "正在暂停", detail: "正在安全封口当前连续 run" },
  paused: { label: "已暂停", detail: "当前 run 已完成，可恢复新 run" },
  stopping: { label: "正在完成", detail: "正在校验并发布本地 artifact" },
  completed: { label: "采集完成", detail: "所有已完成分段均已原子发布" },
  faulted: { label: "采集已停止", detail: "检测到不可继续的本地条件" },
};

const STATE_KICKER: Record<CollectorState, string> = {
  idle: "IDLE",
  discovering: "DISCOVERING",
  bound: "READY",
  preflighting: "PREFLIGHTING",
  starting: "STARTING",
  recording: "RECORDING",
  pausing: "PAUSING",
  paused: "PAUSED",
  stopping: "STOPPING",
  completed: "COMPLETED",
  faulted: "FAULTED",
};

type UiErrorBoundaryProps = { children: React.ReactNode };
type UiErrorBoundaryState = { failed: boolean };

class UiErrorBoundary extends React.Component<UiErrorBoundaryProps, UiErrorBoundaryState> {
  state: UiErrorBoundaryState = { failed: false };

  static getDerivedStateFromError(): UiErrorBoundaryState {
    return { failed: true };
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error("DataMiner UI rendering failed", error, info.componentStack);
  }

  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <main className="fatal-ui" role="alert">
        <section className="fatal-ui-panel">
          <span className="section-kicker">UI RENDER FAILED</span>
          <h1>界面渲染失败</h1>
          <p>采集进程可能仍在运行。请重新载入界面；如果问题再次出现，请保留现场并导出诊断。</p>
          <div className="fatal-ui-actions">
            <button className="button primary" type="button" onClick={() => window.location.reload()}>
              重新载入
            </button>
            <button className="button secondary" type="button" onClick={() => void getCurrentWindow().close()}>
              关闭窗口
            </button>
          </div>
        </section>
      </main>
    );
  }
}

function App() {
  const [view, setView] = useState<"capture" | "history" | "account">("capture");
  const [account, setAccount] = useState<AccountStatus | null>(null);
  const [accountStatusReadFailed, setAccountStatusReadFailed] = useState(false);
  const [games, setGames] = useState<GameSummary[]>([]);
  const [selectedGame, setSelectedGame] = useState("");
  const [candidates, setCandidates] = useState<WindowCandidate[]>([]);
  const [loadedCatalogRosterKey, setLoadedCatalogRosterKey] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<CollectorSnapshot | null>(null);
  const [captureStorage, setCaptureStorage] = useState<CaptureStorageStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [perfCertificationActive, setPerfCertificationActive] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notification, setNotification] = useState<CollectorNotification | null>(null);
  const [consentNotice, setConsentNotice] = useState<ConsentNotice | null>(null);
  const [exportMessage, setExportMessage] = useState<string | null>(null);
  const [history, setHistory] = useState<SessionHistorySummary[]>([]);
  const [historyDetail, setHistoryDetail] = useState<SessionHistoryDetail | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionHistorySummary | null>(null);
  const [dataModel, setDataModel] = useState<DataModelJobStatus | null>(null);
  const [historyDataModel, setHistoryDataModel] = useState<DataModelJobStatus | null>(null);
  const [viewScale, setViewScale] = useState<ViewScaleStatus | null>(null);
  const selectedGameRef = useRef("");
  const viewScaleRevision = useRef(0);
  const currentSessionId = useRef<string | null>(null);
  const historySessionId = useRef<string | null>(null);
  const historyUploadFingerprint = useRef<string | null>(null);
  const dataModelQueryVersions = useRef(new Map<string, number>());
  const historyRequestId = useRef(0);
  const depthPanelRuntimeContextRef = useRef<DepthPanelRuntimeContext | null>(null);

  function applySelectedGame(gameId: string) {
    selectedGameRef.current = gameId;
    setSelectedGame(gameId);
  }

  function requestDataModelStatus(sessionId: string) {
    const queryVersion = (dataModelQueryVersions.current.get(sessionId) ?? 0) + 1;
    dataModelQueryVersions.current.set(sessionId, queryVersion);
    return invoke<DataModelJobStatus | null>("collector_get_data_model_status", {
      request: { sessionId },
    }).then((status) => ({ sessionId, queryVersion, status }));
  }

  function applyDataModelStatus(result: Awaited<ReturnType<typeof requestDataModelStatus>>) {
    if (dataModelQueryVersions.current.get(result.sessionId) !== result.queryVersion) return;
    if (currentSessionId.current === result.sessionId) setDataModel(result.status);
    if (historySessionId.current === result.sessionId) setHistoryDataModel(result.status);
  }

  useEffect(() => {
    let disposed = false;
    let pollSnapshot: number | undefined;
    let unlistenSnapshot: (() => void) | undefined;
    let unlistenNotification: (() => void) | undefined;
    let unlistenDataModel: (() => void) | undefined;
    const applySnapshot = (next: CollectorSnapshot) => {
      if (!disposed) {
        setSnapshot((current) => {
          if (!current || next.revision >= current.revision) {
            currentSessionId.current = next.capture?.sessionId ?? null;
            return next;
          }
          return current;
        });
      }
    };
    // docs/65 J-24：后端在首个 collector_get_snapshot 命令上代发启动期通知，
    // 所以监听注册必须先于首次 invoke 与轮询启动——listen 本身是一次 IPC，
    // 与 invoke 并发发起时没有顺序保证，先 await 才有 happens-after。
    Promise.all([
      listen<CollectorSnapshot>("collector://snapshot", (event) => {
        applySnapshot(event.payload);
      }),
      listen<CollectorNotification>("collector://notification", (event) => {
        if (!disposed) setNotification(event.payload);
      }),
      listen<DataModelJobStatus>("collector://data-model", (event) => {
        if (!disposed) {
          // Events are wake hints only. Reading the latest durable row prevents a
          // delayed pending/running payload from overwriting a newer terminal state.
          void requestDataModelStatus(event.payload.sessionId)
            .then((result) => {
              if (!disposed) applyDataModelStatus(result);
            })
            .catch(() => {});
        }
      }),
    ])
      .then(([stopSnapshot, stopNotification, stopDataModel]) => {
        if (disposed) {
          stopSnapshot();
          stopNotification();
          stopDataModel();
          return;
        }
        unlistenSnapshot = stopSnapshot;
        unlistenNotification = stopNotification;
        unlistenDataModel = stopDataModel;
        pollSnapshot = window.setInterval(() => {
          void invoke<CollectorSnapshot>("collector_get_snapshot")
            .then(applySnapshot)
            .catch(() => {
              // Commands surface actionable failures. Polling is only a safety net
              // for a missed event during WebView reload/startup.
            });
        }, 500);
        return invoke<CollectorSnapshot>("collector_get_snapshot").then((current) => {
          if (disposed) return;
          applySnapshot(current);
        });
      })
      .catch((reason) => setError(String(reason)));
    return () => {
      disposed = true;
      if (pollSnapshot !== undefined) window.clearInterval(pollSnapshot);
      unlistenSnapshot?.();
      unlistenNotification?.();
      unlistenDataModel?.();
    };
  }, []);

  const activeCatalogRosterKey = catalogRosterKey(snapshot?.catalog ?? null);
  useEffect(() => {
    if (activeCatalogRosterKey === null) {
      setGames([]);
      setCandidates([]);
      setSelectedGame("");
      setLoadedCatalogRosterKey(null);
      return;
    }
    const rosterKey = activeCatalogRosterKey;
    const refresh = startCatalogGameRefresh({
      selectedGameId: selectedGameRef.current,
      loadGames: () => invoke<GameSummary[]>("collector_list_games"),
      onBegin: () => {
        setGames([]);
        setCandidates([]);
        // Preserve selectedGameRef so a still-active choice can be restored.
        setSelectedGame("");
        setLoadedCatalogRosterKey(null);
      },
      onSuccess: (availableGames, reconciled) => {
        setGames(availableGames);
        applySelectedGame(reconciled.selectedGameId);
        setLoadedCatalogRosterKey(rosterKey);
        if (reconciled.retiredGameId) {
          setError(
            `活动目录已更新，游戏 ${reconciled.retiredGameId} 已不再可用。请重新选择活动游戏。`,
          );
        }
      },
      onFailure: (reason) => setError(String(reason)),
    });
    return refresh.cancel;
  }, [activeCatalogRosterKey]);

  useEffect(() => {
    const sessionId = snapshot?.capture?.sessionId;
    if (!sessionId) {
      setDataModel(null);
      return;
    }
    void requestDataModelStatus(sessionId)
      .then(applyDataModelStatus)
      .catch(() => {
        // The durable worker event refreshes this if the one-shot read races startup.
      });
  }, [snapshot?.capture?.sessionId]);

  useEffect(() => {
    if (snapshot?.rebindGameId) {
      applySelectedGame(snapshot.rebindGameId);
    }
  }, [snapshot?.rebindGameId]);

  useEffect(() => {
    void invoke<CaptureStorageStatus>("collector_get_capture_storage_status")
      .then(setCaptureStorage)
      .catch((reason) => setError(String(reason)));
  }, [snapshot?.state, snapshot?.capture?.sessionId]);

  // 先读本地状态（纯本地、无网络）让界面立刻有东西显示，再跟服务端对账。
  // 对账失败是常态之一（离线），此时状态停在 unknown 而不是被判成未登录。
  useEffect(() => {
    const applyViewScale = (status: ViewScaleStatus) => {
      if (status.revision <= viewScaleRevision.current) return;
      viewScaleRevision.current = status.revision;
      setViewScale(status);
    };
    let unlisten: (() => void) | undefined;
    void listen<ViewScaleStatus>(VIEW_SCALE_STATUS_EVENT, (event) => {
      applyViewScale(event.payload);
    }).then((stop) => {
      unlisten = stop;
    });
    // 轮询兜底之外还有实际职责：status() 顺带把探针队列排空进活动测量。
    const timer = window.setInterval(() => {
      void invoke<ViewScaleStatus>("view_scale_get_status")
        .then(applyViewScale)
        .catch(() => undefined);
    }, 500);
    return () => {
      unlisten?.();
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    void invoke<AccountStatus>("account_get_status")
      .then((status) => {
        setAccount(status);
        setAccountStatusReadFailed(false);
      })
      .catch(() => {
        // IPC 读取失败与「明确未配置」不是一回事；账号页保留独立失败态。
        setAccountStatusReadFailed(true);
      });
    void invoke<AccountStatus>("account_refresh_session")
      .then((status) => {
        setAccount(status);
        setAccountStatusReadFailed(false);
      })
      .catch(() => {
        // 离线或服务端不可达：保留上一步的本地状态。
      });
  }, []);

  // 条款变更提示（docs/108 §3.6）。快照由 Rust 侧的观察器维护，这里只渲染：
  // 首帧读一次已有快照（后台首检要 20 秒，界面不该空等），之后跟事件走。
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void invoke<ConsentNotice>("account_get_consent_notice").then(setConsentNotice).catch(() => {
      // 观察器不可用不影响采集，横幅整体不显示。
    });
    void listen<ConsentNotice>(CONSENT_STATUS_EVENT, (event) => setConsentNotice(event.payload))
      .then((dispose) => {
        unlisten = dispose;
      })
      .catch(() => {});
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    if (view !== "history") return;
    let disposed = false;
    let inFlight = false;
    const refreshUploadState = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const sessions = await invoke<SessionHistorySummary[]>("collector_list_session_history");
        const currentId = historySessionId.current;
        const selected = sessions.find((item) => item.sessionId === currentId) ?? sessions[0] ?? null;
        const fingerprint = selected ? uploadSummaryFingerprint(selected) : null;
        const detail = selected && fingerprint !== historyUploadFingerprint.current
          ? await invoke<SessionHistoryDetail>("collector_get_session_history_detail", {
              request: { sessionId: selected.sessionId },
            })
          : null;
        if (disposed || historySessionId.current !== currentId) return;
        historySessionId.current = selected?.sessionId ?? null;
        historyUploadFingerprint.current = fingerprint;
        setHistory(sessions);
        setHistoryDetail((current) => {
          if (!selected) return null;
          if (detail) return detail;
          return current?.session.sessionId === selected.sessionId
            ? { ...current, session: selected }
            : current;
        });
      } catch {
        // 上传状态轮询只负责刷新持久化 journal；离线和短暂锁竞争不弹红条。
      } finally {
        inFlight = false;
      }
    };
    const timer = window.setInterval(() => void refreshUploadState(), 2_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [view]);

  const state = snapshot?.state ?? "idle";
  const rebindRequired = snapshot?.rebindRequired ?? false;
  const stateCopy = rebindRequired
    ? { label: "等待重新绑定", detail: "原窗口已关闭，旧 run 已封口" }
    : STATE_COPY[state];
  const stateKicker = rebindRequired ? "REBIND REQUIRED" : STATE_KICKER[state];
  const catalogGamesReady = activeCatalogRosterKey !== null
    && loadedCatalogRosterKey === activeCatalogRosterKey;
  const selectedProfile = catalogGamesReady
    ? games.find((game) => game.gameId === selectedGame)
    : undefined;
  const depthPanelGameId = snapshot?.boundTarget?.gameId
    ?? (selectedGame || selectedGameRef.current);
  const depthPanelRuntimeContext = resolveDepthPanelRuntimeContext(
    activeCatalogRosterKey !== null,
    depthPanelGameId,
    depthPanelSessionContract(
      snapshot?.boundTarget?.sessionContract,
      selectedProfile?.sessionContract,
    ),
    selectedProfile ? selectedProfile.fpsNum / selectedProfile.fpsDen : null,
    depthPanelRuntimeContextRef.current,
  );
  depthPanelRuntimeContextRef.current = depthPanelRuntimeContext;
  // 服务端准入事实覆盖安装包清单；离线或未注册时保持 fail-closed，避免采集员
  // 录完才收到不留数据库记录的 game_not_registered 拒绝（docs/101 §3）。
  const selectedGameOnboarding = selectedProfile !== undefined && !isGameSelectable(selectedProfile);
  const selectedGameOnboardingReason = selectedProfile
    ? onboardingReason(selectedProfile)
    : null;
  const captureActive = ACTIVE_STATES.has(state);
  const profileSelectionLocked = captureActive || state === "paused";
  const windowActionLocked = captureActive || (state === "paused" && !rebindRequired);
  const canStart = Boolean(snapshot?.boundTarget)
    && !captureActive
    && state !== "paused"
    && !captureStorage?.restartRequired;
  const canPause = state === "starting" || state === "recording";
  const canResume = state === "paused" && !rebindRequired;
  const canStop = (captureActive && state !== "preflighting") || state === "paused";
  const inputKeys = useMemo(
    () => [
      ...(snapshot?.capture?.pressedContractKeys ?? []).map(keyLabel),
      ...(snapshot?.capture?.mouseButtons ?? []).map((button) => `M${button}`),
    ],
    [snapshot?.capture?.mouseButtons, snapshot?.capture?.pressedContractKeys],
  );

  async function run<T>(operation: () => Promise<T>): Promise<T | undefined> {
    setBusy(true);
    setError(null);
    try {
      return await operation();
    } catch (reason) {
      if (isCollectorFault(reason)) {
        setNotification(reason);
      } else {
        setError(String(reason));
      }
      return undefined;
    } finally {
      setBusy(false);
    }
  }

  async function discover() {
    if (!catalogGamesReady) return;
    const result = await run(() =>
      invoke<WindowCandidate[]>("collector_list_window_candidates", {
        request: { gameId: selectedGame },
      }),
    );
    if (result) setCandidates(result);
  }

  async function bind(candidateId: string) {
    if (!catalogGamesReady) return;
    const result = await run(() =>
      invoke<CollectorSnapshot>("collector_bind_window", {
        request: { candidateId },
      }),
    );
    if (result) setSnapshot(result);
  }

  async function start() {
    const requestId = globalThis.crypto?.randomUUID?.() ?? `start-${Date.now()}`;
    const result = await run(() =>
      invoke<CollectorSnapshot>("collector_start_session", {
        request: { requestId },
      }),
    );
    if (result) setSnapshot(result);
  }

  async function stop() {
    const result = await run(() =>
      invoke<CollectorSnapshot>("collector_stop", {
        request: { reason: "main_window" },
      }),
    );
    if (result) setSnapshot(result);
  }

  async function pause() {
    const result = await run(() =>
      invoke<CollectorSnapshot>("collector_pause", {
        request: { reason: "main_window" },
      }),
    );
    if (result) setSnapshot(result);
  }

  async function resume() {
    const requestId = globalThis.crypto?.randomUUID?.() ?? `resume-${Date.now()}`;
    const result = await run(() => invoke<CollectorSnapshot>("collector_resume", {
      request: { requestId },
    }));
    if (result) setSnapshot(result);
  }

  async function beginCalibration(kind: ViewCalibrationKindName) {
    const result = await run(() =>
      invoke<ViewScaleStatus>("view_scale_begin_session", { kind }),
    );
    if (result) setViewScale(result);
  }

  async function toggleMeasurement() {
    const result = await run(() =>
      invoke<ViewScaleStatus>("view_scale_toggle_measurement"),
    );
    if (result) setViewScale(result);
  }

  async function abortMeasurement() {
    const result = await run(() =>
      invoke<ViewScaleStatus>("view_scale_abort_measurement"),
    );
    if (result) setViewScale(result);
  }

  async function cancelCalibration() {
    const result = await run(() =>
      invoke<ViewScaleStatus>("view_scale_cancel_session"),
    );
    if (result) setViewScale(result);
  }

  async function finishCalibration() {
    const result = await run(() =>
      invoke<ViewScaleStatus>("view_scale_finish_session"),
    );
    if (result) setViewScale(result);
  }

  async function chooseCaptureStorage() {
    const result = await run(() =>
      invoke<CaptureStorageStatus>("collector_choose_capture_storage_location"),
    );
    if (result) setCaptureStorage(result);
  }

  async function exportDiagnostics(sessionId?: string) {
    setExportMessage(null);
    const result = await run(() =>
      invoke<DiagnosticExportResult>("collector_export_diagnostics", { sessionId }),
    );
    if (result) {
      setExportMessage(`已导出 ${result.fileName} · ${formatNumber(result.eventCount)} 条事件`);
    }
  }

  async function openHistory(preferredSessionId?: string) {
    setView("history");
    const requestId = ++historyRequestId.current;
    const result = await run(async () => {
      const sessions = await invoke<SessionHistorySummary[]>("collector_list_session_history");
      const selected = sessions.find((session) => session.sessionId === preferredSessionId) ?? sessions[0];
      if (historyRequestId.current !== requestId) return null;
      historySessionId.current = selected?.sessionId ?? null;
      setHistoryDetail(null);
      setHistoryDataModel(null);
      const [detail, selectedDataModelQuery] = selected
        ? await Promise.all([
            invoke<SessionHistoryDetail>("collector_get_session_history_detail", {
              request: { sessionId: selected.sessionId },
            }),
            requestDataModelStatus(selected.sessionId),
          ])
        : [null, null];
      return { sessions, detail, selectedDataModelQuery, selectedSessionId: selected?.sessionId ?? null };
    });
    if (
      result
      && historyRequestId.current === requestId
      && historySessionId.current === result.selectedSessionId
    ) {
      setHistory(result.sessions);
      setHistoryDetail(result.detail);
      historyUploadFingerprint.current = result.detail
        ? uploadSummaryFingerprint(result.detail.session)
        : null;
      if (result.selectedDataModelQuery) applyDataModelStatus(result.selectedDataModelQuery);
    }
  }

  async function requestSessionUpload(sessionId: string) {
    const result = await run(() =>
      invoke<SessionHistorySummary>("collector_request_session_upload", {
        request: { sessionId },
      }),
    );
    if (!result) return;
    historyUploadFingerprint.current = uploadSummaryFingerprint(result);
    setHistory((current) => current.map((item) => (
      item.sessionId === result.sessionId ? result : item
    )));
    setHistoryDetail((current) => (
      current?.session.sessionId === result.sessionId
        ? { ...current, session: result }
        : current
    ));
    setExportMessage("已发起本次会话的 R2 上传；可在上传状态中查看进度。");
  }

  async function selectHistorySession(sessionId: string) {
    const requestId = ++historyRequestId.current;
    historySessionId.current = sessionId;
    historyUploadFingerprint.current = null;
    setHistoryDetail(null);
    setHistoryDataModel(null);
    const result = await run(() => Promise.all([
      invoke<SessionHistoryDetail>("collector_get_session_history_detail", {
        request: { sessionId },
      }),
      requestDataModelStatus(sessionId),
    ]));
    if (
      result
      && historyRequestId.current === requestId
      && historySessionId.current === sessionId
    ) {
      setHistoryDetail(result[0]);
      historyUploadFingerprint.current = uploadSummaryFingerprint(result[0].session);
      applyDataModelStatus(result[1]);
    }
  }

  async function openSessionDirectory(sessionId: string) {
    await run(() =>
      invoke("collector_open_session_directory", { request: { sessionId } }),
    );
  }

  async function openDataModelDirectory(sessionId: string) {
    await run(() =>
      invoke("collector_open_data_model_directory", { request: { sessionId } }),
    );
  }

  // The confirmation must come from the user. Echoing sessionId here would make
  // the backend's irreversible-action check a formality that every click passes.
  async function deleteSession(sessionId: string, confirmation: string) {
    const result = await run(() =>
      invoke<SessionDeletionResult>("collector_delete_session", {
        request: { sessionId, confirmation },
      }),
    );
    if (result) {
      setDeleteTarget(null);
      setExportMessage(`已完整删除 ${result.sessionId}`);
      await openHistory();
    }
  }

  const primaryAction = canResume ? resume : canPause ? pause : start;
  const certificationMustUseCard = perfCertificationActive
    && !canPause
    && (canResume || canStart);
  const primaryLabel = rebindRequired
    ? "等待重新绑定"
    : certificationMustUseCard
      ? "请按深度认证卡片操作"
    : canResume
      ? "恢复录制"
      : canPause
        ? "暂停录制"
        : state === "completed" && canStart
          ? "开始新会话"
          : "开始录制";
  const primaryDisabled = busy
    || certificationMustUseCard
    || (!canResume && !canPause && !canStart);
  const currentTopbar = topbarCopy(view, account?.identity.kind === "signedIn");

  return (
    <div className="app-shell">
      <WindowTitlebar />
      <aside className="sidebar">
        <div className="brand">
          <LogoMark />
          <div>
            <strong><em>diogenes</em><i>/</i>dataminer</strong>
            <span>{BUILD_BADGE_TITLE}</span>
          </div>
        </div>

        <nav className="primary-nav" aria-label="主要功能">
          <button className={view === "capture" ? "active" : ""} onClick={() => setView("capture")}>
            <CaptureIcon />
            <span>采集</span>
          </button>
          <button className={view === "history" ? "active" : ""} onClick={() => void openHistory(historyDetail?.session.sessionId)}>
            <HistoryIcon />
            <span>历史</span>
          </button>
          <button className={view === "account" ? "active" : ""} onClick={() => setView("account")}>
            <AccountIcon />
            <span>账号</span>
          </button>
        </nav>

        <div className="sidebar-note">
          <span className="status-led" />
          <div>
            <strong>本地采集服务在线</strong>
            <small>HWND only · GPU pipeline</small>
          </div>
        </div>
        <UpdateStatusCard />
        <div className="dev-badge">{catalogBadge(snapshot?.catalog)}</div>
      </aside>

      <div className="workspace">
        <header className="topbar">
          <div>
            <p>{currentTopbar.eyebrow}</p>
            <h1>{currentTopbar.title}</h1>
          </div>
          <div className="topbar-actions">
            <div className={`global-state state-${state}`}>
              <span className="state-pulse" />
              <div>
                <strong>{stateCopy.label}</strong>
                <small>{stateCopy.detail}</small>
              </div>
            </div>
          </div>
        </header>

        {view === "capture" ? (
        <div className="workspace-grid">
          <main className="primary-workspace">
            <CaptureWorkflow
              bound={Boolean(snapshot?.boundTarget)}
              state={state}
              captureActive={captureActive}
            />
            <section className="target-section">
              <div className="section-title">
                <div>
                  <span className="section-kicker">CAPTURE TARGET</span>
                  <h2>选择游戏窗口</h2>
                  <p>仅显示已配置且通过进程身份校验的窗口，不提供桌面或显示器采集。</p>
                </div>
                <button className="button secondary" disabled={busy || windowActionLocked || !catalogGamesReady || !selectedGame || selectedGameOnboarding} onClick={discover}>
                  <RefreshIcon />
                  扫描窗口
                </button>
              </div>

              <div className="profile-selector">
                <label htmlFor="game-profile">游戏与采集配置</label>
                <select
                  id="game-profile"
                  value={selectedGame}
                  disabled={busy || profileSelectionLocked || !catalogGamesReady}
                  onChange={(event) => {
                    applySelectedGame(event.target.value);
                    setCandidates([]);
                    setError(null);
                  }}
                >
                  {!selectedGame && (
                    <option value="" disabled>
                      {games.length === 0 ? "当前活动目录没有可用游戏" : "请选择活动游戏"}
                    </option>
                  )}
                  {games.map((game) => (
                    <option key={game.gameId} value={game.gameId} disabled={!isGameSelectable(game)}>
                      {game.displayName}{admissionSuffix(game)}
                    </option>
                  ))}
                </select>
                {selectedGameOnboarding && (
                  <div className="profile-meta" role="alert">
                    <span>
                      这一款尚未在服务端开放采集，录制结果无法上传
                      {selectedGameOnboardingReason ? `（${selectedGameOnboardingReason}）` : ""}。
                    </span>
                  </div>
                )}
                {selectedProfile && !selectedGameOnboarding && (
                  <div className="profile-meta">
                    <span>{selectedProfile.width} × {selectedProfile.height}</span>
                    <span>{formatFps(selectedProfile)} FPS</span>
                  </div>
                )}
              </div>

              <div className="window-list" aria-live="polite">
                {candidates.length === 0 ? (
                  <div className="empty-target">
                    <NoTargetIcon />
                    <div>
                      <strong>等待扫描游戏窗口</strong>
                      <span>启动受支持的游戏后，点击“扫描窗口”。</span>
                    </div>
                  </div>
                ) : (
                  candidates.map((candidate) => (
                    <button
                      className="window-candidate"
                      key={candidate.candidateId}
                      disabled={busy || windowActionLocked || !catalogGamesReady}
                      onClick={() => bind(candidate.candidateId)}
                    >
                      <span className="window-preview"><WindowIcon /></span>
                      <span className="candidate-copy">
                        <strong>{candidate.displayName}</strong>
                        <small>{candidate.executableName} · 进程名已匹配</small>
                      </span>
                      <span className="candidate-spec">
                        <strong>{candidate.clientWidth} × {candidate.clientHeight}</strong>
                        <small>窗口缩放 DPI {candidate.dpi}</small>
                      </span>
                      <span className={`candidate-state ${candidate.foreground ? "foreground" : ""}`}>
                        {candidate.foreground ? "前台" : "可绑定"}
                      </span>
                    </button>
                  ))
                )}
              </div>
            </section>

            {rebindRequired && (
              <section className="rebind-banner" role="status">
                <span>↻</span>
                <div>
                  <strong>原 HWND 已销毁，当前 session 仍保留</strong>
                  <p>重新启动同一个合成窗口，保持原始尺寸，点击“扫描窗口”并绑定。通过身份复核后“恢复录制”会创建新 run。</p>
                </div>
              </section>
            )}

            {snapshot?.boundTarget && (
              <section className="recording-section">
                <div className="recording-head">
                  <div>
                    <span className="section-kicker">视角标定</span>
                    <h2>
                      {viewScale?.sessionActive
                        ? `${calibrationKindLabel(viewScale.kind)}进行中`
                        : "视角尺度对表"}
                    </h2>
                    <p>
                      {viewScale?.sessionActive && viewScale.kind === "free"
                        ? "自由测量：不预设方向与圈数，按一下开始、做完动作再按一下结束。本模式只报读数，不做验收，也不会成为有效标定。"
                        : viewScale?.applicable === false
                          ? "该目录声明视角尺度不适用（指针型游戏），无需对表。"
                          : viewScale?.expectedCountsPer360 != null
                            ? `目录常数 counts_per_360 = ${viewScale.expectedCountsPer360}，对表将与其比对。`
                            : "目录常数未定版：本次为测绘模式，实测真值供研究定版（偏差门关闭，一致性与回报率门照常）。"}
                    </p>
                  </div>
                </div>

                {viewScale?.sessionActive ? (
                  <>
                    <div className="capture-rail" aria-label="标定实时指标">
                      <Metric
                        label="测量进度"
                        value={
                          viewScale.kind === "free"
                            ? `已记 ${viewScale.completedCount} 段`
                            : `${viewScale.completedCount}/${viewScale.planTotal}`
                        }
                      />
                      <Metric
                        label="当前段"
                        value={
                          viewScale.kind === "free"
                            ? "自由段（方向与圈数不限）"
                            : `${directionLabel(viewScale.currentDirection)} · ${speedLabel(viewScale.currentSpeed)}`
                        }
                      />
                      <Metric label="累计格数" value={formatNumber(Math.abs(viewScale.currentCounts))} />
                      <Metric label="事件数" value={formatNumber(viewScale.currentEvents)} />
                      <Metric
                        label="队列溢出"
                        value={formatNumber(viewScale.overflowCount)}
                        tone={viewScale.overflowCount ? "danger" : "good"}
                      />
                    </div>
                    {viewScale.multiMouse && (
                      <p><small>⚠ 检测到第二只鼠标移动——结束本段后会作废，请重做该段。</small></p>
                    )}
                    <div className="recording-controls">
                      <button className="button primary" disabled={busy} onClick={toggleMeasurement}>
                        {viewScale.measuring
                          ? "结束本段（Ctrl+Alt+F11）"
                          : viewScale.kind === "free"
                            ? "开始一段自由测量（Ctrl+Alt+F11）"
                            : `开始本段：${directionLabel(viewScale.currentDirection)}三圈 · ${speedLabel(viewScale.currentSpeed)}`}
                      </button>
                      {viewScale.measuring && (
                        <button className="button" disabled={busy} onClick={abortMeasurement}>
                          作废本段
                        </button>
                      )}
                      <button
                        className="button"
                        disabled={
                          busy ||
                          viewScale.measuring ||
                          // 自由会话没有计划，改为至少要有一段——空证据没有意义。
                          (viewScale.kind === "free"
                            ? viewScale.completedCount < 1
                            : viewScale.completedCount < viewScale.planTotal)
                        }
                        onClick={finishCalibration}
                      >
                        完成并写入证据
                      </button>
                      <button className="button stop" disabled={busy} onClick={cancelCalibration}>
                        取消对表
                      </button>
                    </div>
                    {viewScale.measurements.length > 0 && (
                      <p>
                        {viewScale.measurements.map((measurement, index) => (
                          <MeasurementReadout key={index} index={index} measurement={measurement} />
                        ))}
                      </p>
                    )}
                  </>
                ) : (
                  <div className="recording-controls">
                    <button
                      className="button primary"
                      disabled={busy || snapshot?.state !== "bound" || viewScale?.applicable === false}
                      onClick={() => void beginCalibration("full")}
                    >
                      全量对表（左右 × 三速，六段）
                    </button>
                    <button
                      className="button"
                      disabled={busy || snapshot?.state !== "bound" || viewScale?.applicable === false}
                      onClick={() => void beginCalibration("spot_check")}
                    >
                      录前抽检（单段）
                    </button>
                    <button
                      className="button"
                      disabled={busy || snapshot?.state !== "bound" || viewScale?.applicable === false}
                      onClick={() => void beginCalibration("free")}
                      title="不预设方向与圈数，只报读数。用来测垂直倍率、水平视野与开镜常数（docs/59）"
                    >
                      自由测量（测绘用，不产裁决）
                    </button>
                  </div>
                )}

                {viewScale?.lastError && (
                  <p>
                    <small>⚠ {viewScale.lastError}</small>
                  </p>
                )}
                {viewScale?.result && (
                  <p>
                    <small style={{ display: "block" }}>
                      最近结果（{calibrationKindLabel(viewScale.result.kind)}）：
                      {viewScale.result.verdict == null
                        ? "仅读数，无验收裁决"
                        : viewScale.result.verdict === "pass"
                          ? "通过"
                          : "未通过"}
                      {viewScale.result.countsPer360Measured != null
                        ? ` · 实测每圈 ${Math.round(viewScale.result.countsPer360Measured)} 格`
                        : ""}
                      {viewScale.result.deviationPct != null
                        ? ` · 对目录常数偏差 ${viewScale.result.deviationPct.toFixed(2)}%`
                        : ""}
                      {viewScale.result.repeatSpreadPct != null
                        ? ` · 重复散布 ${viewScale.result.repeatSpreadPct.toFixed(2)}%`
                        : ""}
                      {viewScale.result.pollingHzP50 != null
                        ? ` · 回报率 ~${Math.round(viewScale.result.pollingHzP50)}Hz`
                        : ""}
                    </small>
                    {viewScale.result.failReasons.length > 0 && (
                      <small style={{ display: "block" }}>
                        未通过原因：{viewScale.result.failReasons.map(failReasonLabel).join("；")}
                      </small>
                    )}
                    {/* 自由段的产出只有逐段读数，会话结束后 status.measurements
                        已清空，所以从 result 里渲染。 */}
                    {viewScale.result.measurements.map((measurement, index) => (
                      <MeasurementReadout key={index} index={index} measurement={measurement} />
                    ))}
                    {viewScale.result.kind === "free" && (
                      <small style={{ display: "block" }}>
                        换算参考（docs/59）：俯仰限位之间扫满，vertical_scale_ratio = 180 ÷ 纵向换算角度的绝对值；
                        画面左缘→右缘，横向换算角度的绝对值即水平视野。角度按原始推动方向带符号（右为正、下为正）。
                      </small>
                    )}
                    <small style={{ display: "block" }}>证据：{viewScale.result.evidencePath}</small>
                  </p>
                )}
              </section>
            )}

            <section className="recording-section">
              <div className="recording-head">
                <div>
                  <span className="section-kicker">{stateKicker}</span>
                  <h2>{snapshot?.boundTarget?.displayName ?? "尚未绑定采集目标"}</h2>
                  <p>
                    {snapshot?.boundTarget
                      ? `${snapshot.boundTarget.executableName} · ${snapshot.boundTarget.clientWidth} × ${snapshot.boundTarget.clientHeight} · ${snapshot.boundTarget.recordingProfileId}`
                      : "绑定窗口后才能启动画面和输入采集。"}
                  </p>
                </div>
                <time>{formatDuration(snapshot?.capture?.elapsedMs ?? 0)}</time>
              </div>

              <div className="capture-rail" aria-label="采集实时指标">
                <Metric label="输出帧" value={formatNumber(snapshot?.capture?.frameCount)} />
                <Metric label="Ready 分段" value={formatNumber(snapshot?.capture?.readySegments)} />
                <Metric label="DataModel 块" value={formatNumber(dataModel?.publishedChunks)} tone={dataModel?.state === "failed" ? "danger" : dataModel?.publishedChunks ? "good" : undefined} />
                <Metric label="输入事件" value={formatNumber(snapshot?.capture?.inputEventCount)} />
                <Metric label="Queue drop" value={formatNumber(snapshot?.capture?.captureQueueDrops)} tone={snapshot?.capture?.captureQueueDrops ? "danger" : "good"} />
                <Metric
                  label="磁盘可用"
                  value={formatOptionalBytes(snapshot?.storage.freeBytes)}
                  tone={storageMetricTone(snapshot?.storage)}
                />
              </div>

              <div className="recording-controls">
                <button className="button primary" disabled={primaryDisabled} onClick={primaryAction}>
                  <span className={canPause ? "pause-glyph" : "record-glyph"} />
                  {busy ? "正在处理…" : primaryLabel}
                </button>
                <button className="button stop" disabled={busy || !canStop} onClick={stop}>
                  <span className="stop-glyph" />
                  结束会话
                </button>
                <span className="shortcut-hint">
                  {perfCertificationActive
                    ? "认证期间 F8 可提前暂停；下一档请从深度认证卡片继续"
                    : "Ctrl + Alt + F8 暂停/继续 · F9 打点 · F10 结束"}
                </span>
              </div>
            </section>

            <DepthLabPanel
              sessionContract={depthPanelRuntimeContext?.sessionContract}
              collectorState={state}
              gameId={depthPanelRuntimeContext?.gameId ?? depthPanelGameId}
              capture={snapshot?.capture ?? null}
              targetFps={depthPanelRuntimeContext?.targetFps ?? null}
              onCollectorSnapshot={setSnapshot}
              onPerfCertificationChange={setPerfCertificationActive}
            />

            {(error || snapshot?.fault) && (
              <section className="fault-banner" role="alert">
                <span>!</span>
                <div>
                  <strong>{snapshot?.fault?.code ?? "COMMAND_FAILED"}</strong>
                  <p>{snapshot?.fault?.message ?? error}</p>
                  {snapshot?.fault?.action && <small>{snapshot.fault.action}</small>}
                  {snapshot?.fault?.detailsId && <small>详情编号：{snapshot.fault.detailsId}</small>}
                </div>
              </section>
            )}
            {consentNotice?.resignRequired && (
              // 常驻，不可关闭，且**不拦任何采集操作**（docs/108 D3）。条款变更是
              // 运营的动作；让采集员为它付出不能开工的代价是惩罚错了对象，而"该录的
              // 没录"不可逆（docs/100 §2.1）。
              <section className="notice-banner consent-banner" role="status">
                <span>!</span>
                <div>
                  <strong>采集授权需要补充签署</strong>
                  <p>
                    {consentNotice.pendingScopes.length > 0
                      ? `条款已更新，补签后新增授权：${consentNotice.pendingScopes
                          .map(consentScopeLabel)
                          .join("、")}。`
                      : "条款已更新，需要你重新签署当前版本。"}
                  </p>
                  <small>采集不受影响，可以先继续录制。</small>
                </div>
                <button onClick={() => setView("account")}>去签署</button>
              </section>
            )}
            {notification && (
              <section className="notice-banner" role="status">
                <span>i</span>
                <div>
                  <strong>{notification.code}</strong>
                  <p>{notification.message}</p>
                  <small>{notification.action}</small>
                  <small>详情编号：{notification.detailsId}</small>
                </div>
                <button aria-label="关闭提示" onClick={() => setNotification(null)}>×</button>
              </section>
            )}
          </main>

          <aside className="session-inspector">
            <section className="inspector-status">
              <div className={`capture-orb orb-${state}`}><span /></div>
              <div>
                <span>当前状态</span>
                <strong>{stateCopy.label}</strong>
                <small>{snapshot?.capture?.runId ? `${snapshot.capture.sessionId} / ${snapshot.capture.runId}` : "尚未创建本地会话"}</small>
              </div>
            </section>

            <section className="inspector-section">
              <div className="inspector-title">
                <h3>输入监视</h3>
                <span>{snapshot?.capture?.inputQueueOverflows ? "异常" : "正常"}</span>
              </div>
              <div className="input-monitor">
                {inputKeys.length ? inputKeys.map((key) => <kbd key={key}>{key}</kbd>) : <span>等待键盘与鼠标输入</span>}
              </div>
              <dl className="detail-list">
                <div><dt>Raw Input</dt><dd>WM_INPUT / QPC</dd></div>
                <div><dt>队列深度</dt><dd>{snapshot?.capture?.inputQueueDepth ?? 0}</dd></div>
                <div><dt>Overflow</dt><dd className={snapshot?.capture?.inputQueueOverflows ? "text-danger" : "text-good"}>{snapshot?.capture?.inputQueueOverflows ?? 0}</dd></div>
              </dl>
            </section>

            <section className="inspector-section">
              <div className="inspector-title">
                <h3>本地存储</h3>
                <span className={snapshot?.storage.state === "critical" || snapshot?.storage.state === "unavailable" ? "text-danger" : ""}>
                  {storageStateLabel(snapshot?.storage)}
                </span>
              </div>
              <dl className="detail-list">
                <div><dt>数据位置</dt><dd>{captureStorage?.activeLocation ?? "正在读取"}</dd></div>
                {captureStorage?.configuredLocation && (
                  <div><dt>待生效</dt><dd>{captureStorage.configuredLocation}</dd></div>
                )}
                <div><dt>可用空间</dt><dd>{formatOptionalBytes(snapshot?.storage.freeBytes ?? captureStorage?.freeBytes)}</dd></div>
                <div><dt>预计可录</dt><dd>{formatRemaining(snapshot?.storage.estimatedRemainingSeconds)}</dd></div>
                <div><dt>安全保留</dt><dd>{formatOptionalBytes(snapshot?.storage.state === "unknown" ? null : snapshot?.storage.reserveBytes)}</dd></div>
              </dl>
              <p>{storageAction(snapshot?.storage.state, snapshot?.storage.stopRequested)}</p>
              <button
                className="button secondary diagnostic-export"
                disabled={busy || !captureStorage?.canChange}
                onClick={() => void chooseCaptureStorage()}
              >
                选择采集数据位置
              </button>
              <p>{captureStorage?.restartRequired
                ? "新位置已安全保存，将在重启客户端后生效。"
                : captureStorage?.canChange
                  ? "只显示脱敏位置名；journal 与诊断仍保存在应用数据目录。"
                  : "停止采集并完整删除本地 session 后才能切换位置。"}</p>
            </section>

            <UploadStatusSection upload={snapshot?.upload} />

            <section className="inspector-section">
              <div className="inspector-title">
                <h3>目录与预检</h3>
                <span className={snapshot?.preflight?.result === "blocked" || snapshot?.preflight?.result === "failed" ? "text-danger" : ""}>
                  {sessionContractLabel(snapshot?.boundTarget?.sessionContract)}
                </span>
              </div>
              <dl className="detail-list evidence-list">
                <div><dt>目录结果</dt><dd>{snapshot?.catalog.result ?? "等待加载"}</dd></div>
                <div><dt>目录数量</dt><dd>{snapshot?.catalog.activeCatalogs.length ?? 0}</dd></div>
                <div><dt>目录代际</dt><dd>{snapshot ? `${snapshot.catalog.activeGeneration} / ${snapshot.catalog.cachedGeneration}` : "—"}</dd></div>
                <div><dt>目录来源</dt><dd>{snapshot ? catalogProvenanceLabel(snapshot.catalog.provenance) : "—"}</dd></div>
                <div><dt>预检结果</dt><dd className={preflightTone(snapshot?.preflight?.result)}>{snapshot?.preflight?.result ?? "尚未执行"}</dd></div>
                {snapshot?.preflight && <div><dt>预检身份</dt><dd title={snapshot.preflight.preflightId}>{snapshot.preflight.preflightId}</dd></div>}
                {snapshot?.preflight && <div><dt>证据 Hash</dt><dd title={snapshot.preflight.evidenceHash}>{shortHash(snapshot.preflight.evidenceHash)}</dd></div>}
                {snapshot?.preflight?.failureCode && <div><dt>失败代码</dt><dd>{snapshot.preflight.failureCode}</dd></div>}
                {snapshot?.preflight?.detailsId && <div><dt>详情编号</dt><dd>{snapshot.preflight.detailsId}</dd></div>}
                {snapshot?.boundTarget && (
                  <>
                    <div><dt>Catalog</dt><dd title={snapshot.boundTarget.catalogHash}>{identityLabel(snapshot.boundTarget.catalogId, snapshot.boundTarget.catalogVersion, snapshot.boundTarget.catalogHash)}</dd></div>
                    <div><dt>Device</dt><dd title={snapshot.boundTarget.deviceProfileHash}>{identityLabel(snapshot.boundTarget.deviceProfileId, snapshot.boundTarget.deviceProfileVersion, snapshot.boundTarget.deviceProfileHash)}</dd></div>
                    <div><dt>Capture</dt><dd title={snapshot.boundTarget.recordingProfileHash}>{identityLabel(snapshot.boundTarget.recordingProfileId, snapshot.boundTarget.recordingProfileVersion, snapshot.boundTarget.recordingProfileHash)}</dd></div>
                    <div><dt>Game</dt><dd title={snapshot.boundTarget.gameProfileHash}>{identityLabel(snapshot.boundTarget.gameId, snapshot.boundTarget.gameProfileVersion, snapshot.boundTarget.gameProfileHash)}</dd></div>
                    <div><dt>Settings</dt><dd title={snapshot.boundTarget.recordingProfileHash}>{identityLabel(snapshot.boundTarget.recordingProfileId, snapshot.boundTarget.recordingProfileVersion, snapshot.boundTarget.recordingProfileHash)}</dd></div>
                    <div><dt>Mapping</dt><dd title={snapshot.boundTarget.mappingHash}>{identityLabel(snapshot.boundTarget.mappingId, snapshot.boundTarget.mappingVersion, snapshot.boundTarget.mappingHash)}</dd></div>
                  </>
                )}
              </dl>
              {snapshot?.preflight && <PreflightChecks preflight={snapshot.preflight} />}
              <p>采集数据一律 non-trainable；能录哪些游戏由已验签目录决定，预检仍逐项核验。</p>
            </section>

            <section className="inspector-section">
              <div className="inspector-title"><h3>数据契约</h3><span>dm-1</span></div>
              <ul className="contract-list">
                <ContractItem label="H.264 MP4" detail="Hardware only" />
                <ContractItem label="Frames JSONL" detail="逐帧输入投影" />
                <ContractItem label="Events JSONL" detail="Raw Input 事实" />
                <ContractItem label="Markers JSONL" detail="Session QPC 审计" />
                <ContractItem label="Mapping" detail={selectedProfile?.mappingVersion ?? "未选择"} />
                <ContractItem label="SHA-256" detail="Immutable artifact" />
              </ul>
              <button className="button secondary diagnostic-export" disabled={busy} onClick={() => void exportDiagnostics()}>
                导出诊断
              </button>
              {exportMessage && <p className="diagnostic-export-result" role="status">{exportMessage}</p>}
              <p>诊断导出会带上目录身份、预检 checks、失败代码和稳定 details_id。</p>
            </section>

            <section className="inspector-section compact">
              <div className="inspector-title"><h3>采集边界</h3></div>
              <p>只采集绑定的游戏 HWND。失焦、最小化、窗口变化或输入队列故障会立即暂停或停止。</p>
            </section>
          </aside>
        </div>
        ) : view === "account" ? (
          account ? (
            <AccountWorkspace status={account} onStatusChange={setAccount} />
          ) : (
            <AccountStatusReadPlaceholder failed={accountStatusReadFailed} />
          )
        ) : (
          <HistoryWorkspace
            busy={busy}
            sessions={history}
            detail={historyDetail}
            dataModel={historyDataModel}
            message={exportMessage}
            notification={notification}
            onDismissNotification={() => setNotification(null)}
            onRefresh={() => void openHistory(historyDetail?.session.sessionId)}
            onSelect={(sessionId) => void selectHistorySession(sessionId)}
            onOpen={(sessionId) => void openSessionDirectory(sessionId)}
            onOpenDataModel={(sessionId) => void openDataModelDirectory(sessionId)}
            onExport={(sessionId) => void exportDiagnostics(sessionId)}
            onUpload={(sessionId) => void requestSessionUpload(sessionId)}
            onDelete={setDeleteTarget}
          />
        )}
      </div>
      {deleteTarget && (
        <DeleteSessionDialog
          session={deleteTarget}
          busy={busy}
          onCancel={() => setDeleteTarget(null)}
          onConfirm={(confirmation) =>
            void deleteSession(deleteTarget.sessionId, confirmation)
          }
        />
      )}
    </div>
  );
}

function UpdateStatusCard() {
  const [status, setStatus] = useState<UpdateStatusSnapshot | null>(null);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    Promise.all([
      invoke<UpdateStatusSnapshot>("client_get_update_status"),
      listen<UpdateStatusSnapshot>(UPDATE_STATUS_EVENT, (event) => {
        if (!disposed) setStatus(event.payload);
      }),
    ])
      .then(([current, stop]) => {
        if (disposed) {
          stop();
          return;
        }
        setStatus((existing) => existing ?? current);
        unlisten = stop;
      })
      .catch(() => {
        // 版本检查不可用不影响采集；卡片保持隐藏即可。
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  if (!status || status.state === "disabled") return null;
  const checking = status.state === "checking";
  const available = status.latestVersion !== null;
  return (
    <div className={`update-card${available ? " update-available" : ""}`}>
      <div className="update-card-head">
        <div>
          <strong>{available ? `新版本 v${status.latestVersion} 可用` : `当前版本 v${status.currentVersion}`}</strong>
          {available && <small>当前 v{status.currentVersion} · 应用内一键更新随 M-B 提供，请从固定发布渠道获取安装包</small>}
          {!available && status.state === "upToDate" && <small>已是最新</small>}
          {!available && status.state === "failed" && <small>检查未完成，已记录诊断；不影响采集</small>}
        </div>
      </div>
      {available && status.notes && <p className="update-notes">{status.notes}</p>}
      <button
        className="button secondary update-check-button"
        disabled={checking}
        onClick={() => {
          void invoke<UpdateStatusSnapshot>("client_check_for_update")
            .then((next) => setStatus(next))
            .catch(() => {});
        }}
      >
        {checking ? "检查中…" : "检查更新"}
      </button>
    </div>
  );
}

function WindowTitlebar() {
  const appWindow = useMemo(() => getCurrentWindow(), []);
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    let disposed = false;
    let stopResize: (() => void) | undefined;

    const syncMaximized = () => {
      void appWindow.isMaximized().then((value) => {
        if (!disposed) setMaximized(value);
      });
    };

    syncMaximized();
    void appWindow.onResized(syncMaximized).then((stop) => {
      if (disposed) stop();
      else stopResize = stop;
    });

    return () => {
      disposed = true;
      stopResize?.();
    };
  }, [appWindow]);

  const toggleMaximize = () => void appWindow.toggleMaximize();

  return (
    <header className="window-titlebar">
      <div className="window-titlebar-brand" data-tauri-drag-region>
        <LogoMarkSmall />
        <strong data-tauri-drag-region>data<i>/</i>miner</strong>
      </div>

      <div className="window-titlebar-drag" data-tauri-drag-region onDoubleClick={toggleMaximize}>
        <span className="window-titlebar-status" data-tauri-drag-region aria-hidden="true" />
        <span data-tauri-drag-region>LOCAL COLLECTOR</span>
        <span className="window-titlebar-separator" data-tauri-drag-region aria-hidden="true" />
        <span className="window-titlebar-platform" data-tauri-drag-region>WINDOWS · NATIVE CAPTURE</span>
        {BUILD_BADGE_UPPER ? (
          <span className="window-titlebar-environment" data-tauri-drag-region>{BUILD_BADGE_UPPER}</span>
        ) : null}
      </div>

      <div className="window-titlebar-controls">
        <button type="button" aria-label="最小化窗口" title="最小化" onClick={() => void appWindow.minimize()}>
          <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M2 8.5h8" /></svg>
        </button>
        <button
          type="button"
          aria-label={maximized ? "还原窗口" : "最大化窗口"}
          title={maximized ? "还原" : "最大化"}
          onClick={toggleMaximize}
        >
          {maximized ? (
            <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3.5 4.5h5v5h-5z" /><path d="M5 4.5V3h4v4H8.5" /></svg>
          ) : (
            <svg viewBox="0 0 12 12" aria-hidden="true"><rect x="2.5" y="2.5" width="7" height="7" /></svg>
          )}
        </button>
        <button className="window-titlebar-close" type="button" aria-label="关闭到托盘" title="关闭到托盘" onClick={() => void appWindow.hide()}>
          <svg viewBox="0 0 12 12" aria-hidden="true"><path d="m3 3 6 6M9 3 3 9" /></svg>
        </button>
      </div>
    </header>
  );
}

function HistoryWorkspace({
  busy,
  sessions,
  detail,
  dataModel,
  message,
  notification,
  onDismissNotification,
  onRefresh,
  onSelect,
  onOpen,
  onOpenDataModel,
  onExport,
  onUpload,
  onDelete,
}: {
  busy: boolean;
  sessions: SessionHistorySummary[];
  detail: SessionHistoryDetail | null;
  dataModel: DataModelJobStatus | null;
  message: string | null;
  notification: CollectorNotification | null;
  onDismissNotification: () => void;
  onRefresh: () => void;
  onSelect: (sessionId: string) => void;
  onOpen: (sessionId: string) => void;
  onOpenDataModel: (sessionId: string) => void;
  onExport: (sessionId: string) => void;
  onUpload: (sessionId: string) => void;
  onDelete: (session: SessionHistorySummary) => void;
}) {
  const session = detail?.session ?? null;
  const deletable = session ? ["completed", "faulted", "corrupt"].includes(session.state) : false;
  const dataModelReady = dataModel?.state === "completed" && Boolean(dataModel.outputRelativePath);
  return (
    <main className="history-workspace">
      <section className="history-list-panel">
        <div className="history-panel-head">
          <div>
            <span className="section-kicker cyan">SQLITE JOURNAL</span>
            <h2>本地会话</h2>
          </div>
          <button className="button secondary" disabled={busy} onClick={onRefresh}>刷新</button>
        </div>
        <div className="history-list">
          {sessions.length === 0 ? (
            <div className="history-empty">还没有 journal 会话，库里干净得能照镜子。</div>
          ) : sessions.map((item) => (
            <article
              key={item.sessionId}
              className={`history-card${session?.sessionId === item.sessionId ? " active" : ""}`}
            >
              <button className="history-card-main" onClick={() => onSelect(item.sessionId)}>
                <span className={`history-state state-${item.state}`}>{historyStateLabel(item.state)}</span>
                <strong>{item.gameId}</strong>
                <small>{formatUtc(item.startedAtUtc)}</small>
                <span>{item.runCount} runs · {item.readySegments}/{item.segmentCount} ready</span>
                <em className="catalog-mode-tag">{sessionContractLabel(item.sessionContract)}</em>
                {(item.corruptSegments > 0 || item.quarantinedSegments > 0) && (
                  <em>{item.corruptSegments} corrupt · {item.quarantinedSegments} quarantine</em>
                )}
                {/* docs/65 J-29：有 ready 分段就显示云端进度——「0 段传出去」正是
                    最需要被看见的状态，不能再凑巧渲染成空白。 */}
                {item.readySegments > 0 && !item.performanceValidationOnly && (
                  <em className={`upload-summary state-${uploadSummaryState(item)}`}>
                    {uploadSummaryLine(item)}
                  </em>
                )}
                {item.reclaimable && <em className="reclaimable-tag">已全部校验，可安全清理本地</em>}
              </button>
              {!item.performanceValidationOnly && <button
                className={`button history-upload-action${item.uploadRequestable ? " primary" : " secondary"}`}
                disabled={busy || !item.uploadRequestable}
                title={uploadActionTitle(item)}
                onClick={() => onUpload(item.sessionId)}
              >
                {uploadActionLabel(item)}
              </button>}
            </article>
          ))}
        </div>
      </section>

      <section className="history-detail-panel">
        {notification && (
          <section className="notice-banner history-notice" role="status">
            <span>i</span>
            <div>
              <strong>{notification.code}</strong>
              <p>{notification.message}</p>
              <small>{notification.action}</small>
              <small>详情编号：{notification.detailsId}</small>
            </div>
            <button aria-label="关闭提示" onClick={onDismissNotification}>×</button>
          </section>
        )}
        {!session ? (
          <div className="history-empty detail">选择一条会话查看 run、segment 和失败原因。</div>
        ) : (
          <>
            <div className="history-detail-head">
              <div>
                <span className="section-kicker cyan">SESSION DETAIL</span>
                <h2>{session.sessionId}</h2>
                <p>{session.recordingProfileId} · {session.mappingId}@{session.mappingVersion}</p>
              </div>
              <span className={`history-state state-${session.state}`}>{historyStateLabel(session.state)}</span>
            </div>
            <div className="history-metrics">
              <Metric label="RUN" value={formatNumber(session.runCount)} />
              <Metric label="READY" value={formatNumber(session.readySegments)} tone={session.readySegments ? "good" : undefined} />
              <Metric label="FRAMES" value={formatNumber(session.frameCount)} />
              <Metric label="INPUT" value={formatNumber(session.inputEventCount)} />
              <Metric label="MARKERS" value={formatNumber(session.markerCount)} />
              <Metric label="VIDEO" value={formatOptionalBytes(session.videoBytes)} />
              {!session.performanceValidationOnly && <Metric
                label="云端校验"
                value={`${formatNumber(session.uploadVerifiedSegments)}/${formatNumber(session.readySegments)}`}
                tone={session.reclaimable ? "good" : undefined}
              />}
            </div>
            {session.reclaimable && (
              <p className="muted">
                该会话的全部 ready 分段已上传并通过服务端 SHA-256 校验；如需释放磁盘，可用下方「删除会话」走既有安全删除流程（云端副本不受影响）。
              </p>
            )}
            {!session.performanceValidationOnly && <section className="history-evidence upload-evidence">
              <div className="inspector-title data-model-title">
                <div className="data-model-title-copy">
                  <h3>R2 数据上传</h3>
                  <p>本地录制默认不上传。请在左侧会话卡片手动发起；发起后支持后台续传，状态每 2 秒自动刷新。</p>
                </div>
                <span className={`history-state state-${uploadSummaryState(session)}`}>
                  {uploadStateLabel(uploadSummaryState(session))}
                </span>
              </div>
              <dl className="history-identity-grid upload-status-grid">
                <div><dt>排队</dt><dd>{formatNumber(session.uploadPendingSegments)}</dd></div>
                <div><dt>上传中</dt><dd>{formatNumber(session.uploadRunningSegments)}</dd></div>
                <div><dt>等待重试</dt><dd>{formatNumber(session.uploadRetryWaitSegments)}</dd></div>
                <div><dt>已传待校验</dt><dd>{formatNumber(session.uploadStoredSegments)}</dd></div>
                <div><dt>校验通过</dt><dd>{formatNumber(session.uploadVerifiedSegments)} / {formatNumber(session.readySegments)}</dd></div>
                <div><dt>失败</dt><dd>{formatNumber(session.uploadRejectedSegments)}</dd></div>
                <div><dt>上传许可</dt><dd>{session.uploadRequested ? "已手动发起" : "尚未发起"}</dd></div>
                <div><dt>发起时间</dt><dd>{session.uploadRequestedAtUtc ? formatUtc(session.uploadRequestedAtUtc) : "—"}</dd></div>
                <div className="upload-error-cell">
                  <dt>最近错误</dt>
                  <dd title={formatUploadFailure(session) ?? undefined}>
                    {formatUploadFailure(session) ?? "—"}
                  </dd>
                </div>
              </dl>
            </section>}
            <section className="history-evidence">
              <div className="inspector-title data-model-title">
                <div className="data-model-title-copy">
                  <h3>DataDemo 转换产物</h3>
                  <p>录制期间持续拆分，完成校验后发布为 ready 成品。</p>
                </div>
                <div className="data-model-title-actions">
                  <span className={`history-state state-${dataModel?.state ?? "waiting"}`}>
                    {dataModel ? historyStateLabel(dataModel.state) : "未创建"}
                  </span>
                  <button
                    className="button secondary data-model-open"
                    disabled={busy || !dataModelReady}
                    title={dataModelReady ? "打开最终 DataDemo 成品目录" : "转换完成后可打开最终目录"}
                    onClick={() => onOpenDataModel(session.sessionId)}
                  >
                    打开 DataDemo 目录
                  </button>
                </div>
              </div>
              <dl className="history-identity-grid">
                <div><dt>Chunks</dt><dd>{dataModel ? formatNumber(dataModel.publishedChunks) : "—"}</dd></div>
                <div><dt>Frames</dt><dd>{dataModel ? formatNumber(dataModel.publishedFrames) : "—"}</dd></div>
                <div><dt>RAW segments</dt><dd>{dataModel ? `${formatNumber(dataModel.processedSegments)} / ${formatNumber(dataModel.readySegments)}` : "—"}</dd></div>
                <div><dt>Tail pending</dt><dd>{dataModel ? `${formatNumber(dataModel.pendingFrames)} frames` : "—"}</dd></div>
                <div><dt>Output</dt><dd title={dataModel?.outputRelativePath ?? undefined}>{dataModel?.outputRelativePath ?? "等待最终发布"}</dd></div>
                <div><dt>Error</dt><dd>{dataModel?.errorCode ?? "—"}</dd></div>
              </dl>
            </section>
            <section className="history-evidence">
              <div className="inspector-title">
                <h3>Catalog identity</h3>
                <span>{sessionContractLabel(session.sessionContract)}</span>
              </div>
              <dl className="history-identity-grid">
                <div><dt>Catalog</dt><dd title={session.catalogHash}>{identityLabel(session.catalogId, session.catalogVersion, session.catalogHash)}</dd></div>
                <div><dt>Device</dt><dd title={session.deviceProfileHash}>{identityLabel(session.deviceProfileId, session.deviceProfileVersion, session.deviceProfileHash)}</dd></div>
                <div><dt>Capture</dt><dd title={`${session.recordingProfileHashKind}: ${session.recordingProfileHash}`}>{identityLabel(session.recordingProfileId, session.recordingProfileVersion, session.recordingProfileHash)}</dd></div>
                <div><dt>Game</dt><dd title={session.gameProfileHash}>{identityLabel(session.gameId, session.gameProfileVersion, session.gameProfileHash)}</dd></div>
                <div><dt>Settings</dt><dd title={session.recordingProfileHash}>{identityLabel(session.recordingProfileId, session.recordingProfileVersion, session.recordingProfileHash)}</dd></div>
                <div><dt>Mapping</dt><dd title={session.mappingHash}>{identityLabel(session.mappingId, session.mappingVersion, session.mappingHash)}</dd></div>
              </dl>
            </section>
            <section className="history-evidence">
              <div className="inspector-title"><h3>Preflight evidence</h3><span>{detail?.preflights.length ?? 0}</span></div>
              {detail?.preflights.length ? detail.preflights.map((preflight) => (
                <article className="preflight-card" key={preflight.preflightId}>
                  <header>
                    <div><strong>{preflight.result}</strong><small>{preflight.preflightId}</small></div>
                    <span className={`history-state ${preflightTone(preflight.result)}`}>{preflight.phase}</span>
                  </header>
                  <dl>
                    <div><dt>evidence hash</dt><dd title={preflight.evidenceHash}>{shortHash(preflight.evidenceHash)}</dd></div>
                    <div><dt>failure</dt><dd>{preflight.failureCode ?? "—"}</dd></div>
                    <div><dt>details_id</dt><dd>{preflight.detailsId ?? "—"}</dd></div>
                  </dl>
                  <PreflightChecks preflight={preflight} />
                </article>
              )) : <p className="history-delete-guard">旧 journal 或启动前失败可能没有 session 关联；以诊断导出中的 preflight 记录为准。</p>}
            </section>
            {(session.errorCode || session.stopReason) && (
              <div className="history-reason">
                <strong>{session.errorCode ?? session.stopReason}</strong>
                <span>stop reason: {session.stopReason ?? "未记录"}</span>
              </div>
            )}
            <div className="history-actions">
              <button className="button secondary" disabled={busy} onClick={() => onOpen(session.sessionId)}>打开 RAW 目录</button>
              <button className="button secondary" disabled={busy} onClick={() => onExport(session.sessionId)}>导出诊断</button>
              <button className="button danger" disabled={busy || !deletable} onClick={() => onDelete(session)}>完整删除</button>
            </div>
            {!deletable && <p className="history-delete-guard">active/paused 会话不能删除；先安全结束录制。</p>}
            {message && <p className="diagnostic-export-result" role="status">{message}</p>}
            <div className="run-list">
              {detail?.runs.map((run) => (
                <article className="run-card" key={run.runId}>
                  <header>
                    <div><strong>{run.runName}</strong><small>{formatUtc(run.startedAtUtc)}</small></div>
                    <span className={`history-state state-${run.state}`}>{historyStateLabel(run.state)}</span>
                  </header>
                  <dl className="run-summary">
                    <div><dt>帧</dt><dd>{formatNumber(run.frameCount ?? 0)}</dd></div>
                    <div><dt>输入</dt><dd>{formatNumber(run.inputEventCount ?? 0)}</dd></div>
                    <div><dt>Overflow</dt><dd>{formatNumber(run.inputQueueOverflows ?? 0)}</dd></div>
                    <div><dt>原因</dt><dd>{run.errorCode ?? run.stopReason ?? "—"}</dd></div>
                  </dl>
                  <div className="segment-list">
                    {(detail?.segments ?? []).filter((segment) => segment.runId === run.runId).map((segment) => {
                      const segmentUploadState = segmentUploadDisplayState(session, segment.uploadState);
                      return (
                        <div className="segment-row" key={`${segment.runId}-${segment.seq}`}>
                          <strong>SEG {String(segment.seq).padStart(5, "0")}</strong>
                          <span className={`history-state state-${segment.state}`}>{segment.state}</span>
                          <span>{formatNumber(segment.frameCount ?? 0)} 帧</span>
                          <span>{formatOptionalBytes(segment.videoBytes)}</span>
                          <span>{segment.hashesComplete ? "HASH ✓" : "HASH —"}</span>
                          <span>
                            {segment.audioState === null
                              ? "AUDIO —"
                              : `AUDIO ${segment.audioState.toUpperCase()} ${formatOptionalBytes(segment.audioBytes)}`}
                          </span>
                          {!session.performanceValidationOnly && <div className="segment-upload">
                            <span className={`history-state state-${segmentUploadState}`}>
                              R2 {uploadStateLabel(segmentUploadState)}
                            </span>
                            <small title={formatUploadFailure(segment) ?? segment.audioFailureCode ?? segment.errorCode ?? undefined}>
                              {segmentUploadDetail(
                                segment,
                                session.uploadRequested,
                                session.uploadRequestable,
                                session.uploadJobSegments === session.readySegments,
                              )}
                            </small>
                          </div>}
                        </div>
                      );
                    })}
                  </div>
                  <div className="marker-list">
                    {(detail?.markers ?? []).filter((marker) => marker.runId === run.runId).map((marker) => (
                      <div className="marker-row" key={marker.markerId}>
                        <strong>M{String(marker.seq).padStart(6, "0")}</strong>
                        <span className={`history-state ${markerAlignmentClass(marker.alignmentState)}`}>
                          {markerAlignmentLabel(marker.alignmentState)}
                        </span>
                        <span>{formatVideoTime(marker.videoTimeUs)}</span>
                        <span>FRAME {formatNumber(marker.sessionFrame)}</span>
                        <span>{marker.segmentSeq === null ? "SEG —" : `SEG ${String(marker.segmentSeq).padStart(5, "0")}`}</span>
                        <small>QPC {marker.qpc}</small>
                      </div>
                    ))}
                  </div>
                </article>
              ))}
            </div>
          </>
        )}
      </section>
    </main>
  );
}

function DeleteSessionDialog({
  session,
  busy,
  onCancel,
  onConfirm,
}: {
  session: SessionHistorySummary;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (confirmation: string) => void;
}) {
  const [typed, setTyped] = useState("");
  const matches = typed.trim() === session.sessionId;
  return (
    <div className="dialog-backdrop" role="presentation">
      <section className="delete-dialog" role="dialog" aria-modal="true" aria-labelledby="delete-session-title">
        <span className="section-kicker">IRREVERSIBLE ACTION</span>
        <h2 id="delete-session-title">完整删除这个本地会话？</h2>
        <p>将删除 SQLite 中的 session/run/segment、ready artifact 和该会话的 quarantine。此操作无法撤销。</p>
        <code>{session.sessionId}</code>
        <label htmlFor="delete-session-confirm">输入上面的会话 ID 以确认：</label>
        <input
          id="delete-session-confirm"
          type="text"
          autoComplete="off"
          spellCheck={false}
          disabled={busy}
          value={typed}
          onChange={(event) => setTyped(event.target.value)}
        />
        <div>
          <button className="button secondary" disabled={busy} onClick={onCancel}>取消</button>
          <button
            className="button danger"
            disabled={busy || !matches}
            onClick={() => onConfirm(typed.trim())}
          >
            {busy ? "正在删除…" : "确认完整删除"}
          </button>
        </div>
      </section>
    </div>
  );
}

function WorkflowItem({ index, label, active, complete }: { index: string; label: string; active: boolean; complete: boolean }) {
  return <div className={`workflow-item${active ? " active" : ""}${complete ? " complete" : ""}`}><span>{complete ? "✓" : index}</span><strong>{label}</strong></div>;
}

function CaptureWorkflow({ bound, state, captureActive }: { bound: boolean; state: CollectorState; captureActive: boolean }) {
  return (
    <section className="capture-workflow" aria-label="采集流程">
      <div>
        <span className="section-kicker">CAPTURE FLOW</span>
        <p>按顺序完成窗口绑定和录制。</p>
      </div>
      <div className="workflow">
        <WorkflowItem index="01" label="选择游戏" active={!bound} complete={bound} />
        <WorkflowItem index="02" label="检查目标" active={state === "bound"} complete={captureActive || ["paused", "completed"].includes(state)} />
        <WorkflowItem index="03" label="采集数据" active={captureActive || state === "paused"} complete={state === "completed"} />
      </div>
    </section>
  );
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: "good" | "warning" | "danger" }) {
  return <div className={`metric${tone ? ` metric-${tone}` : ""}`}><span>{label}</span><strong>{value}</strong></div>;
}

/**
 * 一段测量的读数。计划型段落报"每圈格数"；自由段没有圈数，改报总计数与
 * 按目录常数换算的角度——那两个数就是自由段的产出（docs/60 §5.2）。
 */
function MeasurementReadout({ index, measurement }: { index: number; measurement: ViewScaleMeasurement }) {
  const head =
    measurement.countsPer360 != null
      ? `${directionLabel(measurement.direction)} · ${speedLabel(measurement.speed)} · 每圈 ${Math.round(measurement.countsPer360)} 格`
      : `自由段 · 横向 ${formatNumber(measurement.totalCountsX)} 格 · 纵向 ${formatNumber(measurement.totalCountsY)} 格`;
  const angles =
    measurement.countsPer360 == null && measurement.sweptDegreesX != null && measurement.sweptDegreesY != null
      ? ` · 换算 横 ${measurement.sweptDegreesX.toFixed(1)}° / 纵 ${measurement.sweptDegreesY.toFixed(1)}°`
      : "";
  return (
    <small style={{ display: "block" }}>
      段 {index + 1}：{head}
      {angles} · {measurement.eventCount} 事件
      {measurement.pollingHzP50 != null ? ` · 回报率 ~${Math.round(measurement.pollingHzP50)}Hz` : ""}
      {measurement.bufferedReportCount > 0
        ? ` · 已补读 ${measurement.bufferedReportCount} 个缓冲报告（最大批次 ${measurement.maxDeliveryBatchSize}）`
        : ""}
    </small>
  );
}

function storageMetricTone(storage: CollectorSnapshot["storage"] | undefined): "good" | "warning" | "danger" | undefined {
  const state = storage?.state;
  if (state === "ready") return storage?.startAllowed ? "good" : "warning";
  if (state === "warning") return "warning";
  if (state === "critical" || state === "unavailable") return "danger";
  return undefined;
}

function storageStateLabel(storage: CollectorSnapshot["storage"] | undefined): string {
  if (storage?.freeBytes !== null && storage?.freeBytes !== undefined && !storage.startAllowed && storage.state === "ready") {
    return "新会话预算不足";
  }
  const labels: Record<CollectorSnapshot["storage"]["state"], string> = {
    unknown: "待检查",
    ready: "充足",
    warning: "30 分钟预警",
    critical: "停止水位",
    unavailable: "不可用",
  };
  return labels[storage?.state ?? "unknown"];
}

function storageAction(state: CollectorSnapshot["storage"]["state"] | undefined, stopRequested: boolean | undefined): string {
  if (stopRequested) return "已关闭输入 gate，正在安全封口当前 run；完成后再释放空间。";
  if (state === "warning") return "建议尽快结束会话并释放空间。已有 ready 数据不会被自动删除。";
  if (state === "critical") return "磁盘已到停止水位，客户端将安全结束当前 run。";
  if (state === "unavailable") return "确认采集目录所在磁盘在线且可访问。";
  return "开始新会话需满足两小时峰值码率预算，并额外保留 10 GiB。";
}

function ContractItem({ label, detail }: { label: string; detail: string }) {
  return <li><span>✓</span><div><strong>{label}</strong><small>{detail}</small></div></li>;
}

/**
 * 采集页常驻的上传面（docs/65 J-29/J-30）。两种降级——上传器未启动、设备凭据
 * 缺席——各有一条不可关闭的横幅；正常态显示队列读数、最近错误与下次重试。
 * 只描述不决策：上传状态永远不影响录制，也不改 RAW 事实。
 */
function UploadStatusSection({ upload }: { upload: CollectorSnapshot["upload"] | undefined }) {
  const queueTone = upload
    ? upload.rejectedSegments > 0 || upload.lastErrorCode
      ? "text-danger"
      : upload.pendingSegments + upload.inflightSegments + upload.awaitingVerificationSegments > 0
        ? ""
        : "text-good"
    : "";
  return (
    <section className="inspector-section">
      <div className="inspector-title">
        <h3>云端上传</h3>
        <span className={queueTone}>{uploadHeadline(upload)}</span>
      </div>
      {upload && !upload.uploaderRunning && (
        <div className="fault-banner" role="alert">
          <span>!</span>
          <div>
            <strong>UPLOADER_UNAVAILABLE</strong>
            <p>上传器未启动：本次运行不会上传任何数据，已录数据安全保留在本地。</p>
            <small>重启客户端重试；反复出现请导出诊断。</small>
          </div>
        </div>
      )}
      {upload?.uploaderRunning && upload.credentialsMissing && (
        <div className="notice-banner" role="status">
          <span>i</span>
          <div>
            <strong>UPLOAD_WAITING_FOR_ENROLLMENT</strong>
            <p>设备凭据缺席，上传排队等待；完成账号登录与设备绑定后自动恢复。</p>
            <small>已录数据不受影响，一直保留在本地。</small>
          </div>
        </div>
      )}
      <dl className="detail-list">
        <div><dt>待传分段</dt><dd>{upload ? formatNumber(upload.pendingSegments) : "—"}</dd></div>
        <div><dt>传输中</dt><dd>{upload ? formatNumber(upload.inflightSegments) : "—"}</dd></div>
        <div><dt>待云端校验</dt><dd>{upload ? formatNumber(upload.awaitingVerificationSegments) : "—"}</dd></div>
        <div><dt>被拒收</dt><dd className={upload?.rejectedSegments ? "text-danger" : ""}>{upload ? formatNumber(upload.rejectedSegments) : "—"}</dd></div>
        <div><dt>会话工件</dt><dd>{upload ? formatNumber(upload.pendingSessionArtifacts) : "—"}</dd></div>
        {upload?.lastErrorCode && (
          <div><dt>最近错误</dt><dd className="text-danger">{upload.lastErrorCode}</dd></div>
        )}
        {upload?.nextRetryUnixMs != null && (
          <div><dt>下次重试</dt><dd>{formatNextRetry(upload.nextRetryUnixMs)}</dd></div>
        )}
      </dl>
      <p>上传只在会话收尾后进行；被拒收分段的逐段原因在「历史」页展开。</p>
    </section>
  );
}

function uploadHeadline(upload: CollectorSnapshot["upload"] | undefined): string {
  if (!upload) return "读取中";
  if (!upload.uploaderRunning) return "未启动";
  if (upload.credentialsMissing) return "等待绑定";
  if (upload.inflightSegments > 0) return "传输中";
  if (upload.pendingSegments > 0) return "排队中";
  if (upload.awaitingVerificationSegments > 0) return "等待校验";
  if (upload.rejectedSegments > 0) return "有拒收";
  return "空闲";
}

function formatNextRetry(nextRetryUnixMs: number): string {
  const deltaSeconds = Math.round((nextRetryUnixMs - Date.now()) / 1000);
  if (deltaSeconds <= 0) return "即将重试";
  if (deltaSeconds < 60) return `约 ${deltaSeconds} 秒后`;
  return `约 ${Math.round(deltaSeconds / 60)} 分钟后`;
}

function PreflightChecks({ preflight }: { preflight: NonNullable<CollectorSnapshot["preflight"]> }) {
  return (
    <ul className="preflight-checks">
      {preflight.checks.map((check) => (
        <li key={check.checkId}>
          <span className={preflightTone(check.result)}>{check.result}</span>
          <strong>{check.checkId}</strong>
          <code title={formatFacts(check.facts)}>{formatFacts(check.facts)}</code>
        </li>
      ))}
    </ul>
  );
}

/**
 * 这场采集属于哪一代摄取契约（issue #855 §B）。
 *
 * 断代前这里读的是目录级的 `catalog.mode`。它走了——契约现在是**逐游戏**的，
 * 一个目录级的单值只能在断代窗口期里说谎，所以标签改读绑定目标 / 会话自己那一格。
 */
function sessionContractLabel(contract: string | undefined): string {
  switch (contract) {
    case "dm-local-dev":
      return "本地 · 不可训练";
    case "dm-session-ingest":
      return "候选 · 待裁决";
    default:
      return contract ?? "UNKNOWN";
  }
}

/**
 * 目录徽章：说这份目录**怎么来的**，不替它声称训练资格。
 *
 * 原文是硬编码的 "NON-TRAINABLE · DEV CATALOG"——拿了 `catalog` 却一眼不看，
 * 无论目录是什么都返回同一句。两半现在都不成立：
 *
 * - `DEV CATALOG`：`CatalogMode` 与 dev/production/certified 渠道语义随 issue #855
 *   PR 4 整个退役，已经没有「DEV 目录」这个概念；
 * - `NON-TRAINABLE`：31 份 bundle 的 `non_trainable` 字段随 PR 2 删光，目录本身
 *   不再声称任何训练资格，这里是在替它声称。
 *
 * 而「这场采集可不可训练」本来就有专门的、逐场读真实契约的标签
 * （`sessionContractLabel`），不需要目录徽章再说一遍——说第二遍还说错。
 *
 * 改读 `provenance`：这是 docs/54 §6.2 定义的、目录如何到达本机的事实，
 * 也正是采集员真正需要区分的（编译内嵌的本地目录 vs 服务端签名下发的目录）。
 */
function catalogBadge(catalog: CollectorSnapshot["catalog"] | undefined): string {
  if (!catalog) return "CATALOG · LOADING";
  const source = catalog.provenance === "delivered" ? "DELIVERED" : "COMPILED";
  return `${source} · ${catalog.entryCounts.games} GAMES`;
}

function preflightTone(result: string | undefined): string {
  // 每一代契约一个通过词（services/preflight.rs）：legacy 是
  // `dev_non_trainable_pass`，中性是 `preflight_pass`；`certified_pass` 随车道
  // 退役（docs/100）。
  if (
    result === "dev_non_trainable_pass" ||
    result === "preflight_pass" ||
    result === "passed" ||
    result === "pass"
  )
    return "text-good";
  if (result === "blocked" || result === "failed") return "text-danger";
  return "";
}

function shortHash(hash: string): string {
  const value = hash.replace(/^sha256:/, "");
  return value.length > 12 ? `${value.slice(0, 12)}…` : value;
}

// 目录来源与契约是两件事：一份中性 schema 的目录既可能编译进二进制（那就永远是
// legacy v1），也可能来自签名下发（那才走 v2）。采集出来的会话要能追回真值，
// 就得知道是哪一种（docs/54 §6.2）。
function catalogProvenanceLabel(provenance: CatalogProvenance): string {
  switch (provenance) {
    case "compiled":
      return "编译内置";
    case "delivered":
      return "签名下发";
    default:
      return provenance;
  }
}

function identityLabel(id: string, version: string | undefined, hash: string): string {
  return `${id}${version ? `@${version}` : ""} · ${shortHash(hash)}`;
}

function formatFacts(facts: unknown): string {
  try {
    return JSON.stringify(facts) ?? "null";
  } catch {
    return "[unserializable facts]";
  }
}

function isCollectorFault(value: unknown): value is CollectorFault {
  if (!value || typeof value !== "object") return false;
  const fault = value as Partial<CollectorFault>;
  return typeof fault.code === "string"
    && typeof fault.message === "string"
    && typeof fault.action === "string"
    && typeof fault.detailsId === "string";
}

function formatNumber(value: number | undefined): string {
  return new Intl.NumberFormat("zh-CN").format(value ?? 0);
}

function formatOptionalBytes(value: number | null | undefined): string {
  if (value === null || value === undefined) return "待检查";
  if (value < 1024 ** 3) return `${(value / 1024 ** 2).toFixed(0)} MB`;
  return `${(value / 1024 ** 3).toFixed(1)} GB`;
}

function formatRemaining(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "待检查";
  if (seconds < 60) return "少于 1 分钟";
  if (seconds < 3_600) return `约 ${Math.floor(seconds / 60)} 分钟`;
  return `约 ${(seconds / 3_600).toFixed(1)} 小时`;
}

function formatDuration(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1_000));
  return [Math.floor(seconds / 3_600), Math.floor((seconds % 3_600) / 60), seconds % 60]
    .map((part) => String(part).padStart(2, "0"))
    .join(":");
}

function formatVideoTime(videoTimeUs: number): string {
  const milliseconds = Math.max(0, Math.floor(videoTimeUs / 1_000));
  return `${formatDuration(milliseconds)}.${String(milliseconds % 1_000).padStart(3, "0")}`;
}

function markerAlignmentClass(state: "pending" | "aligned" | "outside_video"): string {
  if (state === "aligned") return "state-ready";
  if (state === "outside_video") return "state-paused";
  return "";
}

function markerAlignmentLabel(state: "pending" | "aligned" | "outside_video"): string {
  if (state === "aligned") return "ALIGNED";
  if (state === "outside_video") return "OUTSIDE";
  return "PENDING";
}

function formatUtc(value: string | null): string {
  if (!value) return "未开始";
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? value : parsed.toLocaleString("zh-CN", { hour12: false });
}

function uploadSummaryFingerprint(session: SessionHistorySummary): string {
  return [
    session.performanceValidationOnly,
    session.uploadRequested,
    session.uploadJobSegments,
    session.uploadPendingSegments,
    session.uploadRunningSegments,
    session.uploadRetryWaitSegments,
    session.uploadStoredSegments,
    session.uploadVerifiedSegments,
    session.uploadRejectedSegments,
    session.uploadLastErrorCode ?? "",
    session.uploadLastHttpStatus ?? "",
    session.uploadLastServerCode ?? "",
    session.uploadLastRequestId ?? "",
    session.reclaimable,
  ].join("|");
}

function uploadStateLabel(state: string): string {
  const labels: Record<string, string> = {
    performance_only: "仅本地",
    not_queued: "尚未上传",
    manual_required: "待手动上传",
    pending: "排队中",
    running: "上传中",
    retry_wait: "等待重试",
    stored: "已上传待校验",
    verified: "校验通过",
    rejected: "上传失败",
    purged: "云端已清理",
  };
  return labels[state] ?? state;
}

function uploadSummaryLine(session: SessionHistorySummary): string {
  if (session.performanceValidationOnly) return "";
  const parts = [
    `R2 ${uploadStateLabel(uploadSummaryState(session))}`,
    `校验 ${session.uploadVerifiedSegments}/${session.readySegments}`,
  ];
  if (session.uploadRetryWaitSegments > 0) parts.push(`重试 ${session.uploadRetryWaitSegments}`);
  if (session.uploadRejectedSegments > 0) parts.push(`失败 ${session.uploadRejectedSegments}`);
  return parts.join(" · ");
}

function formatUploadBytes(bytes: number): string {
  if (bytes < 1024) return `${formatNumber(bytes)} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  return formatOptionalBytes(bytes);
}

type UploadFailureFields = {
  uploadErrorCode?: string | null;
  uploadLastErrorCode?: string | null;
  uploadHttpStatus?: number | null;
  uploadLastHttpStatus?: number | null;
  uploadServerCode?: string | null;
  uploadLastServerCode?: string | null;
  uploadServerMessage?: string | null;
  uploadLastServerMessage?: string | null;
  uploadRequestId?: string | null;
  uploadLastRequestId?: string | null;
};

function safeUploadServerMessage(message: string | null | undefined): string | null {
  if (!message) return null;
  if (/authorization|bearer|token|presigned|https?:\/\/|[a-z]:\\/i.test(message)) return null;
  return message.slice(0, 512);
}

function formatUploadFailure(failure: UploadFailureFields): string | null {
  const stableCode = failure.uploadErrorCode ?? failure.uploadLastErrorCode ?? null;
  const status = failure.uploadHttpStatus ?? failure.uploadLastHttpStatus ?? null;
  const serverCode = failure.uploadServerCode ?? failure.uploadLastServerCode ?? null;
  const requestId = failure.uploadRequestId ?? failure.uploadLastRequestId ?? null;
  const rawMessage = failure.uploadServerMessage ?? failure.uploadLastServerMessage ?? null;
  const message = serverCode === "device_profile_mismatch"
    ? "历史服务端设备配置校验拒收"
    : serverCode === "catalog_version_not_found"
      ? "服务端尚未注册此录制目录；注册完成后可手动重试"
    : safeUploadServerMessage(rawMessage);
  const parts = [
    stableCode,
    status ? `HTTP ${status}` : null,
    serverCode,
    message,
    requestId ? `请求编号 ${requestId}` : null,
  ].filter((part): part is string => Boolean(part));
  return parts.length > 0 ? parts.join(" · ") : null;
}

function segmentUploadDetail(
  segment: SessionHistoryDetail["segments"][number],
  uploadRequested: boolean,
  uploadRequestable: boolean,
  uploadPreparationComplete: boolean,
): string {
  const localError = segment.audioFailureCode ?? segment.errorCode;
  if (!segment.uploadState) return localError ? `未入队 · ${localError}` : "尚未进入上传队列";
  if (!uploadRequested) {
    if (!uploadPreparationComplete) return "等待本地预处理完成";
    return uploadRequestable ? "等待标注员手动发起" : "会话结束后可手动发起";
  }
  const details = [
    `${formatNumber(segment.uploadedPartCount)} parts`,
    formatUploadBytes(segment.uploadedBytes),
    `第 ${formatNumber(segment.uploadAttemptCount ?? 0)} 次`,
  ];
  if (segment.uploadState === "retry_wait" && segment.uploadNextAttemptAtMs) {
    details.push(`下次 ${new Date(segment.uploadNextAttemptAtMs).toLocaleTimeString("zh-CN", { hour12: false })}`);
  }
  const uploadFailure = formatUploadFailure(segment);
  const error = uploadFailure ?? localError;
  if (error) details.push(error);
  return details.join(" · ");
}

function historyStateLabel(state: string): string {
  const labels: Record<string, string> = {
    active: "进行中",
    paused: "已暂停",
    completed: "已完成",
    faulted: "已故障",
    corrupt: "已损坏",
    starting: "启动中",
    recording: "录制中",
    ready: "READY",
    waiting: "等待新分段",
    pending: "等待拆分",
    running: "拆分中",
    finalizing: "封口中",
    cancelled: "无可输出帧",
    failed: "失败",
    quarantined: "隔离",
    aborted: "中止",
  };
  return labels[state] ?? state;
}

function formatFps(game: GameSummary): string {
  const value = game.fpsNum / game.fpsDen;
  return Number.isInteger(value) ? String(value) : value.toFixed(2);
}

function keyLabel(vk: number): string {
  const special: Record<number, string> = { 16: "Shift", 17: "Ctrl", 18: "Alt", 32: "Space", 37: "←", 38: "↑", 39: "→", 40: "↓" };
  if (special[vk]) return special[vk];
  if ((vk >= 48 && vk <= 57) || (vk >= 65 && vk <= 90)) return String.fromCharCode(vk);
  return `0x${vk.toString(16).toUpperCase()}`;
}

function topbarCopy(
  view: "capture" | "history" | "account",
  accountSignedIn = false,
): { eyebrow: string; title: string } {
  if (view === "capture") return { eyebrow: "采集工作台", title: "游戏数据采集" };
  if (view === "history") return { eyebrow: "LOCAL ARCHIVE", title: "会话历史与产物" };
  if (accountSignedIn) return { eyebrow: "标注员账号", title: "账号与设备" };
  return { eyebrow: "标注员账号", title: "注册与登录" };
}

const windowLabel = getCurrentWebviewWindow().label;

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <UiErrorBoundary>{windowLabel === "overlay" ? <Overlay /> : <App />}</UiErrorBoundary>
  </React.StrictMode>,
);
