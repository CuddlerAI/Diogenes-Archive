# 已被取代 · 研究快照，S0 采集明令禁用

**取代者**：[`../halo-reach-mcc-s0/`](../halo-reach-mcc-s0/)（单作品条目）

## 为什么它对任何单作品采集都是错的

本 bundle 的 mapping 是 MCC **六款游戏的最小公倍数**，对任何单作品采集至少有一处错误：

- `move.sprint`：CE / H2 / H3 / ODST **根本没有冲刺**；Reach 的冲刺是**护甲能力之一**（非独立键）
- `LShift` 的语义逐作不同：H3 = 使用装备｜Reach = 使用护甲能力（**还随 loadout 变**）｜H4 = 冲刺
- MCC 的键位、V-Sync、FOV、画质**全部按作品分页**，不是一套配置

依据与配额修订见
[research/halo/s0-mcc-primary-revision.md](../../../../../model_research/research/halo/s0-mcc-primary-revision.md)。
另按 #243 裁决，S0 主力游戏是 Halo Infinite，MCC 回收为闭环评估环境。

## 保留它的唯一理由

跨作品泛化研究参考 + `test_new_game_catalogs.py` 的并集词表输入。
**不要为它写 `include_bytes!`**。
