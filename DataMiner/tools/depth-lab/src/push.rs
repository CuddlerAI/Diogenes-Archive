//! 把一次采集目录推上 R2（[85 号 §5](../../../docs/85-depth-capture-storage-and-frame-timeline-plan.md) M4）。
//!
//! # 为什么这是运营工具，不是客户端能力
//!
//! 直连桶要桶凭据。DataMiner 客户端装在采集员机器上，**不该持有桶凭据**——
//! 生产 RAW 走的是摄取 API 发的预签名 URL，正是为了不把凭据下发到终端。
//!
//! 深度采集今天是「独立安装包口味、手工发放」的实验能力（`integration` 里 G3 移除
//! 时的裁决），拿到特殊构建的人就是工程侧的人，凭据本来就在他手上。所以推送做成
//! **运营 CLI**：客户端只负责把目录采完整并写下清单，推是另一步、另一个人、另一套凭据。
//!
//! 将来深度转常规采集，这一层要换成摄取 API 发预签名 URL，届时 [`UploadPlan`]
//! 的判断可以原样复用——变的是传输，不是「哪些该传、哪些已经在了」。
//!
//! # 判断与传输分开
//!
//! 本模块的**判断部分是纯函数**（[`plan_upload`] / [`verify_complete`]），
//! 在本机就能测；网络部分只是照着计划做 PUT。断点续传、重复推送、
//! 「传完了但少了几个」这三类问题全部由判断部分负责，它们也正是最容易静默出错的部分。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 采集目录里的一个对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalObject {
    /// 相对采集根目录的键，直接用作 R2 的对象键后缀。
    pub key: String,
    pub size_bytes: u64,
}

/// 远端已有的一个对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    pub key: String,
    pub size_bytes: u64,
}

/// 一次推送的清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureManifest {
    /// 采集目录名，同时是 R2 上的前缀（如 `depth-formal-20260804-195844`）。
    pub capture_id: String,
    /// 索引行数。与实际帧文件数对不上就是**采集本身有问题**，不是推送的问题。
    pub index_rows: usize,
    pub objects: Vec<LocalObject>,
}

impl CaptureManifest {
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.objects.iter().map(|o| o.size_bytes).sum()
    }
}

/// 单个对象在计划里的去向。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectAction {
    /// 远端没有，要传。
    Upload,
    /// 远端有且大小一致，跳过。
    SkipIdentical,
    /// 远端有但大小不同——**重传并记录**。
    ///
    /// 不是静默覆盖：大小不同意味着上一次推送被打断在半路，或者本地文件变了。
    /// 两种都要在报告里看得见，否则「传完了但内容不对」永远查不出来。
    ReuploadSizeMismatch { remote_bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedObject {
    pub key: String,
    pub size_bytes: u64,
    pub action: ObjectAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPlan {
    pub items: Vec<PlannedObject>,
}

impl UploadPlan {
    #[must_use]
    pub fn to_upload(&self) -> Vec<&PlannedObject> {
        self.items
            .iter()
            .filter(|i| i.action != ObjectAction::SkipIdentical)
            .collect()
    }

    #[must_use]
    pub fn bytes_to_upload(&self) -> u64 {
        self.to_upload().iter().map(|i| i.size_bytes).sum()
    }

    #[must_use]
    pub fn skipped(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.action == ObjectAction::SkipIdentical)
            .count()
    }
}

/// 比对本地清单与远端已有对象，算出该传什么。
///
/// 续传的判据是**大小**而不是 sha256：R2 的 LIST 直接给大小，而逐对象取 ETag
/// 再比对要对每个对象多一次请求——一次采集有几万个对象，那是几万次往返。
/// 大小不同一定重传；大小相同而内容不同这种情况，由 [`verify_complete`]
/// 之外的抽样校验负责，不在续传的热路径上做。
#[must_use]
pub fn plan_upload(local: &CaptureManifest, remote: &[RemoteObject]) -> UploadPlan {
    let index: BTreeMap<&str, u64> = remote
        .iter()
        .map(|r| (r.key.as_str(), r.size_bytes))
        .collect();
    let items = local
        .objects
        .iter()
        .map(|o| {
            let action = match index.get(o.key.as_str()) {
                None => ObjectAction::Upload,
                Some(&remote_bytes) if remote_bytes == o.size_bytes => ObjectAction::SkipIdentical,
                Some(&remote_bytes) => ObjectAction::ReuploadSizeMismatch { remote_bytes },
            };
            PlannedObject {
                key: o.key.clone(),
                size_bytes: o.size_bytes,
                action,
            }
        })
        .collect();
    UploadPlan { items }
}

/// 推送完成后的核对结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub expected_objects: usize,
    pub present_objects: usize,
    /// 本地有、远端没有的键。**这份名单不能只给个数字**——
    /// 少了哪些帧决定了该重传还是该重采。
    pub missing: Vec<String>,
    /// 大小不一致的键。
    pub size_mismatch: Vec<String>,
    /// 索引行数与帧文件数是否自洽。不自洽是采集侧的问题，推送修不了。
    pub index_consistent: bool,
}

