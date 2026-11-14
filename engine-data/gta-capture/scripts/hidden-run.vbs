' Runs one PowerShell script with no window at all, for the scheduled tasks of gta-desktop-tasks.ps1: a console
' window, even one hidden at once, can take the foreground from the game for a moment -- and a take stops the
' program at the controls the moment the game leaves the foreground.
'   wscript.exe hidden-run.vbs <script.ps1>
Set shell = CreateObject("WScript.Shell")
shell.Run "powershell.exe -NoProfile -ExecutionPolicy Bypass -File """ & WScript.Arguments(0) & """", 0, True
