"""AB-P0 第 1 步 · phase0 IDM 头在 COD 具结语料上的域内微调.

    python -m diogenes_wm.ab_p0.finetune --config configs/ab_p0_cod.yaml \
        [--init-checkpoint runs/idm-valorant-epoch-.../checkpoint.pt]

与 `diogenes_phase0.train` 的三点刻意不同(其余尽数复用):

1. **契约车道是 `dm1-attested`**(contract.py): COD chunk 是 dm-1/per-game 映射,
   phase0 的两条 legacy 车道会硬拒它 —— 那道闸拦的是**静默**投影。
   本车道把投影变成有意的: 只收 dm-1、治理门全开(is_trainable 四项与, 无
   method-validation 松弛), 且本入口必须把 12 键词表吃不下的按键份额报出来
   (research/21 E7: 词表外事件在 D2E 四游戏上实测 18.9%)。
2. **划分是会话 id 升序取尾 N(默认 2), 与 wm01/readiness.py、MV split_rule
   同一口径** —— 不是 train.py 的 session-hash。这不只是口径统一: AB 消融臂
   (A1/A2/A3)与本微调必须共享同一条 holdout 边界, 否则伪标注头在 wm01 的
   holdout 会话上带着「训过」的优势, E1a 保留率读数就被污染了。
3. **combat 两通道从语义补位**: 实测 COD chunk 的 `keys` 列没有鼠标键(VK 1/2),
   `combat.aim` / `combat.primary_attack` 只以语义 `actions` 出现。不补位,
   SCANNED_VKEYS 里这两个键位得不到任何正监督, 伪标签会天生缺掉 wm01 六通道中
   帧占比第二的 `combat.aim`(22.15%)。补位表与 wm01 native 车道(A1 臂)消费的
   语义同源(action_space.AB_P0_VK_BY_SEMANTIC), 六通道上的监督与 A1 同一定义。

不设硬门: 30 号文已经写明外部 1B IDM 也没过平凡基线, AB-P0 的验收在下游
(A2/A3 臂 vs A1 臂的配对读数), 这里只报数 —— per-key F1 / 平凡基线并列 /
词表外份额, 全部落 finetune_report.json。

## GPU runbook —— `diogenes-vl-idmgpu2` (L4 24 GB, us-east1-b, 停机未删), 数小时档

本文件不开机也不自动执行以下任何一步(35 号文纪律: 只登记, 不动钱/不点火)。

    # 0. 开机(owner/操作者手动):
    #    gcloud compute instances start diogenes-vl-idmgpu2 --zone=us-east1-b
    # 1. 同步代码与语料(chunk 契约目录是 mp4 符号链接, rsync 要 -L 展开):
    #    rsync -avzL ~/Documents/DiogenesData/full/chunks_attested/ vm:~/ml/data/cod_attested/
    #    (chunk 由 cod_adapter --rights first_party --consent-id <具结 ID> 产出)
    # 2. 环境: pip install -e 'ml[data]'  (mp4 解码需要 opencv; DINOv2 首载要网络)
    # 3. 微调(本机先跑过 --max-steps 2 冒烟再上卡):
    #    python -m diogenes_wm.ab_p0.finetune --config configs/ab_p0_cod.yaml --device cuda
    #    - save_every 必设: 本入口对 save_every<=0 直接拒绝启动, 不是警告。
    #      (34 号文报账纪律同款: 崩在最后一小时不能烧掉整轮权重)
    #    - 规模账: 254,587 帧 / window 5 / stride 2 ≈ 12.7 万窗, 尾 2 会话留出后
    #      ≈ 11.5 万训练窗; 6,000 步 × bs48 ≈ 2.5 epoch, 对齐 P1 浅语料参考位
    #      (epoch_align 口径)。L4 上参考吞吐 ≈ 1.5 it/s → 微调 ~70 min,
    #      加 pseudo_label 全量导出一趟(25.4 万帧推理) 总计数小时档, ≈ $5–10。
    # 4. 导出伪标签(第 2 步, 同机接着跑):
    #    python -m diogenes_wm.ab_p0.pseudo_label --checkpoint runs/ab-p0-cod-idm-*/checkpoint.pt \
    #        --src ~/ml/data/cod_attested --out ~/ml/data/cod_pseudo
    # 5. 自停(hostname -s 核对是 diogenes-vl-idmgpu2 再执行): sudo shutdown -h now
    # 6. 报账: ml/scripts/gcp_cost.py 读实测 uptime, 不估算。
"""
from __future__ import annotations

