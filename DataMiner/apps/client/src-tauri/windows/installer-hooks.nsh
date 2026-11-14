; Interactive installs must never be mistaken for a signed production release.
; Silent install is reserved for the repository's guarded installer test script.
!macro NSIS_HOOK_PREINSTALL
  IfSilent dataminer_unsigned_warning_done
  MessageBox MB_OK|MB_ICONEXCLAMATION "UNSIGNED NON-PRODUCTION DEVELOPMENT BUILD.$\r$\n$\r$\nThis package is not code-signed or connected to a production release channel."
dataminer_unsigned_warning_done:
!macroend

; The stock Tauri 2.11 NSIS uninstaller owns the separate, unchecked-by-default
; "Delete the application data" confirmation. This hook intentionally contains
; no Delete/RMDir instruction, so default and silent uninstall retain app data.
