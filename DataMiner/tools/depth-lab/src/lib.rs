//! `depth-lab` 的**离线核心**：与平台无关的纯计算。
//!
//! 设计见 [77 号](../../../docs/77-depth-ground-truth-hook-experiment-plan.md)。
//! 整个 depth-lab 分两半：
//!
//! | 部分 | 平台 | 本 crate 是否包含 |
//! |---|---|---|
//! | probe（注入 DLL、D3D hook、readback、水印写入） | **Windows only** | ❌ 后续独立 crate |
//! | 离线核心（线性化、变体派生、水印解码、逐帧配对） | **任意平台** | ✅ 就是这里 |
//!
//! 这样切分不是洁癖：本 crate 覆盖的恰好是风险登记里最贵的几条——
//! R-4（配对静默错位）、R-7（fp16 装不下 1cm）、R-23（Variant S 再生确定性）——
//! 它们全部是纯计算，**可以在碰 Windows 之前就用测试锁死**。
//! Windows 侧只剩「把像素搬出来」这一件事，失败时归因也就清楚了。

/// 68 号 §6.4 指定的 EXR 单通道名。**单通道，名字就是 `Z`**。
///
/// 放在 crate 根而不是 `exr_out` 里：索引行要记通道名，而索引行在没有
/// `exr-out` feature 的 probe 构建里也要能写。
pub const EXR_CHANNEL_NAME: &str = "Z";

/// 独立 depth-lab R2 桶的 manifest、对象键与完成校验纯契约（docs/77 §7）。
pub mod archive_contract;
/// P4：从常量缓冲认出 view/projection 矩阵并提取相机位姿与内参。
pub mod cadence;
pub mod calibration;
pub mod camera;
pub mod camera_action;
pub mod camera_authoring;
pub mod camera_binding;
pub mod camera_discovery;
pub mod camera_evidence;
pub mod camera_track;
pub mod capture_timing;
pub mod catalog_depth;
pub mod certification;
/// 客户端 U2：严格聚合 #529 segment，并原子物化 #524 archive；不含 HTTP/凭据。
#[cfg(feature = "client-orchestration")]
pub mod client_archive;
pub mod clip_plan;
pub mod compatibility;
pub mod cp2077_bound_camera;
pub mod cp2077_camera_report;
#[cfg(feature = "client-orchestration")]
pub mod cp2077_local_audit;
#[cfg(feature = "segment-finalize")]
pub mod cp2077_metric_evidence;
#[cfg(feature = "segment-finalize")]
pub mod cp2077_metric_promotion;
pub mod cp2077_pose_join;
#[cfg(feature = "segment-finalize")]
pub mod cp2077_pr405_package;
#[cfg(feature = "segment-finalize")]
pub mod cp2077_raw_depth_qc;
pub mod cp2077_raw_feedback;
pub mod d3d12_camera;
pub mod d3d12_camera_alias;
pub mod d3d12_camera_copy_series;
pub mod d3d12_camera_local;
pub mod d3d12_selection;
pub mod decode;
/// 采集 → 68 号三件交付物（video / depth / json）。需要 EXR 编码器。
#[cfg(feature = "exr-out")]
pub mod deliver;
pub mod depth;
pub mod dsv;
pub mod dsv_discovery;
/// Two-run D3D11 DSV report/activation/receipt/visual-review reduction capped at a bridge-only
/// selection attestation.
#[cfg(feature = "selection-validation")]
pub mod dsv_selection_attestation;
/// ETS2 交付桥：SCS 遥测流 → `deliver --pose` 吃的位姿旁车。需要 `deliver`
/// 的 `PoseRow`，所以随 `exr-out` 一起启用。
#[cfg(feature = "ets2")]
pub mod ets2;
/// EXR 交付层。只在 `exr-out` feature 下编译——probe 不需要 EXR 编码器。
#[cfg(feature = "exr-out")]
pub mod exr_out;
#[cfg(feature = "client-orchestration")]
pub mod final_action_io;
pub mod frame_health;
pub mod half;
/// 连续异常自动停录的记录与现场窗口（130 号 §6.5）。判据在 [`frame_health`]。
pub mod health_abort;
pub mod integration;
/// KCD2 D3D12 live-pose research evidence. This is a pure, fail-closed
/// authoring layer; it is intentionally not wired into the production catalog.
pub mod kcd2_pose;
pub mod local_discovery_seal;
pub mod map_bounds;
pub mod metrics;
pub mod pairing;
pub mod perf_run;
pub mod pipeline;
pub mod present_rate;
pub mod probe_config;
/// Client <-> injected probe lifecycle protocol. The transport is deliberately
/// file based, but the command/ACK semantics are independent from that choice.
pub mod probe_control;
pub mod profile;
/// 把采集目录推上 R2 的**判断**部分（85 号 M4）。传输在 CLI 的 push 子命令里。
pub mod push;
/// 推送的传输层（S3 SigV4）。只在 `push` feature 下编译。
#[cfg(feature = "push")]
pub mod push_s3;
pub mod raw_depth_demo;
pub mod readback;
/// 把 probe 的 run 级输出按已验证的客户端段边界原子收口；不含任何上传实现。
#[cfg(feature = "segment-finalize")]
pub mod segment_finalize;
/// Vulkan attachment discovery is currently a fail-closed contract skeleton only. It contains no
/// loader layer runtime and cannot produce runtime evidence or readiness.
pub mod vulkan_attachment_discovery;
pub mod watermark;

