import type { CatalogStatus, GameSummary } from "./collector-contract";
// 显式带 `.ts`：`pnpm test` 用 Node 自己的 ESM 加载器，它不做扩展名推断。
// 上面那行 `import type` 会被整行剥掉所以不受影响，这一行是值导入，必须写全。
import { isGameSelectable } from "./game-admission.ts";

export type GameSelectionReconciliation = {
  selectedGameId: string;
  retiredGameId: string | null;
};

export type CatalogGameRefresh = {
  cancel: () => void;
  completion: Promise<void>;
};

type CatalogGameRefreshOptions = {
  selectedGameId: string;
  loadGames: () => Promise<GameSummary[]>;
  onBegin: () => void;
  onSuccess: (
    availableGames: GameSummary[],
    reconciliation: GameSelectionReconciliation,
  ) => void;
  onFailure: (reason: unknown) => void;
};

/**
 * A catalog identity is immutable, so this key changes exactly when the
 * backend's active game roster may have changed. Grant/freshness-only snapshot
 * updates intentionally do not make the UI reload the same list every 500 ms.
 */
export function catalogRosterKey(catalog: CatalogStatus | null): string | null {
  if (!catalog) return null;
  return JSON.stringify({
    provenance: catalog.provenance,
    activeGeneration: catalog.activeGeneration,
    activeCatalogs: catalog.activeCatalogs.map((identity) => [
      identity.catalogId,
      identity.catalogVersion,
      identity.contentHash,
    ]),
  });
}

/**
 * Keep a still-active selection, initialize an empty selection, or retire a stale one.
 *
 * 两处与准入相关的刻意选择：
 *
 * 1. **空选择的默认值跳过 onboarding 项**。原来取 `availableGames[0]`，而内置
 *    顺序里排第一的完全可能是一款服务端还没注册的游戏——那样一打开客户端就
 *    默认选中了一条死路，采集员没做任何选择就已经在录废数据了。
 * 2. **已经选中的 onboarding 项不强行改**。到这一步说明用户是自己点的，
 *    UI 会把它显示成不可用并给出原因；这里悄悄换掉反而让人看不懂发生了什么。
 *    真正的拦截在服务端，客户端只负责别替人选错。
 */
export function reconcileGameSelection(
  selectedGameId: string,
  availableGames: readonly GameSummary[],
): GameSelectionReconciliation {
  if (selectedGameId && availableGames.some((game) => game.gameId === selectedGameId)) {
    return { selectedGameId, retiredGameId: null };
  }
  if (!selectedGameId) {
    const firstSelectable = availableGames.find(isGameSelectable);
    return {
      selectedGameId: firstSelectable?.gameId ?? "",
      retiredGameId: null,
    };
  }
  return { selectedGameId: "", retiredGameId: selectedGameId };
}

/**
 * Invalidate the visible roster synchronously, then publish only the latest
 * successful load. This keeps a delivered-catalog switch fail-closed even when
 * the IPC call is slow, fails, or completes after a newer refresh starts.
 */
export function startCatalogGameRefresh(
  options: CatalogGameRefreshOptions,
): CatalogGameRefresh {
  let cancelled = false;
  options.onBegin();

  const completion = Promise.resolve().then(async () => {
    if (cancelled) return;
    try {
      const availableGames = await options.loadGames();
      if (cancelled) return;
      options.onSuccess(
        availableGames,
        reconcileGameSelection(options.selectedGameId, availableGames),
      );
    } catch (reason) {
      if (!cancelled) options.onFailure(reason);
    }
  });

  return {
    cancel: () => {
      cancelled = true;
    },
    completion,
  };
}
