//! Diagnostic-only actor-root read at a real CPU sampling time, with NO fabricated Present.
//! Uses the existing exact-build chain/audit and decoder, never the formal sampler's cache.
use super::*;

#[derive(Debug)]
pub(crate) struct Sample {
    pub position: [f64; 3],
    pub rotation: [f64; 4],
    pub stable_read_attempts: usize,
    pub words: [u32; 8],
    pub audit_qpc: i64,
}

fn stable_words(
    mut read: impl FnMut() -> Result<WorldTransformWords, String>,
) -> Result<(WorldTransformWords, usize), String> {
    let mut previous = read()?;
    for attempts in 2..=STABLE_READ_ATTEMPTS {
        let current = read()?;
        if current == previous {
            return Ok((current, attempts));
        }
        previous = current;
    }
    Err("diagnostic actor-root changed during bounded stable reads".into())
}

fn identity_matches(
    addresses: FormalPlayerAddresses,
    mut read: impl FnMut(usize) -> Result<usize, String>,
) -> Result<(), String> {
    if read(addresses.player_slot as usize)? != addresses.player_object
        || read(addresses.transform_component_slot as usize)? != addresses.transform_component
        || read(addresses.component_owner_slot as usize)? != addresses.player_object
    {
        return Err("diagnostic actor-root pointer/owner identity changed".into());
    }
    Ok(())
}

pub(crate) fn sample(
    addresses: FormalPlayerAddresses,
    axis_mapping: depth_lab::D3d12AxisMapping,
    metres_per_unit: f64,
    generation: u64,
    sample_qpc: i64,
    qpc_frequency: i64,
) -> Result<Sample, String> {
    if sample_qpc <= 0 || qpc_frequency <= 0 {
        return Err("diagnostic sampling QPC/frequency invalid".into());
    }
    let verdict = {
        let guard = PLAYER_AUDITS
            .try_lock()
            .map_err(|_| "diagnostic player audit busy or poisoned")?;
        if let Some(error) = &guard.fatal_error {
            return Err(error.clone());
        }
        guard
            .latest_at_or_before(sample_qpc)
            .ok_or("diagnostic player audit unavailable at sampling time")?
            .sampler_verdict()?
    };
    player_verdict_admits(
        &verdict,
        generation,
        sample_qpc,
        qpc_frequency.saturating_mul(AUDIT_VERDICT_TTL_MS) / 1_000,
        addresses,
    )?;
    // No audit/diagnostic/tracker lock is held across RPM. Re-check identity after the read;
    // stability is not an engine tick, transaction, or lifetime lease (RPM remains fallible).
    identity_matches(addresses, read_process_value_no_query::<usize>)?;
    let (words, stable_read_attempts) = stable_words(|| {
        read_process_value_no_query::<WorldTransformWords>(addresses.world_transform as usize)
    })?;
    identity_matches(addresses, read_process_value_no_query::<usize>)?;
    let (position, rotation) = decode_world_transform(words, axis_mapping, metres_per_unit)?;
    Ok(Sample {
        position,
        rotation,
        stable_read_attempts,
        words,
        audit_qpc: verdict.audited_qpc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rpm_uses_sampling_clock_and_rejects_busy_future_expired_or_wrong_audit() {
        let _guard = crate::camera_probe::tests::CAMERA_TEST_LOCK.lock().unwrap();
        clear_player_verdict();
        let player = 2usize;
        let component = 3usize;
        let owner = 2usize;
        let words = [131072u32, 262144, 393216, 0, 0, 0, 0, 1.0f32.to_bits()];
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: &player,
            player_object: player,
            transform_component_slot: &component,
            transform_component: component,
            component_owner_slot: &owner,
            world_transform: &words,
        };
        let axis = depth_lab::D3d12AxisMapping {
            delivery_x_from: depth_lab::D3d12SignedAxis::PositiveX,
            delivery_y_from: depth_lab::D3d12SignedAxis::PositiveZ,
            delivery_z_from: depth_lab::D3d12SignedAxis::PositiveY,
        };
        let read = |generation, qpc| sample(addresses, axis, 1., generation, qpc, 1000);
        assert!(read(7, 100).is_err());
        // Synthetic owned memory, not a game RTTI audit. No outside process is read.
        publish_player_verdict(Ok(PlayerAuditOutcome {
            generation: 7,
            addresses,
            completed_qpc: 100,
            result: Ok(()),
        }));
        assert!(read(7, 99).is_err());
        assert!(read(8, 101).is_err());
        assert!(read(7, 1101).is_err());
        let result = read(7, 101).unwrap();
        assert_eq!(result.position, [1., 3., 2.]);
        assert_eq!(result.rotation, [0., 0., 0., 1.]);
        assert_eq!(result.stable_read_attempts, 2);
        assert_eq!(result.words, words);
        assert_eq!(result.audit_qpc, 100);
        {
            let _audit_lock = PLAYER_AUDITS.lock().unwrap();
            assert!(read(7, 101).unwrap_err().contains("busy"));
        }
        publish_player_verdict(Ok(PlayerAuditOutcome {
            generation: 7,
            addresses,
            completed_qpc: 200,
            result: Err("new audit failure".into()),
        }));
        assert!(read(7, 101).is_ok());
        assert_eq!(read(7, 200).unwrap_err(), "new audit failure");
        clear_player_verdict();
    }

    #[test]
    fn stable_read_counts_whole_payloads_and_never_carries_a_failed_value() {
        let a = [1; 8];
        let mut b = a;
        b[7] = 2;
        let mut input = [Ok(a), Ok(b), Ok(b)].into_iter();
        assert_eq!(stable_words(|| input.next().unwrap()).unwrap(), (b, 3));
        let mut input = [Ok(a), Err("RPM failed".into())].into_iter();
        assert_eq!(
            stable_words(|| input.next().unwrap()).unwrap_err(),
            "RPM failed"
        );
        let mut calls = 0;
        assert!(
            stable_words(|| {
                calls += 1;
                Ok([calls; 8])
            })
            .is_err()
        );
        assert_eq!(calls as usize, STABLE_READ_ATTEMPTS);
    }

    #[test]
    fn pointer_identity_rejects_each_stale_relation_without_raw_dereference() {
        let addresses = FormalPlayerAddresses {
            system_object: 1,
            player_slot: 10usize as *const usize,
            player_object: 2,
            transform_component_slot: 20usize as *const usize,
            transform_component: 3,
            component_owner_slot: 30usize as *const usize,
            world_transform: std::ptr::null(),
        };
        let value = |address| match address {
            10 | 30 => 2,
            20 => 3,
            _ => panic!(),
        };
        assert!(identity_matches(addresses, |a| Ok(value(a))).is_ok());
        for bad in [10, 20, 30] {
            assert!(
                identity_matches(addresses, |a| Ok(if a == bad { 99 } else { value(a) })).is_err()
            );
        }
        assert!(identity_matches(addresses, |_| Err("RPM failure".into())).is_err());
    }
}
