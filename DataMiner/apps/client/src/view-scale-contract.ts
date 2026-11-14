// 视角标定契约（docs/46 §4.1）：Rust services/view_scale.rs 快照的手写镜像，
// 字段随 dm-view-scale-status-3 演进，两侧必须同步修改。
//
// 这是**手写**镜像：Rust 侧改了字段，TS 这边不同步也照样编译过，只会在运行
// 时读到 undefined。改 Rust 的人必须逐字段核对这个文件。
//
// v3（docs/60 §6 Q4 自由测量段）：kind 加 "free"；自由段没有计划也没有裁决，
// 因此 direction/speed/countsPer360/verdict 一律可空，并新增逐段的
// totalCountsX/Y、durationMs 与换算角度 sweptDegreesX/Y。

export const VIEW_SCALE_STATUS_EVENT = "view-scale://status";

/** 标定种类。`free` = 自由测量段：测绘工具，不是标定，没有裁决。 */
export type ViewCalibrationKindName = "full" | "spot_check" | "free";

export type ViewScaleMeasurement = {
  /** 自由段无声明方向/转速，为 null。 */
  direction: string | null;
  speed: string | null;
  /** 自由段没有圈数，算不出每圈格数，为 null。 */
  countsPer360: number | null;
  totalCountsX: number;
  totalCountsY: number;
  durationMs: number;
  /**
   * 本段计数按目录常数换算的角度（度，带符号，Raw Input 原始口径：
   * 右为正、**下为正**）；目录常数缺失时为 null。
   * 自由段的用途就在这两个数上（docs/60 §5.2）：
   * - 画面左缘→右缘：|sweptDegreesX| 即水平视野，用来定 fov_axis；
   * - 俯仰限位之间扫满：vertical_scale_ratio = 180 / |sweptDegreesY|。
   */
  sweptDegreesX: number | null;
  sweptDegreesY: number | null;
  eventCount: number;
  pollingHzP50: number | null;
  pollingEstimatedReportCount: number;
  deliveryGroupCount: number;
  maxDeliveryBatchSize: number;
  bufferedReportCount: number;
};

export type ViewScaleResult = {
  calibrationId: string;
  kind: ViewCalibrationKindName;
  /** null = 自由测量段：没有验收裁决。 */
  verdict: "pass" | "fail" | null;
  countsPer360Measured: number | null;
  deviationPct: number | null;
  repeatSpreadPct: number | null;
  leftRightDeltaPct: number | null;
  fastSlowDeltaPct: number | null;
  pollingHzP50: number | null;
  pollingEstimatorVersion: string;
  failReasons: string[];
  /** 逐段读数随结果返回——完成后 status.measurements 已清空。 */
  measurements: ViewScaleMeasurement[];
  evidencePath: string;
};

export type ViewScaleStatus = {
  schemaVersion: string;
  revision: number;
  bound: boolean;
  applicable: boolean | null;
  expectedCountsPer360: number | null;
  sessionActive: boolean;
  kind: ViewCalibrationKindName | null;
  /** 自由会话不预设段数，为 0（= 不限）。 */
  planTotal: number;
  completedCount: number;
  measuring: boolean;
  currentDirection: string | null;
  currentSpeed: string | null;
  currentCounts: number;
  currentEvents: number;
  overflowCount: number;
  multiMouse: boolean;
  measurements: ViewScaleMeasurement[];
  lastError: string | null;
  result: ViewScaleResult | null;
};

export type ViewCalibrationAudit = {
  calibrationId: string;
  kind: string;
  parentCalibrationId: string | null;
  gameId: string;
  recordingProfileHash: string;
  deviceProfileHash: string;
  createdUtc: string;
  createdQpc: number;
  /** null = 自由测量段（kind === "free"），没有裁决。 */
  verdict: string | null;
  countsPer360Measured: number | null;
  deviationPct: number | null;
  pollingHzP50: number | null;
  filePath: string;
  fileSha256: string;
  invalidatedUtc: string | null;
  invalidationReason: string | null;
};

export function directionLabel(direction: string | null): string {
  if (direction === "cw") return "右转";
  if (direction === "ccw") return "左转";
  return "—";
}

export function calibrationKindLabel(kind: ViewCalibrationKindName | null): string {
  if (kind === "full") return "全量对表";
  if (kind === "spot_check") return "录前抽检";
  if (kind === "free") return "自由测量";
  return "—";
}

export function speedLabel(speed: string | null): string {
  if (speed === "slow") return "慢速";
  if (speed === "normal") return "正常";
  if (speed === "fast") return "快速";
  return "—";
}

// fail_reasons 槽位 → 用户可读文案（槽位表 = crates/view-scale FailReason，
// snake_case 序列化）。未知槽位原样返回：契约演进允许先出码、后补文案。
export function failReasonLabel(reason: string): string {
  switch (reason) {
    case "insufficient_measurements":
      return "有效测量不足（有测量段缺失或圈数为零）";
    case "insufficient_events":
      return "单段鼠标事件数过少，结果不可信（请转满整圈，别太快）";
    case "insufficient_polling_samples":
      return "单段可用于估算回报率的样本不足，请持续平稳转动后重测";
    case "direction_mismatch":
      return "计数方向与声明的旋转方向相反（检查是否转反了方向）";
    case "deviation_exceeds_tolerance":
      return "实测值偏离目录常数超容差（多半是游戏内灵敏度被改动，改回统一设置后重测）";
    case "repeat_inconsistent":
      return "重复测量彼此不一致（请重测；排除鼠标垫打滑、中途碰键盘）";
    case "left_right_mismatch":
      return "左转与右转结果不一致";
    case "fast_slow_mismatch":
      return "快慢档尺度不一致（疑似游戏或驱动开了鼠标加速/平滑，关闭后重测）";
    case "polling_below_floor":
      return "回报率低于政策下限（现行 500Hz）：到鼠标驱动（罗技 G HUB / 雷蛇雷云等）调到 1000Hz；可先在 mousetester.io 自查实际回报率；无线鼠标请关省电模式或改有线";
    default:
      return reason;
  }
}
