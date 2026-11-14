# 外部转换工具契约(mac / 非 Windows 研发机)

> docs/102 §6 PR-C。背景:仓库禁 FFmpeg(AGENTS.md),水印提取与叠加预览的官方实现
> 走 Windows Media Foundation(`dm-depth-extract`)。研发机(mac)在 0806–0807 用
> FFmpeg+numpy 的外部脚本完成了同等工作并产出两条业务核验包。**脚本本体不入库**;
> 本文入库的是「契约」——任何人可据此重建外部实现,并用仓库内工具验证其正确性。

## 1 · 水印提取(video.mp4 → watermark-samples.jsonl)

输出行:`{"session_frame": <解码顺序帧号,从 0>, "block_means": [<24 个 u8>]}`

| 项 | 契约值 | 来源 |
|---|---|---|
| 采样矩形 | 视频左上 `(origin_x, origin_y)` 起 192×8 px(24 块 × 8px) | `watermark::{BLOCK_PX, TOTAL_BLOCKS, RECT_W, RECT_H}` |
| 块均值 | 块中心 4×4 像素:x ∈ `[bx+2, bx+6)`,y ∈ `[oy+2, oy+6)` | `dm-depth-extract::block_mean` |
| 亮度 | **逐像素**整数运算 `(r*299 + g*587 + b*114) / 1000`(截断),再 16 像素整数均值(截断) | 同上;浮点亮度或先均值后转亮度都会在边界值上翻位 |
| 帧序 | 解码器输出顺序即 `session_frame`,不得跳帧不得去重 | MF 与 FFmpeg 对 CFR H.264 的输出帧序一致 |

**验收方法(必须执行)**:产出的 samples 交 `depth-lab pair` 跑一遍,
`watermark.decoded == 总帧数` 且 `sync_mismatch == 0` 才算提取正确。
0806 实测:39.1s/120s×2 三条 clip 均 100% 解码零失配;判决(阈值、绕回展开)全部在
仓库内 `watermark::decode`/`unwrap_seq` 完成,外部脚本只负责「把像素量出来」。

## 2 · RGB×Depth 叠加预览(QC 随包件)

- 输入:交付 EXR(线性米制)+ 对应偶数帧 RGB(缩放到 1280×720);
- 深度着色:`t = 1 − (ln z − ln 0.2) / (ln 5100 − ln 0.2)`(近亮远暗),NaN → 品红 `(200,0,200)`;
- 合成:`0.5·RGB + 0.5·vis`,15fps(每偶数帧一张);
- 用途:68 号 §7.2「无重影」目检证据,业务包 QC 目录必带。

## 3 · 等价线性化(docs/102 裁决 B,过渡期)

v5 索引被认证门拒收期间,业务包深度由外部实现转换。契约:

```
d = 1 − s                    (storage.transform = one_minus_d)
z = proj_b / (d − proj_a) × metric_scale     (与 depth.rs::linearize 逐位一致)
无效:s == 0(清屏值)→ NaN;z 非有限或 ≤0 → NaN
量化:trunc(z×100)/100 → fp16,单通道 Z,EXR ZIP(块 16 行,delta+interleave 预测器)
参数:逐帧取 depth-index 行内值,禁止 run 级常量覆盖
```

**退役条件**(裁决 B-2):v6 索引的新采集产出后,官方 `deliver` 与外部实现对同段数据
逐字节对账一次,通过即退役,只保留公式测试。

## 4 · 已知产出(实物锚点)

`r2:diogenes-tempbucket/depth-demo-delivery-20260806/` 下三条 clip 的
`QC/depth-conversion-manifest.jsonl` 含逐帧参数与 sha256,可作重建后的对账基准。

## 5 · v6 索引 raw EXR 的读取契约(2026-08-11 增补,issue #709 实证)

probe 0.7.4 起 raw 深度以 **EXR 单通道 `Z`·HALF·ZIP(scanline,16 行/块)** 落盘
(`storage: {encoding: exr-half-zip, domain: raw_buffer, transform: one_minus_d}`),
线性化仍走 §3 公式,参数逐帧取 depth-index v6 行内 `projection.{proj_a,proj_b,metric_scale}`。

- ⚠️ **OpenCV 读不了单通道 `Z` 的 EXR,会静默返回全 0**(它只映射 R/G/B/Y/A)——
  用 cv2 做这一步会把整段深度转成 NaN 还不报错。必须用最小 EXR 读取器
  (与 `engine-data/qc/engine_qc/exr.py` 同构:zlib 解块 → 一阶反预测器 → 反字节重排),
  并对照该参考实现做**逐位一致**自检后再批量转换。
- 写出侧 cv2 可用(单通道写成 `Y`,客户允许 Y/Z;HALF+ZIP 参数显式传)。
- 0012 实证锚点:`~/Documents/diogenes-capture-runs/issue-709-fo4-demo-20260811-delivery/`
  (五件套+scripts/+QC 叠加图),客户脚本全项通过;已知遗留 `depth.align` 恒 +1
  (亚帧相位差,#717),整数重映射不可修。
