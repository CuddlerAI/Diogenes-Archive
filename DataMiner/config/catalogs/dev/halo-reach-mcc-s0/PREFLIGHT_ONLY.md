# 已编入预检列表 · S0 单作品条目尚未 D6 定稿

**状态更新 2026-08-04**：本 bundle 已登记于 shipped-games.json 编入 All Games DEV（2026-08-23 起随 dev-catalog 内嵌，逐游戏 feature 退役）
installer，操作员可以选择它做本机预检；产出固定 `non_trainable: true`。它是
[`../halo-mcc-steam-release-test/`](../halo-mcc-steam-release-test/)（六合一映射，已
SUPERSEDED）的单作品取代者，S0 不得退回六合一目录。

## 仍未定稿

- `window_class` / `executable_sha256` 仍为 `null`；本机已确认 Steam build 19905945 与
  `MCC-Win64-Shipping.exe`，但窗口身份和实际运行映像仍需 H-1 取证
- 按 #243 裁决：S0 主力游戏是 Halo Infinite，Reach/MCC 用作闭环评估环境
- mapping 仍需对照游戏内设置逐键核验

剩余流程：H-1 真机取证 → D6 升版定稿 → D7 smoke（docs/51）。在此之前只能用于
unsigned dev 预检，不得晋升 certified 或 trainable。
