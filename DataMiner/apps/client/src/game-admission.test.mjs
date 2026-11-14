import assert from "node:assert/strict";
import test from "node:test";
import {
  admissionSuffix,
  gameAdmission,
  isGameSelectable,
  onboardingRef,
  onboardingReason,
} from "./game-admission.ts";
import shippedGames from "../../../config/catalogs/shipped-games.json" with { type: "json" };

test("已在生产库注册的游戏可选且无后缀", () => {
  assert.equal(gameAdmission("halo-infinite-steam-release-test"), "admitted");
  assert.equal(isGameSelectable("halo-infinite-steam-release-test"), true);
  assert.equal(admissionSuffix("halo-infinite-steam-release-test"), "");
  assert.equal(onboardingRef("halo-infinite-steam-release-test"), null);
});

test("原接入中游戏开放后，离线兜底也可选且不再显示排期", () => {
  assert.equal(gameAdmission("nioh2-steam"), "admitted");
  assert.equal(isGameSelectable("nioh2-steam"), true);
  assert.equal(admissionSuffix("nioh2-steam"), "");
  assert.equal(onboardingRef("nioh2-steam"), null);
});

/**
 * 清单之外的 game_id 只能来自签名下发目录带来了新游戏。那种情况下服务端显然
 * 已经认识它，用一份滞后的本地清单去挡才是错的——所以 unknown 必须可选。
 */
test("清单里没有的游戏按可用处理，不被本地清单挡住", () => {
  assert.equal(gameAdmission("some-future-game-steam"), "unknown");
  assert.equal(isGameSelectable("some-future-game-steam"), true);
  assert.equal(admissionSuffix("some-future-game-steam"), "");
  assert.equal(onboardingRef("some-future-game-steam"), null);
});

test("清单本身自洽：gameId 唯一、admission 取值合法、onboarding 必须给出处", () => {
  const ids = shippedGames.games.map((game) => game.gameId);
  assert.equal(new Set(ids).size, ids.length, "gameId 有重复");
  for (const game of shippedGames.games) {
    assert.ok(
      game.admission === "admitted" || game.admission === "onboarding",
      `${game.gameId} 的 admission 非法：${game.admission}`,
    );
    if (game.admission === "onboarding") {
      assert.ok(game.onboardingRef, `${game.gameId} 是 onboarding 但没写 onboardingRef`);
    }
  }
});

/**
 * 这条锁的是本次修复的具体事故形态：2026-08-05/06 有一台客户端 31 次会话注册
 * 被 404 拒绝且服务端零记录（docs/101 §3）。只要清单里还有 onboarding 项，
 * 客户端就必须能把它们标出来——如果哪天全都 admitted 了，这条测试自然放行。
 */
test("所有 onboarding 项都会被判为不可选", () => {
  const onboarding = shippedGames.games.filter((game) => game.admission === "onboarding");
  for (const game of onboarding) {
    assert.equal(isGameSelectable(game.gameId), false, `${game.gameId} 应当不可选`);
  }
});

function liveGame(gameId, admission = "unknown", admissionReason = null) {
  return { gameId, admission, admissionReason };
}

test("live server admission overrides the packaged fallback", () => {
  const newlyOpened = liveGame("nioh2-steam", "admitted");
  assert.equal(gameAdmission(newlyOpened), "admitted");
  assert.equal(isGameSelectable(newlyOpened), true);
  assert.equal(admissionSuffix(newlyOpened), "");
});

test("server onboarding reason is shown directly", () => {
  const paused = liveGame(
    "halo-infinite-steam-release-test",
    "onboarding",
    "capture_not_allowed",
  );
  assert.equal(isGameSelectable(paused), false);
  assert.equal(onboardingReason(paused), "capture_not_allowed");
});

test("unknown live state falls back to the production-verified shipped roster", () => {
  const offline = liveGame("fallout4-steam-release-test");
  assert.equal(gameAdmission(offline), "admitted");
  assert.equal(isGameSelectable(offline), true);
  assert.equal(onboardingReason(offline), null);
});

test("an unrecognized live game fails closed while admission is unknown", () => {
  const future = liveGame("future-game");
  assert.equal(gameAdmission(future), "unknown");
  assert.equal(isGameSelectable(future), false);
  assert.equal(admissionSuffix(future), "（准入状态未知）");
});
