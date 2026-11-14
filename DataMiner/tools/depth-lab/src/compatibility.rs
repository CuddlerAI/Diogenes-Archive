//! Bounded local Present-cadence verification for reuse of a signed baseline.
//! This checks current operating conditions; it never computes a performance certificate.
use serde::{Deserialize, Serialize};

pub const RECEIPT_FILE: &str = "depth-local-compatibility.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCompatibilityReceipt {
    pub schema: String,
    pub identity: crate::StreamIdentity,
    pub required_fps: u32,
    pub observed_fps: f64,
    pub observed_seconds: f64,
}

impl LocalCompatibilityReceipt {
    pub fn validate(
        &self,
        identity: &crate::StreamIdentity,
        required_fps: u32,
    ) -> Result<(), String> {
        if self.schema != "depth-local-compatibility/v1"
            || &self.identity != identity
            || self.required_fps != required_fps
            || !matches!(required_fps, 30 | 60)
            || !self.observed_seconds.is_finite()
            || self.observed_seconds < 2.0
            || !self.observed_fps.is_finite()
            || (self.observed_fps < f64::from(required_fps) * 0.95
                || self.observed_fps > f64::from(required_fps) * 1.05)
        {
            return Err(
                "DEPTH_LOCAL_VALIDATION_FAILED: 本机帧率校验缺失、身份不符或不满足基准工况".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct CadenceCheck {
    required_fps: u32,
    frequency: u64,
    start: Option<i64>,
    last: Option<i64>,
    intervals: u64,
    pub last_pass: Option<(f64, f64)>,
    pub error: Option<String>,
}

impl CadenceCheck {
    pub fn new(required_fps: u32, frequency: u64) -> Result<Self, String> {
        if !matches!(required_fps, 30 | 60) || frequency == 0 {
            return Err("invalid local cadence contract".into());
        }
        Ok(Self {
            required_fps,
            frequency,
            start: None,
            last: None,
            intervals: 0,
            last_pass: None,
            error: None,
        })
    }

    pub fn observe(&mut self, qpc: i64) {
        if self.error.is_some() {
            return;
        }
        if qpc <= 0 || self.last.is_some_and(|last| qpc <= last) {
            self.error = Some("DEPTH_LOCAL_VALIDATION_FAILED: Present时钟无效或未递增".into());
            return;
        }
        self.last = Some(qpc);
        let Some(start) = self.start else {
            self.start = Some(qpc);
            return;
        };
        self.intervals += 1;
        let seconds = (qpc - start) as f64 / self.frequency as f64;
        if seconds < 2.0 {
            return;
        }
        let fps = self.intervals as f64 / seconds;
        if fps < f64::from(self.required_fps) * 0.95 || fps > f64::from(self.required_fps) * 1.05 {
            self.error = Some(format!(
                "DEPTH_PRESENT_RATE_MISMATCH: 基准要求 {} FPS（±5%），本机实测 {fps:.2} FPS；请检查游戏限帧、帧生成和当前负载",
                self.required_fps
            ));
        } else {
            self.last_pass = Some((fps, seconds));
        }
        self.start = Some(qpc);
        self.intervals = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sixty_fps_baseline_checks_actual_source_cadence() {
        for (actual, expected_pass) in [(60, true), (30, false), (120, false)] {
            let mut check = CadenceCheck::new(60, 6000).unwrap();
            for i in 0..=240 {
                check.observe(1 + i * (6000 / actual));
            }
            assert_eq!(
                check.last_pass.is_some() && check.error.is_none(),
                expected_pass
            );
        }
        let identity = crate::StreamIdentity {
            session_id: "s".into(),
            run_id: "r".into(),
            generation: 1,
        };
        let receipt = LocalCompatibilityReceipt {
            schema: "depth-local-compatibility/v1".into(),
            identity: identity.clone(),
            required_fps: 60,
            observed_fps: 60.0,
            observed_seconds: 2.0,
        };
        assert!(receipt.validate(&identity, 60).is_ok());
        assert!(receipt.validate(&identity, 30).is_err());
    }
    #[test]
    fn correct_cadence_needs_a_complete_window_and_remains_monitored() {
        let mut gate = CadenceCheck::new(30, 3000).unwrap();
        for i in 0..60 {
            gate.observe(1 + i * 100);
        }
        assert!(gate.last_pass.is_none());
        gate.observe(6001);
        assert_eq!(gate.last_pass, Some((30.0, 2.0)));
        for i in 1..=240 {
            gate.observe(6001 + i * 25);
        }
        assert!(gate.error.unwrap().contains("120.00"));
    }
    #[test]
    fn slow_or_fast_or_invalid_clock_cannot_pass() {
        for step in [25, 200] {
            let mut gate = CadenceCheck::new(30, 3000).unwrap();
            for i in 0..=240 {
                gate.observe(1 + i * step);
            }
            assert!(gate.error.is_some());
            assert!(gate.last_pass.is_none());
        }
        let mut gate = CadenceCheck::new(30, 3000).unwrap();
        gate.observe(1);
        gate.observe(1);
        assert!(gate.error.is_some());
    }
    #[test]
    fn receipt_cannot_be_reused_across_runs_or_accept_nonfinite_values() {
        let identity = crate::StreamIdentity {
            session_id: "s".into(),
            run_id: "r0".into(),
            generation: 1,
        };
        let mut receipt = LocalCompatibilityReceipt {
            schema: "depth-local-compatibility/v1".into(),
            identity: identity.clone(),
            required_fps: 30,
            observed_fps: 30.0,
            observed_seconds: 2.0,
        };
        assert!(receipt.validate(&identity, 30).is_ok());
        receipt.identity.generation = 2;
        assert!(receipt.validate(&identity, 30).is_err());
        receipt.identity = identity.clone();
        receipt.observed_fps = f64::NAN;
        assert!(receipt.validate(&identity, 30).is_err());
    }
}
