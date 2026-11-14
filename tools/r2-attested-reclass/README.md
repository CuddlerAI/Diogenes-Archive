# r2-attested-reclass — 存量第一方语料的具结车道迁移

把 `diogenes-model-training` 桶内的第一方存量语料（Halo 生产摄取 + COD manual dev）
服务端复制进具结车道 `raw/v1/attested-trainable/`，并在车道根写入机器可读的
`WAIVER.json`（owner 具结：第一方采集、已获授权可训练）。

**为什么是复制而不是改标识**——治理与机制依据见
[research/44 §3](../../model_research/research/44-halo-100h-playability-eval-and-stock-attestation.md)：

- [93 号裁决](../../DataMiner/docs/93-halo-trainable-lane-unblock-decision.md)二：封存对象
  与 DB 行一律不改写（车道断言可信性）；
- 六件套内容寻址（`meta-<sha256>.json` 文件名即内容哈希，manifest 逐件引用），原地改写
  必破完整性校验；
- DB 侧 `clip_trainability`（migration 0032）fail-closed，consent 时间窗判据使事后补签
  产生不了 `is_trainable=true`——追溯解禁在数据库层面机制性不可能。

训练面从 `attested-trainable` 车道取数，取数资格由 WAIVER 承载（与 COD owner 具结
先例同构）。COD 原有 `no_publish` / `no_product_training` 具结已由 owner 2026-08-10
两次明示全部解除（见 research/44 §3.4），现与 Halo 同等待遇，仅保留 DATASHEET 披露纪律。
外部开源镜像（`diogenes-external-datasets`、HF 镜像）许可证禁训，**永不进入本车道**。

## 凭据

凭据走 GCP Secret Manager（项目 `hakkolite`）的 `r2-training-*` 四件套，配成
`RCLONE_CONFIG_R2_*` 环境变量，不落配置文件：

```bash
for s in account-id access-key-id secret-access-key bucket; do
  eval "$(echo $s | tr - _)=$(gcloud secrets versions access latest \
    --secret=r2-training-$s --project=hakkolite)"
done
export RCLONE_CONFIG_R2_TYPE=s3 RCLONE_CONFIG_R2_PROVIDER=Cloudflare RCLONE_CONFIG_R2_REGION=auto
export RCLONE_CONFIG_R2_ENDPOINT="https://${account_id}.r2.cloudflarestorage.com"
export RCLONE_CONFIG_R2_ACCESS_KEY_ID="$access_key_id"
export RCLONE_CONFIG_R2_SECRET_ACCESS_KEY="$secret_access_key"
export R2_TRAINING_BUCKET="$bucket"
```

（`gcloud secrets list` 在本机代理下几乎必失败，按名直读即可；偶发失败重试 3–5 次。）

## 用法

```bash
./reclass.py plan       # 只读盘点 src/dst 读数，不动数据
./reclass.py execute    # 服务端复制（幂等增量，可中断重跑）+ 校验 + 写 WAIVER
./reclass.py verify     # 只读复核 src/dst 一致性
```

- `execute` 在任一来源校验不过时**不写 WAIVER**并以非零码退出；重跑即续传。
- 源前缀仍在持续摄取时，verify 出现差额属预期；重跑 `execute` 追平并刷新 WAIVER 读数。
- `--source halo-production-ingest` 可单独跑某一来源（此时不写 WAIVER）。

## game_filter（research/50 §4.3 P1 / §6 R1–R3，docs/141 §5）

生产摄取前缀 `dataminer-api-production/sessions` 已多游戏混杂
（halo/fallout4/cyberpunk2077/w2/wuchang/cod），而 **FO4 与 CP2077 未过权利闸**——
整前缀复制会造成无人裁决的权利越界。因此 `SOURCES` 条目支持可选 `game_filter`
（game id 白名单，按 meta 的 `catalog.game.id` 判定）；带该字段的来源不再整前缀
复制，改走对象级白名单：

1. `rclone lsf -R` 列举 src 前缀全量对象，按 `sessions/<uuid>/` 分组；
2. 每 session 取一份 `meta-*.json`（`rclone cat`）读 `catalog.game.id`，得
   session→game 映射。映射由内容寻址对象决定、不可变，cache 到本地
   `~/.cache/r2-attested-reclass/session-game-<bucket>-<name>.json`——重跑只补
   新 session，不重读旧 meta；删掉 cache 文件即强制全量重读；
3. **game 读不出的 session（meta 无 catalog 块 / 无 meta 文件）一律排除**并逐个
   列入报告——宁可漏放不可错放；
4. 只把白名单 session 的对象用 `rclone copy --files-from --size-only` 复制进车道。

**对账口径随之改变**：src 侧读数 = 白名单清单的 count/bytes（不再 `rclone size`
整前缀）；dst 侧 = 复制后 dst 前缀全量读数。dst 前缀里已有 2026-08-11 整前缀复制
遗留的**越界对象**（research/50 §6 R2/R3：8,744 clip 中混有 cod 6 / w2 67 /
wuchang 7 / 无 target 孤儿 1），所以 **dst ≥ src 白名单属预期**。verify 的 PASS
判据是「白名单内逐对象存在且 size 一致」；dst 侧白名单外对象**单独报告清单**
（stdout 按 session 汇总 + 全量清单落
`~/.cache/r2-attested-reclass/out-of-scope-<bucket>-<name>.json`）——
**只报告不删除**，是否从车道清除另行裁决（docs/141 §5），消费侧按 game 过滤兜底。

WAIVER 变化：带 `game_filter` 的 `sources[]` 条目写入 `game_filter` 字段、
`objects/bytes` 用白名单口径、`known_out_of_scope_in_lane` 记录 execute 复制后
逐对象核对当场测得的越界读数；顶层新增 `refreshed_at`（本次执行的 UTC 日期），
`attested_at` 保持 2026-08-10 不变（具结时点不因读数刷新而漂移）。

`cod-manual-dev` 自 2026-08-13 起不再是单游戏前缀（混入 21 个 FO4 depth-lab
session，research/50 §2.1），同样带 `game_filter`，但布局是 `manual`（journal
镜像）：session 键在 `non-trainable-sessions/` 之后一段，game 证据读
`<sid>/session.json` 的 `catalog.game.id`（与 API 布局 clip meta 的 catalog 块
同形）；`non-trainable-sessions/` 标记之外的设备级文件（`journal.sqlite3` 等
客户端台账）不属于任何 session，恒放行（08-11 首次复制先例）。

## 测试

不碰网络的离线单测（lsf 解析、session 分组、白名单、对账、WAIVER 生成、
mock rclone 的 execute 全链路）：

```bash
python3 tests/test_reclass.py
```

## 执行史

| 日期 | 操作 | 读数 |
|---|---|---|
| 2026-08-10 | 首次 execute | 死于代理链路 EOF ×8 + 重试整树重拷（modtime 比对陷阱，见 research/44 §3.5）；由此给 copy 加 `--size-only` |
| 2026-08-11 | execute 重跑 + verify | ✅ halo 52,464 obj / 174.31 GiB；COD 11,054 obj / 36.90 GiB；两来源 src/dst 一致，WAIVER 已写（COD 限制按 08-10 owner 指令解除） |
