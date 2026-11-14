# delivery · 交付形态工具 + R2 交付桶（TOS 过渡替代）

## engine_delivery 工具

```bash
cd engine-data/delivery

# 采集目录 → 恰好 12 文件的交付目录（+ 台账行 + 元信息骨架）
python3 -m engine_delivery pack-mc <采集次级文件夹> --out <交付根> \
    --game MegacityMetro --group 2026_08_04_megacity_01 \
    --prefix internal-validation --qc-verdict PASS \
    --ledger delivery-ledger.csv --gameinfo gameinfo.json

# 审交付树形态：本地目录，或一份对象键清单（直接审 R2/TOS 前缀，不必把数据拉回来）
python3 -m engine_delivery audit-mc <交付根>
python3 -m engine_delivery audit-mc --keys keys.txt

# 判一次 depth 采集能不能转成 68 号交付形态
python3 -m engine_delivery audit-depth <采集 run 目录>
```

三件事分开的理由：

- **打包是白名单拷贝**，不是排除已知文件。采集目录里除 12 必要文件外还有
  `plan.json`（我方轨迹生成参数）/ `provenance.json`（本机绝对路径 + 场景授权标记）
  等内部件；整目录 `rclone copy` 就会把它们一起送出门。黑名单会随采集侧新增中间件失效。
- **形态审计与内容质检分开**：`engine_qc` 看一条数据内部对不对，本工具只看它被摆成
  什么样子。内容对但摆错位置 = 入库路径不对、整批返工；混在一个工具里前者会被后者盖掉。
- **depth 审计判的是原料不是产物**：`depth-lab deliver` 要求命令行给投影参数并拒绝
  `assumed` 出门，但它拦不住「采集当时就没记下这些参数」——数据已落盘、投影矩阵没跟着落，
  事后补不回来。这条判定要在进 deliver 之前给出。

对已在库数据实跑的结果见
[docs/87](../../DataMiner/docs/87-collected-data-vs-target-gap-audit.md)。

## R2 交付桶

**桶：`diogenes-engine-delivery`**（已于 2026-08-03 创建；用户裁决先用 R2 代替 TOS，
凭据/上传规范到位后平移切换——结构与 TOS 目标一致，切换即 rclone 改 remote 纯拷贝）。

## 目录结构

SFM 侧（docs/68 §8 规范）：

```
SFM/<运镜种类>/<YYDD>/<场景>/<次级文件夹>/          # 正式
SFM-demo/<运镜种类>/<YYDD>/<场景>/<次级文件夹>/     # demo 阶段
```

MC 侧（🟩 2026-08-04 已读到《【multi-camera2期】引擎数据上传文件步骤及要求》原件，按实际规范更新，
替换此前的暂定镜像）：

```
2MultiCamera/<gameProcessName>/<MMDD 如 0411>/<时间_场景姓名拼音_标注序号>/   # 组命名如 2026_03_11_xingqiuchongqi_01
```

- MC 实际交付走**火山引擎 TOS 平台**（供应商专属账号+项目空间；开通方式=把「项目名、供应商名、
  权限名单」提交给对接人木必中/郭明超/张阔）；demo 与正式分不同顶层文件夹
- 上传后需提交：汇总所有场景的元信息文件 + 按日期分 sheet 的正式交付表单（按模板命名）到对接群
- 本 R2 桶按同结构做暂存/演练，TOS 账号开通后平移
- 运镜种类目录名严格取 docs/68 §3.3/3.4 统一轨迹名（QC 白名单断言）
- 次级文件夹 = 一条完整数据（`{scene}-{traj}-{seed}-{ts}`，全局唯一、互不重复）
- MC 次级文件夹内 12 文件：`video_N.mp4 + systeminfo_N.json + action_camera_N.json`（N=1..4）
- 本桶只做交付暂存，**与训练桶（diogenes-model-training 等）严格隔离，桶内数据不入训练管线**

## 凭据配方（不落配置文件，与既有 R2 作业一致）

凭据在 GCP Secret Manager（项目 `hakkolite`）：`r2-training-{account-id,access-key-id,secret-access-key}`。
注意 gcloud 在本机代理下 `secrets list` 易挂，**按名字直读并重试 3–5 次**：

```bash
fetch(){ for i in 1 2 3 4 5; do v=$(gcloud secrets versions access latest --secret="$1" --project=hakkolite 2>/dev/null) && { echo "$v"; return; }; sleep 2; done; return 1; }
export RCLONE_CONFIG_R2_TYPE=s3 RCLONE_CONFIG_R2_PROVIDER=Cloudflare RCLONE_CONFIG_R2_REGION=auto
export RCLONE_CONFIG_R2_ACCESS_KEY_ID=$(fetch r2-training-access-key-id)
export RCLONE_CONFIG_R2_SECRET_ACCESS_KEY=$(fetch r2-training-secret-access-key)
export RCLONE_CONFIG_R2_ENDPOINT="https://$(fetch r2-training-account-id).r2.cloudflarestorage.com"
rclone copy <本地次级文件夹> "r2:diogenes-engine-delivery/SFM-demo/<运镜种类>/<YYDD>/<场景>/<次级文件夹>/" -P
```

⚠️ 这把凭据是**账号级读写且能建删桶**（见 memory/仓内审计记录）——只用于本桶路径，
其他桶一律视为只读红线。跨境慢（ETA>1h）走 cloud-relay skill 的 GCE 中转。

## TOS 切换清单（凭据到位时）

1. docs/72 B1/B3 拿到：69 上传规范原件 + TOS 凭据/bucket/S3 端点
2. 若 MULTI 路径结构与 69 规范不一致 → 先按规范重排本桶再搬运
3. `rclone copy r2:diogenes-engine-delivery/<前缀> tos:<bucket>/<前缀>`（或 volc SDK/tosutil）
4. 交付表按《正式交付数据》模板回填，provenance 索引同步留档
