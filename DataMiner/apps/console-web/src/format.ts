export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value >= 100 || exponent === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[exponent]}`;
}

export function formatDateTime(value: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString("zh-CN", { hour12: false });
}

/** 秒 → 「3 小时 12 分」。配额是按小时给的，界面上就该按小时读。 */
export function formatHours(seconds: number): string {
  if (seconds <= 0) return "0 分";
  // 先把秒取整到分钟再拆：分开取整会让 59.7 分进位成 60，读出「3 小时 60 分」。
  const totalMinutes = Math.round(seconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours === 0) return `${minutes} 分`;
  return minutes === 0 ? `${hours} 小时` : `${hours} 小时 ${minutes} 分`;
}

/**
 * 秒 → 「5.4 h」。矩阵一行有几十格，「5 小时 24 分」横向放不下也扫不动；
 * 读数本身在表头写着，格子里只要数值。合计列同一口径，免得两处单位不一样。
 */
export function formatHoursShort(seconds: number): string {
  if (seconds <= 0) return "0 h";
  return `${(seconds / 3600).toFixed(1)} h`;
}

/**
 * 占比 → 「83.49%」。非零但小于 0.01% 时给「<0.01%」——写 0.00% 会把真零和
 * 极小混成一类，而普查表里「有一点」和「一点没有」是两个结论。
 */
export function formatShare(part: number, total: number): string {
  if (total <= 0 || part <= 0) return "0%";
  const percent = (part / total) * 100;
  if (percent < 0.01) return "<0.01%";
  return `${percent.toFixed(2)}%`;
}

export function formatDuration(seconds: number | null): string {
  if (seconds === null) return "—";
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  return `${minutes}m ${rest}s`;
}

export function shortId(value: string | null): string {
  if (!value) return "—";
  return value.length > 12 ? `${value.slice(0, 8)}…` : value;
}

/**
 * 来源分级词表（issue #855 §B）。
 *
 * 两个词回答的是同一个问题的两端：这场采集是**永久**不可训练，还是**还没裁决**。
 * 界面上必须把「candidate 不等于通过」写死在词条里——candidate 只表示有资格等待
 * 裁决，`clip_trainability` 的每一道 blocker 照旧要清零。
 */
export function describeDataClassification(value: string): { label: string; note: string } {
  switch (value) {
    case "training_candidate":
      return {
        label: "训练候选",
        note: "来源链完整，但尚未对游戏、配置、程序版本和实际数据质量做最终裁决——此刻仍然不可训练。",
      };
    case "non_trainable":
      return {
        label: "不可训练",
        note: "本地测试/诊断采集，或缺少不可事后补造的采集时事实；永久不进入晋升路径。",
      };
    default:
      return { label: value, note: "" };
  }
}

/**
 * `clip_trainability` 的 blocker 词表。
 *
 * 视图是唯一的判定者，前端不复算也不改判据——这里只把机器名翻成人话，`code`
 * 原文照旧展示，方便与库里的 blockers 数组逐字对照。未收录的名字原样透出，
 * 不猜、不吞。
 */
export function describeTrainabilityBlocker(blocker: string): string {
  return trainabilityBlockerLabels[blocker] ?? "";
}

const trainabilityBlockerLabels: Record<string, string> = {
  source_non_trainable: "来源分级不是训练候选",
  missing_annotator: "会话没有归属标注员",
  annotator_inactive: "标注员不在活跃状态",
  missing_consent: "会话没有关联同意书",
  consent_inactive: "同意书已撤回或失效",
  consent_disallows_trainable: "同意书未授权训练用途",
  consent_expired: "同意书已过期",
  consent_purge_active: "同意书进入删除流程",
  consent_window_mismatch: "采集时刻不在同意书有效窗口内",
  consent_scope_insufficient: "同意书未覆盖本次采集的全部模态",
  depth_consent_not_covered: "该 policy 版本未覆盖 depth",
  device_inactive: "设备不在活跃状态",
  game_unapproved: "游戏未获采集或训练批准",
  device_profile_unapproved: "device profile 版本未获训练批准",
  recording_profile_unapproved: "录制 profile 版本未获训练批准",
  mapping_unapproved: "mapping 版本未获训练批准",
  executable_unapproved: "观测到的程序指纹不在名单上",
  view_scale_unverified: "视角尺度标定未通过",
  clip_not_stored: "片段尚未完成上传",
  clip_not_verified: "片段尚未通过传输验证",
  quality_gate_not_passed: "QA 判定尚未通过（等 operator 裁决）",
  clip_purge_active: "片段进入删除流程",
  clip_retention_expired: "片段已过留存期",
  artifact_set_incomplete: "产物件数与申报不符",
  artifact_not_stored: "仍有产物未落存储",
  artifact_not_verified: "仍有产物未通过校验",
};

/** 小时 → 「1.5 小时」/「2.1 天」。QA 队列的等待时长按人读得懂的量级给。 */
export function formatWaitHours(hours: number | null): string {
  if (hours === null) return "—";
  if (hours < 1) return `${Math.max(0, Math.round(hours * 60))} 分钟`;
  if (hours < 48) return `${hours.toFixed(1)} 小时`;
  return `${(hours / 24).toFixed(1)} 天`;
}
