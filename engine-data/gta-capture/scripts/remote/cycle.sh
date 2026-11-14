#!/bin/zsh
# cycle.sh <take> <lane>: one whole export lane of $GROUP -- COOLDOWN_S after GTA last closed, boot, Story Mode, arm,
# Editor export, back, collect, close, encode, check, delete the frames -- stopping at the first thing that is off.
#   GROUP=beach-demo-113 CLIP_C=0011 ROW_C=2 zsh cycle.sh C 1
# CLIP_<take>: the take's clip number (片段-NNNN, what the arm reports as source_clip_name); ROW_<take>: its row in
# the Editor's newest-first clip list (the last take recorded is row 1). REDO=1: a lane exported before, again
# (a new tape: stage-take-export --redo sets the old result aside). Chain lanes:
#   for N in 1 2 3 4; do zsh cycle.sh C $N || break; done > $MC4_WORK/chain.log 2>&1
source ${0:A:h}/env.sh
HERE=${0:A:h}; L="zsh $HERE/lane.sh"; T=$1; N=$2
CLIPN=${(P)${:-CLIP_$T}}; ROW=${(P)${:-ROW_$T}}
die() { echo "STOP $T$N: $*"; exit 1; }
[ -n "$CLIPN" ] && [ -n "$ROW" ] || die "set CLIP_$T and ROW_$T"
ready=$(( $(cat $MC4_WORK/closed_at 2>/dev/null || echo 0) + COOLDOWN_S ))
while [ $(date +%s) -lt $ready ]; do sleep 15; done
echo "$(date +%H:%M:%S) boot $T$N"
$L boot | grep -q "GTA up" || die "GTA did not start"
# one RIGHT at a time until Story Mode (rightmost) is lit: the menu sometimes opens with an item already lit, so
# counting items and pressing that many overshoots
lit=""; for i in 1 2 3 4 5 6; do $L right 1 >/dev/null; [ "$($MC4_PY $HERE/checks/screencheck.py lit $MC4_WORK/bootsel-bar.png)" = lit ] && { lit=1; break; }; done
[ -n "$lit" ] || die "Story Mode not lit after 6 RIGHTs"
out=$($L arm $T $N $ROW ${REDO:+--redo})
echo "$out" | grep -q "片段-$CLIPN.clip" || die "armed the wrong clip: $out"
echo "$out" | grep -q '"armed": true' || die "not armed: $out"
echo "$out" | grep -q '"activated": true' || die "editor not opened: $out"
row=$($MC4_PY $HERE/checks/screencheck.py row $MC4_WORK/cliplist-small.png)
[ "$row" = "$ROW" ] || die "clip row $row selected, want $ROW"
$L export $N | head -1 | grep -q "^authorized" || die "export not authorized"
echo "$(date +%H:%M:%S) rendering"
r=$($L wait); echo "$r" | grep -q "renderer completed" || die "render: $r"
$L leave >/dev/null
for i in 1 2 3 4 5 6; do $MC4_PY $HERE/checks/storycheck.py $MC4_WORK/story.png 2>/dev/null | grep -q "^story" && break
  sleep 15; zsh $HERE/shot.sh story >/dev/null 2>&1; done
c=$($L collect $T $N); echo "$c"
echo "$c" | grep -q "collect ok True pointer_frames 0 frames 3630" || die "collect: $c"
e=$($L encode $T $N); echo "$e" | tail -2
echo "$e" | grep -q '"deleted"' || die "encode/check: $e"
echo "CYCLE OK $T$N $(date +%H:%M:%S)"
