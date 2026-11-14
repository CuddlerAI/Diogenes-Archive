# windows-capture-wgc fork notice

This crate is a deliberately small fork of `windows-capture` 2.0.0 (MIT).

Kept from the upstream design:

- `IGraphicsCaptureItemInterop::CreateForWindow` acquisition;
- WinRT/D3D11 device interop;
- frame-pool/session lifetime and event-handler cleanup;
- callback-scoped frame access.

DataMiner changes:

- accepts only a raw `HWND` wrapper; monitor, picker, unknown-item and desktop-duplication APIs are absent;
- requires a caller-provided D3D11 device/context;
- uses `Direct3D11CaptureFramePool::CreateFreeThreaded(..., 3)`;
- replaces the DispatcherQueue/message loop with a joinable control signal;
- treats source geometry changes as terminal instead of recreating the pool;
- exposes no CPU buffer, image saving, staging texture or encoder helper.

Upstream version: `2.0.0` from crates.io. Upgrade only with a reviewed source diff and a full P0 rerun.
