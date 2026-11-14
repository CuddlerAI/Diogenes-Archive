# Exact local identity pinned · full D7 capture still blocked

**Status updated 2026-08-04.** This bundle is included in the All Games DEV
installer only behind `elden-ring-nightreign-steam-preflight`. Every output is
fixed `non_trainable: true`; this is not a certified or trainable admission.

## Closed evidence gaps

- Steam App `2622380`, build `22818764` was launched through the normal Steam / Easy
  Anti-Cheat path.
- The live target image is only `nightreign.exe` version `1.3.3.0`, SHA-256
  `9545e8732919bb6a63904cb73223641b3c4f9a67869be5722612acca413e8b3a`.
- Authenticode is valid for `株式会社フロム・ソフトウェア`.
- The live top-level window class is exactly `ELDEN RING NIGHTREIGN`; the observed
  windowed client area is 1920×1080 SDR.
- `start_protected_game.exe` is a launcher, has no target HWND, and is intentionally
  excluded from the game identity.

## Remaining blocker

Two serial local attempts passed preflight, exact HWND/PID binding, WGC startup,
and NVIDIA hardware H.264 startup. The retry produced three complete 300-frame
ready segments and seven DataModel blocks, then failed closed with the repeatable
encoder error `GPU-copy completion exceeded its bounded wait budget` (76.380 ms
observed versus the 66.667 ms limit). No Raw Input event was observed in this
anti-cheat-protected run. Recovery verified the retained outputs and quarantined
the four incomplete items.

The safety budget must not be weakened to turn this into a pass. `view_scale`
also remains `null`, and the existing mapping has not gained new screenshot
evidence in this run. The bundle therefore remains an unsigned dev preflight:
exactly pinned and partially exercised, but not D7-complete and never eligible
for certified or trainable promotion.