import argparse
import json
import math
import time
from collections import Counter, OrderedDict
from pathlib import Path

import numpy as np
import torch
import yaml
from torch.utils.data import DataLoader

from diogenes_phase0.contract import ChunkRecord
from diogenes_phase0.data.dataset import (WindowDataset,
                                          filter_by_action_density,
                                          scan_dataset)
from diogenes_phase0.evaluate import run_eval
from diogenes_phase0.keys import N_KEYS, SCANNED_VKEYS, VK_TO_INDEX
from diogenes_phase0.models.idm import build_model, trainable_params
from diogenes_phase0.train import (compute_loss, get_device, save_checkpoint)

from ..wm01.action_space import AB_P0_VK_BY_SEMANTIC


def _session_of(rec: ChunkRecord) -> str:
    return rec.chunk_dir.parent.name


def split_session_tail(records: list[ChunkRecord],
                       holdout_sessions: int = 2) -> dict[str, list[ChunkRecord]]:
    """会话 id 升序取尾 N 作 holdout —— 与 wm01/readiness.py、MV split_rule 同口径.

    单操作者语料里一个会话就是一段打法(34 号文 §3), holdout 指标必须与漂移表
    并列读 —— 那张表由 `python -m diogenes_wm.wm01.readiness` 出, 不在这里重造。
    """
    sessions = sorted({_session_of(r) for r in records})
    if holdout_sessions >= len(sessions):
        raise SystemExit(
            f"holdout 要 {holdout_sessions} 个会话, 语料只有 {len(sessions)} 个")
    tail = set(sessions[-holdout_sessions:]) if holdout_sessions else set()
    return {
        "train": [r for r in records if _session_of(r) not in tail],
        "holdout": [r for r in records if _session_of(r) in tail],
        "holdout_sessions": sorted(tail),
        "sessions": sessions,
    }


def _load_semantics(rec: ChunkRecord) -> list[list[str]]:
    return [json.loads(x).get("actions") or []
            for x in rec.actions_path.read_text().splitlines() if x.strip()]


class CodWindowDataset(WindowDataset):
    """WindowDataset + 语义补位: 把 wm01 六语义映到 SCANNED_VKEYS 位并入 keys 目标.

    对 WASD 是幂等的(物理键本来就在 `keys` 列); 实际补进来的是 combat 两个
    鼠标键位 —— 见模块 docstring 第 3 点。六通道上的标签定义从此与 A1 臂
    (wm01 native, 语义驱动)重合, 这正是消融臂能配对比较的前提。
    """

    def __init__(self, records, **kw):
        super().__init__(records, **kw)
        self._sem_bits: OrderedDict[int, np.ndarray] = OrderedDict()

    def _sem_bits_for(self, ri: int) -> np.ndarray:
        if ri in self._sem_bits:
            self._sem_bits.move_to_end(ri)
            return self._sem_bits[ri]
        sems = _load_semantics(self.records[ri])
        bits = np.zeros((len(sems), N_KEYS), dtype=np.float32)
        for t, frame_sems in enumerate(sems):
            for s in frame_sems:
                vk = AB_P0_VK_BY_SEMANTIC.get(s)
                if vk is not None:
                    bits[t, VK_TO_INDEX[vk]] = 1.0
        self._sem_bits[ri] = bits
        while len(self._sem_bits) > self._cache_chunks:
            self._sem_bits.popitem(last=False)
        return bits

    def __getitem__(self, i: int) -> dict:
        item = super().__getitem__(i)
        ri, t = self.index[i]
        bits = self._sem_bits_for(ri)
        if t < len(bits):
            item["keys"] = torch.maximum(item["keys"], torch.from_numpy(bits[t]))
        return item


