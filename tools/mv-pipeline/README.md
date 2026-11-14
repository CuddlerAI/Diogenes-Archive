# mv-pipeline —— RAW→DATASET 方法验证管线（experimental）

> **method-validation only。** 产物永不可训练；计划、红线与正反方辩论见
> [model_research/research/20 号文](../../model_research/research/20-raw-to-dataset-method-validation-pipeline.md)，
> 执行记录见该文 §10。这不是 Forge：Forge v1（docs/12 P8）落地后本目录应并入或退役。

2026-07-27 首次执行：`diogenes-model-training` RAW（18 completed session）→
`diogenes-training-dev` `method-validation/mv-gameplay/v2026.07/`（25 shard / 2,128 chunk /
16.96 GB），另有 `v2026.07.1` 撤销演练版本。

## 组件

| 脚本 | 跑在哪 | 干什么 |
|---|---|---|
| `mv_pipeline_driver.mjs` | Windows VM（hakko-win-e2e） | 逐 session：presigned GET 下载 RAW → `dataminer-data-demo convert/verify --profile local-debug` → 调 assembler → presigned PUT 上传 → 清理。首个 session 做双跑字节确定性对拍。faulted session 预期被 converter 拒绝并记录 |
| `assemble_mv_dataset.mjs` | 同上（Node ≥20） | ready 会话 → per-session tar shard（自实现确定性 ustar：条目按 chunk 序、mtime=0）+ samples.jsonl + session-index.json。shard 不跨 session |
| `finalize_global.py` | mac（boto3） | 合并 per-session 索引 → `index/shards.json` + samples/framepairs parquet + MANIFEST + PROVENANCE + DATASHEET，上传并与桶内 listing 对账 |
| `s4_verify.py` | mac | ml 合规门验证：`gate_chunks` 无旗标必须 `ComplianceError`，显式 method-validation 旗标放行；chunk mp4 解码帧数 == p0.jsonl 行数 |
| `s5_revocation_drill.py` | mac | 撤销演练：撤 1 session → server-side copy 其余 shard 出新版本，验证新版本无泄漏、hash 不变、父版本不动 |
| `r2_rename_prefix.py` | mac | 前缀改名（脱敏对齐）：server-side copy 全量 → 校验齐全 → 删旧前缀，并回写 MANIFEST 的 `dataset`/`renamed_from`。字节不出 R2 |

## 约定与坑

- 凭据永不落 VM：源桶只读 presigned GET、dev 桶 presigned PUT（R2 需 **SigV4**——boto3
  必须 `Config(signature_version="s3v4", region_name="auto")`，默认签出的 v2 URL 会 401）。
- URL 里的 `&` 会被 cmd/sed 吃掉：VM 侧统一走 curl config 文件 + PowerShell 字面 here-string。
- 本地↔R2 单流吞吐极差且会断流：mac 侧上传用 boto3 multipart（128KB 分片 ×16 并发），
  下载加整对象重试；大流量一律放 VM（GCP↔R2 实测 ~1GB/9s）。
- 目标桶写凭据：GCP Secret Manager `dataminer-api-e2e-r2-*`（scoped 含 dev 桶）。
