//! Dense (actor-root, FPP camera) pair sampling for the content join.
//!
//! The simulation usually runs one frame ahead of the renderer and slips phase around stalls;
//! after a stall it can publish two states inside one render interval, and the one in between
//! is what the next Present renders. A read at Present time never sees it. This thread reads
//! the pair about once per millisecond into a small ring of distinct states, and the Present
//! path asks the ring which state carried the camera it just rendered
//! (`depth_lab::cp2077_pose_join`).
//!
//! The thread is the ring's ONLY writer: the join's earliest-entry rule needs entries in
//! observation order, and a second producer reading outside the lock can publish late. The
//! Present path therefore reads no process memory at all; it only scans the ring.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use depth_lab::cp2077_pose_join::{
    CP2077_POSE_JOIN_TOLERANCE, POSE_JOIN_RING_CAPACITY, PoseJoin, PoseJoinRefusal, SimulationPair,
    SimulationPairRing,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};

use super::fpp_camera::FppCameraChain;
use super::{
    PLAYER_AUDITS, PlayerAuditVerdict, STABLE_READ_ATTEMPTS, WorldTransformWords,
    decode_world_transform, player_audit_qpc, player_verdict_admits, read_actor_root,
};
use crate::cp2077_pose_wiring::FormalPlayerAddresses;

const PERIOD: Duration = Duration::from_millis(1);
const PAIR_ATTEMPTS: usize = 4;
/// The audit verdict is re-read this often, not on every tick and not faster after a failed
/// read, so the thread almost never holds the audit lock when another path tries it.
const VERDICT_REFRESH_MS: i64 = 50;
/// Two successful reads further apart than this are not one continuous observation: the join's
/// earliest-entry rule will not walk back across them. Longer than a simulation state lives
/// (about 30 ms) and than the diagnostic thread's scheduling holes (35–39 ms), so a short
/// hole or a read that fails while the state is changing still lets the torn pair that
/// follows it resolve to the consistent pair before it. A hole longer than this (50.6 ms was
/// seen once at HIGHEST priority) only stops the walk back; it never joins across it.
const CONTINUITY_MS: i64 = 50;
/// A join is refused when the newest successful read is older than this at the Present anchor.
/// A sampler that is alive but keeps failing (expired audit, replaced PlayerPuppet) must not
/// go on serving an old state that still matches a standing camera.
const FRESHNESS_MS: i64 = 100;

/// 0 = no sampler thread has started yet, 1 = priority not raised, 2 = raised. The sampler
/// starts before the diagnostic sidecars exist, so they read this when they close.
static PRIORITY_RAISED: AtomicU8 = AtomicU8::new(0);