def oov_report(records: list[ChunkRecord]) -> dict:
    """12 键词表吃不下的按键份额 —— research/21 E7 的纪律: 缩减词表可以, 但要报数.

    份额按「键×帧」事件计(一个键在一帧被按住 = 1 个事件), 语义补位的两个鼠标
    键位不在 `keys` 列里, 不参与分母 —— 这里量的是物理键盘词表的损失。
    """
    in_vocab = out_vocab = 0
    dropped: Counter = Counter()
    for rec in records:
        for line in rec.actions_path.read_text().splitlines():
            if not line.strip():
                continue
            for vk in json.loads(line).get("keys") or []:
                if int(vk) in VK_TO_INDEX:
                    in_vocab += 1
                else:
                    out_vocab += 1
                    dropped[int(vk)] += 1
    total = in_vocab + out_vocab
    return {
        "key_events": total,
        "in_vocab": in_vocab,
        "out_of_vocab": out_vocab,
        "out_of_vocab_share": (out_vocab / total) if total else 0.0,
        "dropped_vks_top": dropped.most_common(10),
        "vocab": list(SCANNED_VKEYS),
    }


def load_init_checkpoint(model: torch.nn.Module, path: str | Path,
                         cfg: dict, device) -> None:
    """可选: 从既有 phase0 checkpoint 热启(几何必须逐项相同, 否则拒绝).

    26 号文的结论是跨游戏 zero-shot 不成立, 所以热启不是默认 —— 它是一个
    待测的选项(域内微调从跨游戏权重出发是否比随机初始化好), 两种起点都合法。
    """
    ckpt = torch.load(path, map_location=device, weights_only=False)
    old = ckpt.get("config", {})
    for section, key in (("model", "trunk"), ("model", "d_model"),
                         ("model", "n_layers"), ("model", "n_heads"),
                         ("data", "window")):
        a = old.get(section, {}).get(key)
        b = cfg.get(section, {}).get(key)
        if a != b:
            raise SystemExit(
                f"--init-checkpoint 几何不符: {section}.{key} = {a!r}(ckpt) vs "
                f"{b!r}(config) —— 形状错位的热启不是微调, 是静默重训")
    model.load_state_dict(ckpt["model"])


def build_cod_loaders(cfg: dict) -> tuple[dict[str, DataLoader | None], dict]:
    d = cfg["data"]
    records = []
    for root in d["roots"]:
        recs = scan_dataset(root)
        if not recs:
            raise SystemExit(f"{root} 下没有契约格式 chunk")
        records += recs
    if d.get("min_key_frac") is not None:
        records, fstats = filter_by_action_density(
            records, min_key_frac=d["min_key_frac"])
        print(f"action-density filter: {json.dumps(fstats)}")
        if not records:
            raise SystemExit("全部 chunk 被过滤 —— 检查 min_key_frac")

    splits = split_session_tail(records,
                                holdout_sessions=d.get("holdout_sessions", 2))
    oov = oov_report(records)
    print(f"词表外按键事件份额: {100 * oov['out_of_vocab_share']:.1f}% "
          f"(top: {oov['dropped_vks_top'][:5]})")

    def make(recs, shuffle):
        if not recs:
            return None
        ds = CodWindowDataset(
            recs, window=d.get("window", 5), stride=d.get("stride", 1),
            input_res=tuple(d.get("input_res", [224, 224])),
            delta_scale=d.get("delta_scale", 50.0),
            contract_mode=d.get("contract_mode", "dm1-attested"),
            delta_source=d.get("delta_source", "pointer"),
            delta_clamp=d.get("delta_clamp"),
            expected_fps=d.get("expected_fps"),
            cache_chunks=d.get("cache_chunks", 8),
        )
        bs = cfg["train"].get("batch_size", 32)
        return DataLoader(ds, batch_size=bs, shuffle=shuffle,
                          num_workers=cfg["train"].get("num_workers", 0),
                          drop_last=shuffle)

    print(f"chunks: train={len(splits['train'])} holdout={len(splits['holdout'])} "
          f"(holdout 会话 = {splits['holdout_sessions']})")
    loaders = {"train": make(splits["train"], True),
               "holdout": make(splits["holdout"], False)}
    info = {"sessions": splits["sessions"],
            "holdout_sessions": splits["holdout_sessions"],
            "chunks": {"train": len(splits["train"]),
                       "holdout": len(splits["holdout"])},
            "oov": oov}
    return loaders, info


