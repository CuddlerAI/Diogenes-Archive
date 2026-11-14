//! catalog 里的 depth 两件套：解析、校验、拒绝。
//!
//! [130 号 §2](../../../docs/130-depth-capture-inputs-auto-provisioning.md) 的 P1。
//! 采集员现在要手填 `depth-profile` 与 `camera-binding` 两个绝对路径；
//! 它们按游戏分发，并严格按 `executable_sha256` 钉死。
//! 普通 RGB 窗口发现可只按进程名；Depth 不得将旧版内存布局套用到未验证版本。
//! 所以该随 catalog bundle 走，而不是让人填路径。
//!
//! # 为什么两个 payload 都是可选的
//!
//! 绝大多数游戏没有 depth 能力。用「payload 在不在」表达「这个游戏支不支持
//! depth 采集」，比另设一个布尔字段更不容易与事实脱节——布尔字段会漂移，
//! 而「文件不在那儿」不会。
//!
//! # 为什么必须两个都在或两个都不在
//!
//! 只有 profile 没有 binding：能采深度但没有相机位姿，产出的是
//! **缺一件交付物的包**，而这件事要到交付阶段才发现。
//! 只有 binding 没有 profile：连 DSV 都选不出来，探针根本起不来。
//!
//! 两种半配都是配置事故，不是合法状态——所以在解析这一层就拒绝。

use serde::{Deserialize, Serialize};

/// catalog 里一条 payload 的登记（与 `game_profiles` 等同构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogPayloadRef {
    pub id: String,
    pub version: String,
    /// `sha256:<64 hex>`。catalog 既有的逐 payload 钉扣。
    pub content_hash: String,
}

/// 解析 depth 两件套时的拒绝原因。
///
/// **每一条都要能区分「这个游戏不支持」与「支持但配错了」**——
/// 前者是正常状态，后者要人去修，混在一起会让采集员对着一个
/// 本来就没有 depth 的游戏反复排查。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum DepthCatalogRefusal {
    /// 这个游戏的 catalog 里没有 depth 两件套。**正常状态，不是错误。**
    NotOffered,
    /// 只登记了一件。见模块文档：两种半配都是配置事故。
    HalfConfigured {
        has_profile: bool,
        has_binding: bool,
    },
    /// payload 字节与 catalog 登记的 `content_hash` 不符。
    ContentHashMismatch {
        payload: &'static str,
        expected: String,
        actual: String,
    },
    /// payload 解析不了。
    Malformed {
        payload: &'static str,
        error: String,
    },
    /// **profile 与 binding 钉的不是同一个可执行文件。**
    ///
    /// 这说明 catalog 里两件套来自不同批次的发现——按其中任一个采都是错的。
    ExecutableMismatchBetweenPayloads { profile: String, binding: String },
    /// 钉的可执行文件与**实际运行的游戏**不符。
    ///
    /// 最常见的原因是游戏更新了。文案必须说清这一点——
    /// 说成泛泛的「配置错误」会让人去查配置，而真因是 Steam 推了补丁。
    ExecutableMismatchWithRunningGame { pinned: String, running: String },
}

impl DepthCatalogRefusal {
    /// 给采集员看的一句话。**不出现内部术语。**
    #[must_use]
    pub fn annotator_message(&self) -> String {
        match self {
            Self::NotOffered => "这个游戏暂不支持深度采集。".into(),
            Self::ExecutableMismatchWithRunningGame { .. } => {
                "当前游戏版本尚未完成深度采集适配——可能是游戏更新了。\
                 请联系工程适配并验证该版本，暂时不能开启深度采集；重跑性能认证不能解除此限制。"
                    .into()
            }
            _ => "深度采集配置有问题，暂不能开启。请联系工程并附上诊断信息。".into(),
        }
    }

    /// 给工程看的补充。采集员文案有「不出现内部术语」的硬规则，
    /// 而这些错不给出具体值就查不动——所以分两路（与 `GateDenial` 同一分法）。
    #[must_use]
    pub fn diagnostic(&self) -> Option<String> {
        match self {
            Self::NotOffered => None,
            Self::HalfConfigured {
                has_profile,
                has_binding,
            } => Some(format!(
                "catalog 只登记了一件：profile={has_profile} binding={has_binding}。\
                 两件套必须同时存在——只有 profile 会产出缺相机位姿的包（到交付才发现），\
                 只有 binding 则探针起不来。"
            )),
            Self::ContentHashMismatch {
                payload,
                expected,
                actual,
            } => Some(format!(
                "{payload} 内容哈希不符：登记 {expected}，实得 {actual}"
            )),
            Self::Malformed { payload, error } => Some(format!("{payload} 解析失败：{error}")),
            Self::ExecutableMismatchBetweenPayloads { profile, binding } => Some(format!(
                "profile 钉 {profile}，binding 钉 {binding}——两件套来自不同批次的发现，\
                 按其中任一个采都是错的。"
            )),
            Self::ExecutableMismatchWithRunningGame { pinned, running } => Some(format!(
                "配置钉 {pinned}，实际运行 {running}。按 docs/95 重新发现 binding 并提交新证据。"
            )),
        }
    }
}

