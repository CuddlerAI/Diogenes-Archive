# 已被取代 · 研究快照，不进客户端

**取代者**：[`../wolfenstein2-steam/`](../wolfenstein2-steam/)

本 bundle 是研究侧起草物（`executable_sha256` / `window_class` 均为 `null`，name-only 识别）。
D6 exact-identity 版本是同级的 `wolfenstein2-steam/`，它钉了本机实测的 EXE SHA-256
（`68e6c21a…`）与真实窗口类，并已产出 D7 证据（[docs/43](../../../docs/43-d7-wolfenstein2-smoke.md)）。

## 为什么不能并列编入

同 BioShock：下拉框里两个几乎同名的 Wolfenstein II，其中一个不钉身份。

## 保留它的唯一理由

`test_new_game_catalogs.py` 的并集词表输入。**不要为它写 `include_bytes!`**。
