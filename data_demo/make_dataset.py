#!/usr/bin/env python3
"""
make_dataset.py v2 — 把游戏录屏处理成 rocket-science(MIRA) 式带标注数据集。

数据卡决策(2026-07-19 用户锁定):
  1. 抽帧率 30fps            2. 分辨率 1080p (1920x1080)
  3. 载体 H.264 mp4          4. FramePair 任意 H / 可重叠 (默认 gapH=8, stride=4)
  5. 每帧流合并为单一 jsonl   6. 组织按 rocket-science: 4s/chunk(=120帧), 键 {session}_c{NNNN},
                               每 chunk = p0.mp4 + p0.jsonl + meta.json (单视角 → p0)

用法:
    python3 make_dataset.py <video.mp4> --game "BioShock Infinite" --category FPS \
        [--actions actions.jsonl]   # 标注器导出的确认版; 缺省用 AI 预标注(annotator 注入数据)
    python3 make_dataset.py --reindex

依赖: ffmpeg/ffprobe + stdlib。
"""
import argparse, json, math, os, re, subprocess, sys

FPS = 30
W, H = 1920, 1080
CHUNK_S = 4                    # rocket-science: 4s/chunk
CHUNK_FRAMES = FPS * CHUNK_S   # 120
GAP_H, STRIDE = 8, 4           # 可重叠 FramePair (stride < gapH)

ROOT = os.path.dirname(os.path.abspath(__file__))
DATASET = os.path.join(ROOT, "dataset")
KEY_SCHEMA = "VK codes; W/A/S/D=87/65/83/68, mouseL/R=1/2, 1..6=49..54 (mirrors input_recorder.rs SCANNED_VKEYS)"


def sh(cmd): return subprocess.run(cmd, capture_output=True, text=False)
def slug(s): return "".join(c if c.isalnum() else "_" for c in s.lower()).strip("_")
def wjsonl(p, rows):
    with open(p, "w") as f:
        for r in rows: f.write(json.dumps(r, ensure_ascii=False) + "\n")

def foveated_bin(d, mb=10, mu=8.0):
    if d == 0 or mb <= 0: return 0
    s = 1 if d > 0 else -1
    c = math.log(1 + mu*abs(d)) / math.log(1 + mu*200.0)
    return s * max(0, min(mb, round(c*mb)))


def probe(video):
    r = subprocess.run(["ffprobe","-v","error","-select_streams","v:0",
        "-show_entries","stream=width,height,r_frame_rate,duration","-of","json",video],
        capture_output=True, text=True)
    st = json.loads(r.stdout)["streams"][0]
    num, den = st["r_frame_rate"].split("/")
    return {"w": int(st["width"]), "h": int(st["height"]),
            "fps": round(int(num)/int(den), 3), "dur": float(st.get("duration", 0))}


def load_actions(path, n):
    """标注器导出的 actions.jsonl(确认版) 或 annotator.html 里的 AI 预标注(sparse)。"""
    per = [{"keys": [], "dx": 0, "dy": 0, "src": "empty"} for _ in range(n)]
    src_kind = "none"
    if path and os.path.exists(path):
        for line in open(path):
            line = line.strip()
            if not line: continue
            o = json.loads(line)
            f = o.get("frame", -1)
            if 0 <= f < n:
                per[f] = {"keys": sorted(o.get("keys", [])), "dx": o.get("pointerDx", 0),
                          "dy": o.get("pointerDy", 0), "src": o.get("src", "manual")}
        src_kind = "file"
    else:
        # 从 annotator.html 抠 AI 预标注
        html_p = os.path.join(ROOT, "annotator.html")
        if os.path.exists(html_p):
            m = re.search(r"let PRELABELS = (\[.*?\]); /\*__PRELABELS__\*/",
                          open(html_p, encoding="utf-8").read(), re.S)
            if m:
                for o in json.loads(m.group(1)):
                    f = o["frame"]
                    if 0 <= f < n:
                        per[f] = {"keys": sorted(o.get("keys", [])), "dx": o.get("pointerDx", 0),
                                  "dy": o.get("pointerDy", 0), "src": "ai"}
                src_kind = "ai_prelabel"
    return per, src_kind


