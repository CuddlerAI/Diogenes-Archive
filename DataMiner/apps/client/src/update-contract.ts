// M-A 版本检查契约（docs/15 §2.3–§2.5）。Rust 是事实源：前端只渲染
// client_get_update_status / updater://status 的快照，不发起任何网络请求，
// 也不触达 tauri-plugin-updater 原始 API（capabilities 未授予，裁决 D3）。

export type UpdateCheckState =
  | "disabled"
  | "idle"
  | "checking"
  | "upToDate"
  | "updateAvailable"
  | "failed";

export type UpdateStatusSnapshot = {
  currentVersion: string;
  state: UpdateCheckState;
  latestVersion: string | null;
  notes: string | null;
  checkedAtUnixMs: number | null;
  lastError: string | null;
};

export const UPDATE_STATUS_EVENT = "updater://status";
