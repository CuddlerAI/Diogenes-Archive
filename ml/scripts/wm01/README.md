# wm01 执行工件（kda-bench-a100 侧，2026-08-02 点火轮）

| 文件 | 用途 |
|---|---|
| `patch_longlive_lora.py` | 给 LongLive `trainer/diffusion.py` 移植 LoRA(adapter) 支持——逐段镜像上游 `trainer/distillation.py` 的既有模式（wrap 在 FSDP 前、LoRA-only 存档、logdir 断点续跑）。diffusion trainer 上游只有全参训练。应用前先备份 `diffusion.py.orig` |
| `phase0full.sh` | Phase 0 全量零样本基线：holdout 126 clip 展平成逐 clip 条目 + `save_with_index` 命名 + `inference_iter=-1`。冒烟版陷阱：数据集把会话当条目（2 个）且同 caption 互相覆盖，只出 1 条 rollout |
| `phase1.sh` | Phase 1 域适配 LoRA（34 号文配方）：rank64/alpha64、lr 1e-5、bs1×accum4×4 卡、4,000 步、每 500 步存、EMA 关、训练期评估关 |
| `e1a_arm_rollout.py` | E1a 消融臂评测 rollout（VM 实跑终版）：holdout 126 clip 真值动作条件化 I2V；keyboard 分支全跨度精确 / mouse 分支块截断近似，`--selfcheck` 与 vendor 整段前向逐位一致；写 `pairs.txt` 真实 idx→clip 映射（dataset 枚举序≠sorted 的对位错排修复）；`--allow-cross-arm` 放行三臂统一真值协议 |
| `postarm_eval.sh` | 臂后自动评测（VM 实跑终版）：等 ckpt_2000 + 训练单元退出 → rollout → 按 pairs.txt 重链 flat 名 → `score_rollout` 评分 → 产物上 GCS。用法 `ARM=<臂> UNIT=<单元> bash postarm_eval.sh` |
| `perclip_e1a.py` | 三臂 E1a 逐 clip 分析 + 配对检验（终版统计口径）：池化判据档（\|Δ\|<p90）耦合 + clip 级配对 bootstrap ×2000，替代有小样本正偏的逐 clip \|r\| 均值 |

VM 侧另有 `phase0.sh`（冒烟版，sha256 验包 + holdout 切分的出处）与 `wm01_5b.sh`（权重下载）。
执行顺序与判据见 [34 号文](../../../model_research/research/34-wm01-training-method.md)；
5B 代码路径硬性要求 flash-attn（`assert FLASH_ATTN_2_AVAILABLE`，SDPA 回退只在 vendored MG3 模块里），
VM 上编译 flash-attn 2.8.3 约 1.5h（`wm01-flash.service`）。

## 点火轮实测踩坑（2026-08-02）

1. **torchvision 0.27+ 移除 `write_video/read_video`**，LongLive 训练链
   （`pipeline/self_forcing_training.py`、两个 trainer）在模块导入期静态 import 它——
   `train.py` 四 rank 全灭。上一轮只在 `inference.py` 顶部放了 imageio 猴补丁（这也是
   phase0 推理能跑通的原因）。修法：把该 shim 块抽成 `LongLive/tv_video_shim.py`，
   `train.py` 第 1 行 `import tv_video_shim`（必须先于一切项目 import）。
2. **Phase 0 冒烟版只出 1 条 rollout 的两个原因**（`phase0full.sh` 注释同款记录）：
   数据集把会话当条目（2 个），且同 caption 的输出文件名互相覆盖。
3. Phase 0 全量实测：126 clip 单卡 24m42s（11.7 s/it），A0 分母齐。
4. **镜像 `/etc/profile.d/nccl_env.sh` 是为 a3u/gIB（IB 机型）写的**，login shell（`bash -lc`）
   会带进 `NCCL_NET=gIB` **和 `LD_LIBRARY_PATH=/usr/local/gib/lib64`**——前者让无 IB 的 a2
   直接 `Failed to initialize any NET plugin`，后者把 gib 库影子进 torch 动态链接，报错更早更哑。
   训练脚本里两者都要 unset，systemd 单元用非 login 的 `bash -c`。
5. **上游 sp 校验**：`潜帧数 % (sequence_parallel_size × num_frame_per_block) == 0`。24 潜帧
   过不了 sp=4×nfpb=8；`sequence_parallel_size: 1`（4 卡改 DP）正合 34 号文「有效 batch 16」。
6. **peft 0.20 × torchao**：venv 里残留 torchao 0.13（NVFP4 轮的）会让 peft 的
   `is_torchao_available()` 在 LoRA 注入时直接抛 ImportError（<0.16 不是返回 False 是 raise）。
   升到 torchao 0.17 解决；cutlass_90a/mxfp8 .so 加载失败是 H100 专用内核，A100 上无害。
7. 调试期在同一台机上手动 `torchrun --standalone` 复现会与 systemd 单元抢 29400 rendezvous
   端口，互相秒杀且 elastic 摘要不写明——复现一律 `timeout` 包裹并等它死透再挂单元。
8. Phase 1g 实测启动画像：LoRA 300 Linear / 161.2M 可训（3.12%），四卡各 ~28GB、~47%。
