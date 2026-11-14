#!/bin/zsh
# keys.sh KEY ... : press keys in the GTA window on the capture PC, one argument a line -- UP DOWN LEFT RIGHT ENTER ESC
# BACK SPACE TAB Q E V, "WAIT 3000", "MOUSE dx dy", CENTER -- through the scheduled task of gta-window-keys.ps1 (it
# presses nothing unless GTA is the window in front). Prints the task's note of what it pressed.
source ${0:A:h}/env.sh
printf '%s\r\n' "$@" | ssh -o BatchMode=yes "$MC4_HOST" "powershell -NoProfile -Command \"\$input | Set-Content -Encoding UTF8 '$MC4_ROOT_WIN\\screens\\keys.txt'\""
printf '%s\n' "Start-ScheduledTask -TaskName 'Diogenes-mc4 GtaKeys'" '$t0 = Get-Date' \
  "do { Start-Sleep -Milliseconds 400; \$s = (Get-ScheduledTask -TaskName 'Diogenes-mc4 GtaKeys').State } while (\$s -eq 'Running' -and ((Get-Date) - \$t0).TotalSeconds -lt 60)" \
  "Get-Content '$MC4_ROOT_WIN\\screens\\keys-note.txt'" | win_ps 2>&1 | tail -1
