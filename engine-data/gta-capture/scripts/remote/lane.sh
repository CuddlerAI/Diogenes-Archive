#!/bin/zsh
# lane.sh <step> [args]: one export lane of $GROUP on the capture PC, a step at a time, from this Mac over ssh
# (RUNBOOK-MC4-FOUR-WORLDLINES.md §二 4-6; cycle.sh runs the steps in order). Screens land in $MC4_WORK.
#   boot                      start GTA (the StartGTA task), wait for the start menu, answer Script Hook V's update
#                             question No; prints the menu bar's crop
#   right <n>                 RIGHT n times on the start menu (Story Mode is the rightmost item)
#   arm <take> <lane> <row> [--redo]
#                             ENTER Story Mode, arm the lane (stage-take-export --arm-only), open the Editor, new
#                             project, add clip, down to row <row> of the newest-first clip list (the take's clip)
#   export <lane>             place the clip, project menu, Export; nudge the mouse (CENTER: the Editor authorizes
#                             an armed export only after mouse input) and go only once RE+ logs the lane authorized
#   wait                      until RE+ logs the render completed (or failed)
#   leave                     export page -> project menu -> Exit -> don't save -> close the Editor -> Story Mode
#   collect <take> <lane>     collect the lane (pointer check of every frame), then close GTA (the cool-down starts)
#   encode <take> <lane>      encode the lane's video, check it against the frames, delete the frames (verify_clean.py)
source ${0:A:h}/env.sh
HERE=${0:A:h}
[ -n "$GROUP" ] || { echo "set GROUP (the group's folder under $MC4_ROOT_WIN\\groups)"; exit 2; }
K="zsh $HERE/keys.sh"; SHOT="zsh $HERE/shot.sh"
GROUP_WIN="$MC4_ROOT_WIN\\groups\\$GROUP"
relog() { printf '%s\n' "Get-Content '$RELOG_WIN' -Tail ${1:-20}" | win_ps 2>&1; }
gta_up() { printf '%s\n' "if (Get-Process GTA5 -ErrorAction SilentlyContinue) { 'up' } else { 'no' }" | win_ps 2>&1 | tail -1 | tr -d '\r'; }
bar() { $SHOT $1 >/dev/null 2>&1; sips -c 80 1100 --cropOffset 995 800 $MC4_WORK/$1.png --out $MC4_WORK/$1-bar.png >/dev/null 2>&1; echo "$MC4_WORK/$1-bar.png"; }
export_lane() {   # <take> <lane> [flags]: stage-take-export of one lane, arm (--arm-only) or collect
  win_gc "python -m gta_capture stage-take-export --group $GROUP_WIN --take $1 --lane $2 ${@:3} --render-ini \"$MC4_GAME_WIN\\RockstarEditorPlus\\Render.ini\""
}
parse_collect() {
  $MC4_PY - "$1" <<'PY'
import json, sys
s = open(sys.argv[1], encoding="utf-8", errors="replace").read(); i = s.find("{")
try: d = json.loads(s[i:])
except Exception: print(s[-1200:]); raise SystemExit(1)
pc = d.get("pixel_check"); ln = d.get("lane") or {}
print("collect ok", d.get("ok"), "pointer_frames", pc.get("pointer_frames") if isinstance(pc, dict) else pc,
      "frames", ln.get("frame_count"), ln.get("frame_pattern"))
PY
}
case $1 in
boot)
  r=$(gta_up)
  for attempt in 1 2; do   # the launcher can take 3 minutes to start GTA5: wait that long before asking again
    [ "$r" = up ] && break
    started=1; printf '%s\n' "Start-ScheduledTask -TaskName 'Diogenes-mc4 StartGTA'" | win_ps >/dev/null 2>&1
    for i in $(seq 1 18); do sleep 10; r=$(gta_up); [ "$r" = up ] && break; done
  done
  echo "GTA $r${started:+ started}"; [ -n "$started" ] && sleep 55
  # Script Hook V sometimes asks whether to check for updates: No (TAB to it, SPACE) -- Yes goes online
  for i in 1 2 3; do $SHOT shv >/dev/null 2>&1; [ "$($MC4_PY $HERE/checks/shvdialog.py $MC4_WORK/shv-small.png)" = dialog ] || break
    echo "Script Hook V update question: No"; $K TAB "WAIT 400" SPACE >/dev/null; sleep 40; done
  bar boot ;;
