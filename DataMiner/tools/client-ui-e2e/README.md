# client-ui-e2e — 客户端 UI 端到端走查工具

在**有交互桌面的 Windows 机器**上跑发布构建的 `dataminer-client.exe`，用 Chrome DevTools
Protocol 驱动它的 WebView2，逐屏截图 + DOM 断言 + 指令面清扫，产出一份结构化 `report.json`。

首轮走查结论见 [`docs/129-client-ui-e2e-review.md`](../../docs/129-client-ui-e2e-review.md)。

## 为什么是这个形状

- **必须在交互桌面**。Tauri 窗口在 session 0（服务会话）没有桌面可画，截图全黑。
- **必须提权**。`build.rs` 给 release 产物嵌的是 `windows/app.manifest`
  （`requestedExecutionLevel = requireAdministrator`），非提权启动直接失败
  （`The requested operation requires elevation`）。
- **必须走 `additionalBrowserArgs`，不能用环境变量**。`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`
  在 Tauri 下无效——wry 总会自己传一份 additional browser args，程序化传参优先于环境变量。
  要开调试端口只能改 `tauri.conf.json` 的 `app.windows[].additionalBrowserArgs` 并重新构建。
- **必须把出网钉在回环**。`services/account.rs` 与 `services/uploader.rs` 都默认连**生产**。
  不钉，走查就会在生产库上注册账号、消耗邀请码、登记设备。

## 文件

| 文件 | 作用 |
|---|---|
| `run-e2e.ps1` | 在交互会话里编排一轮：起 mock → 起客户端 → 跑驱动 → 收产物 → 打包 |
| `cdp-drive.mjs` | CDP 驱动本体，零依赖（Node 22 自带全局 `WebSocket`） |
| `mock-app-api.mjs` | `/app/v1/*` 的回环替身，按 `HttpAccountApi` 真正反序列化的线格式实现 |

## 跑一轮

前置：Node 22（`C:\dm\node22`）、已构建的 `dataminer-client.exe`、一个已登录的交互会话。

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File run-e2e.ps1 `
  -Exe C:\dm\cargo-target\release\dataminer-client.exe `
  -Root C:\dm\e2e\run -ToolDir C:\dm\e2e\tools
```

产物落在 `-Root`：`report.json`、`NN-*.png`（CDP 截图）、`desktop-*.png`（GDI 桌面截图）、
`app-api-requests.jsonl`（mock 收到的请求）、`run.log` / `transcript.log`，并打包成
`C:\dm\e2e\e2e-bundle.zip`。

### 从 session 0 的 runner 发起

云端 VM 上 job 跑在 SYSTEM/session 0，需要把进程送进 session 1：

```powershell
$prin = New-ScheduledTaskPrincipal -UserId "hakkoadmin" -LogonType Interactive -RunLevel Highest
Register-ScheduledTask -TaskName DmUiE2ERun -Action $act -Principal $prin -Force
schtasks /run /tn DmUiE2ERun
```

`Register-ScheduledTask` 需要 SYSTEM 身份；hakkoadmin 的非提权 runner 会 `Access is denied`。
双 runner 抢队列时脚本首行 `whoami` 判身份、不对就退出重推（见 `hakko-win-e2e` skill 坑 12）。

## 已知限制

- **账号流程需要 `debug-assertions` 构建**。release 下 `cfg!(debug_assertions)` 为假，
  `HttpAccountApi::new` 拒绝回环 http，账号服务根本构造不出来
  （`docs/129` F2）。要走通注册/登录，用
  `CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true cargo build -p dataminer-client --release`。
  代价：会同时跳过 `catalog.rs` 里 `#[cfg(not(debug_assertions))]` 的断言。
- **采集主干跑不了**。需要 NVENC；无 GPU 的 VM 上录制起不来（`docs/129` §5 P2）。
- **更新通道与 Sentry 不受回环收敛**，走查期间仍打真实网络。

## 加步骤

`cdp-drive.mjs` 里每一步都是 `await step("名字", async () => {...})`，返回值会原样进
`report.json`。惯用件：

- `evalJs(expr, awaitPromise?)` —— 在页面里求值
- `clickButton("按钮文案")` —— 返回 `clicked` / `disabled` / `not-found`
- `invokeCmd(name, args)` —— 直接打 Tauri 指令面，返回 `{ok, value|error}`
- `shot("名字")` —— 截图
- `OUTLINE_JS` —— 当前屏的紧凑大纲（标题、按钮及其 disabled、下拉项、输入框、告警、正文）

指令面的参数形状**不统一**（`docs/129` F7）：多数收 `{ request: {...} }`，
`collector_set_overlay_visible` 收 `{ visible }`，`view_scale_begin_session` 收 `{ kind }`。
加命令前先看调用点。

会开原生对话框或改动数据的命令**不要**放进清扫表——`collector_choose_capture_storage_location`
会弹模态目录选择框把整轮挂死，`collector_open_session_directory` 会在桌面上拉起 explorer，
`collector_delete_session` 是破坏性的。
