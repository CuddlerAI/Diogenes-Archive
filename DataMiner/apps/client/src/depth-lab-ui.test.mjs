import assert from "node:assert/strict";
import test from "node:test";
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
  depthPanelSessionContract,
  depthUiEnvironmentEligible,
  earlyLaunchGameName,
  formatDepthDiagnostic,
  perfStageFlushState,
  perfCertificationAction,
  presentPerfCertificationGate,
  presentDepthStatus,
  presentDiscoverySmoke,
  presentPerfRunOutcome,
  resolveDepthPanelRuntimeContext,
  validateDepthSettings,
} from "./depth-lab-ui.ts";

test("CP2077 short validation needs backend capability, matching game and explicit selection", () => {
  const available = { ...EMPTY_DEPTH_STATUS, cp2077LocalValidationAvailable: true };
  const game = "cyberpunk-2077-steam";
  const stalePerfSettings = { ...DEFAULT_DEPTH_SETTINGS, performanceValidationOnly: true, performanceMetricsEnabled: true };
  assert.equal(cp2077LocalValidationAvailable(EMPTY_DEPTH_STATUS, game), false);
  assert.equal(cp2077LocalValidationAvailable(available, "kingdom-come-deliverance-2-steam"), false);
  const short = depthToggleSettings(DEFAULT_DEPTH_SETTINGS, available, game, true, true, true);
  assert.equal(short.performanceValidationOnly, true);
  assert.equal(short.performanceMetricsEnabled, true);
  assert.equal(short.plannedSeconds, 60);
  assert.equal(short.riskAcknowledged, true);
  for (const [status, gameId, enabled, selected] of [
    [EMPTY_DEPTH_STATUS, game, true, true],
    [available, "kingdom-come-deliverance-2-steam", true, true],
    [available, game, true, false],
    [available, game, false, true],
  ]) {
    const request = depthToggleSettings(stalePerfSettings, status, gameId, enabled, true, selected);
    assert.equal(request.performanceValidationOnly, false);
    assert.equal(request.performanceMetricsEnabled, false);
    if (!enabled) assert.equal(request.riskAcknowledged, false);
  }
  assert.equal(depthToggleSettings(DEFAULT_DEPTH_SETTINGS, available, game, true, false, true).riskAcknowledged, false);
});

test("Depth uses the selected contract before binding and the frozen target after binding", () => {
  assert.equal(depthPanelSessionContract(undefined, "dm-local-dev"), "dm-local-dev");
  assert.equal(
    depthPanelSessionContract("dm-session-ingest", "dm-local-dev"),
    "dm-session-ingest",
  );
  assert.equal(depthPanelSessionContract(undefined, undefined), undefined);
});

test("Depth route survives a transient catalog refresh but not a retired or different game", () => {
  const selected = resolveDepthPanelRuntimeContext(
    true,
    "kingdom-come-deliverance-2-steam",
    "dm-local-dev",
    30,
    null,
  );
  assert.deepEqual(selected, {
    gameId: "kingdom-come-deliverance-2-steam",
    sessionContract: "dm-local-dev",
    targetFps: 30,
  });
  assert.deepEqual(
    resolveDepthPanelRuntimeContext(
      true,
      "kingdom-come-deliverance-2-steam",
      undefined,
      null,
      selected,
    ),
    selected,
  );
  assert.equal(resolveDepthPanelRuntimeContext(true, "", undefined, null, selected), null);
  assert.equal(
    resolveDepthPanelRuntimeContext(true, "fallout4-steam-release-test", undefined, null, selected),
    null,
  );
  assert.equal(
    resolveDepthPanelRuntimeContext(
      false,
      "kingdom-come-deliverance-2-steam",
      undefined,
      null,
      selected,
    ),
    null,
  );
});

test("early-launch labels follow the configured game instead of assuming CP2077", () => {
  assert.equal(
    earlyLaunchGameName("kingdom-come-deliverance-2-steam"),
    "Kingdom Come: Deliverance II",
  );
  assert.equal(earlyLaunchGameName("cyberpunk-2077-steam"), "Cyberpunk 2077");
  assert.equal(earlyLaunchGameName("another-game"), "当前游戏");
});

