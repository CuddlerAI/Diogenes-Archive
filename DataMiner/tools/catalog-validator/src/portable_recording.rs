use std::sync::Arc;

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecordingCatalogEntry {
    pub id: String,
    pub version: String,
    pub content_hash: String,
    #[serde(skip)]
    pub source_bytes: Arc<Vec<u8>>,
}

/// Mirrors `capture_core::recording::CatalogProvenance`. Kept in sync by
/// construction: `main.rs` compiles the real client `catalog.rs` against this
/// module, so a field or variant that exists there and not here is a build
/// failure of this validator, not a silent divergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogProvenance {
    Compiled,
    Delivered,
}

impl CatalogProvenance {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compiled => "compiled",
            Self::Delivered => "delivered",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecordingCatalogContext {
    /// 与 capture-core 的同名字段一致：这场采集属于哪一代摄取契约（issue #855 §B）。
    #[serde(skip)]
    pub contract: crate::ingest_contract::SessionContract,
    /// legacy 目录身份的 `mode` 字面量；中性契约缺席。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// legacy 一代 manifest 级的渠道派生分类（docs/57 §3.4）；中性契约缺席。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_trainable: Option<bool>,
    pub provenance: CatalogProvenance,
    pub catalog_id: String,
    pub catalog_version: String,
    pub catalog_content_hash: String,
    #[serde(skip)]
    pub catalog_source_bytes: Arc<Vec<u8>>,
    pub device: RecordingCatalogEntry,
    /// capture + settings 并成一本（docs/57 §3.1）。
    pub recording: RecordingCatalogEntry,
    pub game: RecordingCatalogEntry,
    pub mapping: RecordingCatalogEntry,
}

#[derive(Clone, Debug)]
pub(crate) struct HardwareWindowsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct HardwarePreflightEvidence {
    pub windows: HardwareWindowsVersion,
    pub adapter_vendor_id: u32,
    pub adapter_device_id: u32,
    pub adapter_driver_version: Option<String>,
}
