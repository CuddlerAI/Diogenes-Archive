mod artifact;
mod hash;
mod media;
mod model;
mod plan;
mod spec;

pub use artifact::{NormalizationReport, VerificationReport, normalize, verify};
pub use media::{FfmpegBackend, MediaBackend, MediaIdentity, VideoProbe};
pub use model::{
    FramePlanRow, NormalizationRequest, NormalizedLabelRow, RequestRun, RequestSegment,
    SourceFrameTiming,
};
pub use plan::{PreparedNormalization, prepare};
pub use spec::{LoadedSpec, NormalizationSpec, RationalRate};

pub const TOOL_NAME: &str = "dataminer-dataset-normalizer";
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