/// catalog 侧的两件套输入（客户端读出来后交进来）。
pub struct DepthCatalogInput<'a> {
    /// Catalog game identity; no game may bypass the executable hash gate.
    pub game_id: &'a str,
    /// catalog.json 里的登记，`None` 表示没登记。
    pub profile_ref: Option<&'a CatalogPayloadRef>,
    pub binding_ref: Option<&'a CatalogPayloadRef>,
    /// 对应 payload 的原始字节。
    pub profile_bytes: Option<&'a [u8]>,
    pub binding_bytes: Option<&'a [u8]>,
    /// 实际运行的游戏 exe 的 sha256（`sha256:<hex>`）。
    pub running_executable_sha256: &'a str,
}

/// 解析结果：两件套的原始字节（已校验），交给探针配置去用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDepthCatalog {
    pub profile_json: String,
    pub binding_json: String,
    /// 两件套共同钉的可执行文件。
    pub executable_sha256: String,
}

/// 计算 `sha256:<hex>`。由调用方注入，避免本 crate 在 no-feature 下背上 sha2。
pub type Sha256Hex = fn(&[u8]) -> String;

/// 解析并校验 catalog 里的 depth 两件套。
///
/// 顺序是刻意的：**先判「有没有」，再判「对不对」**。
/// 反过来会让一个根本不提供 depth 的游戏报出哈希错误。
pub fn resolve(
    input: &DepthCatalogInput<'_>,
    sha256_hex: Sha256Hex,
) -> Result<ResolvedDepthCatalog, DepthCatalogRefusal> {
    let (pr, br) = match (input.profile_ref, input.binding_ref) {
        (None, None) => return Err(DepthCatalogRefusal::NotOffered),
        (Some(p), Some(b)) => (p, b),
        (p, b) => {
            return Err(DepthCatalogRefusal::HalfConfigured {
                has_profile: p.is_some(),
                has_binding: b.is_some(),
            });
        }
    };
    let pb = input
        .profile_bytes
        .ok_or(DepthCatalogRefusal::HalfConfigured {
            has_profile: false,
            has_binding: true,
        })?;
    let bb = input
        .binding_bytes
        .ok_or(DepthCatalogRefusal::HalfConfigured {
            has_profile: true,
            has_binding: false,
        })?;

    for (payload, bytes, r) in [("depth-profile", pb, pr), ("camera-binding", bb, br)] {
        let actual = sha256_hex(bytes);
        if actual != r.content_hash {
            return Err(DepthCatalogRefusal::ContentHashMismatch {
                payload,
                expected: r.content_hash.clone(),
                actual,
            });
        }
    }

    let profile_exe = pinned_executable(pb, "depth-profile")?;
    let binding_exe = pinned_executable(bb, "camera-binding")?;
    if profile_exe != binding_exe {
        return Err(DepthCatalogRefusal::ExecutableMismatchBetweenPayloads {
            profile: profile_exe,
            binding: binding_exe,
        });
    }
    if profile_exe != input.running_executable_sha256 {
        return Err(DepthCatalogRefusal::ExecutableMismatchWithRunningGame {
            pinned: profile_exe,
            running: input.running_executable_sha256.to_string(),
        });
    }

    Ok(ResolvedDepthCatalog {
        profile_json: String::from_utf8_lossy(pb).into_owned(),
        binding_json: String::from_utf8_lossy(bb).into_owned(),
        executable_sha256: profile_exe,
    })
}

fn pinned_executable(bytes: &[u8], payload: &'static str) -> Result<String, DepthCatalogRefusal> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| DepthCatalogRefusal::Malformed {
            payload,
            error: e.to_string(),
        })?;
    let executable = v.get("executable_sha256").and_then(|x| x.as_str()).ok_or(
        DepthCatalogRefusal::Malformed {
            payload,
            error: "缺 executable_sha256".into(),
        },
    )?;
    if !is_canonical_sha256(executable) {
        return Err(DepthCatalogRefusal::Malformed {
            payload,
            error: "executable_sha256 must be sha256:<64 lowercase hex>".into(),
        });
    }
    Ok(executable.to_owned())
}

