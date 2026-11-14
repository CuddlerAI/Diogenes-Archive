# env.sh: where things are, for the scripts beside it (sourced by each). Set any of these in the environment to
# override; GROUP (the group's folder name under groups\) has no default.
: "${MC4_HOST:?Set MC4_HOST to your own capture-host SSH alias}"                                                # the capture PC, an ssh alias (~/.ssh/config)
: ${MC4_ROOT_WIN:='D:\workspace\mc4'}                                # groups\ renders\ delivery\ screens\ tmp\ under it
: ${MC4_REPO_WIN:='D:\workspace\mc4\Diogenes'}                       # this repo checked out on the capture PC
: ${MC4_GAME_WIN:='D:\SteamLibrary\steamapps\common\Grand Theft Auto V'}
: ${MC4_WORK:=$HOME/.cache/mc4-remote}                                # here: screenshots, logs, the last close time
: ${MC4_PY:=python3}                                                  # here, with numpy and pillow (checks/)
: ${COOLDOWN_S:=180}                                                  # seconds GTA stays closed between lanes
GC_WIN="$MC4_REPO_WIN\\engine-data\\gta-capture"
RELOG_WIN="$MC4_GAME_WIN\\RockstarEditorPlus\\RockstarEditorPlus.log"
mkdir -p "$MC4_WORK"
# a command line for cmd.exe on the capture PC (the default shell of Windows' OpenSSH server), from gta-capture
win_gc() { ssh -o BatchMode=yes -o ServerAliveInterval=30 "$MC4_HOST" "cmd /c \"cd /d $GC_WIN && set PYTHONIOENCODING=utf-8&& $*\""; }
# a PowerShell script on stdin (one statement a line: stdin scripts lose multi-line blocks)
win_ps() { ssh -o BatchMode=yes "$MC4_HOST" 'powershell -NoProfile -ExecutionPolicy Bypass -Command -'; }