test("performance certification gate clearly separates passed, blocked and unchecked", () => {
  const passed = presentPerfCertificationGate({
    state: "passed",
    certificationId: "cert-4060",
    gpuModel: "NVIDIA GeForce RTX 4060 Laptop GPU",
    runtimeVersion: "depth-2026.09.1",
    rendererBackend: "d3d11",
    backendGeneration: 7,
    detail: "matched",
  }, false, null);
  assert.equal(passed.label, "认证已通过");
  assert.equal(passed.tone, "good");
  assert.match(passed.detail, /RTX 4060 Laptop GPU/);
  assert.match(passed.detail, /cert-4060/);

  const blocked = presentPerfCertificationGate({
    state: "blocked",
    certificationId: null,
    gpuModel: "NVIDIA GeForce RTX 4050 Laptop GPU",
    runtimeVersion: "depth-2026.09.1",
    rendererBackend: "d3d11",
    backendGeneration: 7,
    detail: "当前 GPU 型号不在覆盖名单里。",
  }, false, null);
  assert.deepEqual(
    [blocked.label, blocked.tone, blocked.detail],
    ["认证未通过", "danger", "当前 GPU 型号不在覆盖名单里。"],
  );

  assert.equal(presentPerfCertificationGate(null, false, null).label, "尚未核验");
  assert.equal(presentPerfCertificationGate(null, true, null).label, "正在核验");
});

function perfOutcome(overrides = {}) {
  return {
    avgFpsDrop: 0.01602,
    low1FpsDrop: 0.08143,
    watermarkAvgDrop: 0.005,
    readbackAvgDrop: 0.004,
    writerAvgDrop: 0.00702,
    watermarkLow1Drop: 0.01,
    readbackLow1Drop: 0.02,
    writerLow1Drop: 0.05143,
    readbackOnlyBypassedFrames: 4_200,
    fullWriterTiming: {
      sampleCount: 4_096,
      totalCommitted: 4_200,
      wrapped: true,
      overBudgetCount: 17,
      durationUs: { p50: 10_000, p95: 25_000, p99: 60_000, max: 119_300 },
    },
    maxAvgFpsDrop: 0.02,
    maxLow1FpsDrop: 0.05,
    passes: false,
    ...overrides,
  };
}

test("performance outcome names each gate and the amount over its limit", () => {
  const result = presentPerfRunOutcome(perfOutcome());

  assert.equal(result.avgFps.message, "平均 FPS 1.60% ≤ 2.00%，通过");
  assert.equal(result.low1Fps.message, "1% low 8.14% > 5.00%，超出 3.14 个百分点");
  assert.equal(result.conclusion, "认证结论：未通过");
  assert.equal(result.valid, true);
  assert.equal(result.passes, false);
  assert.match(result.attribution[1], /writer \+5\.14 个百分点/);
  assert.match(result.attribution[3], /p99 60\.00 ms.*max 119\.30 ms/);
});

test("discovery states never claim Recording Ready", () => {
  const states = [
    "idle",
    "preparing",
    "hooks_armed",
    "waiting_observations",
    "sealing",
    "report_sealed",
    "faulted",
  ];
  for (const state of states) {
    assert.equal(presentDiscoverySmoke(state).recordingReady, false);
  }
  assert.equal(presentDiscoverySmoke("hooks_armed").label, "Hooks armed");
  assert.equal(presentDiscoverySmoke("waiting_observations").label, "Waiting observations");
  assert.equal(presentDiscoverySmoke("report_sealed").label, "Report sealed");
  assert.equal(presentDiscoverySmoke("preparing").canSeal, false);
  assert.equal(presentDiscoverySmoke("waiting_observations").canSeal, true);
});