fn is_canonical_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "sha256:428f9996cc4248e26c0f62f9fdd3eaf0e5eb305834b67ee5996538e593218b61";
    const OTHER: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    /// 测试用的假哈希：内容前缀化，够用来区分「一致/不一致」。
    fn fake_sha(b: &[u8]) -> String {
        format!("sha256:{:064x}", b.len())
    }

    fn payload(exe: &str) -> Vec<u8> {
        format!(r#"{{"executable_sha256":"{exe}"}}"#).into_bytes()
    }

    fn r#ref(bytes: &[u8]) -> CatalogPayloadRef {
        CatalogPayloadRef {
            id: "x".into(),
            version: "1".into(),
            content_hash: fake_sha(bytes),
        }
    }

    #[test]
    fn a_game_without_depth_is_not_an_error() {
        // 绝大多数游戏没有 depth 能力。把它报成错误，会让采集员
        // 对着一个本来就不支持的游戏反复排查。
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: None,
                binding_ref: None,
                profile_bytes: None,
                binding_bytes: None,
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();
        assert_eq!(e, DepthCatalogRefusal::NotOffered);
        assert!(e.diagnostic().is_none(), "正常状态不该有工程诊断");
        assert!(e.annotator_message().contains("暂不支持"));
    }

    #[test]
    fn half_configured_is_refused_at_parse_time() {
        // 只有 profile → 能采深度但没相机位姿，缺一件交付物，
        // 而那要到交付阶段才发现。必须在这里就拦住。
        let p = payload(EXE);
        let pr = r#ref(&p);
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: None,
                profile_bytes: Some(&p),
                binding_bytes: None,
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(e, DepthCatalogRefusal::HalfConfigured { .. }));
        assert!(e.diagnostic().unwrap().contains("到交付才发现"));
    }

    #[test]
    fn a_stale_content_hash_is_caught() {
        let p = payload(EXE);
        let b = payload(EXE);
        let mut pr = r#ref(&p);
        pr.content_hash = "sha256:deadbeef".into();
        let br = r#ref(&b);
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            DepthCatalogRefusal::ContentHashMismatch {
                payload: "depth-profile",
                ..
            }
        ));
    }

    #[test]
    fn two_payloads_pinned_to_different_builds_are_refused() {
        // 两件套来自不同批次的发现——按其中任一个采都是错的，
        // 而单看每一个都"合法"。只有交叉比对能发现。
        let p = payload(EXE);
        let b = payload(OTHER);
        let (pr, br) = (r#ref(&p), r#ref(&b));
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            DepthCatalogRefusal::ExecutableMismatchBetweenPayloads { .. }
        ));
    }

    #[test]
    fn a_game_update_says_so_instead_of_saying_config_error() {
        // 最常见的失败。说成「配置错误」会让人去查配置，而真因是 Steam 推了补丁。
        let p = payload(EXE);
        let b = payload(EXE);
        let (pr, br) = (r#ref(&p), r#ref(&b));
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: OTHER,
            },
            fake_sha,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            DepthCatalogRefusal::ExecutableMismatchWithRunningGame { .. }
        ));
        assert!(
            e.annotator_message().contains("游戏更新"),
            "{}",
            e.annotator_message()
        );
        assert!(
            e.diagnostic().unwrap().contains("docs/95"),
            "要指出下一步去哪"
        );
    }

    #[test]
    fn fallout4_rejects_a_different_runtime_executable() {
        let p = payload(EXE);
        let b = payload(EXE);
        let (pr, br) = (r#ref(&p), r#ref(&b));
        let error = resolve(
            &DepthCatalogInput {
                game_id: "fallout4-steam-release-test",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: OTHER,
            },
            fake_sha,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DepthCatalogRefusal::ExecutableMismatchWithRunningGame { .. }
        ));
        assert!(error.annotator_message().contains("尚未完成深度采集适配"));
        assert!(error.annotator_message().contains("重跑性能认证不能解除"));
    }

    #[test]
    fn a_complete_and_matching_pair_resolves() {
        let p = payload(EXE);
        let b = format!(r#"{{"executable_sha256":"{EXE}","bind_slot":12}}"#).into_bytes();
        let (pr, br) = (r#ref(&p), r#ref(&b));
        let ok = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap();
        assert_eq!(ok.executable_sha256, EXE);
        assert!(ok.binding_json.contains("bind_slot"));
    }

    #[test]
    fn executable_pin_must_be_canonical_lowercase_sha256() {
        let p = payload("SHA256:ABC");
        let b = payload(EXE);
        let (pr, br) = (r#ref(&p), r#ref(&b));
        let error = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: Some(&pr),
                binding_ref: Some(&br),
                profile_bytes: Some(&p),
                binding_bytes: Some(&b),
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DepthCatalogRefusal::Malformed {
                payload: "depth-profile",
                ..
            }
        ));
    }

    #[test]
    fn not_offered_is_checked_before_hashes() {
        // 顺序是刻意的：先判「有没有」再判「对不对」。反过来会让一个
        // 根本不提供 depth 的游戏报出哈希错误。
        let e = resolve(
            &DepthCatalogInput {
                game_id: "synthetic",
                profile_ref: None,
                binding_ref: None,
                profile_bytes: Some(b"garbage"),
                binding_bytes: Some(b"garbage"),
                running_executable_sha256: EXE,
            },
            fake_sha,
        )
        .unwrap_err();
        assert_eq!(e, DepthCatalogRefusal::NotOffered);
    }
}
