# 已被取代 · 研究快照，不进客户端

**取代者**：[`../bioshock-infinite-steam-e2e/`](../bioshock-infinite-steam-e2e/)

本 bundle 是更早的研究侧起草物，**故意不编入 Rust**。实际接入客户端的 BioShock catalog 是
同级的 `bioshock-infinite-steam-e2e/`（PR #157 新建，钉了本机实测的精确 EXE SHA-256，
真机验过发现 + 绑定链路）。

## 为什么不能并列编入

两套 bundle 同时编入会让操作员在下拉框里看到两个几乎同名的 BioShock，其中一个还不钉
EXE 哈希。[docs/54](../../../docs/54-control-plane-reconciliation-and-drift.md) 记录的采集机
漂移正是这类歧义的实例。

## 保留它的唯一理由

它仍是 `ml/player/tests/test_new_game_catalogs.py` 五游戏并集词表的输入。
**不要为它写 `include_bytes!`**——`verify-release-surface.ps1` 的 embedded-payload 门会放行
（文件受 git 跟踪），拦不住这个错误，所以它靠本文与 README 约束。
