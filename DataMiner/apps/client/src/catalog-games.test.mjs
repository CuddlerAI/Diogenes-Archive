import assert from "node:assert/strict";
import test from "node:test";
import {
  catalogRosterKey,
  reconcileGameSelection,
  startCatalogGameRefresh,
} from "./catalog-games.ts";

function game(gameId, admission = "admitted") {
  return { gameId, admission, admissionReason: null };
}

function catalog(overrides = {}) {
  return {
    provenance: "compiled",
    activeGeneration: 0,
    activeCatalogs: [],
    ...overrides,
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

test("catalog roster key changes when a delivered roster replaces the compiled roster", () => {
  const compiled = catalog({
    activeCatalogs: [{ catalogId: "fallout", catalogVersion: "1", contentHash: "sha256:a" }],
  });
  const delivered = catalog({
    provenance: "delivered",
    activeGeneration: 1,
    activeCatalogs: [{ catalogId: "halo", catalogVersion: "1", contentHash: "sha256:b" }],
  });

  assert.notEqual(catalogRosterKey(compiled), catalogRosterKey(delivered));
  assert.equal(catalogRosterKey(null), null);
});

test("game selection remains stable while its game is still active", () => {
  assert.deepEqual(reconcileGameSelection("fallout", [game("halo"), game("fallout")]), {
    selectedGameId: "fallout",
    retiredGameId: null,
  });
});

test("an empty initial selection adopts the first active game", () => {
  assert.deepEqual(reconcileGameSelection("", [game("halo"), game("fallout")]), {
    selectedGameId: "halo",
    retiredGameId: null,
  });
});

/**
 * 内置目录的排列顺序里，排第一的完全可能是一款服务端还没注册的游戏
 * （docs/101 §4.2：15 款内置里 7 款接入中）。默认选中它 = 采集员什么都没点
 * 就已经在录一场注定被 404 拒绝、且服务端零记录的数据。
 */
test("空选择的默认值跳过接入中的游戏", () => {
  assert.deepEqual(
    reconcileGameSelection("", [
      game("nioh2-steam", "onboarding"),
      game("halo-infinite-steam-release-test"),
    ]),
    { selectedGameId: "halo-infinite-steam-release-test", retiredGameId: null },
  );
});

test("整份名单都接入中时，宁可不选也不默认选一条死路", () => {
  assert.deepEqual(
    reconcileGameSelection("", [
      game("nioh2-steam", "onboarding"),
      game("halo-reach-mcc-s0", "onboarding"),
    ]),
    { selectedGameId: "", retiredGameId: null },
  );
});

/**
 * 已经选中的接入中项不强行改：到这一步说明是用户自己点的，UI 会显示成不可用
 * 并给出原因。悄悄换掉反而让人看不懂刚才发生了什么。
 */
test("用户自己选中的接入中项保持不变，由 UI 去解释", () => {
  assert.deepEqual(
    reconcileGameSelection("nioh2-steam", [
      game("nioh2-steam", "onboarding"),
      game("halo-infinite-steam-release-test"),
    ]),
    { selectedGameId: "nioh2-steam", retiredGameId: null },
  );
});

test("a game removed by catalog activation is cleared instead of remaining selectable", () => {
  assert.deepEqual(reconcileGameSelection("fallout", [game("halo")]), {
    selectedGameId: "",
    retiredGameId: "fallout",
  });
});

test("catalog refresh clears stale UI before loading and stays fail-closed on IPC failure", async () => {
  const load = deferred();
  const state = {
    games: [game("fallout")],
    candidates: ["fallout-window"],
    selectedGameId: "fallout",
    ready: true,
    error: null,
  };

  const refresh = startCatalogGameRefresh({
    selectedGameId: state.selectedGameId,
    loadGames: () => load.promise,
    onBegin: () => {
      state.games = [];
      state.candidates = [];
      state.selectedGameId = "";
      state.ready = false;
    },
    onSuccess: () => assert.fail("a failed load must not publish a roster"),
    onFailure: (reason) => {
      state.error = String(reason);
    },
  });

  assert.deepEqual(state.games, []);
  assert.deepEqual(state.candidates, []);
  assert.equal(state.selectedGameId, "");
  assert.equal(state.ready, false);

  load.reject(new Error("IPC failed"));
  await refresh.completion;
  assert.equal(state.error, "Error: IPC failed");
  assert.deepEqual(state.games, []);
  assert.equal(state.ready, false);
});

test("a cancelled stale refresh cannot overwrite a newer roster", async () => {
  const oldLoad = deferred();
  const newLoad = deferred();
  const applied = [];
  const callbacks = {
    onBegin: () => {},
    onFailure: (reason) => assert.fail(String(reason)),
  };

  const oldRefresh = startCatalogGameRefresh({
    ...callbacks,
    selectedGameId: "fallout",
    loadGames: () => oldLoad.promise,
    onSuccess: (games) => applied.push(games.map((item) => item.gameId)),
  });
  // Let the first IPC request start before the newer roster cancels it.
  await Promise.resolve();
  oldRefresh.cancel();
  const newRefresh = startCatalogGameRefresh({
    ...callbacks,
    selectedGameId: "fallout",
    loadGames: () => newLoad.promise,
    onSuccess: (games) => applied.push(games.map((item) => item.gameId)),
  });

  newLoad.resolve([game("halo")]);
  await newRefresh.completion;
  oldLoad.resolve([game("fallout")]);
  await oldRefresh.completion;

  assert.deepEqual(applied, [["halo"]]);
});
