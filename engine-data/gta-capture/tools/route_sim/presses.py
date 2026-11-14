"""Every key press of a played take (C or D): how long, which keys, the camera and the keys' way off the body as they
went down, then how far the person moved and how far the body turned by the time it stood still again; last, how many
taps (held under 0.5 s) walked instead of turning on the spot (moved 0.6 m and more), on one key and on two -- on two
the game walked a metre 16 times in 41, on one never (win-pc, 2026-10-17), so the controller taps on one key.
    python presses.py <take dir>"""
import json, math, sys
T = sys.argv[1]
taps = {"one key": [0, 0], "two keys": [0, 0]}
log = [json.loads(l) for l in open(T + "/source/input-camera-log.jsonl", encoding="utf-8")]
log = [r for r in log if r.get("controlled")]
def keys(r):
    k = set(r.get("keyCode") or []); return "".join(c for c, n in (("w",87),("a",65),("s",83),("d",68)) if n in k)
def way(k):
    f = ("w" in k) - ("s" in k); l = ("a" in k) - ("d" in k); return math.degrees(math.atan2(l, f))
def w(a): return (a + 180) % 360 - 180
i = 0
print(" t_s   keys held_s  cam-body  keys-way-body  moved_m  turned  settle_s")
while i < len(log):
    k = keys(log[i])
    if not k: i += 1; continue
    j = i
    while j + 1 < len(log) and keys(log[j + 1]) == k: j += 1
    r0 = log[i]; t0 = r0["game_ms"] / 1000
    held = (log[j + 1]["game_ms"] if j + 1 < len(log) else log[j]["game_ms"]) / 1000 - t0
    body0, cam0 = r0["controlled"]["h"], r0["game_cam"]["rot"][2]
    p0 = r0["controlled"]["pos"][:2]
    # still again: the next 10 ticks under 2 cm each, or the next press
    m = j + 1
    while m + 10 < len(log) and not keys(log[m]):
        if all(math.dist(log[q]["controlled"]["pos"][:2], log[q - 1]["controlled"]["pos"][:2]) < 0.02 and abs(w(log[q]["controlled"]["h"] - log[q - 1]["controlled"]["h"])) < 0.5 for q in range(m, m + 10)): break
        m += 1
    m = min(m, len(log) - 1)
    moved = math.dist(log[m]["controlled"]["pos"][:2], p0)
    turned = w(log[m]["controlled"]["h"] - body0)
    print(f"{t0:6.1f} {k:4s} {held:5.2f}   {w(cam0 - body0):7.1f}   {w(cam0 + way(k) - body0):9.1f}     {moved:5.2f}  {turned:6.1f}   {(log[m]['game_ms'] - log[j]['game_ms']) / 1000:5.2f}")
    if held < 0.5:
        n = taps["two keys" if len(k) == 2 else "one key"]
        n[0] += moved >= 0.6; n[1] += 1
    i = j + 1
print("taps that walked:", ", ".join(f"{kind} {n[0]} of {n[1]}" for kind, n in taps.items()))
