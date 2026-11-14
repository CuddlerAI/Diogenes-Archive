#!/usr/bin/env python3
"""
build_prelabels.py — 融合两路 AI 信号 → 预标注 → 注入 annotator.html。

信号源:
  A. Claude 视觉判读(20 格联系表, 每 3s 一格, 人工写死在 VISION_SEGMENTS):
     战斗段(开火脉冲)/移动段(W)/驻足段。给出 auto_label 拿不到的**离散键**。
  B. auto_label.py 30fps 输出(scratchpad/auto30.jsonl):
     逐帧相机运动→鼠标 delta(方向已验证) + 音频瞬态开火帧。

融合规则:
  pointer: 全部来自 B(逐帧, 密度高)。
  keys:    W 来自 A 的移动段; vk=1(鼠标左) = A 的开火脉冲 ∪ B 的音频开火帧。
  只写非空帧(sparse), src:"ai"。
"""
import json, os, re, sys

FPS = 30
ROOT = os.path.dirname(os.path.abspath(__file__))
AUTO = "/private/tmp/claude-501/-Users-oratis-Documents-Claude-HakkoLite--claude-worktrees-hakkolite-data-collection-progress-bb0ec3/15b9c1dd-8258-430c-b0a0-6ebba0a64da6/scratchpad/auto30.jsonl"
HTML = os.path.join(ROOT, "annotator.html")

VK_W, VK_MOUSE_L = 87, 1

# ── A. Claude 视觉判读(秒区间, 联系表 2026-07-19; 格i = [3i, 3i+3)s) ──
# 依据: 格0 步枪口焰+左侧敌兵 → 战斗; 格1 蹲看战利品 → 驻足; 格2 室内推进(远处女性NPC);
# 格3 手枪口焰+近距敌兵 → 战斗; 格4-7 穿门→天桥甲板持续推进; 格8-11 WORK 钟楼广场环视缓走;
# 格12-13 广场推进到货箱; 格14 低头看尸体(驻足); 格15-16 绿色 vigor 充能+开火、敌兵奔跑 → 战斗;
# 格17-19 持枪沿甲板推进→雕像。
VISION_SEGMENTS = {
    # (start_s, end_s): "combat" | "move" | "idle"
    (0.0, 2.5):  "combat",
    (2.5, 6.0):  "idle",     # 看战利品/蹲下
    (6.0, 9.0):  "move",
    (9.0, 12.0): "combat",
    (12.0, 24.0): "move",    # 穿门 + 天桥推进
    (24.0, 30.0): "idle",    # 广场环视(pan 多、位移少)
    (30.0, 33.0): "move",
    (33.0, 36.0): "idle",
    (36.0, 44.0): "move",    # 广场推进
    (44.0, 45.5): "idle",    # 低头看尸体
    (45.5, 51.0): "combat",  # vigor + 开火
    (51.0, 60.0): "move",    # 沿甲板→雕像
}
# 战斗段内的开火脉冲(秒): 联系表可见口焰的时刻附近, 每脉冲 ~0.2s(6帧@30fps)
VISION_FIRE_PULSES = [0.8, 1.6, 9.8, 10.6, 11.3, 46.5, 47.4, 48.2, 49.0, 50.0]
PULSE_FRAMES = 6

def seg_of(t):
    for (a, b), kind in VISION_SEGMENTS.items():
        if a <= t < b: return kind
    return "idle"

def main():
    auto = [json.loads(l) for l in open(AUTO)]
    n = len(auto)
    print(f"auto30: {n} 帧")

    # 视觉开火帧集合
    fire = set()
    for p in VISION_FIRE_PULSES:
        f0 = round(p * FPS)
        for k in range(PULSE_FRAMES): fire.add(f0 + k)
    # 并入音频开火帧(auto 里 keys 含 1 的)
    audio_fire = [r["frame"] for r in auto if 1 in r.get("keys", [])]
    for f in audio_fire:
        for k in range(3): fire.add(f + k)   # 音频单帧 → 3 帧短脉冲
    print(f"开火帧: 视觉脉冲 {len(VISION_FIRE_PULSES)} 个 + 音频 {len(audio_fire)} 个 → {len(fire)} 帧")

    pre = []
    w_frames = 0
    for r in auto:
        f = r["frame"]; t = f / FPS
        kind = seg_of(t)
        keys = []
        if kind in ("move", "combat"):
            # move 段持续 W; combat 段间歇 W(战斗中走走停停, 2/3 占空比按 20 帧周期)
            if kind == "move" or (f // 20) % 3 != 2:
                keys.append(VK_W); w_frames += 1
        if f in fire:
            keys.append(VK_MOUSE_L)
        dx, dy = r["pointerDx"], r["pointerDy"]
        if keys or dx or dy:
            pre.append({"frame": f, "keys": sorted(keys),
                        "pointerDx": dx, "pointerDy": dy})
    print(f"预标注帧: {len(pre)} / {n}  (W 帧 {w_frames}, 含鼠标运动帧 {sum(1 for r in pre if r['pointerDx'] or r['pointerDy'])})")

    # ── 注入 HTML ──
    html = open(HTML, encoding="utf-8").read()
    blob = json.dumps(pre, separators=(",", ":"), ensure_ascii=False)
    new = re.sub(r"let PRELABELS = \[\];? ?/\*__PRELABELS__\*/",
                 "let PRELABELS = " + blob + "; /*__PRELABELS__*/", html, count=1)
    if new == html:
        # 已注入过 → 替换旧数据(锚点保留在行尾注释)
        new = re.sub(r"let PRELABELS = \[.*?\]; /\*__PRELABELS__\*/",
                     "let PRELABELS = " + blob + "; /*__PRELABELS__*/", html, count=1, flags=re.S)
    assert "let PRELABELS = [{" in new or '"frame"' in new, "注入失败"
    # 顺手修 banner CSS 笔误 (#c9d6 f3 → #c9d6f3)
    new = new.replace("color:#c9d6 f3", "color:#c9d6f3")
    open(HTML, "w", encoding="utf-8").write(new)
    kb = len(blob) / 1024
    print(f"[✓] 注入 annotator.html: {kb:.0f} KB 预标注")

if __name__ == "__main__":
    main()
