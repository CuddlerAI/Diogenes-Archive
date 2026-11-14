"""Model of RE+'s plan-owned sliding pump (patches/replus-v19-sliding.patch,
src/capture/render.cpp) against the engine timings it has to survive.

Per game update, in one of two orders:
  process-first:  engine Process -> pump -> bridge -> render/Present
  pump-first:     pump -> bridge -> engine Process -> render/Present
Engine: an integer step accumulator (sm_exportTotalNs); the float clock the pump
reads shows it at once (lag 0) or as of the previous Process (lag 1, what win-pc
showed). A render shows the instant the float clock names. Add-on: captures at
the Present of the request's update and acks 0-2 Presents later (0 = inside that
same Present). Bridge: applies a new frame's camera one update after seeing it.
Busy updates hold; with skip_pump the pump runs but returns right after its top
(the add-on heartbeat has not moved since the last pump).

Checks every captured frame k (rendered instant == camera == k) and that the lane
finishes. Logic variants, each the build that ran on win-pc:
  old  release again when the float clock looks behind -> double step at frame 7
  new  release by the accumulator                       -> double step at frame 10
       (a heartbeat early return after a request left the step released)
  top  new + re-hold at the very top of every pump once the released step is
       taken (shipped). pump-first/lag 0 renders one step late in every variant;
       win-pc frames 1-6 matched walking frames exactly, which rules that order out.

--seam: a two-clip project whose first clip ends at a fractional step. The pump
must never step the clock onto the last half millisecond of a clip (there the
engine changes clip by itself); it holds instead, and once that frame is written
jumps to the next clip at the frame's own instant (the jump loads for a random
number of updates) and takes no step for the landed frame.

Usage: python3 replus_slide_sim.py [runs] [--seam]
"""
import random
import sys


SEAM_EPS = 0.015     # half a millisecond, in 33.33 ms steps
LANDED = -1          # kSeamLandedNs: no release test matches it


