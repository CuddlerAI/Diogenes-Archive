#!/bin/zsh
# shot.sh <name>: a picture of the GTA window on the capture PC (gta-window-shot.ps1's scheduled task: only the
# window, only while it is in front) into $MC4_WORK/<name>.png, and <name>-small.png at 1280 wide (macOS sips).
source ${0:A:h}/env.sh
note=$(printf '%s\n' "Start-ScheduledTask -TaskName 'Diogenes-mc4 GtaShot'" '$t0 = Get-Date' \
  "do { Start-Sleep -Milliseconds 500; \$s = (Get-ScheduledTask -TaskName 'Diogenes-mc4 GtaShot').State } while (\$s -eq 'Running' -and ((Get-Date) - \$t0).TotalSeconds -lt 30)" \
  "Get-Content '$MC4_ROOT_WIN\\screens\\gta-latest.txt'" | win_ps 2>&1 | tail -1 | tr -d '\r')
echo "$note"
[[ "$note" == *" saved "* ]] || exit 1   # nothing new: GTA not running, minimized or not in front
scp -q "$MC4_HOST:${MC4_ROOT_WIN//\\//}/screens/gta-latest.png" "$MC4_WORK/$1.png" &&
  sips -Z 1280 "$MC4_WORK/$1.png" --out "$MC4_WORK/$1-small.png" >/dev/null