def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", required=True)
    ap.add_argument("--device", default="auto")
    ap.add_argument("--max-steps", type=int, default=None)
    ap.add_argument("--init-checkpoint", default=None,
                    help="可选: 从既有 phase0 checkpoint 热启(几何必须相同)")
    args = ap.parse_args()

    cfg = yaml.safe_load(Path(args.config).read_text())
    tcfg = cfg["train"]
    # runbook 纪律前置到启动时: 一次数小时的 GPU 微调, 崩溃不能等于白跑
    if int(tcfg.get("save_every", 0)) <= 0:
        raise SystemExit("train.save_every 必须 > 0(35 §3.3 runbook 纪律); "
                         "拒绝启动而不是事后追悔")
    torch.manual_seed(cfg.get("seed", 0))
    device = get_device(args.device)

    run_dir = Path(cfg.get("output", {}).get("dir", "runs")) / \
        f"{cfg.get('run_name', 'ab-p0')}-{time.strftime('%Y%m%d-%H%M%S')}"
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "config.yaml").write_text(yaml.safe_dump(cfg))

    loaders, split_info = build_cod_loaders(cfg)
    model = build_model(cfg).to(device)
    if args.init_checkpoint:
        load_init_checkpoint(model, args.init_checkpoint, cfg, device)
    print(f"trainable params: {trainable_params(model):,} (budget 50M) on {device}")

    max_steps = args.max_steps or tcfg.get("max_steps", 400)
    warmup = tcfg.get("warmup_steps", max(20, max_steps // 20))
    opt = torch.optim.AdamW(
        (p for p in model.parameters() if p.requires_grad),
        lr=tcfg.get("lr", 3e-4), weight_decay=tcfg.get("weight_decay", 0.01))
    sched = torch.optim.lr_scheduler.LambdaLR(
        opt, lambda s: min(1.0, (s + 1) / warmup) *
        (0.5 * (1 + math.cos(math.pi * min(1.0, s / max_steps)))))

    log_path = run_dir / "train_log.jsonl"
    save_every = int(tcfg.get("save_every"))
    step = 0
    t0 = time.time()
    model.train()
    while step < max_steps:
        for batch in loaders["train"]:
            out = model(batch["frames"].to(device))
            loss, parts = compute_loss(out, batch, tcfg.get("loss_weights", {}),
                                       device)
            opt.zero_grad(set_to_none=True)
            loss.backward()
            torch.nn.utils.clip_grad_norm_(
                (p for p in model.parameters() if p.requires_grad), 1.0)
            opt.step()
            sched.step()
            step += 1
            if step % tcfg.get("log_every", 25) == 0 or step == 1:
                rec = {"step": step, **parts, "lr": sched.get_last_lr()[0],
                       "sec": round(time.time() - t0, 1)}
                print(json.dumps(rec))
                with open(log_path, "a") as f:
                    f.write(json.dumps(rec) + "\n")
            if step % save_every == 0:
                save_checkpoint(run_dir, model, cfg, step)
            if step >= max_steps:
                break
        model.train()

    # eval 前先落权重(与 train.py 同一条理由: eval 失败不该吞掉训练产物)
    save_checkpoint(run_dir, model, cfg, step)

    report = {
        "run_name": cfg.get("run_name"),
        "steps": step,
        "split": split_info,
        # 不设硬门: AB-P0 的验收在下游消融臂(A2/A3 vs A1), 这里只报数。
        # per-key F1 里自带 key_support —— 平凡基线并列的原料(31 号文形态)。
        "holdout": run_eval(model, loaders["holdout"], device)
        if loaders["holdout"] else None,
    }
    (run_dir / "finetune_report.json").write_text(
        json.dumps(report, indent=1, ensure_ascii=False))
    print(json.dumps({k: report[k] for k in ("run_name", "steps")}, ensure_ascii=False))
    if report["holdout"]:
        h = report["holdout"]
        print(f"holdout: key_macro_f1={h.get('key_macro_f1'):.4f} "
              f"key_exact={h.get('key_exact'):.4f}")
    print(f"run dir: {run_dir}")


if __name__ == "__main__":
    main()