def run(logic, order, lag, seed, frames=60, skip_pump=0.0, busy=0.02, seam=None):
    rnd = random.Random(seed)
    clip = 0             # clip on screen
    offset = 0.0         # project instant minus accumulated steps (moves at a jump)
    seam_pending = False
    jump_left = None     # updates until the jump lands

    total = 0            # steps taken (accumulator)
    shown = 0            # steps the float clock shows
    hold = True          # begin(): held for frame 0
    frame = 0            # s_frame
    capture = False      # s_slideCapture
    flush = False        # s_slideFlush
    ready = 0            # s_slideReadyPumps
    guard_wait = 0
    guard_armed = False
    release_total = 0    # s_slideReleaseNs
    applied = -1         # bridge: frame whose camera is the final writer
    pending = None
    ack_left = None      # presents until ack of the in-flight request
    captured = []        # (frame, rendered instant, camera)
    request_this_update = None
    frame_started = [False]
    no_present = [False]

    def pump_top():
        nonlocal hold
        if logic == "top" and frame_started[0] and total != release_total:
            hold = True

    def pump():
        nonlocal hold, frame, capture, flush, ready, guard_wait, guard_armed, release_total
        nonlocal request_this_update, clip, offset, seam_pending, jump_left, shown
        pump_top()
        if no_present[0]:
            return None                       # heartbeat unchanged: early return
        if flush:
            if ack_left is not None:          # not acked yet
                hold = True
                return None
            hold = True
            flush = False
            capture = False
            frame += 1
            if frame >= frames:
                return "done"
        if seam_pending or jump_left is not None:   # ownedSeamPump
            hold = True
            if seam_pending:
                seam_pending = False
                jump_left = rnd.randint(1, 60)
                return None
            jump_left -= 1
            if jump_left > 0:
                return None
            jump_left = None
            clip = 1
            offset = frame - total            # landed at this frame's own instant
            shown = frame
            release_total = LANDED
            return None
        if rnd.random() < busy:              # replayBusy() || spinner
            hold = True
            return None
        clock = shown                         # the float clock the pump reads
        if not capture:
            if logic == "old":
                if clock < frame - 0.5:       # displayed clock "behind": release
                    hold = False
                    return None
            else:
                if frame > 0 and total == release_total:
                    hold = False              # the owed step not taken yet
                    return None
                hold = True
            capture = True
            ready = 0
        # gate
        hold = True
        want = frame
        if logic == "old":
            if applied != frame:
                ready = 0
                return None
            ready += 1
            if ready < 2:
                return None
        else:
            if shown > want:
                return f"abort: clock ahead at frame {frame} (shows {shown})"
            if applied != frame or shown < want:
                ready = 0
                return None
            ready += 1
            if ready < 2:
                return None
        if not guard_armed:
            guard_armed = True
            guard_wait = 3
        if guard_wait > 0:
            guard_wait -= 1
            return None
        if shown != want:
            return f"abort: clock off grid at frame {frame} (shows {shown})"
        release_total = total
        frame_started[0] = True
        # the next frame past the clip's last half millisecond: hold, jump later
        seam_pending = seam is not None and clip == 0 and frame + 1 > seam - SEAM_EPS
        hold = seam_pending
        request_this_update = frame
        flush = True
        return None

    def bridge():
        nonlocal applied, pending
        if applied == frame:
            return
        if pending != frame:
            pending = frame
            return
        applied = frame

    def process():
        nonlocal total, shown
        before = total
        if not hold:
            total += 1
        if seam is not None and clip == 0 and total + offset > seam - SEAM_EPS:
            return f"engine changed clip by itself at frame {frame}"
        shown = (before if lag == 1 else total) + offset

    for update in range(frames * 40):
        request_this_update = None
        # skip_pump now means: the pump runs but the add-on has not presented
        # since the last one, so it returns right after its top
        no_present[0] = rnd.random() < skip_pump
        if order == "process-first":
            steps = ("process", "pump", "bridge")
        else:
            steps = ("pump", "bridge", "process")
        for step in steps:
            if step == "process":
                r = process()
                if r:
                    return r, update
            elif step == "pump":
                r = pump()
                if r == "done":
                    bad = [c for c in captured if c[1] != c[0] or c[2] != c[0]]
                    if not bad and seam is not None and clip != 1:
                        return "never reached the second clip", update
                    return ("ok" if not bad else f"WRONG PIXELS {bad[:3]}"), update
                if r:
                    return r, update
            else:
                bridge()
        # render + Present of this update
        if request_this_update is not None:
            captured.append((request_this_update, shown, applied))
            # 0: the observer releases and the add-on acks in this same Present,
            # so the very next pump already sees the frame written
            ack_left = rnd.randint(0, 3) - 1
            if ack_left < 0:
                ack_left = None
        elif ack_left is not None:
            ack_left -= 1
            if ack_left < 0:
                ack_left = None
    return f"STALL at frame {frame}", frames * 40


if __name__ == "__main__":
    runs = int(sys.argv[1]) if len(sys.argv[1:2]) and sys.argv[1].isdigit() else 2000
    if "--seam" in sys.argv:
        # the shipped logic only; seams anywhere, including a hair before a frame
        for order in ("process-first", "pump-first"):
            for lag in (0, 1):
                for skip in (0.0, 0.05, 0.3):
                    outcomes = {}
                    for seed in range(runs):
                        at = random.Random(seed ^ 0x5EA).choice(
                            [20.0, 20.0 + SEAM_EPS / 2, 20.5, 21.0 - SEAM_EPS / 2, 21.0 - 2 * SEAM_EPS,
                             random.Random(seed).uniform(10, 50)])
                        result, _ = run("top", order, lag, seed, skip_pump=skip, seam=at)
                        key = result.split(" at ")[0] if not result.startswith("ok") else "ok"
                        outcomes[key] = outcomes.get(key, 0) + 1
                    print(f"seam top {order:13} lag={lag} skip={skip:.2f}: {outcomes}")
        sys.exit(0)
    for logic in ("new", "top"):
        for order in ("process-first", "pump-first"):
            for lag in (0, 1):
                for skip in (0.0, 0.05, 0.3):
                    outcomes = {}
                    updates = []
                    for seed in range(runs):
                        result, n = run(logic, order, lag, seed, skip_pump=skip)
                        key = result.split(" at ")[0] if not result.startswith("ok") else "ok"
                        outcomes[key] = outcomes.get(key, 0) + 1
                        if result == "ok":
                            updates.append(n / 60)
                    per = f"{sum(updates) / len(updates):.1f} updates/frame" if updates else "-"
                    print(f"{logic:3} {order:13} lag={lag} skip={skip:.2f}: {outcomes}  {per}")
