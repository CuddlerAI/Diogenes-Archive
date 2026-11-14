// 带 `with { type: "json" }`：Vite 不需要它，但 `pnpm test` 走的是 Node 自己的
// ESM 加载器，缺了它会 ERR_IMPORT_ATTRIBUTE_MISSING，测试根本跑不起来。
import shippedGames from "../../../config/catalogs/shipped-games.json" with { type: "json" };
import type { GameSummary } from "./collector-contract";

/**
 * 一款内置游戏在**摄取面**的准入状态。数据源是
 * `config/catalogs/shipped-games.json`，由 `tools/check-shipped-games.mjs`
 * 在 CI 上与安装包的 feature 清单比对。
 *
 * 为什么客户端需要知道这件事：内置目录与服务端 `games` 表是两份互不知情的
 * 清单。漂移时采集员会看到一款游戏、选中、录完，然后每次会话注册都被 404
 * `game_not_registered` 拒绝——而那类拒绝在服务端数据库里不留任何行
 * （docs/101 §3），所以没有任何人会发现。2026-08-05/06 实际发生过 31 次。
 *
 * 这**不是一道闸**：真正的准入判定永远在服务端。这里只负责别让采集员白录一场。
 */
export type GameAdmission = "admitted" | "onboarding" | "unknown";

type ShippedGame = {
  gameId: string;
  displayName: string;
  admission: "admitted" | "onboarding";
  onboardingRef?: string;
};

const ADMISSION = new Map<string, ShippedGame>(
  (shippedGames.games as ShippedGame[]).map((game) => [game.gameId, game]),
);

/**
 * `unknown` 是刻意的第三态，且**按可用处理**（见 `isGameSelectable`）。
 * 一个不在清单里的 game_id 只能来自签名下发目录带来了新游戏——那种情况下
 * 服务端显然已经认识它，用一份滞后的本地清单去挡才是错的。
 */
export function gameAdmission(game: GameSummary | string): GameAdmission {
  if (typeof game === "string") {
    return ADMISSION.get(game)?.admission ?? "unknown";
  }
  return game.admission === "unknown"
    ? (ADMISSION.get(game.gameId)?.admission ?? "unknown")
    : game.admission;
}

/**
 * `GameSummary` carries authenticated server state and therefore fails closed
 * when neither the server nor the packaged fallback knows the game.  The
 * string overload remains the legacy static-roster helper for signed catalogs.
 */
export function isGameSelectable(game: GameSummary | string): boolean {
  const admission = gameAdmission(game);
  return typeof game === "string" ? admission !== "onboarding" : admission === "admitted";
}

/** onboarding 项的排期出处，用于在 UI 上说明「为什么这一款是灰的」。 */
export function onboardingRef(gameId: string): string | null {
  const game = ADMISSION.get(gameId);
  return game?.admission === "onboarding" ? (game.onboardingRef ?? null) : null;
}

/** Prefer the live server reason; use the packaged governance reference offline. */
export function onboardingReason(game: GameSummary): string | null {
  if (gameAdmission(game) !== "onboarding") return null;
  if (game.admission !== "unknown" && game.admissionReason) return game.admissionReason;
  return onboardingRef(game.gameId);
}

/** 下拉项的后缀。`admitted` / `unknown` 无后缀，保持列表干净。 */
export function admissionSuffix(game: GameSummary | string): string {
  const admission = gameAdmission(game);
  if (admission === "onboarding") return "（未开放，接入中）";
  return typeof game === "string" || admission === "admitted" ? "" : "（准入状态未知）";
}