def chunk_video(video, sdir, n_total):
    """H.264 1080p30 无音频 → cNNNN/p0.mp4, 精确 120 帧/chunk (rocket-science 载体口径)。
    固定 GOP=CHUNK_FRAMES(关键帧恰在 chunk 边界)+ 显式 -segment_frames 帧号切割
    (segment_time 的浮点 pts 比较会丢边界: 实测切出 192/177 或尾部 240 合并)。"""
    tmpl = os.path.join(sdir, "_c%04d.mp4")
    bounds = ",".join(str(f) for f in range(CHUNK_FRAMES, n_total + CHUNK_FRAMES, CHUNK_FRAMES))
    r = sh(["ffmpeg","-hide_banner","-loglevel","error","-y","-i",video,
        "-vf",f"fps={FPS},scale={W}:{H}","-an",
        "-c:v","libx264","-preset","veryfast","-pix_fmt","yuv420p",
        "-g",str(CHUNK_FRAMES),"-keyint_min",str(CHUNK_FRAMES),"-sc_threshold","0",
        "-f","segment","-segment_frames",bounds,"-reset_timestamps","1", tmpl])
    if r.returncode: sys.exit("ffmpeg segment 失败:\n" + r.stderr.decode()[-800:])
    chunks = sorted(f for f in os.listdir(sdir) if re.match(r"_c\d{4}\.mp4$", f))
    for i, f in enumerate(chunks):
        cdir = os.path.join(sdir, f"c{i:04d}"); os.makedirs(cdir, exist_ok=True)
        os.replace(os.path.join(sdir, f), os.path.join(cdir, "p0.mp4"))
    return len(chunks)


RIGHTS_CHOICES = ("third_party", "first_party", "licensed")

def rights_fields(rights, consent_id):
    """D-2 契约通道: is_trainable = (first_party ∨ licensed) ∧ consent_id 存在。
    licensed 时 consent_id 填合作协议 ID(如 Halo/微软合作), 见 research/halo/d2-contract-change-spec.md。"""
    first_party = rights == "first_party"
    trainable = rights in ("first_party", "licensed") and consent_id is not None
    return {"rights": rights, "first_party": first_party,
            "consent_id": consent_id, "is_trainable": trainable}


def build(video, game, category, actions_path, rights="third_party", consent_id=None):
    session = f"{slug(game)}_s0001"
    sdir = os.path.join(DATASET, slug(game), session)
    os.makedirs(sdir, exist_ok=True)
    meta = probe(video)
    n_total = round(meta["dur"] * FPS)

    print(f"[1/4] 切 chunk: H.264 {W}x{H}@{FPS} 无音频, {CHUNK_S}s/chunk …", flush=True)
    n_chunks = chunk_video(video, sdir, n_total)
    print(f"      {n_chunks} chunks")

    print(f"[2/4] 载入标注 …", flush=True)
    per, src_kind = load_actions(actions_path, n_total)
    labeled = sum(1 for p in per if p["keys"] or p["dx"] or p["dy"])
    print(f"      来源={src_kind}, 非空帧 {labeled}/{n_total}")

    print(f"[3/4] 每 chunk 写 p0.jsonl(合并流) + meta.json …", flush=True)
    prev_keys = set()
    for c in range(n_chunks):
        cdir = os.path.join(sdir, f"c{c:04d}")
        rows = []
        for k in range(CHUNK_FRAMES):
            g = c * CHUNK_FRAMES + k
            if g >= n_total: break
            p = per[g]
            cur = set(p["keys"]); ev = []
            for vk in sorted(cur - prev_keys): ev.append({"type": "key_down", "vk": vk})
            for vk in sorted(prev_keys - cur): ev.append({"type": "key_up", "vk": vk})
            if p["dx"] or p["dy"]: ev.append({"type": "mouse_move", "dx": p["dx"], "dy": p["dy"]})
            prev_keys = cur
            # 合并版每帧行: 动作 + 事件 一行装下 (决策5)
            rows.append({"frame": k, "gframe": g, "video_t_ms": round(g*1000/FPS),
                         "keys": p["keys"], "pointerDx": p["dx"], "pointerDy": p["dy"],
                         "pointerBins": [foveated_bin(p["dx"]), foveated_bin(p["dy"])],
                         "events": ev, "src": p["src"]})
        wjsonl(os.path.join(cdir, "p0.jsonl"), rows)
        json.dump({
            "sample_key": f"{session}_c{c:04d}", "session": session, "chunk": c,
            "game": game, "category": category, "platform": "pc",
            "video": {"file": "p0.mp4", "codec": "h264", "width": W, "height": H,
                      "fps": FPS, "frames": len(rows), "audio": False},
            "annotation_source": src_kind, "key_schema": KEY_SCHEMA,
            **rights_fields(rights, consent_id), "is_cutscene": False,
        }, open(os.path.join(cdir, "meta.json"), "w"), ensure_ascii=False, indent=2)

    print(f"[4/4] session 级 framepairs.jsonl (gapH={GAP_H}, stride={STRIDE} 可重叠) …", flush=True)
    pairs = []
    for t in range(0, n_total - GAP_H, STRIDE):
        pairs.append({"clipId": session, "consentId": None,
                      "frameT": t, "gapH": GAP_H,
                      "chunkT": t // CHUNK_FRAMES, "chunkTH": (t+GAP_H) // CHUNK_FRAMES,
                      "actionT": {"keys": per[t]["keys"], "pointerDx": per[t]["dx"],
                                   "pointerDy": per[t]["dy"]}})
    wjsonl(os.path.join(sdir, "framepairs.jsonl"), pairs)

    json.dump({
        "workflow_id": session, "game": game, "category": category, "platform": "pc",
        "source_video": os.path.basename(video),
        "org": f"rocket-science-style: {CHUNK_S}s/chunk x {n_chunks}, key {session}_cNNNN, p0.mp4+p0.jsonl+meta.json",
        "capture": {"fps": FPS, "width": W, "height": H, "codec": "h264",
                    "frame_count": n_total, "chunks": n_chunks, "chunk_frames": CHUNK_FRAMES},
        "duration_ms": round(meta["dur"]*1000),
        "frame_pairs": {"count": len(pairs), "gap_h": GAP_H, "stride": STRIDE, "overlapping": True},
        "annotation": {"source": src_kind, "labeled_frames": labeled,
                       "note": "ai_prelabel = Claude 视觉分段 + 相机运动 + 音频瞬态, 未经人工确认"
                               if src_kind == "ai_prelabel" else "annotator.html 人工确认版"},
        **rights_fields(rights, consent_id), "is_cutscene": False,
        "tags": ([("risk:third_party_content" if rights == "third_party" else f"rights:{rights}"),
                  "format_demo",
                  "ai_prelabel" if src_kind == "ai_prelabel" else "human_verified"]),
    }, open(os.path.join(sdir, "metadata.json"), "w"), ensure_ascii=False, indent=2)
    print(f"  ✓ {session}: {n_chunks} chunks · {len(pairs)} framepairs · 标注 {labeled} 帧 ({src_kind})")