test("performance outcome fails closed without rendering non-finite values", () => {
  for (const invalid of [undefined, Number.NaN, Number.POSITIVE_INFINITY]) {
    const result = presentPerfRunOutcome(perfOutcome({
      avgFpsDrop: invalid,
      low1FpsDrop: 0.01,
      passes: true,
    }));
    const rendered = `${result.avgFps.message} ${result.low1Fps.message} ${result.conclusion}`;
    assert.match(result.avgFps.message, /数据无效/);
    assert.doesNotMatch(rendered, /NaN|Infinity|undefined/);
    assert.equal(result.valid, false);
    assert.equal(result.passes, false);
  }
});

test("performance outcome rejects an implausibly large negative drop", () => {
  const result = presentPerfRunOutcome(perfOutcome({
    avgFpsDrop: -0.0045,
    low1FpsDrop: -0.2377,
    watermarkAvgDrop: -0.0039,
    readbackAvgDrop: -0.0008,
    writerAvgDrop: 0.0002,
    watermarkLow1Drop: -0.1998,
    readbackLow1Drop: -0.0863,
    writerLow1Drop: 0.0484,
    passes: true,
  }));
  assert.equal(result.low1Fps.valid, false);
  assert.equal(result.passes, false);
  assert.match(result.low1Fps.message, /本轮无效/);
  assert.match(result.conclusion, /数据无效/);
});

test("performance outcome rejects a verdict that disagrees with its metric gates", () => {
  const result = presentPerfRunOutcome(perfOutcome({
    avgFpsDrop: 0.01,
    low1FpsDrop: 0.01,
    watermarkAvgDrop: 0,
    readbackAvgDrop: 0,
    writerAvgDrop: 0.01,
    watermarkLow1Drop: 0,
    readbackLow1Drop: 0,
    writerLow1Drop: 0.01,
    passes: false,
  }));
  assert.equal(result.valid, false);
  assert.equal(result.passes, false);
  assert.match(result.conclusion, /不一致/);
});

function status(overrides = {}) {
  return { ...EMPTY_DEPTH_STATUS, ...overrides };
}

test("lifecycle mapping never promotes a pre-Ready run to Active", () => {
  assert.equal(presentDepthStatus(status({ enabled: true, lifecycle: "starting" }), "recording").label, "STARTING");
  assert.equal(presentDepthStatus(status({ enabled: true, lifecycle: "started" }), "recording").label, "STARTING");
  assert.equal(presentDepthStatus(status({ enabled: true, lifecycle: "ready" }), "starting").label, "READY");
  assert.equal(presentDepthStatus(status({ enabled: true, lifecycle: "ready" }), "recording").label, "ACTIVE");
});

test("a status read failure replaces stale Active with Fault", () => {
  const staleReady = status({ enabled: true, lifecycle: "ready" });
  const presentation = presentDepthStatus(staleReady, "recording", "IPC unavailable");
  assert.equal(presentation.label, "FAULT");
  assert.equal(presentation.segmentComplete, false);
  assert.match(presentation.detail, /不会沿用/);
});

test("depth UI accepts both local and managed session contracts", () => {
  assert.equal(depthUiEnvironmentEligible("dm-local-dev"), true);
  assert.equal(depthUiEnvironmentEligible("dm-session-ingest"), true);
  assert.equal(depthUiEnvironmentEligible("dev_non_trainable"), false);
  assert.equal(depthUiEnvironmentEligible(undefined), false);
});

test("stop, flush and fault states do not claim an early complete segment", () => {
  const stopping = presentDepthStatus(status({ enabled: true, lifecycle: "stopping" }), "stopping");
  const flushing = presentDepthStatus(status({ enabled: true, lifecycle: "stopped" }), "stopping");
  const flushed = presentDepthStatus(status({ enabled: true, lifecycle: "flushed" }), "paused");
  const fault = presentDepthStatus(status({ enabled: true, lifecycle: "faulted" }), "faulted");

  assert.deepEqual([stopping.label, stopping.segmentComplete], ["STOPPING", false]);
  assert.deepEqual([flushing.label, flushing.segmentComplete], ["FLUSHING", false]);
  assert.deepEqual([flushed.label, flushed.segmentComplete], ["OFF", true]);
  assert.deepEqual([fault.label, fault.segmentComplete], ["FAULT", false]);
});