pub use present_identity::candidate_ring as selected_present_candidate_ring;
pub use present_identity::feedback as selected_present_feedback;
#[cfg(windows)]
pub use present_identity::transport as selected_present_transport;

pub use calibration::{
    AffineFit, CalibrationError, CrossCheck, MetricAnchor, MetricScaleCalibration,
    MetricScaleError, MetricScaleSource, ScaleEstimate,
};
pub use camera::{CameraPose, Intrinsics, MatrixKind};
pub use camera_action::{
    ActionEvent, ActionFrameRow, CAMERA_POSE_INDEX_SCHEMA_V1, CAMERA_POSE_INDEX_SCHEMA_V2,
    CP2077_PLAYER_POSE_SOURCE, CameraActionJoinError, CameraActionJoinInputs, CameraPoseProvenance,
    CameraPoseRow, CameraPoseSource, Cp2077CameraJoinReceipt, FinalCameraActionInputs,
    FinalCameraIdentity, FinalPoseRow, IdentifiedRows, JoinedCameraActionRow, MouseNormalization,
    PERFORMANCE_CAMERA_POSE_INDEX_SCHEMA_V1, PLAYER_POSE_INDEX_SCHEMA_V1, PlayerPoseRow,
    RawMouseDelta, StreamIdentity, assemble_final_pose_rows, join_camera_action,
    validate_camera_pose_row, validate_performance_camera_pose_row, validate_player_pose_row,
};
pub use camera_authoring::{
    AuthoringArtifact, CAMERA_BINDING_AUTHORING_SCHEMA_V1, CAMERA_BINDING_TRIAL_SCHEMA_V1,
    CameraBindingAuthoringManifest, author_render_camera_binding, author_render_camera_trial,
};
pub use camera_binding::{
    BoundBufferAtDraw, CAMERA_BINDING_SCHEMA_V1, CAMERA_BINDING_SCHEMA_V2,
    CAMERA_BINDING_SCHEMA_V3, CAMERA_POSE_DIAGNOSTIC_SCHEMA_V1, CameraBinding,
    CameraBindingDocument, CameraBindingError, CameraDrawSample, CameraPoseAssembler,
    CameraPoseAssemblyError, CameraPoseDiagnosticReason, CameraPoseDiagnosticRow,
    CameraPoseStreamContext, Cp2077HolsteredCamera, D3d12AxisMapping, D3d12CameraBinding,
    D3d12CameraBindingError, D3d12DescriptorTableBinding, D3d12DrawRelation,
    D3d12MatrixSourceBinding, D3d12SignedAxis, D3d12ViewSourceBinding, GameCameraBinding,
    KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V1, KCD2_COMBINED_CAMERA_BINDING_SCHEMA_V2,
    Kcd2CombinedCameraBinding, MatrixEncoding, MatrixLayout, MatrixRole, MatrixSourceBinding,
    ShaderStage, ViewSourceBinding, fallout_render_rotation_from_basis,
    fallout_render_rotation_from_bound_buffer, projection_from_bound_buffer,
    view_pose_from_bound_buffer,
};
pub use camera_discovery::{
    CAMERA_DISCOVERY_PROGRESS_SCHEMA_V1, CAMERA_DISCOVERY_REPORT_SCHEMA_V1,
    CAMERA_DISCOVERY_REPORT_SCHEMA_V2, CAMERA_DISCOVERY_REPORT_SCHEMA_V3,
    CAMERA_DISCOVERY_REPORT_SCHEMA_V4, CameraDiscoveryProgress, CameraDiscoveryReport,
    CameraDiscoveryTracker, DiscoveryBindingSnapshot, DiscoveryBlocker, DiscoveryBoundBuffer,
    DiscoveryDrawSample, DiscoveryPresentBatch, DiscoveryReferenceCameraObservation,
    DiscoverySamplingStats, DiscoveryViewCandidate, DiscoveryViewObservation,
};
pub use camera_evidence::{
    CAMERA_EVIDENCE_REPORT_SCHEMA_V1, CAMERA_EVIDENCE_REPORT_SCHEMA_V2,
    CAMERA_EVIDENCE_SWEEP_REPORT_SCHEMA_V1, CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SCHEMA_V1,
    CAMERA_REFERENCE_FREE_CANDIDATE_REVIEW_SWEEP_SCHEMA_V1, CameraEvidenceAnalysisMode,
    CameraEvidenceReport, CameraEvidenceSelection, CameraEvidenceSweepBuilder,
    CameraEvidenceSweepInput, CameraEvidenceSweepReport, CameraEvidenceSweepWindow,
    D3D12_CAMERA_AUTHORING_LAUNCH_SCHEMA_V1, D3D12_CAMERA_AUTHORING_PAIR_SCHEMA_V1,
    D3D12_CAMERA_EGEO_ATTESTATION_PAYLOAD_SCHEMA_V1, D3D12_CAMERA_EGEO_ATTESTATION_SCHEMA_V1,
    D3D12_CAMERA_EGEO_CANONICALIZATION_V1, D3d12AuthoringLaunchRole, D3d12AxisMotionObservation,
    D3d12CameraAuthoringLaunchEvidence, D3d12CameraAuthoringPairVerdict, D3d12ControlledMotion,
    D3d12EgeoAttestation, D3d12EgeoAttestationClaims, D3d12EgeoAttestationPayload,
    D3d12FovPhaseObservation, FALLOUT_HYBRID_EVIDENCE_SCHEMA_V2, FALLOUT_HYBRID_SWEEP_SCHEMA_V2,
    ReferenceFreeCameraCandidate, ReferenceFreeCameraCandidateReview,
    ReferenceFreeCameraCandidateReviewSweep, ReferenceFreeCameraCandidateReviewSweepBuilder,
    ReferenceFreeCameraCandidateReviewSweepWindow, analyze_camera_evidence,
    analyze_camera_evidence_for_phases, analyze_camera_evidence_sweep,
    analyze_camera_evidence_with_mode, analyze_reference_free_camera_candidate_review,
    analyze_reference_free_camera_candidate_review_sweep, seal_d3d12_egeo_attestation,
    validate_d3d12_camera_authoring_pair,
};
pub use capture_timing::{CaptureTiming, DepthIndexEntry};
#[cfg(feature = "segment-finalize")]
pub use cp2077_raw_depth_qc::{
    CP2077_RAW_DEPTH_QC_SCHEMA_V1, RawDepthQcContract, RawDepthQcReport,
    audit_cp2077_raw_depth_session,
};
pub use d3d12_selection::{
    D3d12DepthSelection, D3d12DsvIdentity, D3d12FallbackDrawIdentity, D3d12ProductionSelector,
    D3d12ResourceIdentity, D3d12SelectionFault, D3d12SelectionStatus, D3d12TransitionIdentity,
};
pub use decode::{DepthFormat, ZConvention};
pub use depth::{Projection, Variant};
pub use dsv::{DsvObservation, DsvRanking, DsvSelectionAudit, Viewport};
#[cfg(feature = "evidence-authoring")]
pub use dsv_discovery::materialize_dsv_candidate_pass_summary;
pub use dsv_discovery::{
    DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_FILE_NAME, DSV_DISCOVERY_CANDIDATE_PASS_SUMMARY_SCHEMA_V1,
    DSV_DISCOVERY_CONFIG_SCHEMA_V1, DSV_DISCOVERY_CONFIG_SCHEMA_V2, DSV_DISCOVERY_REPORT_FILE_NAME,
    DSV_DISCOVERY_REPORT_SCHEMA_V1, DSV_DISCOVERY_REPORT_SCHEMA_V2, DSV_DISCOVERY_REPORT_SCHEMA_V3,
    DsvDiscoveryBlocker, DsvDiscoveryBuilder, DsvDiscoveryCandidateObservation,
    DsvDiscoveryCandidatePassSummary, DsvDiscoveryCandidatePassSummaryBlocker,
    DsvDiscoveryCompletedPassObservation, DsvDiscoveryConfig, DsvDiscoveryDescriptor,
    DsvDiscoveryFrameObservation, DsvDiscoveryHookIntegrityFailureReasons, DsvDiscoveryLimits,
    DsvDiscoveryLossSummary, DsvDiscoveryPassOrdinalStats, DsvDiscoveryReport,
    DsvDiscoveryRuntimeCounters, DsvDiscoverySummary,
    MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME, create_new_evidence_directory,
    materialize_validated_report, read_report_bounded, summarize_dsv_candidate_passes,
    write_report_create_new, write_report_create_new_under_root,
};
#[cfg(feature = "evidence-authoring")]
pub use dsv_selection_attestation::materialize_dsv_selection_attestation;
#[cfg(feature = "selection-validation")]
pub use dsv_selection_attestation::{
    DSV_CANDIDATE_VISUAL_REVIEW_FILE_NAME, DSV_CANDIDATE_VISUAL_REVIEW_SCHEMA_V1,
    DSV_DISCOVERY_ACTIVATION_ACK_FILE_NAME, DSV_DISCOVERY_ACTIVATION_ACK_SCHEMA_V1,
    DSV_SELECTION_ATTESTATION_FILE_NAME, DSV_SELECTION_ATTESTATION_SCHEMA_V1,
    DsvSelectionAttestation, DsvSelectionAttestationRun, DsvSelectionEvidenceBinding,
    DsvSelectionExecutableIdentity, DsvSelectionReportInput, DsvSelectionRunInput,
    DsvSelectionSourceBinding, DsvSelectionTargetIdentity,
    GENERIC_D3D11_INJECTION_RECEIPT_FILE_NAME, GENERIC_D3D11_INJECTION_RECEIPT_SCHEMA_V1,
    MAX_DSV_DISCOVERY_ACTIVATION_ACK_BYTES, MAX_DSV_SELECTION_ATTESTATION_BYTES,
    MAX_DSV_SELECTION_BOUND_SNAPSHOT_BYTES, MAX_DSV_SELECTION_RECEIPT_BYTES,
    MAX_DSV_SELECTION_VISUAL_EVIDENCE_BYTES, MAX_DSV_SELECTION_VISUAL_REVIEW_BYTES,
    ValidatedDsvSelectionActivationAck, ValidatedDsvSelectionAttestation,
    ValidatedDsvSelectionReport, ValidatedDsvSelectionReportPair,
    validate_dsv_selection_activation_ack_pair, validate_dsv_selection_attestation_with_sources,
    validate_dsv_selection_report_pair,
};
pub use integration::{DepthToggle, GateDenial, GateFacts, evaluate_gates};
pub use kcd2_pose::{
    KCD2_COMBINED_BINDING_ID, KCD2_COMBINED_BINDING_ID_V2, KCD2_COMBINED_BINDING_VERSION,
    KCD2_COMBINED_BINDING_VERSION_V2, KCD2_DEPTH_SOURCE_DRAW_ORDINAL,
    KCD2_DEPTH_SOURCE_ROOT_SIGNATURE_SHA256, KCD2_EXECUTABLE_SHA256,
    KCD2_LIVE_POSE_EVENT_SCHEMA_V2, KCD2_LIVE_POSE_EVENT_SCHEMA_V3, KCD2_LIVE_POSE_PLAN_SCHEMA_V1,
    KCD2_LIVE_POSE_PLAN_SCHEMA_V2, KCD2_LIVE_POSE_PLAN_SCHEMA_V3, KCD2_MAIN_SCENE_DSV_ID,
    KCD2_MAINLINE_DEPTH_CANDIDATE_SCHEMA_V1, KCD2_MAINLINE_GAME_ID, KCD2_METRIC_EVIDENCE_SHA256,
    KCD2_PROJECTION_A, KCD2_PROJECTION_B, KCD2_VIEW_PROJECTION_ALIAS_ROOT_SIGNATURE_SHA256,
    KCD2_VIEW_PROJECTION_ROOT_SIGNATURE_SHA256, Kcd2CameraIntrinsics, Kcd2LivePoseBinding,
    Kcd2LivePoseDiscard, Kcd2LivePoseExtractor, Kcd2LivePoseOutcome, Kcd2LivePosePair,
    Kcd2LivePosePlan, Kcd2LivePoseSourceIdentity, Kcd2LivePoseSourceKind,
    Kcd2MainlineDepthCandidate, Kcd2PassOrdinalPolicy,
};
pub use local_discovery_seal::{
    DISCOVERY_CONFIG_SCHEMA_V1 as CP2077_DISCOVERY_CONFIG_SCHEMA_V1,
    LOCAL_DISCOVERY_SEALED_SCHEMA_V1, LocalDiscoveryAddressAudit, LocalDiscoverySealError,
    LocalDiscoverySealedMarker, MANUAL_LOCAL_SMOKE_PROVENANCE,
    POSE_DISCOVERY_REPORT_FILE as CP2077_POSE_DISCOVERY_REPORT_FILE,
    POSE_DISCOVERY_REPORT_SCHEMA_V1, seal_cp2077_local_discovery,
};
pub use map_bounds::{
    BETHESDA_CELL_UNITS, CellGrid, MapBoundsError, SCS_SECTOR_METRES, SectorGrid, SectorIndex,
    WorldBounds,
};
pub use pairing::{CameraMode, FramePair, PairingReport};
pub use pipeline::E2eReport;
pub use probe_config::{
    CAMERA_DISCOVERY_SCHEMA_V1, CAMERA_DISCOVERY_SCHEMA_V2, CAMERA_DISCOVERY_SCHEMA_V3,
    CameraDiscoveryConfig, CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity,
    CameraProductionConfig, DiscoveryDepthFormat, ProbeConfig, render_identity_from_dsv_descriptor,
};
pub use probe_control::{
    AckKind as ProbeAckKind, CameraBindingIdentity, CandidateGpuAllocationSummary, ClientLifecycle,
    ClientLifecycleState, ControlCommand as ProbeControlCommand,
    ControlEnvelope as ProbeControlEnvelope, DepthProfileIdentity,
    FlushSummary as ProbeFlushSummary, GpuAdapterIdentity, LatencyQuantiles,
    PresentIntervalEvidence, PresentTimingSummary, ProbeAck, SelectedPresentFeedbackBootstrap,
    SelectedPresentFlushSummary, SelectedPresentLatencySummary, StartRun, StopRun,
    WriterTimingSummary,
};
pub use profile::{DepthCadence, DepthProfile, Lane, ProfileError, RendererBackend};
pub use raw_depth_demo::{
    CP2077_BUILD_20383525_EXE_SHA256, CP2077_GAME_ID, ISSUE735_LOCAL_CONTRACT,
    ISSUE735_LOCAL_DURATION_MS, PR405_LOCAL_CONTRACT, PR405_LOCAL_DURATION_MS,
    RAW_DEPTH_DEMO_PROFILE_SCHEMA_V1, RawDepthDemoAuthoringError, RawDepthDemoProfile,
    RawDepthDemoSelector, author_cp2077_raw_depth_demo, validate_capture_contract,
};
pub use readback::{DropStats, ReadbackPool};
pub use vulkan_attachment_discovery::{
    MAX_VULKAN_DISCOVERY_CANDIDATES, MAX_VULKAN_DISCOVERY_CANDIDATES_PER_FRAME,
    MAX_VULKAN_DISCOVERY_FRAMES, REQUIRED_VULKAN_DISCOVERY_ENTRYPOINTS,
    VULKAN_ATTACHMENT_DISCOVERY_CONFIG_SCHEMA_V1, VULKAN_ATTACHMENT_DISCOVERY_REPORT_SCHEMA_V1,
    VULKAN_DISCOVERY_BACKEND_STATE, VulkanAttachmentDescriptor, VulkanAttachmentDiscoveryBuilder,
    VulkanAttachmentDiscoveryConfig, VulkanAttachmentDiscoveryReport, VulkanAttachmentIdentity,
    VulkanAttachmentObservation, VulkanAttachmentRegistry, VulkanCommandBufferIdentity,
    VulkanCommandBufferLevel, VulkanCommandBufferRegistry, VulkanDiscoveryBlocker,
    VulkanDiscoveryLimits, VulkanDiscoveryRuntimeCounters, VulkanImageDescriptor,
    VulkanImageViewDescriptor, VulkanPresentObservation, VulkanRenderingPath, VulkanTimelineSignal,
};
