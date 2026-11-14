#!/bin/zsh
# simeon.sh [quiet_s]: in Story Mode, answer every phone call to the protagonist (his green phone bottom right, >2% of
# that corner green) and hang up, until quiet_s seconds (default 240) pass with no call. Run it before recording a take:
# a call left ringing, or refused, comes back -- into the take (beach-demo-113 C, 2026-10-18: the hidden protagonist
# rides the actor about to be taken over, so his phone and ringtone were in lanes 5-8). ENTER answers (often only the
# second press); once the ringing screen is gone, wait 4 s and BACK ends the call.
source ${0:A:h}/env.sh
HERE=${0:A:h}; quiet=${1:-240}; last=$(date +%s); n=0
green() { zsh $HERE/shot.sh $1 >/dev/null 2>&1; $MC4_PY $HERE/checks/greenphone.py $MC4_WORK/$1-small.png; }
while [ $(( $(date +%s) - last )) -lt $quiet ]; do
  g=$(green poll)
  if [ "$(echo "$g > 2.0" | bc)" = 1 ]; then
    n=$((n+1))
    for press in 1 2 3; do
      zsh $HERE/keys.sh ENTER >/dev/null; sleep 1.5
      [ "$(echo "$(green ans) > 2.0" | bc)" = 1 ] || break   # the ringing screen is gone: answered (or ended)
    done
    sleep 4; zsh $HERE/keys.sh BACK >/dev/null; sleep 2
    echo "$(date +%H:%M:%S) call $n answered after $press press(es), hung up; green now $(green after)"
    last=$(date +%s)
  fi
  sleep 3
done
echo "$(date +%H:%M:%S) quiet for ${quiet}s after $n call(s)"