test("enabled Idle remains OFF until the probe really starts", () => {
  const idle = presentDepthStatus(status({ enabled: true, lifecycle: "idle" }), "bound");
  assert.equal(idle.label, "OFF");
  assert.match(idle.detail, /尚未启动/);
  assert.doesNotMatch(idle.detail, /probe|gpu|writer|ack/i);
});

test("failed toggle rolls back to a freshly read backend status", async () => {
  const previous = status({ enabled: false, lifecycle: "idle" });
  const backend = status({ enabled: false, lifecycle: "flushed" });
  const request = { ...DEFAULT_DEPTH_SETTINGS, enabled: true, riskAcknowledged: true };
  const result = await configureDepthToggle(
    previous,
    request,
    async () => { throw new Error("CameraBinding 文件不存在"); },
    async () => backend,
  );

  assert.equal(result.status, backend);
  assert.equal(result.status.enabled, false);
  assert.match(result.rawError, /CameraBinding/);
});

test("performance validation mode is explicit and survives the configure boundary", async () => {
  let observedRequest;
  const request = {
    ...DEFAULT_DEPTH_SETTINGS,
    enabled: true,
    riskAcknowledged: true,
    performanceValidationOnly: true,
  };
  const result = await configureDepthToggle(
    EMPTY_DEPTH_STATUS,
    request,
    async (next) => {
      observedRequest = next;
      return status({ enabled: true, performanceValidationOnly: true });
    },
    async () => EMPTY_DEPTH_STATUS,
  );

  assert.equal(observedRequest.performanceValidationOnly, true);
  assert.equal(result.status.performanceValidationOnly, true);
  assert.equal(result.rawError, null);
});

test("risk acknowledgement is required before an enable request", () => {
  assert.equal(depthEnableIntent(false), "confirm_risk");
  assert.equal(depthEnableIntent(true), "submit");
  assert.equal(
    validateDepthSettings({ ...DEFAULT_DEPTH_SETTINGS, enabled: true }),
    "请先阅读并确认深度采集的风险说明。",
  );
});

test("startup timeout leaves room for probe warmup to fault cleanly", () => {
  const base = {
    ...DEFAULT_DEPTH_SETTINGS,
    enabled: true,
    riskAcknowledged: true,
  };
  assert.match(
    validateDepthSettings({ ...base, startTimeoutMs: DEPTH_START_TIMEOUT_MIN_MS - 1 }),
    /15000/,
  );
  assert.equal(
    validateDepthSettings({ ...base, startTimeoutMs: DEPTH_START_TIMEOUT_MIN_MS }),
    null,
  );
});

test("annotator copy hides internal probe vocabulary while diagnostics retain it", () => {
  const raw = "stale generation ACK identity mismatch; writer queue not empty";
  const message = annotatorMessage(raw);
  assert.doesNotMatch(message, /probe|ack|generation|writer|gpu/i);
  assert.match(message, /旧状态/);

  const diagnostic = formatDepthDiagnostic(
    status({
      enabled: true,
      lifecycle: "faulted",
      activeIdentity: { session_id: "session-1", run_id: "run-2", generation: 7 },
      lastError: raw,
    }),
    "faulted",
    null,
    "2026-08-05T00:00:00.000Z",
  );
  assert.match(diagnostic, /"generation": 7/);
  assert.match(diagnostic, /writer queue not empty/);
  assert.match(annotatorMessage("StopRun timeout before Flushed"), /未能确认完整封口/);
  assert.doesNotMatch(
    annotatorMessage("readback-only 档缺少 probe Flushed 证据"),
    /身份校验失败/,
  );
});

