use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAPPING_SCHEMA_VERSION: &str = "dm-mapping-2";
/// docs/57 断代前的上一代 mapping schema（`settings_profile_id` 键）。只被
/// [`MappingSnapshot::from_legacy_v1_slice`] 消费；客户端一律走 [`MappingSnapshot::from_slice`]，
/// 它对这一代照旧硬拒。
const LEGACY_MAPPING_SCHEMA_V1: &str = "dm-mapping-1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GameMapping {
    pub schema_version: String,
    pub mapping_id: String,
    pub version: String,
    pub game: MappingGame,
    pub taxonomy: MappingTaxonomy,
    pub recording_profile_id: String,
    pub bindings: Vec<InputBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MappingGame {
    pub id: String,
    pub name: String,
    pub platform: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MappingTaxonomy {
    pub content: String,
    pub view: String,
    pub material: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputBinding {
    pub device: InputDevice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<MouseControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<u16>,
    pub semantic: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDevice {
    Keyboard,
    Mouse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseControl {
    DeltaX,
    DeltaY,
    Wheel,
    WheelHorizontal,
}

#[derive(Clone, Debug)]
pub struct MappingSnapshot {
    mapping: Arc<GameMapping>,
    source_bytes: Arc<[u8]>,
    content_hash: Arc<str>,
}

impl MappingSnapshot {
    pub fn from_slice(bytes: &[u8]) -> anyhow::Result<Self> {
        let mapping: GameMapping =
            serde_json::from_slice(bytes).context("invalid game mapping JSON")?;
        mapping.validate()?;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Ok(Self {
            mapping: Arc::new(mapping),
            source_bytes: Arc::from(bytes),
            content_hash: Arc::from(format!("sha256:{:x}", hasher.finalize())),
        })
    }

    /// 读 docs/57 断代前的 v1 mapping（`dm-mapping-1`，`settings_profile_id` 键）。
    ///
    /// **只给 data-demo-converter 的 bundle-1 遗留车道读封存 RAW 用**（issue #811
    /// 路线 A）；客户端目录消费一律走 [`Self::from_slice`]，对 v1 保持断代硬拒。
    /// v1→v2 的全部差异就是 schema 字符串 + `settings_profile_id` 改名
    /// `recording_profile_id`（e433de36），所以这里在内存里做规范化：字段搬进
    /// `recording_profile_id`、schema 写成当前值，让绑定校验与投影零分叉。
    /// `content_hash`/`source_bytes` 仍取**原始字节**——封存链路声明的是 v1 文件的
    /// 哈希，规范化只存在于内存，绝不回写。
    pub fn from_legacy_v1_slice(bytes: &[u8]) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyGameMappingV1 {
            schema_version: String,
            mapping_id: String,
            version: String,
            game: MappingGame,
            taxonomy: MappingTaxonomy,
            settings_profile_id: String,
            bindings: Vec<InputBinding>,
        }
        let legacy: LegacyGameMappingV1 =
            serde_json::from_slice(bytes).context("invalid legacy v1 game mapping JSON")?;
        ensure!(
            legacy.schema_version == LEGACY_MAPPING_SCHEMA_V1,
            "unsupported legacy mapping schema {} (expected {LEGACY_MAPPING_SCHEMA_V1})",
            legacy.schema_version
        );
        let mapping = GameMapping {
            schema_version: MAPPING_SCHEMA_VERSION.to_owned(),
            mapping_id: legacy.mapping_id,
            version: legacy.version,
            game: legacy.game,
            taxonomy: legacy.taxonomy,
            recording_profile_id: legacy.settings_profile_id,
            bindings: legacy.bindings,
        };
        mapping.validate()?;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Ok(Self {
            mapping: Arc::new(mapping),
            source_bytes: Arc::from(bytes),
            content_hash: Arc::from(format!("sha256:{:x}", hasher.finalize())),
        })
    }

    #[must_use]
    pub fn mapping(&self) -> &GameMapping {
        &self.mapping
    }

    #[must_use]
    pub fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }

    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
}

impl GameMapping {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema_version == MAPPING_SCHEMA_VERSION,
            "unsupported mapping schema {}",
            self.schema_version
        );
        for (name, value) in [
            ("mapping_id", self.mapping_id.as_str()),
            ("version", self.version.as_str()),
            ("game.id", self.game.id.as_str()),
            ("game.name", self.game.name.as_str()),
            ("game.platform", self.game.platform.as_str()),
            ("taxonomy.content", self.taxonomy.content.as_str()),
            ("taxonomy.view", self.taxonomy.view.as_str()),
            ("taxonomy.material", self.taxonomy.material.as_str()),
            ("recording_profile_id", self.recording_profile_id.as_str()),
        ] {
            ensure!(!value.trim().is_empty(), "mapping {name} must not be empty");
        }
        ensure!(!self.bindings.is_empty(), "mapping has no input bindings");

        let mut physical = HashSet::new();
        for binding in &self.bindings {
            ensure!(
                valid_semantic(&binding.semantic),
                "invalid mapping semantic {}",
                binding.semantic
            );
            let key = match binding.device {
                InputDevice::Keyboard => {
                    let scan_code = binding
                        .scan_code
                        .context("keyboard binding has no scan_code")?;
                    let extended = binding
                        .extended
                        .context("keyboard binding has no extended flag")?;
                    ensure!(
                        binding.control.is_none() && binding.button.is_none(),
                        "keyboard binding contains mouse fields"
                    );
                    format!("keyboard:{scan_code}:{extended}")
                }
                InputDevice::Mouse => {
                    ensure!(
                        binding.scan_code.is_none() && binding.extended.is_none(),
                        "mouse binding contains keyboard fields"
                    );
                    match (binding.control, binding.button) {
                        (Some(control), None) => format!("mouse:control:{control:?}"),
                        (None, Some(button)) => {
                            ensure!(
                                matches!(button, 1 | 2 | 4 | 5 | 6),
                                "unsupported mouse button {button}"
                            );
                            format!("mouse:button:{button}")
                        }
                        _ => anyhow::bail!(
                            "mouse binding must declare exactly one control or button"
                        ),
                    }
                }
            };
            ensure!(physical.insert(key), "mapping contains a duplicate binding");
        }
        Ok(())
    }

    #[must_use]
    pub fn keyboard_semantic(&self, scan_code: u16, extended: bool) -> Option<&str> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.device == InputDevice::Keyboard
                    && binding.scan_code == Some(scan_code)
                    && binding.extended == Some(extended)
            })
            .map(|binding| binding.semantic.as_str())
    }

    #[must_use]
    pub fn mouse_button_semantic(&self, button: u16) -> Option<&str> {
        self.bindings
            .iter()
            .find(|binding| binding.device == InputDevice::Mouse && binding.button == Some(button))
            .map(|binding| binding.semantic.as_str())
    }

    #[must_use]
    pub fn mouse_control_semantic(&self, control: MouseControl) -> Option<&str> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.device == InputDevice::Mouse && binding.control == Some(control)
            })
            .map(|binding| binding.semantic.as_str())
    }
}