def reindex():
    sessions = []
    for game in sorted(os.listdir(DATASET)):
        gdir = os.path.join(DATASET, game)
        if not os.path.isdir(gdir): continue
        for sess in sorted(os.listdir(gdir)):
            mp = os.path.join(gdir, sess, "metadata.json")
            if os.path.exists(mp): sessions.append(json.load(open(mp)))
    if not sessions: return print("dataset/ 无 session")
    wjsonl(os.path.join(DATASET, "metadata.jsonl"), sessions)
    stats = {
        "sessions": len(sessions), "games": sorted({s["game"] for s in sessions}),
        "total_hours": round(sum(s["duration_ms"] for s in sessions)/3.6e6, 3),
        "total_frames": sum(s["capture"]["frame_count"] for s in sessions),
        "total_chunks": sum(s["capture"]["chunks"] for s in sessions),
        "total_frame_pairs": sum(s["frame_pairs"]["count"] for s in sessions),
        "datacard": {"fps": FPS, "resolution": f"{W}x{H}", "carrier": "h264_mp4_chunks",
                     "chunk_s": CHUNK_S, "gap_h": GAP_H, "stride": STRIDE, "overlapping": True,
                     "org": "rocket-science-style", "merged_jsonl": True},
        "compliance": {"all_third_party": all(s.get("rights") == "third_party" for s in sessions),
                       "trainable_sessions": sum(1 for s in sessions if s.get("is_trainable")),
                       "rights_breakdown": {r: sum(1 for s in sessions if s.get("rights") == r)
                                            for r in sorted({s.get("rights", "third_party") for s in sessions})},
                       "note": "第三方 AAA; is_trainable=False; 仅研究/格式验证"
                               if all(s.get("rights") == "third_party" for s in sessions)
                               else "混合 rights; licensed/first_party 会话按 is_trainable 门进训练"},
    }
    json.dump(stats, open(os.path.join(DATASET, "_dataset_stats.json"), "w"),
              ensure_ascii=False, indent=2)
    open(os.path.join(DATASET, "DATASHEET.md"), "w").write(datasheet(stats))
    print(f"✓ 索引: {len(sessions)} sessions · {stats['total_chunks']} chunks · "
          f"{stats['total_frame_pairs']} pairs")