test("game executable mismatch is not flattened into a generic profile error", () => {
  const raw = "游戏版本与深度采集配置不匹配。profile 钉 sha256:old，实际运行 sha256:new";
  const message = annotatorMessage(raw);

  assert.match(message, /当前游戏版本尚未完成深度采集适配/);
  assert.match(message, /重跑性能认证不能解除/);
  assert.equal(annotatorMessage("Fallout camera sampler exact-build mismatch: expected old, got new"), message);
  assert.equal(annotatorMessage("Fallout player sampler exact-build mismatch: expected old, got new"), message);
  assert.doesNotMatch(message, /配置不可用/);
  assert.equal(annotatorMessage("这个游戏暂不支持深度采集。"), "这个游戏暂不支持深度采集。");
});

test("performance stage waits for the same run to reach Flushed", () => {
  const activeIdentity = { session_id: "session-1", run_id: "r0003", generation: 7 };
  assert.equal(
    perfStageFlushState(status({ lifecycle: "stopping", activeIdentity }), "r0003"),
    "waiting",
  );
  assert.equal(
    perfStageFlushState(status({ lifecycle: "flushed", activeIdentity }), "r0003"),
    "ready",
  );
  assert.equal(
    perfStageFlushState(status({ lifecycle: "flushed", activeIdentity }), "r0002"),
    "identity_mismatch",
  );
  assert.equal(
    perfStageFlushState(status({ lifecycle: "faulted", activeIdentity }), "r0003"),
    "faulted",
  );
});

test("generic resume during certification points back to the certification card", () => {
  assert.match(
    annotatorMessage("DEPTH_PERF_STAGE_REQUIRED: certification is active"),
    /认证卡片/,
  );
});

test("stop-before-seal is presented as a secondary capture failure", () => {
  const message = annotatorMessage(
    "probe fault stop_before_generation_sealed: StopRun arrived before GenerationSealed",
  );
  assert.match(message, /主链路先异常停止/);
  assert.match(message, /首个故障/);
  assert.doesNotMatch(message, /身份校验失败/);
});

test("a WGC clock skew is not reported as an identity failure", () => {
  const message = annotatorMessage(
    "probe fault feedback_timestamp_invalid: WGC source clock leads the capture "
      + "callback by 10674us, over the 33333us ceiling",
  );
  assert.match(message, /帧时间/);
  assert.match(message, /偏移/);
  assert.doesNotMatch(message, /身份校验失败/);
});

test("no feedback fault borrows the identity line through the ack in its name", () => {
  // "feedback" contains "ack": every one of these used to render as
  // "本次运行身份校验失败", pointing the operator at session/run/generation
  // state that was never involved.
  for (const code of [
    "feedback_timestamp_invalid",
    "feedback_queue_full",
    "feedback_telemetry_overflow",
    "feedback_before_arm",
  ]) {
    assert.doesNotMatch(annotatorMessage(`probe fault ${code}`), /身份校验失败/, code);
  }
  // A real ACK fault still reaches that line.
  assert.match(annotatorMessage("probe fault ack_queue_full"), /身份校验失败/);
  assert.match(annotatorMessage("probe fault identity_mismatch"), /身份校验失败/);
  assert.match(annotatorMessage("probe fault generation_mismatch"), /身份校验失败/);
});

test("diagnostics retain the non-deliverable performance-validation marker", () => {
  const diagnostic = formatDepthDiagnostic(
    status({ enabled: true, performanceValidationOnly: true }),
    "recording",
    null,
  );
  assert.match(diagnostic, /"performanceValidationOnly": true/);
});

test("diagnostics retain the raw performance certification check failure", () => {
  const raw = "hardware preflight scratch directory is not approved";
  const diagnostic = formatDepthDiagnostic(
    status({ enabled: true }),
    "bound",
    null,
    undefined,
    raw,
  );

  assert.match(diagnostic, /hardware preflight scratch directory is not approved/);
  assert.match(diagnostic, /"performanceCertificationError"/);
});