impl VerifyReport {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.size_mismatch.is_empty() && self.index_consistent
    }
}

/// 核对远端是否真的收全了。
#[must_use]
pub fn verify_complete(local: &CaptureManifest, remote: &[RemoteObject]) -> VerifyReport {
    let index: BTreeMap<&str, u64> = remote
        .iter()
        .map(|r| (r.key.as_str(), r.size_bytes))
        .collect();
    let mut missing = Vec::new();
    let mut size_mismatch = Vec::new();
    for o in &local.objects {
        match index.get(o.key.as_str()) {
            None => missing.push(o.key.clone()),
            Some(&b) if b != o.size_bytes => size_mismatch.push(o.key.clone()),
            Some(_) => {}
        }
    }
    // 帧文件 = 除索引以外的对象。索引自己不算一帧。
    let frame_objects = local
        .objects
        .iter()
        .filter(|o| !o.key.ends_with("depth-index.jsonl"))
        .count();
    VerifyReport {
        expected_objects: local.objects.len(),
        present_objects: local.objects.len() - missing.len(),
        missing,
        size_mismatch,
        index_consistent: frame_objects == local.index_rows,
    }
}

/// 扫描采集目录，生成清单。
///
/// **`.tmp` 一律跳过**：writer 是「先写临时文件再原子改名」，扫到 `.tmp`
/// 说明那一帧还在写或者写崩了，推上去只会得到半截字节。
pub fn scan_capture_dir(root: &Path) -> Result<CaptureManifest, String> {
    let capture_id = root
        .file_name()
        .ok_or_else(|| format!("{} 没有目录名", root.display()))?
        .to_string_lossy()
        .into_owned();

    let index_path = root.join("depth-index.jsonl");
    let index_text = std::fs::read_to_string(&index_path)
        .map_err(|e| format!("读 {} 失败: {e}", index_path.display()))?;
    let index_rows = index_text.lines().filter(|l| !l.trim().is_empty()).count();

    let mut objects = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("读 {} 失败: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("遍历 {} 失败: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".tmp") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|_| format!("{} 不在 {} 之下", path.display(), root.display()))?;
            objects.push(LocalObject {
                key: rel.to_string_lossy().replace('\\', "/"),
                size_bytes: entry
                    .metadata()
                    .map_err(|e| format!("stat {} 失败: {e}", path.display()))?
                    .len(),
            });
        }
    }
    // 稳定顺序：同一个目录两次扫描必须给出同一份清单，否则计划不可复现。
    objects.sort_by(|a, b| a.key.cmp(&b.key));

    Ok(CaptureManifest {
        capture_id,
        index_rows,
        objects,
    })
}

/// 本地键 → R2 对象键。前缀就是 `capture_id`，与桶里现有布局一致。
#[must_use]
pub fn remote_key(capture_id: &str, local_key: &str) -> String {
    format!("{capture_id}/{local_key}")
}