fn valid_semantic(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &[u8] = br#"{
      "schema_version":"dm-mapping-2",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "recording_profile_id":"test-recording-v1",
      "bindings":[
        {"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"},
        {"device":"mouse","control":"delta_x","semantic":"camera.yaw"},
        {"device":"mouse","button":1,"semantic":"combat.primary_attack"}
      ]
    }"#;

    #[test]
    fn mapping_snapshot_preserves_identity_and_source_hash() {
        let snapshot = MappingSnapshot::from_slice(VALID).unwrap();
        assert_eq!(snapshot.mapping().mapping_id, "game.test.pc");
        assert_eq!(snapshot.source_bytes(), VALID);
        assert!(snapshot.content_hash().starts_with("sha256:"));
        assert_eq!(
            snapshot.mapping().keyboard_semantic(17, false),
            Some("move.forward")
        );
    }

    /// 与封存 RAW 同形的 v1 mapping（真实 COD 车道的键序与字段集，issue #811）。
    const LEGACY_V1: &[u8] = br#"{
      "schema_version":"dm-mapping-1",
      "mapping_id":"game.test.pc",
      "version":"1",
      "game":{"id":"test","name":"Test","platform":"pc"},
      "taxonomy":{"content":"act","view":"first_person","material":"3d"},
      "settings_profile_id":"test-settings-v1",
      "bindings":[
        {"device":"keyboard","scan_code":17,"extended":false,"semantic":"move.forward"},
        {"device":"mouse","control":"delta_x","semantic":"camera.yaw"},
        {"device":"mouse","button":1,"semantic":"combat.primary_attack"}
      ]
    }"#;

    #[test]
    fn legacy_v1_reader_canonicalizes_in_memory_but_hashes_original_bytes() {
        let snapshot = MappingSnapshot::from_legacy_v1_slice(LEGACY_V1).unwrap();
        // settings_profile_id 搬进 recording_profile_id，schema 规范化为当前值。
        assert_eq!(snapshot.mapping().recording_profile_id, "test-settings-v1");
        assert_eq!(snapshot.mapping().schema_version, MAPPING_SCHEMA_VERSION);
        // 哈希与源字节必须仍指向 v1 原文件——封存链路声明的是它。
        assert_eq!(snapshot.source_bytes(), LEGACY_V1);
        let mut hasher = Sha256::new();
        hasher.update(LEGACY_V1);
        assert_eq!(
            snapshot.content_hash(),
            format!("sha256:{:x}", hasher.finalize())
        );
        assert_eq!(
            snapshot.mapping().keyboard_semantic(17, false),
            Some("move.forward")
        );
    }

    #[test]
    fn legacy_v1_reader_refuses_current_generation_and_vice_versa() {
        // 断代不放松：客户端入口照旧拒 v1……
        assert!(MappingSnapshot::from_slice(LEGACY_V1).is_err());
        // ……legacy 入口也不吃 v2（形状不对：recording_profile_id 是未知字段）。
        assert!(MappingSnapshot::from_legacy_v1_slice(VALID).is_err());
        // 杂交形态（v1 schema + v2 键名）两个入口都必须拒。
        let hybrid = String::from_utf8(VALID.to_vec())
            .unwrap()
            .replace("dm-mapping-2", "dm-mapping-1");
        assert!(MappingSnapshot::from_slice(hybrid.as_bytes()).is_err());
        assert!(MappingSnapshot::from_legacy_v1_slice(hybrid.as_bytes()).is_err());
    }

    #[test]
    fn mapping_rejects_ambiguous_physical_bindings() {
        let text = String::from_utf8(VALID.to_vec())
            .unwrap()
            .replace(
                "{\"device\":\"mouse\",\"button\":1,\"semantic\":\"combat.primary_attack\"}",
                "{\"device\":\"mouse\",\"button\":1,\"semantic\":\"combat.primary_attack\"},\n        {\"device\":\"mouse\",\"button\":1,\"semantic\":\"combat.alt_attack\"}",
            );
        assert!(MappingSnapshot::from_slice(text.as_bytes()).is_err());
    }
}