def datasheet(st):
    g = ", ".join(st["games"]); d = st["datacard"]
    return f"""# DataDemo 数据集 v2 — 数据卡

## 决策(2026-07-19 锁定)
| # | 决策点 | 值 |
|---|---|---|
| 1 | 抽帧率 | **{d['fps']} fps** |
| 2 | 分辨率 | **{d['resolution']}** (1080p) |
| 3 | 载体 | **H.264 mp4**(无音频, 同 rocket-science) |
| 4 | FramePair | **gapH={d['gap_h']}, stride={d['stride']}(可重叠)** |
| 5 | 每帧流 | **合并单一 p0.jsonl**(动作+事件一行) |
| 6 | 组织 | **rocket-science 式**: {d['chunk_s']}s/chunk(={d['chunk_s']*d['fps']}帧), 键 `{{session}}_cNNNN`, 每 chunk `p0.mp4+p0.jsonl+meta.json` |

## 规模
{st['sessions']} session ({g}) · {st['total_chunks']} chunks · {st['total_frames']} 帧 · {st['total_frame_pairs']} FramePair(可重叠) · {st['total_hours']}h

## 标注
`ai_prelabel` = Claude 视觉分段(联系表判读 战斗/移动/驻足 + 开火脉冲) ⊕ 相机全局运动(→鼠标 delta, 方向已验证) ⊕ 音频瞬态(→开火)。
经 `annotator.html` 人工确认后重跑本脚本 `--actions <导出>` 升级为 `human_verified`。
W 键为段级推断(非逐帧真值); 鼠标 delta 快速甩枪撞 ±200 饱和; WASD 之外的键未推断。

## 动作聚合语义(消费必读, docs/148 A3)
字段形态遵循 dm-1 p0.jsonl 契约(规范: DataMiner docs/02 §3-§4; 降采: docs/58 §1.4), 值为 ai_prelabel 推断:
- `keys` 语义为**帧窗结束时按住**的 VK 集合(非"窗口内按过"); 本库由段级推断填充, 不是逐帧采集真值。
- `pointerDx/Dy` = 帧窗内鼠标位移求和后 **clamp ±200**, 不可作回归目标; 本库**无 `pointerRawDx/Dy` 字段**, 未截断值需从各行 `events` 内 `mouse_move` 的 dx/dy 逐窗求和重算。
- `pointerBins` 为 **legacy 刻度** µ-law 分箱(60→8 / 120→9 / 180→10), 与现行 `dm1-v2` 刻度**不可混用**(60px 处差 2 个 bin); 跨库消费一律丢弃存储 bins、从 raw 重算(research/07 裁决)。
- 降采到更低 fps: 按时间建窗; 位移**逐窗求和绝不抽样**; 键通道取窗内**覆盖比例 [0,1] 而非 OR**(OR 会把点射时长放大近 2 倍); 已 clamp 的值禁止二次相加再 clamp; 视频与动作同一组窗口切; 不支持上采样。

## ⚠️ 合规
{'全部第三方 AAA → `first_party=false` / `consent_id=null` → **`is_trainable=false`**。' if st['compliance']['all_third_party'] else f"混合 rights: {st['compliance']['rights_breakdown']} · 可训练 session: {st['compliance']['trainable_sessions']}(门: (first_party ∨ licensed) ∧ consent_id)。"}
{'仅研究/方法验证/格式对拍, 不可商用训练(同 rocket-science 非商用 / gaming-500h 许可真空的定位)。' if st['compliance']['all_third_party'] else 'licensed 会话的 consent_id 为合作协议 ID; 未授权第三方内容仍不可训练。'}
"""


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("video", nargs="?")
    ap.add_argument("--game"); ap.add_argument("--category", default="unknown")
    ap.add_argument("--actions", default=None, help="标注器导出的 actions.jsonl(确认版)")
    ap.add_argument("--rights", default="third_party", choices=RIGHTS_CHOICES,
                    help="内容权利: third_party(默认,不可训练) / first_party / licensed(合作授权)")
    ap.add_argument("--consent-id", default=None,
                    help="consent 记录 ID; licensed 时填合作协议 ID (缺省则 is_trainable=false)")
    ap.add_argument("--reindex", action="store_true")
    a = ap.parse_args()
    if a.rights != "third_party" and not a.consent_id:
        print("⚠ rights=%s 但未给 --consent-id → is_trainable 仍为 false" % a.rights, file=sys.stderr)
    os.makedirs(DATASET, exist_ok=True)
    if a.reindex: reindex()
    elif a.video and a.game:
        build(a.video, a.game, a.category, a.actions, a.rights, a.consent_id); reindex()
    else: ap.error("给 <video> --game, 或 --reindex")