/// 采集根目录下的完整本地路径。
#[must_use]
pub fn local_path(root: &Path, key: &str) -> PathBuf {
    root.join(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(objects: &[(&str, u64)], index_rows: usize) -> CaptureManifest {
        CaptureManifest {
            capture_id: "depth-formal-20260804-195844".into(),
            index_rows,
            objects: objects
                .iter()
                .map(|(k, s)| LocalObject {
                    key: (*k).into(),
                    size_bytes: *s,
                })
                .collect(),
        }
    }

    fn remote(objects: &[(&str, u64)]) -> Vec<RemoteObject> {
        objects
            .iter()
            .map(|(k, s)| RemoteObject {
                key: (*k).into(),
                size_bytes: *s,
            })
            .collect()
    }

    #[test]
    fn a_fresh_capture_uploads_everything() {
        let m = manifest(
            &[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)],
            1,
        );
        let plan = plan_upload(&m, &[]);
        assert_eq!(plan.to_upload().len(), 2);
        assert_eq!(plan.bytes_to_upload(), 99_100);
        assert_eq!(plan.skipped(), 0);
    }

    /// 续传的全部意义：第二次跑不该把已经在的几万个对象再传一遍。
    #[test]
    fn an_interrupted_push_resumes_instead_of_restarting() {
        let m = manifest(
            &[
                ("depth-index.jsonl", 100),
                ("raw/000000000002.exr", 99_000),
                ("raw/000000000004.exr", 98_000),
            ],
            2,
        );
        let r = remote(&[("raw/000000000002.exr", 99_000)]);
        let plan = plan_upload(&m, &r);
        assert_eq!(plan.skipped(), 1);
        assert_eq!(plan.to_upload().len(), 2);
        assert_eq!(plan.bytes_to_upload(), 98_100);
    }

    /// 大小不同 = 上次被打断在半路，或本地变了。两种都必须重传**且看得见**，
    /// 静默覆盖会让「传完了但内容不对」永远查不出来。
    #[test]
    fn a_half_written_remote_object_is_reuploaded_and_reported() {
        let m = manifest(&[("raw/000000000002.exr", 99_000)], 1);
        let r = remote(&[("raw/000000000002.exr", 40_000)]);
        let plan = plan_upload(&m, &r);
        assert_eq!(
            plan.items[0].action,
            ObjectAction::ReuploadSizeMismatch {
                remote_bytes: 40_000
            }
        );
        assert_eq!(plan.to_upload().len(), 1);
    }

    #[test]
    fn verify_names_what_is_missing_not_just_how_many() {
        let m = manifest(
            &[
                ("depth-index.jsonl", 100),
                ("raw/000000000002.exr", 99_000),
                ("raw/000000000004.exr", 98_000),
            ],
            2,
        );
        let r = remote(&[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)]);
        let report = verify_complete(&m, &r);
        assert!(!report.is_complete());
        assert_eq!(report.missing, vec!["raw/000000000004.exr"]);
        assert_eq!(report.present_objects, 2);
    }

    #[test]
    fn a_complete_push_verifies_clean() {
        let m = manifest(
            &[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)],
            1,
        );
        let r = remote(&[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)]);
        assert!(verify_complete(&m, &r).is_complete());
    }

    /// 索引说有 2 帧、盘上只有 1 帧——这是**采集**出了问题，推送修不了。
    /// 必须报出来而不是安静地把不全的数据推上去。
    #[test]
    fn an_index_that_disagrees_with_the_frames_fails_verification() {
        let m = manifest(
            &[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)],
            2,
        );
        let r = remote(&[("depth-index.jsonl", 100), ("raw/000000000002.exr", 99_000)]);
        let report = verify_complete(&m, &r);
        assert!(!report.index_consistent);
        assert!(!report.is_complete());
    }

    #[test]
    fn remote_keys_are_prefixed_by_capture_id() {
        assert_eq!(
            remote_key("depth-formal-20260804-195844", "raw/000000000002.exr"),
            "depth-formal-20260804-195844/raw/000000000002.exr"
        );
    }

    #[test]
    fn scanning_skips_half_written_temp_files_and_is_stable() {
        let d = std::env::temp_dir().join(format!("dm-push-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("raw")).unwrap();
        std::fs::write(d.join("depth-index.jsonl"), "{\"a\":1}\n{\"a\":2}\n").unwrap();
        std::fs::write(d.join("raw/000000000002.exr"), vec![0u8; 10]).unwrap();
        std::fs::write(d.join("raw/000000000004.exr"), vec![0u8; 20]).unwrap();
        // 还在写的一帧
        std::fs::write(d.join("raw/000000000006.exr.tmp"), vec![0u8; 5]).unwrap();

        let m = scan_capture_dir(&d).unwrap();
        assert_eq!(m.index_rows, 2);
        let keys: Vec<&str> = m.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "depth-index.jsonl",
                "raw/000000000002.exr",
                "raw/000000000004.exr"
            ],
            "半截文件不该进清单，且顺序必须稳定"
        );
        assert_eq!(m.total_bytes(), 10 + 20 + 16);
        // 两次扫描必须给出同一份清单，否则计划不可复现
        assert_eq!(scan_capture_dir(&d).unwrap(), m);
        let _ = std::fs::remove_dir_all(&d);
    }
}
