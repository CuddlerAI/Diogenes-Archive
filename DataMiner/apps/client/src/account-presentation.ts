import type { AccountStatus } from "./account-contract";

export type AccountUnavailableCopy = {
  title: string;
  body: string;
};

export type AccountEnvironmentBadge = {
  label: "STAGING" | "PRODUCTION";
  tone: "warn" | "ok";
  accessibleLabel: string;
};

export function accountStatusReadCopy(failed: boolean): AccountUnavailableCopy {
  return failed
    ? {
        title: "账号状态读取失败",
        body: "客户端无法读取账号服务状态，请重启后重试。新采集可用性请以采集页为准；已有本地采集文件不受影响。",
      }
    : {
        title: "正在读取账号状态",
        body: "客户端正在确认账号服务配置。新采集可用性请以采集页为准；已有本地采集文件不受影响。",
      };
}

export type AccountEnvironment = Extract<
  AccountStatus["availability"],
  { kind: "ready" }
>["environment"];

export type AccountReadyContext = "signIn" | "overview";

export type DeviceEnrollmentPresentationState =
  | { kind: "enrolled" }
  | { kind: "inactive"; status: string }
  | { kind: "credentialMissing" }
  | { kind: "unregistered" };

export function accountUnavailableCopy(status: AccountStatus): AccountUnavailableCopy | null {
  switch (status.availability.kind) {
    case "unconfigured":
      return {
        title: "账号服务未配置",
        body: "这个构建没有配置账号服务，注册与登录不可用，也不会启用任何账号环境目录。已有本地采集文件不受影响。",
      };
    case "invalid":
      return {
        title: "账号服务配置无效",
        body: `${status.availability.message} 注册与登录已停止，也不会启用任何账号环境目录。已有本地采集文件不受影响。`,
      };
    case "ready":
      return null;
  }
}

export function accountEnvironmentBadge(status: AccountStatus): AccountEnvironmentBadge | null {
  if (status.availability.kind !== "ready") return null;
  return status.availability.environment === "staging"
    ? {
        label: "STAGING",
        tone: "warn",
        accessibleLabel: "账号环境：STAGING（测试环境）",
      }
    : {
        label: "PRODUCTION",
        tone: "ok",
        accessibleLabel: "账号环境：PRODUCTION（生产环境）",
      };
}

export function accountReadyLead(
  environment: AccountEnvironment,
  context: AccountReadyContext,
): string {
  if (environment === "staging") {
    return context === "signIn"
      ? "登录仅用于 STAGING 账号；上传环境不在本次验收范围。"
      : "本地采集当前可用。登录与登记仅影响 STAGING 账号；上传环境不在本次验收范围。";
  }
  return context === "signIn"
    ? "登录后，这台机器采集的数据才能归到你的账号下并上传。"
    : "本地采集当前可用。登录与登记只影响这台机器的数据能否上传。";
}

export function deviceEnrollmentStateCopy(
  environment: AccountEnvironment,
  state: DeviceEnrollmentPresentationState,
): string {
  if (environment === "staging") {
    switch (state.kind) {
      case "enrolled":
        return "本机已在 STAGING 登记并保存设备凭据；上传环境不在本次验收范围。";
      case "inactive":
        return `本机 STAGING 登记状态为 ${state.status}；上传环境不在本次验收范围。`;
      case "credentialMissing":
        return "本机已在 STAGING 服务端登记，但本地 STAGING 设备凭据缺失；上传环境不在本次验收范围。";
      case "unregistered":
        return "本机尚未登记到 STAGING 账号；上传环境不在本次验收范围，本地 non-trainable 采集仍可使用。";
    }
  }

  switch (state.kind) {
    case "enrolled":
      return "本机已登记并保存设备凭据；上传器会在请求时验证它。";
    case "inactive":
      return `本机登记状态为 ${state.status}，当前不能用于上传。`;
    case "credentialMissing":
      return "本机已在服务端登记，但本地设备凭据缺失；幂等 enroll 不会补发凭据。";
    case "unregistered":
      return "本机尚未登记到这个账号；本地 non-trainable 采集仍可使用。";
  }
}

export function deviceEnrollmentSuccessCopy(
  environment: AccountEnvironment,
  credentialIssued: boolean,
): string {
  if (environment === "staging") {
    return credentialIssued
      ? "本机已登记，STAGING 设备凭据已安全保存；上传环境不在本次验收范围。"
      : "本机已在 STAGING 服务端登记；现有 STAGING 设备凭据保持不变；上传环境不在本次验收范围。";
  }
  return credentialIssued
    ? "本机已登记，设备凭据已安全保存，上传器会自动继续。"
    : "本机已在服务端登记；现有设备凭据保持不变。";
}
