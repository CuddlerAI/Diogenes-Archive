// 账号命令面的线格式（docs/11 §7）。与 collector-contract / update-contract 同级：
// 纯类型，没有逻辑。
//
// 这里**没有 token 类型**，而且不该有：会话凭据只在 Rust 侧的 Credential Manager
// 与 HTTP header 之间移动，从不经过 WebView。

/** 登录态。`unknown` 不是「未登录」——是「本地有凭据但还没和服务端对上账」，
 *  离线启动就是这个状态。把它显示成未登录会诱导用户重新注册。 */
export type AccountIdentity =
  | { kind: "unknown" }
  | { kind: "signedIn"; annotatorId: string }
  | { kind: "signedOut" };

export type AccountStatus = {
  schemaVersion: "dm-account-status-2";
  availability:
    | { kind: "unconfigured" }
    | { kind: "invalid"; code: string; message: string }
    | { kind: "ready"; environment: "staging" | "production" };
  identity: AccountIdentity;
};

export type Terms = {
  policyVersion: string;
  purpose: string;
  termsHash: string;
  body: string;
};

export type SignupChallenge = {
  challengeId: string;
  expiresInSeconds: number;
};

export type AccountDevice = {
  deviceId: string;
  clientDeviceId: string;
  status: string;
  clientVersion: string;
  lastSeenAt: string | null;
  createdAt: string;
};

export type DeviceEnrollmentStatus = {
  schemaVersion: "dm-device-enrollment-status-3";
  clientDeviceId: string;
  deviceCredentialPresent: boolean;
  devices: AccountDevice[];
};

type AssertNever<T extends never> = T;
/** Compile-time contract guard: tab changes or refreshes cannot restore a profile selector from status. */
export type DeviceEnrollmentProfileSelectionMustStayAbsent = AssertNever<
  Extract<"deviceProfiles" | "catalogSync" | "selectedDeviceProfile", keyof DeviceEnrollmentStatus>
>;

export type EnrollOutcome = {
  deviceId: string;
  clientDeviceId: string;
  credentialIssued: boolean;
};

/** 联络资料（docs/34 §5.4）。全部可空；没有资料是合法状态，不是缺数据。 */
export type AccountProfile = {
  displayName: string | null;
  phone: string | null;
  imKind: string | null;
  imHandle: string | null;
  region: string | null;
  updatedAt: string;
};

/** 资料更新：整体替换语义——留空的字段会被清掉，不是保持不动。 */
export type ProfileUpdate = {
  displayName?: string;
  phone?: string;
  imKind?: string;
  imHandle?: string;
  region?: string;
};

/** Rust 侧映射好的错误：稳定机读码 + 一句中文。服务端原文不会到这里。 */
export type AccountError = {
  code: string;
  message: string;
  retryable: boolean;
  httpStatus?: number;
  requestId?: string;
  retryAfterSeconds?: number;
};

export function isAccountError(value: unknown): value is AccountError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as AccountError).code === "string" &&
    typeof (value as AccountError).message === "string"
  );
}

/**
 * 当前 consent 与服务端在服务的条款是否一致（Rust `ConsentState` 的镜像）。
 *
 * `matchesCurrentTerms` 比对 hash 而非版本串：同一版本号配不同字节就是另一份
 * 文书，那正是最该被发现的漂移。
 */
export type SignedConsent = {
  policyVersion: string;
  allowsTrainableUse: boolean;
  acceptedAt: string | null;
  /** 这份文书声明授权的范围（docs/108 §3.2）。老服务端不返回，缺省空集。 */
  scopes: string[];
};

export type ConsentState = {
  consent: SignedConsent | null;
  currentPolicyVersion: string;
  currentAllowsTrainableUse: boolean;
  matchesCurrentTerms: boolean;
  /** 补签会**新增**哪些范围。这是提示里唯一有信息量的部分。 */
  pendingScopes: string[];
  currentScopes: string[];
  resignRequired: boolean;
};

/**
 * 观察器的快照（Rust `ConsentNoticeSnapshot` 的镜像，docs/108 §3.6）。
 *
 * `checked` 不是冗余：`resignRequired === false` 且 `checked === false` 是「还不知道」，
 * 把它渲染成「没有待办」等于对采集员说了一件我们并不知道的事。
 */
export type ConsentNotice = {
  resignRequired: boolean;
  currentPolicyVersion: string | null;
  pendingScopes: string[];
  signedScopes: string[];
  checked: boolean;
};

export const CONSENT_STATUS_EVENT = "consent://status";

/** scope 词的中文说法。未知词原样回显——漏掉它比显示一个英文词糟得多。 */
export function consentScopeLabel(scope: string): string {
  switch (scope) {
    case "input":
      return "键鼠输入";
    case "video":
      return "游戏画面";
    case "audio":
      return "游戏进程音频";
    case "depth":
      return "深度与相机参数";
    case "public_release":
      return "公开发布";
    case "trainable_research":
      return "模型训练研究";
    default:
      return scope;
  }
}