test("switch stays locked through Ready, Stopped and active collector states", () => {
  assert.equal(depthControlLocked(status({ enabled: true, lifecycle: "ready" }), "bound"), true);
  assert.equal(depthControlLocked(status({ enabled: true, lifecycle: "stopped" }), "paused"), true);
  assert.equal(depthControlLocked(status({ enabled: true, lifecycle: "flushed" }), "recording"), true);
  assert.equal(depthControlLocked(status({ enabled: true, lifecycle: "flushed" }), "paused"), false);
});

test("missing packaged Depth commands identify the installed version failure", () => {
  for (const name of ["depth_lab_launch_cp2077", "depth_lab_preflight_cp2077"]) {
    assert.match(annotatorMessage(`Command ${name} not found`), /当前安装版本缺少深度启动接口/);
  }
});

test("CP2077 trial prepare requires an idle collector terminal state", () => {
  for (const collectorState of ["idle", "completed", "faulted"]) {
    assert.equal(cp2077TrialPrepareBlocker(collectorState, "idle"), null);
  }
  for (const collectorState of [
    "discovering",
    "bound",
    "preflighting",
    "starting",
    "recording",
    "pausing",
    "paused",
    "stopping",
  ]) {
    assert.match(
      cp2077TrialPrepareBlocker(collectorState, "idle"),
      /主采集器当前不是空闲终态/,
      collectorState,
    );
  }
});

test("CP2077 trial prepare remains locked through the preparing lifecycle", () => {
  for (const phase of ["preparing", "prepared", "recording", "processing"]) {
    assert.match(
      cp2077TrialPrepareBlocker("idle", phase),
      /正在准备或运行/,
      phase,
    );
  }
  assert.equal(cp2077TrialPrepareBlocker("idle", "completed"), null);
  assert.equal(cp2077TrialPrepareBlocker("idle", "faulted"), null);
});


test("completed four-stage failures never offer another full capture", () => {
  const status = { active: true, certificationId: "test", nextStage: null,
    outcome: null, candidatePath: null, completionError: "UnstableComparison" };
  assert.equal(perfCertificationAction(status, "paused"), "finish_session");
  assert.equal(perfCertificationAction({ ...status, active: false }, "paused"), "finish_session");
  assert.equal(perfCertificationAction({ ...status, active: false }, "bound"), "begin");
  const exportPending = { ...status, active: false, outcome: { passes: true }, completionError: "disk" };
  assert.equal(perfCertificationAction(exportPending, "paused"), "export");
  assert.equal(perfCertificationAction({ ...exportPending, candidatePath: "candidate.json" }, "paused"), "finish_session");
  assert.equal(perfCertificationAction({ ...status, nextStage: "full" }, "paused"), "stage");
  const diagnostic = JSON.parse(formatDepthDiagnostic(EMPTY_DEPTH_STATUS, "paused", "later error", undefined, null, status));
  assert.equal(diagnostic.performanceCertification.completionError, "UnstableComparison");
  assert.equal(diagnostic.performanceCertification.nextStage, null);
});


test("local compatibility failures describe the failed condition instead of generic Ready", () => {
  assert.match(annotatorMessage("DEPTH_PRESENT_RATE_MISMATCH: 120 fps"), /实际游戏帧率/);
  assert.match(annotatorMessage("DEPTH_DEVICE_REQUIREMENTS_FAILED"), /硬件或驱动/);
  assert.match(annotatorMessage("DEPTH_CERTIFICATION_BLOCKED"), /基准认证/);
  assert.match(annotatorMessage("DEPTH_LOCAL_VALIDATION_FAILED"), /本机采集条件/);
});


test("baseline reuse never claims the local machine ran performance certification", () => {
  const value=presentPerfCertificationGate({state:"baseline_matched",certificationId:"base",gpuModel:"new",detail:"开录前需本机校验",rendererBackend:"d3d12",backendGeneration:7,runtimeVersion:"test"},false,null);
  assert.equal(value.label,"基准已匹配");
  assert.match(value.detail,/需本机校验/);
});
