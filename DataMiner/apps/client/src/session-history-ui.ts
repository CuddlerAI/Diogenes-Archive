import type { SessionHistorySummary } from "./collector-contract";

export type UploadDisplayState = "performance_only" | "not_queued" | "manual_required" | "pending" | "running" | "retry_wait" | "stored" | "verified" | "rejected";

export function uploadSummaryState(session: SessionHistorySummary): UploadDisplayState {
  if (session.performanceValidationOnly) return "performance_only";
  if (
    session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_REJECTED"
    || session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_RETRY_EXHAUSTED"
  ) return "rejected";
  if (session.uploadRejectedSegments > 0) return "rejected";
  if (!session.uploadRequested) return session.uploadRequestable ? "manual_required" : "not_queued";
  if (session.uploadRunningSegments > 0) return "running";
  if (session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_RETRYABLE") return "retry_wait";
  if (session.uploadRetryWaitSegments > 0) return "retry_wait";
  if (session.uploadPendingSegments > 0) return "pending";
  if (session.uploadStoredSegments > 0) return "stored";
  if (session.readySegments > 0 && session.uploadVerifiedSegments === session.readySegments) return "verified";
  return "not_queued";
}

export function uploadActionLabel(session: SessionHistorySummary): string {
  if (session.performanceValidationOnly) return "";
  if (session.reclaimable) return "已上传并校验";
  if (session.uploadRequested && session.uploadRequestable) return "重试上传";
  if (session.uploadRequested) {
    if (
      session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_REJECTED"
      || session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_RETRY_EXHAUSTED"
    ) return "会话封存失败";
    if (session.uploadRejectedSegments > 0) return "上传失败";
    if (
      session.uploadRunningSegments > 0
      || session.uploadPendingSegments > 0
      || session.uploadRetryWaitSegments > 0
      || session.uploadStoredSegments > 0
    ) return "上传进行中";
    return "已发起上传";
  }
  if (session.uploadRequestable) return "上传到 R2";
  if (session.state !== "completed") return "录制结束后可上传";
  if (session.readySegments === 0) return "无可上传数据";
  if (session.uploadJobSegments !== session.readySegments) return "等待本地预处理";
  return "暂不可上传";
}

export function uploadActionTitle(session: SessionHistorySummary): string {
  if (session.performanceValidationOnly) return "";
  if (session.uploadRequested && session.uploadRequestable) {
    const reason = session.uploadLastServerCode === "catalog_version_not_found"
      ? "服务端目录尚未注册"
      : "历史设备配置校验";
    return `服务端问题修复后，重试此前因${reason}而拒收的分段；本地 RAW 与既有远端身份保持不变`;
  }
  if (session.uploadRequestable) return "手动发起该会话全部 ready 数据的 R2 上传";
  if (
    session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_REJECTED"
    || session.uploadLastErrorCode === "UPLOAD_SESSION_SEAL_RETRY_EXHAUSTED"
  ) return "会话封存已终止；本地 RAW 保留，请导出诊断并联系运营处理";
  if (session.uploadRequested) return "本次会话已经手动发起上传，后台会继续处理和重试";
  if (session.state !== "completed") return "会话完成后才能手动上传";
  if (session.readySegments > 0 && session.uploadJobSegments !== session.readySegments) {
    return `本地正在准备上传队列（${session.uploadJobSegments}/${session.readySegments}）`;
  }
  return "当前会话没有可发起的上传任务";
}

export function segmentUploadDisplayState(session: SessionHistorySummary, state: string | null): string {
  if (session.performanceValidationOnly) return "performance_only";
  return !session.uploadRequested && state
    ? (session.uploadRequestable ? "manual_required" : "not_queued")
    : state ?? "not_queued";
}
