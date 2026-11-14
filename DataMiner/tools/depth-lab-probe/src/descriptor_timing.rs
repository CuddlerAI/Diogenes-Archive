//! Fixed-size, address-free copy diagnostics. Existing shard clocks are reused.
//! Wait includes scheduling and is summed across visits, not thread CPU time.
use std::time::Duration;

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPhase {
    Source,
    Destination,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DescriptorCopySample {
    pub descriptors: u32,
    /// Union of source/destination shards; not the number of lock acquisitions.
    pub shard_count: u32,
    pub source_visits: u32,
    pub destination_visits: u32,
    pub completed: bool,
    pub total_ns: u64,
    pub prepare_ns: u64,
    pub wait_ns: u64,
    pub apply_ns: u64,
    pub residual_ns: u64,
    pub max_wait_ns: u64,
    pub max_wait_shard: Option<u8>,
    pub max_wait_phase: Option<CopyPhase>,
    pub max_wait_group_descriptors: u32,
}

impl DescriptorCopySample {
    pub(crate) fn visit(&mut self, shard: usize, phase: CopyPhase, count: usize, wait: Duration) {
        match phase {
            CopyPhase::Source => self.source_visits += 1,
            CopyPhase::Destination => self.destination_visits += 1,
        }
        let wait_ns = crate::execute_prepare::nanos(wait);
        if self.max_wait_shard.is_none() || wait_ns > self.max_wait_ns {
            self.max_wait_ns = wait_ns;
            self.max_wait_shard = Some(shard as u8); // store has exactly 64 shards
            self.max_wait_phase = Some(phase);
            self.max_wait_group_descriptors = count as u32; // plan <=4096
        }
    }

    pub(crate) fn finish(&mut self, total: Duration) {
        self.total_ns = crate::execute_prepare::nanos(total);
        let children = self
            .prepare_ns
            .saturating_add(self.wait_ns)
            .saturating_add(self.apply_ns);
        // Includes generation allocation, guard release, Vec drop and instrumentation.
        self.residual_ns = self.total_ns.saturating_sub(children);
    }
}
