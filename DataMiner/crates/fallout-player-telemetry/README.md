# Fallout 4 DEV player telemetry v1

`fallout-player-telemetry/v1` is the player-side foundation for the Fallout 4
DEV/non-trainable depth lane. It does not implement the camera probe, final
`action_camera.json` assembly, mouse normalization, client UI, R2 delivery, or a
real-machine end-to-end claim.

## Contract and semantics

Every JSON-line fact carries the schema version, `run_id`, `game_update_seq`,
sample QPC and frequency, explicit `gameplay_state`, validity/reason, Fallout
executable SHA-256, Fallout runtime version, F4SE version, plugin build Git SHA,
and the explicit coordinate transform. A valid payload contains:

- `position_m`: the authoritative `PlayerCharacter` actor-root world translation,
  converted by the exact-gated Fallout X/Y/Z (right/forward/up, right-handed) to
  DataMiner X/Y/Z (right/up/forward, left-handed) transform and from game units
  to metres;
- `rotation_quaternion_xyzw`: the complete actor-root authoritative world rotation;
  a yaw-only runtime capability is `rotation_capability_insufficient`, never a
  fabricated pitch/roll;
- scalar `speed_mps`: direct game-logic velocity magnitude when available, otherwise
  actor-root displacement across consecutive valid game ticks divided by QPC time.
  Missing/repeated/regressed ticks never use Present timing and never become zero.

The target DEV build is exact: Steam App 377160 / build 22848228,
`Fallout4.exe` 1.11.221.0 with
`sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61`,
F4SE 0.7.8, and the configured plugin build SHA. Menus, loading, missing player,
identity drift, stale/future QPC, old runs, invalid numerics, and sequence faults
all emit invalid facts without a player payload. The consumer poisons the run on
the first rejected fact.

The state vocabulary is `gameplay_player_valid`, `loading`, `menu_or_pipboy`,
`transition_or_fast_travel`, `no_player`, and `unknown_probe_timing_possible`.
Only `gameplay_player_valid` may carry a valid player payload. Every other state
emits its own game-update sequence and QPC, clears the displacement baseline, and
never carries the last player value across the interval.

Long empty-depth runs currently appear correlated with loading, Pip-Boy/menu,
cutscene/transition, and fast travel, but that is an unverified hypothesis until
the in-process state facts are joined to RGB/empty-depth QPCs. Roughly 7% of empty
samples are isolated; they must remain `unknown_probe_timing_possible` (or probe
timing loss) rather than being forced into a game-state label. Empty depth itself
is never accepted as the source of a stronger state classification.

The current DEV conversion uses `0.0142875 m/game-unit`, recorded at MED confidence
in `config/depth-map-bounds/public-survey.json`; its required real-machine anchor
cross-check and axis/sign validation remain acceptance gates, not certified facts.

The wire schema is in `schema/fallout-player-telemetry-v1.schema.json`; the C sink
ABI is in `include/fallout_player_telemetry_v1.h`. `try_publish` is single-attempt
and non-blocking. The transport may later be a pipe, shared memory, or another
mechanism; this task deliberately does not choose one.

## Why there is no F4SE DLL in this repository

The omission is a fail-closed licensing and runtime-symbol decision, not a stub:

1. The [F4SE readme](https://github.com/ianpatt/f4se/blob/master/f4se_readme.txt)
   says F4SE itself may not be redistributed or modified and native plugin source
   must be publicly available. Diogenes is private, so a distributable private
   plugin would not satisfy that condition. No F4SE headers or binaries are
   vendored here.
2. F4SE master commit `cb39721a8155b0ec31640291f2997fa1c43474af`
   targets runtime 1.11.221 / F4SE 0.7.8 and exposes `g_player`, actor-root access,
   and `NiAVObject::m_worldTransform`. The repository has not yet validated a
   per-game-update callback, root-layout access, menu/load gate, or QPC sampling
   on the target machine.
3. The public headers do not expose a documented player direct-velocity accessor.
   A runtime-specific symbol may unlock the preferred source; until verified, the
   adapter permits only the consecutive-game-tick actor-root/QPC fallback and
   records its provenance.

## Remaining inputs and real-machine acceptance

Before a real plugin can be built, provide either a public plugin source repository
or written F4SE permission compatible with a private source tree; pin the external
F4SE SDK/header commit without copying it into this repository; and validate the
1.11.221 x64 symbols/callbacks for player root, complete rotation, loading/menu
state, update sequence, and (if used) direct velocity.

On the target Windows machine:

1. re-hash `Fallout4.exe`, verify file/runtime 1.11.221.0 and F4SE 0.7.8, then
   launch only through the matching F4SE loader;
2. build the public plugin with its Git SHA embedded and confirm the exact-build
   gate rejects one-at-a-time EXE, F4SE, runtime, and plugin drift;
3. prove the sampled node is the third-person `PlayerCharacter` actor root and
   the matrix yields a full normalized quaternion through standing, turning,
   pitch-affecting animation, load, fast travel, death, and first/third-person
   transitions;
   also cross-check the configured axis/sign mapping and `0.0142875 m/game-unit`
   scale against an independent in-game anchor;
4. prove menu/loading/no-player intervals emit no valid player payload, and verify
   menu/Pip-Boy, loading, transition/fast-travel, no-player, and unknown intervals
   each retain their own `game_update_seq` + QPC without last-value carry; verify
   QPC age across a new run and a deliberately replayed old run;
5. correlate those state/QPC facts with RGB and empty-depth timestamps; keep
   isolated empty samples unknown and report the state-correlation hypothesis
   separately from probe-timing failures;
6. compare direct velocity (if found) with the root/QPC fallback, then run a
   no-blocking stress capture with a deliberately full sink.

Downstream CA/T-A integration remains TODO. It must bind this contract's `run_id`
and exact build identity to the capture `session_id`, capture run, and generation;
v1 does not emit those downstream identities, so a consumer must reject a missing
or ambiguous binding rather than infer one. Correlation must use elapsed QPC time
plus actual `game_update_seq`/capture-sequence continuity across 30/60 fps. It must
not use a fixed 15-frame threshold or input-array adjacency. In particular, #528's
"presumed gameplay gap" is only a heuristic: it cannot upgrade
`unknown_probe_timing_possible` to gameplay or a confirmed non-gameplay state.

Passing this crate's tests establishes only the contract/adapter/gate. It does not
establish a loadable F4SE plugin or Fallout 4 real-machine acceptance.