#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) fn priority_raised() -> Option<bool> {
    match PRIORITY_RAISED.load(Ordering::Acquire) {
        0 => None,
        state => Some(state == 2),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PairSource {
    pub addresses: FormalPlayerAddresses,
    pub camera: FppCameraChain,
    pub axis_mapping: depth_lab::D3d12AxisMapping,
    pub metres_per_unit: f64,
}

impl PairSource {
    /// The camera stable-read is bracketed by two identical actor-root stable-reads, so a pair
    /// cannot straddle an actor-root update, and by two camera-chain identity checks, so a
    /// component replaced during the read cannot pass as the owned FPP camera. The engine still
    /// writes the root before the camera within one step; the join's earliest-entry rule deals
    /// with that torn pair.
    fn read_words(
        &self,
        verdict: &PlayerAuditVerdict,
    ) -> Result<(WorldTransformWords, WorldTransformWords), String> {
        let mut before = read_actor_root(self.addresses, verdict)?;
        for _ in 0..PAIR_ATTEMPTS {
            self.camera.check_identity()?;
            let camera = stable_camera(|| self.camera.read_transform())?;
            self.camera.check_identity()?;
            let after = read_actor_root(self.addresses, verdict)?;
            if after == before {
                return Ok((after, camera));
            }
            before = after;
        }
        Err("CP2077 actor root changed across every bracketed camera read".into())
    }

    fn read(
        &self,
        verdict: &PlayerAuditVerdict,
        qpc: impl Fn() -> Result<i64, String>,
    ) -> Result<SimulationPair, String> {
        let first_qpc = qpc()?;
        let (player_words, camera_words) = self.read_words(verdict)?;
        let read_end_qpc = qpc()?;
        let (player_position, player_rotation) =
            decode_world_transform(player_words, self.axis_mapping, self.metres_per_unit)?;
        let (camera_position, camera_rotation) =
            decode_world_transform(camera_words, self.axis_mapping, self.metres_per_unit)?;
        Ok(SimulationPair {
            first_qpc,
            first_end_qpc: read_end_qpc,
            last_qpc: read_end_qpc,
            reads: 1,
            player_words,
            camera_words,
            player_position,
            player_rotation,
            camera_position,
            camera_rotation,
            follows_gap: false,
        })
    }
}

fn stable_camera(
    mut read: impl FnMut() -> Result<WorldTransformWords, String>,
) -> Result<WorldTransformWords, String> {
    let mut previous = read()?;
    for _ in 1..STABLE_READ_ATTEMPTS {
        let current = read()?;
        if current == previous {
            return Ok(current);
        }
        previous = current;
    }
    Err("CP2077 FPP camera transform changed during the bounded stable read".into())
}

/// Audit verdict cached by the sampling thread. Refreshed on a fixed cadence: a failed read is
/// usually not the verdict's fault, and retrying the audit lock every millisecond during a
/// failing stretch would contend with exactly the paths that are trying to recover.
#[derive(Default)]
struct VerdictCache {
    refreshed_qpc: Option<i64>,
    verdict: Option<Result<PlayerAuditVerdict, String>>,
}

impl VerdictCache {
    fn admitted(
        &mut self,
        now: i64,
        refresh_qpc: i64,
        lookup: impl FnOnce(i64) -> Result<PlayerAuditVerdict, String>,
    ) -> Result<PlayerAuditVerdict, String> {
        if self
            .refreshed_qpc
            .is_none_or(|at| now.saturating_sub(at) > refresh_qpc)
        {
            self.refreshed_qpc = Some(now);
            self.verdict = Some(lookup(now));
        }
        self.verdict
            .clone()
            .expect("the verdict was looked up above")
    }
}

fn audit_verdict_at(now: i64) -> Result<PlayerAuditVerdict, String> {
    // Blocking is fine here: this is never a game thread.
    let guard = PLAYER_AUDITS
        .lock()
        .map_err(|_| "CP2077 player audit state poisoned".to_string())?;
    if let Some(error) = &guard.fatal_error {
        return Err(error.clone());
    }
    match guard.latest_at_or_before(now) {
        Some(outcome) => outcome.sampler_verdict(),
        None => Err(match guard.earliest_qpc() {
            Some(audited_qpc) => format!(
                "CP2077 player audit has no verdict at or before this read: lead_qpc={}",
                audited_qpc.saturating_sub(now)
            ),
            None => "CP2077 player pointer audit has not run yet".into(),
        }),
    }
}

/// Scheduling health of the sampler thread, logged once when it stops, in every build. A join
/// only fails when a hole swallows a whole simulation state, so a clean run alone does not say
/// how close it came; the longest gaps between successful reads do.
struct CadenceStats {
    reads: u64,
    failures: u64,
    previous_read_qpc: Option<i64>,
    max_gap_qpc: i64,
    ten_ms_qpc: i64,
    thirty_ms_qpc: i64,
    gaps_over_10ms: u32,
    gaps_over_30ms: u32,
}

impl CadenceStats {
    fn new(ten_ms_qpc: i64, thirty_ms_qpc: i64) -> Self {
        Self {
            reads: 0,
            failures: 0,
            previous_read_qpc: None,
            max_gap_qpc: 0,
            ten_ms_qpc,
            thirty_ms_qpc,
            gaps_over_10ms: 0,
            gaps_over_30ms: 0,
        }
    }

    /// `completed_qpc` is when the read finished; gaps are measured between completions.
    fn read(&mut self, completed_qpc: i64) {
        self.reads += 1;
        if let Some(previous) = self.previous_read_qpc.replace(completed_qpc) {
            let gap = completed_qpc.saturating_sub(previous);
            self.max_gap_qpc = self.max_gap_qpc.max(gap);
            self.gaps_over_10ms += u32::from(gap > self.ten_ms_qpc);
            self.gaps_over_30ms += u32::from(gap > self.thirty_ms_qpc);
        }
    }

    fn failed(&mut self) {
        self.failures += 1;
    }

    fn summary(&self, qpc_frequency: i64, priority_raised: bool) -> String {
        let max_gap_ms = self.max_gap_qpc as f64 * 1_000.0 / qpc_frequency.max(1) as f64;
        format!(
            "CP2077 pair sampler stopped: reads={} failures={} max_gap_ms={max_gap_ms:.1} gaps_over_10ms={} gaps_over_30ms={} priority_raised={priority_raised}",
            self.reads, self.failures, self.gaps_over_10ms, self.gaps_over_30ms
        )
    }
}

struct Shared {
    ring: SimulationPairRing,
    /// The most recent read failure, which may be long past. Reported with a refused join: a
    /// Present that finds the sampler stale or no matching state must name the cause.
    last_failure: Option<String>,
}

pub(crate) struct PairSampler {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    freshness_qpc: i64,
}

impl PairSampler {
    pub(crate) fn start(
        source: PairSource,
        generation: u64,
        qpc_frequency: i64,
        ttl_qpc: i64,
    ) -> Result<Self, String> {
        let qpc_per_ms = |ms: i64| qpc_frequency.saturating_mul(ms) / 1_000;
        let shared = Arc::new(Mutex::new(Shared {
            ring: SimulationPairRing::new(POSE_JOIN_RING_CAPACITY, qpc_per_ms(CONTINUITY_MS)),
            last_failure: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        PRIORITY_RAISED.store(0, Ordering::Release);
        let (thread_shared, stopped) = (Arc::clone(&shared), Arc::clone(&stop));
        let refresh_qpc = qpc_per_ms(VERDICT_REFRESH_MS);
        let (ten_ms_qpc, thirty_ms_qpc) = (qpc_per_ms(10), qpc_per_ms(30));
        let join = std::thread::Builder::new()
            .name("dm-cp2077-pair-sampler".into())
            .spawn(move || {
                // Roughly 40 us of reads per millisecond. Above the game's threads so the
                // cadence holds when every core is busy: a starved sampler is what loses the
                // short-lived state this thread exists to catch. HIGHEST was not enough: the
                // 2026-09-18 certification run saw 50.6 ms and 31.7 ms holes while the game kept
                // presenting every ~33 ms, i.e. this thread alone was not scheduled.
                let priority_raised =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }
                        .is_ok();
                PRIORITY_RAISED.store(1 + u8::from(priority_raised), Ordering::Release);
                if !priority_raised {
                    crate::probe::log("CP2077 pair sampler could not raise its thread priority");
                }
                let mut verdicts = VerdictCache::default();
                let mut cadence = CadenceStats::new(ten_ms_qpc, thirty_ms_qpc);
                while !stopped.load(Ordering::Acquire) {
                    let sampled = player_audit_qpc().and_then(|now| {
                        let verdict = verdicts.admitted(now, refresh_qpc, audit_verdict_at)?;
                        player_verdict_admits(
                            &verdict,
                            generation,
                            now,
                            ttl_qpc,
                            source.addresses,
                        )?;
                        source.read(&verdict, player_audit_qpc)
                    });
                    match &sampled {
                        Ok(pair) => cadence.read(pair.first_end_qpc),
                        Err(_) => cadence.failed(),
                    }
                    // Publish first; the diagnostic copy must never delay the join's ring.
                    if let Ok(mut shared) = thread_shared.lock() {
                        match &sampled {
                            Ok(pair) => {
                                if !shared.ring.observe(pair.clone()) {
                                    shared.last_failure =
                                        Some("CP2077 pair sampler clock went backwards".into());
                                }
                            }
                            // A failed read is simply no observation: the ring judges
                            // continuity and freshness by time between successful reads.
                            Err(reason) => {
                                if shared.last_failure.as_deref() != Some(reason.as_str()) {
                                    shared.last_failure = Some(reason.clone());
                                }
                            }
                        }
                    }
                    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
                    match &sampled {
                        Ok(pair) => crate::cp2077_camera_diagnostics::dense_pairs::observe(pair),
                        Err(reason) => {
                            crate::cp2077_camera_diagnostics::dense_pairs::interrupt(reason);
                        }
                    }
                    std::thread::sleep(PERIOD);
                }
                crate::probe::log(&cadence.summary(qpc_frequency, priority_raised));
            })
            .map_err(|error| format!("start CP2077 pair sampler: {error}"))?;
        Ok(Self {
            shared,
            stop,
            join: Some(join),
            freshness_qpc: qpc_per_ms(FRESHNESS_MS),
        })
    }

    /// Which sampled simulation state carried the camera this Present rendered.
    pub(crate) fn join(
        &self,
        present_qpc: i64,
        camera_position: [f64; 3],
        camera_rotation: [f64; 4],
    ) -> Result<PoseJoin, String> {
        if self.join.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err("CP2077 pair sampler thread is not running".into());
        }
        let shared = self
            .shared
            .lock()
            .map_err(|_| "CP2077 pair ring poisoned".to_string())?;
        shared
            .ring
            .join(
                present_qpc,
                camera_position,
                camera_rotation,
                CP2077_POSE_JOIN_TOLERANCE,
                self.freshness_qpc,
            )
            .map_err(|refusal| {
                let cause = shared
                    .last_failure
                    .as_deref()
                    .map_or_else(String::new, |reason| {
                        format!(" last_sampler_failure={reason:?}")
                    });
                match refusal {
                    PoseJoinRefusal::Empty => {
                        format!("CP2077 pair ring has no sampled state yet{cause}")
                    }
                    PoseJoinRefusal::SamplerStale {
                        newest_observation_age_qpc,
                    } => format!(
                        "CP2077 pair sampler has no successful read within {FRESHNESS_MS} ms of this Present: newest_observation_age_qpc={newest_observation_age_qpc}{cause}"
                    ),
                    PoseJoinRefusal::NoCameraMatch {
                        entries,
                        eligible,
                        nearest_position_metres,
                        nearest_rotation_degrees,
                    } => format!(
                        "no sampled CP2077 simulation state carries this Present's camera: entries={entries} eligible={eligible} nearest_position_m={nearest_position_metres:.6} nearest_rotation_deg={nearest_rotation_degrees:.6}{cause}"
                    ),
                }
            })
    }
}

impl Drop for PairSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            crate::probe::log("CP2077 pair sampler thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(qpc: i64) -> PlayerAuditVerdict {
        PlayerAuditVerdict {
            generation: 1,
            addresses: FormalPlayerAddresses {
                system_object: 0,
                player_slot: std::ptr::null(),
                player_object: 0,
                transform_component_slot: std::ptr::null(),
                transform_component: 0,
                component_owner_slot: std::ptr::null(),
                world_transform: std::ptr::null(),
            },
            audited_qpc: qpc,
        }
    }

    #[test]
    fn cadence_stats_count_the_holes_that_can_swallow_a_state() {
        // 1 QPC tick per microsecond: 10 ms = 10_000 ticks.
        let mut stats = CadenceStats::new(10_000, 30_000);
        stats.failed();
        for completed in [1_000, 2_000, 3_500, 53_500, 54_500, 86_000, 97_000] {
            stats.read(completed);
        }
        assert_eq!((stats.reads, stats.failures), (7, 1));
        assert_eq!(stats.max_gap_qpc, 50_000);
        // 50 ms, 31.5 ms and 11 ms holes; only the first two can hide a ~33 ms state.
        assert_eq!((stats.gaps_over_10ms, stats.gaps_over_30ms), (3, 2));
        assert_eq!(
            stats.summary(1_000_000, true),
            "CP2077 pair sampler stopped: reads=7 failures=1 max_gap_ms=50.0 gaps_over_10ms=3 gaps_over_30ms=2 priority_raised=true"
        );
    }

    #[test]
    fn a_failing_stretch_does_not_make_the_sampler_hammer_the_audit_lock() {
        let mut cache = VerdictCache::default();
        let mut lookups = 0;
        let mut at = |now, cache: &mut VerdictCache, result: Result<PlayerAuditVerdict, String>| {
            cache.admitted(now, 50, |_| {
                lookups += 1;
                result
            })
        };
        assert!(at(1_000, &mut cache, Err("audit has not run yet".into())).is_err());
        // Every millisecond of a failing stretch reuses the cached answer ...
        for now in 1_001..=1_050 {
            assert!(at(now, &mut cache, Ok(verdict(now))).is_err());
        }
        // ... and the refresh cadence, not the failure, decides when the lock is taken again.
        assert_eq!(
            at(1_051, &mut cache, Ok(verdict(1_051)))
                .unwrap()
                .audited_qpc,
            1_051
        );
        assert_eq!(
            at(1_060, &mut cache, Ok(verdict(1_060)))
                .unwrap()
                .audited_qpc,
            1_051
        );
        assert_eq!(lookups, 2);
    }
}
