; This release uses production services but does not yet carry Authenticode.
; Silent install is reserved for the repository's guarded installer test script.
!macro NSIS_HOOK_PREINSTALL
  IfSilent dataminer_unsigned_delivered_warning_done
  MessageBox MB_OK|MB_ICONEXCLAMATION "UNSIGNED DELIVERED CATALOG RELEASE.$\r$\n$\r$\nThis package connects to the production account, catalog, and ingest services, but it is not Authenticode-signed. Install it only when downloaded from the official DataMiner release channel; Windows SmartScreen may warn."
dataminer_unsigned_delivered_warning_done:
!macroend

; The stock Tauri 2.11 NSIS uninstaller owns the separate, unchecked-by-default
; "Delete the application data" confirmation. This hook intentionally contains
; no Delete/RMDir instruction, so default and silent uninstall retain app data.
