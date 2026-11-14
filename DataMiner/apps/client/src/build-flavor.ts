/**
 * 构建风味（build flavor）——UI 上那个环境徽章的唯一来源。
 *
 * 以前徽章是两处硬编码的 "UNSIGNED DEV · NON-PRODUCTION" 字符串。硬编码的问题不是
 * 难改，而是**它和产物之间没有任何联系**：谁都可以把它改成 "PRODUCTION"，而安装包
 * 依然未签名、依然在安装时弹「非生产开发版」的警告、文件名依然带
 * `UNSIGNED_NON_PRODUCTION`。界面就这样对自己的认证状态作出与事实相反的声明。
 *
 * 现在它由构建期注入，并且 `tools/build-windows-installer.ps1` 在打包结束后会断言
 * 产物真实的 Authenticode 状态与这里的风味一致（unsigned-dev 必须 NotSigned，
 * production 必须是有效签名）。不一致就构建失败。于是「production 徽章 + 未签名
 * 产物」这个组合在构造上就不可能被发布出去。
 *
 * 默认值是 unsigned-dev：忘了设环境变量的构建只会低估自己，不会高估。
 * `unsigned-delivered` 是线上验签目录口味；它仍显示 UNSIGNED，但不再把
 * 「没有 Authenticode」错写成「不连生产」。
 *
 * ── 徽章文案只陈述「有没有签名」这一件事 ──────────────────────────────
 *
 * 以前徽章写的是 "UNSIGNED DEV · NON-PRODUCTION"，把两件互相独立的事焊成了一句话。
 * 签名解决的是「Windows 安装时要不要弹警告」；「连的是不是生产环境」是另一回事，
 * 而客户端**默认就连生产** API（`services/api_endpoint.rs` 的
 * `DEFAULT_PRODUCTION_API_BASE_URL`，仅可由 `DATAMINER_API_BASE_URL` 在运行期改写）。
 * 于是一个 release 编译、连生产、真在给生产库上传的客户端，界面上却自称
 * DEV / NON-PRODUCTION——采集员据此会以为自己录的东西不算数。
 *
 * 这里只删掉不成立的那半，保留成立的那半：徽章现在只说 UNSIGNED。
 *
 * **不在编译期断言「这是生产」**，因为 API 地址是运行期可改的：写死就等于把当初
 * 这个模块要根治的「界面对自己作出与事实相反的声明」换个方向再犯一次。要展示实际
 * 连的是哪个环境，正确做法是把运行期的 base URL 显示出来，那是另一件事。
 *
 * `BuildFlavor` / `IS_PRODUCTION_BUILD` 只描述 Authenticode/UI 身份，不参与 Depth
 * 能力判断；在线 unsigned 与未来已签名 production 必须具备同一采集能力。
 */

declare const __DATAMINER_BUILD_FLAVOR__: string;

export type BuildFlavor = "unsigned-dev" | "unsigned-delivered" | "production";

function readFlavor(): BuildFlavor {
  // typeof 守卫：vitest / ts-node 等没走 Vite define 的环境下不至于抛 ReferenceError。
  const raw = typeof __DATAMINER_BUILD_FLAVOR__ === "string" ? __DATAMINER_BUILD_FLAVOR__ : "";
  if (raw === "production" || raw === "unsigned-delivered") return raw;
  return "unsigned-dev";
}

export const BUILD_FLAVOR: BuildFlavor = readFlavor();

export const IS_PRODUCTION_BUILD = BUILD_FLAVOR === "production";

/** 标题栏用：全大写、mono、带分隔点。生产构建不显示环境徽章——没有需要警示的事。 */
export const BUILD_BADGE_UPPER: string | null = IS_PRODUCTION_BUILD
  ? null
  : "UNSIGNED";

/** 侧栏品牌块下方那行小字。 */
export const BUILD_BADGE_TITLE: string = IS_PRODUCTION_BUILD
  ? "Local Collector"
  : BUILD_FLAVOR === "unsigned-delivered"
    ? "Online Collector · Unsigned"
    : "Local Collector · Unsigned";