right)
  args=(); for i in $(seq 1 $2); do args+=(RIGHT "WAIT 600"); done; $K "${args[@]}" >/dev/null; sleep 1; bar bootsel ;;
arm)
  $K ENTER >/dev/null; sleep 65
  export_lane $2 $3 --arm-only $5 > $MC4_WORK/arm-$2$3.json 2>&1
  grep -o '"source_clip_name"[^,]*\|"armed": [a-z]*' $MC4_WORK/arm-$2$3.json
  win_gc "python -m gta_capture editor-control activate" 2>&1 | grep -o '"activated": [a-z]*'
  sleep 12
  args=(ENTER "WAIT 2500" ENTER "WAIT 5000" ENTER "WAIT 2500" ENTER "WAIT 800"); for ((i=2; i<=$4; i++)); do args+=(DOWN "WAIT 500"); done
  $K "${args[@]}" >/dev/null; sleep 2; $SHOT cliplist >/dev/null 2>&1; echo "$MC4_WORK/cliplist-small.png" ;;
export)
  $K ENTER "WAIT 6000" ESC "WAIT 3000" DOWN "WAIT 400" DOWN "WAIT 400" DOWN "WAIT 400" DOWN "WAIT 400" DOWN "WAIT 400" \
     DOWN "WAIT 400" DOWN "WAIT 400" DOWN "WAIT 1500" ENTER "WAIT 3000" CENTER >/dev/null
  for i in $(seq 1 15); do sleep 2; relog 4 | grep -q "authorized plan lane $2 ready" && { auth=1; break; }; done
  if [ -z "$auth" ]; then echo "NOT AUTHORIZED"; relog 6 | cut -c1-160; $SHOT exportfail >/dev/null 2>&1; echo "$MC4_WORK/exportfail-small.png"; exit 1; fi
  echo authorized; $K ENTER "WAIT 3000" ENTER "WAIT 3000" ENTER >/dev/null; sleep 20; relog 4 | cut -c1-160 ;;
wait)
  for i in $(seq 1 80); do l=$(relog 40)
    if echo "$l" | grep -q "renderer completed\|cursor guard\|before the armed\|failed"; then
      echo "$l" | grep "renderer completed\|cursor guard\|before the armed\|failed" | tail -3 | cut -c1-200; exit 0; fi
    sleep 30; done; echo TIMEOUT ;;
leave)
  $K "WAIT 2000" ESC "WAIT 3000" DOWN "WAIT 2000" ENTER "WAIT 3000" SPACE "WAIT 3000" ESC "WAIT 3000" ENTER >/dev/null
  sleep 70; $SHOT story >/dev/null 2>&1; echo "$MC4_WORK/story-small.png" ;;
collect)
  export_lane $2 $3 > $MC4_WORK/collect-$2$3.json 2>&1
  parse_collect $MC4_WORK/collect-$2$3.json
  printf '%s\n' "Get-Process GTA5 -ErrorAction SilentlyContinue | Stop-Process -Force" "Start-Sleep -Seconds 8" \
    "if (Get-Process GTA5 -ErrorAction SilentlyContinue) { 'GTA5 still running' } else { 'GTA5 closed' }" | win_ps 2>&1 | tail -1
  date +%s > $MC4_WORK/closed_at ;;
encode)
  win_gc "python -m gta_capture stage-take-encode --group $GROUP_WIN --take $2 --lane $3" > $MC4_WORK/encode-$2$3.txt 2>&1
  grep -o '"ok": [a-z]*' $MC4_WORK/encode-$2$3.txt | head -1
  win_gc "python scripts\\remote\\win\\verify_clean.py $MC4_ROOT_WIN $GROUP $2 $3 --delete" ;;
*) sed -n '2,20p' $0; exit 2 ;;
esac
