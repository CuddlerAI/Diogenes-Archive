//! 连续异常自动停录的**记录**：判据到线之后要留下的那份东西。
//!
//! 130 号 §6.5 定的处置是「连续 300 帧异常 → 自动停录并保留现场」，
//! [`crate::frame_health::DefectStreak`] 负责判到线，这里负责说清楚
//! **停在哪、为什么停、现场在哪**。
//!
//! # 为什么现场以异常段的第一帧为中心
//!
//! 到线时已经是第 300 帧，而第 300 帧和第 2 帧长得一模一样——整帧清除值就是
//! 整帧清除值，看一百张也不会多知道什么。有信息量的是**转折点**：最后几帧正常
//! 画面 + 头几帧坏画面。所以窗口取 `first_seq` 前后各
//! [`SCENE_HALF_WINDOW`] 帧，而不是「停录前 20 帧」。
//!
//! # 两路文案（130 号 §6.7）
//!
//! [`HealthAbort::annotator_message`] 给采集员，[`HealthAbort::diagnostic`] 给工程。
//! 分开的理由与 [`crate::integration::GateDenial`] 那次一样：采集员文案不许出现
//! 内部术语（有测试盯着），而这类失败**不给出缺陷码与帧号就查不动**。

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::frame_health::FrameDefect;

pub const HEALTH_ABORT_SCHEMA_V1: &str = "depth-health-abort/v1";

/// `Faulted` ACK 上的 `code`。客户端靠它把这次 Fault 与「探针挂了」区分开——
/// 两者的采集员处置完全不同，前者是「停下来找人」，后者是「重新绑定重试」。
///
/// 常量放在共享 crate 里：probe 写、客户端读，写成两个字面量迟早会漂。
pub const HEALTH_ABORT_FAULT_CODE: &str = "depth_frame_health_abort";

/// 现场窗口的半径：`first_seq` 前后各多少帧。
///
/// 10 帧 ≈ Lane-30 下 0.67 秒。够看清转折点，又不至于让诊断包变大到
/// 采集员发不出来——一帧 fp16 EXR 约 210 KiB，21 帧约 4 MiB。
pub const SCENE_HALF_WINDOW: usize = 10;

/// 现场目录名，落在 run 的 depth 输出目录下。
pub const SCENE_DIR: &str = "health-abort";
/// 现场里 probe 日志的快照名。
pub const SCENE_PROBE_LOG: &str = "probe.log";
/// 现场里窗口帧的索引行（逐帧 `health.raw` 统计在这里，EXR 里没有）。
pub const SCENE_INDEX_WINDOW: &str = "depth-index-window.jsonl";
/// 现场记录本身的文件名。
pub const SCENE_RECORD: &str = "health-abort.json";

/// 一次自动停录的完整记录。probe 落盘，客户端读它取采集员文案。
///
/// `defects` 是**到线那一帧**的缺陷列表。异常段中途缺陷种类可能变（例如先
/// `RawUniform` 后 `StoredQuantizationCollapse`），但停录的因果要指向一个具体
/// 判据，取当帧比取「这一段见过的所有种类」更可执行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthAbort {
    pub schema: String,
    /// 异常段的第一帧。现场窗口以它为中心，诊断文案也报它。
    pub first_present_seq: u64,
    /// 到线那一帧，即最后一帧被写盘的深度。
    pub abort_present_seq: u64,
    /// 到线时的连续段长度，正常情况下等于 [`crate::frame_health::DEFAULT_ABORT_STREAK`]。
    pub streak: u32,
    pub defects: Vec<FrameDefect>,
    /// 整个 run 的累计，用来判断这是「一直坏」还是「跑着跑着坏了」。
    pub defective_frames: u64,
    pub total_frames: u64,
    /// 现场窗口里实际写过盘的帧号，升序。
    pub scene_present_seqs: Vec<u64>,
    /// 留存现场时踩到的错误。**空表示现场是全的**；非空时诊断文案会带上，
    /// 免得工程拿到一个缺文件的包却以为那就是全部。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scene_errors: Vec<String>,
}

impl HealthAbort {
    #[must_use]
    pub fn new(
        first_present_seq: u64,
        abort_present_seq: u64,
        streak: u32,
        defects: Vec<FrameDefect>,
        defective_frames: u64,
        total_frames: u64,
        scene_present_seqs: Vec<u64>,
    ) -> Self {
        Self {
            schema: HEALTH_ABORT_SCHEMA_V1.into(),
            first_present_seq,
            abort_present_seq,
            streak,
            defects,
            defective_frames,
            total_frames,
            scene_present_seqs,
            scene_errors: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != HEALTH_ABORT_SCHEMA_V1 {
            return Err(format!("unsupported health abort schema `{}`", self.schema));
        }
        if self.streak == 0 {
            return Err("health abort streak must be positive".into());
        }
        if self.first_present_seq > self.abort_present_seq {
            return Err(format!(
                "health abort first_present_seq {} is after abort_present_seq {}",
                self.first_present_seq, self.abort_present_seq
            ));
        }
        if self.defective_frames > self.total_frames {
            return Err(format!(
                "health abort defective_frames {} exceeds total_frames {}",
                self.defective_frames, self.total_frames
            ));
        }
        if u64::from(self.streak) > self.defective_frames {
            return Err(format!(
                "health abort streak {} exceeds the run's defective_frames {}",
                self.streak, self.defective_frames
            ));
        }
        Ok(())
    }

    /// 给采集员的一句话。**不出现内部术语**，也不叫人重试。
    ///
    /// 130 号 §6.5：连续 300 帧异常是系统性问题，重录只会再废一次并且冲掉现场。
    /// 所以这条文案的落点是「停下来找人」，不是「再试一次」。
    #[must_use]
    pub fn annotator_message(&self) -> String {
        format!(
            "深度采集已停止——连续 {} 帧没有采到有效深度。\
             请截图本页并联系工程，先不要继续录制。\
             本次录制到停止为止的画面与操作已经保存。",
            self.streak
        )
    }

    /// 给工程的一段。130 号 §6.7 的四元组：症状、疑似真因、证据在哪、下一步。
    ///
    /// 疑似真因**按缺陷码给**，不写成一句放之四海皆准的话：`RawUniform` 与
    /// `StoredQuantizationCollapse` 的下一步完全不同，混成一条等于没写。
    #[must_use]
    pub fn diagnostic(&self) -> String {
        let defect = self
            .defects
            .first()
            .map_or_else(|| "none".to_string(), defect_code);
        let (cause, next) = self
            .defects
            .first()
            .map_or(UNKNOWN_DEFECT_GUIDANCE, suspected_cause_and_next);
        let scene = format!(
            "{SCENE_DIR}/{SCENE_PROBE_LOG}, {SCENE_DIR}/{SCENE_INDEX_WINDOW}, \
             raw/{:012}.exr（前后各 {SCENE_HALF_WINDOW} 帧，实得 {} 帧）",
            self.first_present_seq,
            self.scene_present_seqs.len()
        );
        let mut text = format!(
            "defect={defect} streak={} first_seq={} abort_seq={} \
             defective={}/{}\nsuspected_cause={cause}\nevidence={scene}\nnext={next}",
            self.streak,
            self.first_present_seq,
            self.abort_present_seq,
            self.defective_frames,
            self.total_frames,
        );
        if !self.scene_errors.is_empty() {
            text.push_str(&format!(
                "\nscene_incomplete={}",
                self.scene_errors.join("; ")
            ));
        }
        text
    }
}

const UNKNOWN_DEFECT_GUIDANCE: (&str, &str) = (
    "缺陷码缺失——判到线却没带缺陷列表，这本身是 bug",
    "查 depth-index.jsonl 里该帧的 health 字段，并核对 frame_health 的判据版本",
);

fn defect_code(defect: &FrameDefect) -> String {
    match defect {
        FrameDefect::RawAllNonFinite => "raw_all_non_finite",
        FrameDefect::RawUniform => "raw_uniform",
        FrameDefect::StoredAllNonFinite => "stored_all_non_finite",
        FrameDefect::StoredUniform => "stored_uniform",
        FrameDefect::StoredQuantizationCollapse { .. } => "stored_quantization_collapse",
    }
    .to_string()
}

/// 缺陷码 → (疑似真因, 下一步)。
///
/// 每一条都来自本线真踩过的排障，不是设想的失败模式（130 号 §6.1）。
fn suspected_cause_and_next(defect: &FrameDefect) -> (&'static str, &'static str) {
    match defect {
        FrameDefect::RawUniform => (
            "DSV 快照环失效，或场景切换后几何不再写这张 DSV（#518 的形状）",
            "按 95 号复核 camera binding；binding 未变则查 DSV 选择与 unbind_from_end",
        ),
        FrameDefect::RawAllNonFinite => (
            "回读到的不是深度：资源认错，或 readback 拿到了未初始化显存",
            "核对 depth-index 的 buffer_format 与 dsv_id，再查 readback 的资源生命周期",
        ),
        FrameDefect::StoredAllNonFinite | FrameDefect::StoredUniform => (
            "落盘变换把有几何的一帧打平了（94 号：变换与 Z 约定不匹配）",
            "核对 depth profile 的 projection_b 符号与索引里的 storage.transform",
        ),
        FrameDefect::StoredQuantizationCollapse { .. } => (
            "存了原始 d 而不是逆深度，深度层次在 fp16 里被抹掉（94 号）",
            "核对 depth profile 的 Z 约定；改对之后必须重采，已落盘的帧补不回来",
        ),
    }
}

/// 现场窗口：`first_seq` 前后各 [`SCENE_HALF_WINDOW`] 帧的**帧号**。
///
/// 只记帧号不记像素：那些帧此刻已经作为 `raw/*.exr` 落在盘上了，
/// 再在 writer 线程里留一份 f32 拷贝要 3.7 MiB 一帧，白白压在被注入的游戏进程上。
///
/// # 为什么不能等到线再回头数
///
/// 到线时异常段的第一帧已经过去 299 帧，那时 `before` 侧的帧号早就滚出去了。
/// 所以窗口在**异常段开始的那一刻**就冻结，之后只往 `after` 侧填。
#[derive(Debug, Clone, Default)]
pub struct SceneWindow {
    before: VecDeque<u64>,
    onset: Option<u64>,
    after: Vec<u64>,
}

impl SceneWindow {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一帧已经写盘的深度。`defective` 与
    /// [`crate::frame_health::DefectStreak::record`] 用同一个判据。
    pub fn record(&mut self, defective: bool, present_seq: u64) {
        match (defective, self.onset) {
            // 异常段起点：`before` 就此冻结，它装的正是最后那几帧正常画面。
            (true, None) => self.onset = Some(present_seq),
            (true, Some(_)) => {
                if self.after.len() < SCENE_HALF_WINDOW {
                    self.after.push(present_seq);
                }
            }
            // 恢复了：这一段没走到中止，窗口作废，重新滚动。
            // 不保留「上一段的现场」——中止的现场必须来自中止的那一段。
            (false, _) => {
                if self.onset.take().is_some() {
                    self.after.clear();
                }
                if self.before.len() == SCENE_HALF_WINDOW {
                    self.before.pop_front();
                }
                self.before.push_back(present_seq);
            }
        }
    }

    /// 窗口里的帧号，升序：`before` + 起点 + `after`。
    ///
    /// 没有正在进行的异常段时为空——那时没有「现场」可言。
    #[must_use]
    pub fn present_seqs(&self) -> Vec<u64> {
        let Some(onset) = self.onset else {
            return Vec::new();
        };
        let mut seqs: Vec<u64> = self.before.iter().copied().collect();
        seqs.push(onset);
        seqs.extend(self.after.iter().copied());
        seqs
    }
}

/// `raw/` 下的帧文件名。与 probe writer 的命名规则同源。
#[must_use]
pub fn raw_frame_name(present_seq: u64) -> String {
    format!("{present_seq:012}.exr")
}

/// 留存现场：probe 日志快照 + 窗口帧的 EXR + 窗口帧的索引行。
///
/// 返回踩到的错误，**逐条继续、不中断**——现场缺一件也比不留强，
/// 缺了什么由 [`HealthAbort::scene_errors`] 如实说出来（诊断文案会带上）。
///
/// # 为什么是拷贝而不是「指一下原文件」
///
/// `raw/` 与 probe 日志之后都还会被动：前者要进收口与打包流程，后者被下一次注入
/// 继续追加、并且根本不在 run 目录里。诊断包要能整个发出去，就必须在这一刻定格。
/// 21 帧 fp16 EXR 约 4 MiB——换的是「#518 那种失败下次能在一小时内定位」。
///
/// `probe_log` 为 `None` 表示日志路径不可解析（probe 只在 `TEMP` 存在时写日志）。
/// 那种情况**记成缺件**，不是静默跳过。
pub fn preserve_scene(
    output_dir: &Path,
    probe_log: Option<&Path>,
    abort: &HealthAbort,
) -> Vec<String> {
    let mut errors = Vec::new();
    let scene_dir = output_dir.join(SCENE_DIR);
    if let Err(error) = fs::create_dir_all(&scene_dir) {
        errors.push(format!("建现场目录 {} 失败: {error}", scene_dir.display()));
        return errors;
    }

    match probe_log {
        Some(source) => {
            if let Err(error) = fs::copy(source, scene_dir.join(SCENE_PROBE_LOG)) {
                errors.push(format!(
                    "拷贝 probe 日志 {} 失败: {error}",
                    source.display()
                ));
            }
        }
        None => errors.push("probe 日志路径不可解析，现场缺日志".into()),
    }

    let raw_dir = scene_dir.join("raw");
    if let Err(error) = fs::create_dir_all(&raw_dir) {
        errors.push(format!("建现场帧目录 {} 失败: {error}", raw_dir.display()));
    } else {
        for &present_seq in &abort.scene_present_seqs {
            let name = raw_frame_name(present_seq);
            if let Err(error) = fs::copy(output_dir.join("raw").join(&name), raw_dir.join(&name)) {
                errors.push(format!("拷贝现场帧 {name} 失败: {error}"));
            }
        }
    }

    // 逐帧 `health.raw` 统计只在索引行里，EXR 里没有——而「原始缓冲值当时长什么样」
    // 正是判 collapse 的那一路证据（#550）。窗口很小，整表扫一遍即可。
    if let Err(error) = copy_index_window(
        &output_dir.join("depth-index.jsonl"),
        &scene_dir.join(SCENE_INDEX_WINDOW),
        &abort.scene_present_seqs,
    ) {
        errors.push(error);
    }
    errors
}

/// 从 `depth-index.jsonl` 里挑出窗口那几帧的行，顺序保持原样（即写盘顺序）。
///
/// 解析不了的行直接跳过：这是排障用的旁支，不该因为索引尾部半行未刷新
/// 就把整份现场丢掉。
fn copy_index_window(
    index_path: &Path,
    destination: &Path,
    present_seqs: &[u64],
) -> Result<(), String> {
    let wanted: BTreeSet<u64> = present_seqs.iter().copied().collect();
    let text = fs::read_to_string(index_path)
        .map_err(|error| format!("读 {} 失败: {error}", index_path.display()))?;
    let mut kept = String::new();
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row["present_seq"]
            .as_u64()
            .is_some_and(|seq| wanted.contains(&seq))
        {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    write_sync(destination, kept.as_bytes())
}

/// 写现场记录本身。**必须最后写**：`scene_errors` 要先填完，
/// 否则记录会声称现场是全的，而工程拿到的是个缺件的包。
///
/// 自己建目录，不依赖 [`preserve_scene`] 先跑过：现场哪怕一件都没拷成，
/// 这份「为什么停」也必须留下来。
pub fn write_record(output_dir: &Path, abort: &HealthAbort) -> Result<(), String> {
    abort.validate()?;
    let bytes = serde_json::to_vec_pretty(abort)
        .map_err(|error| format!("序列化自动停录记录失败: {error}"))?;
    let scene_dir = output_dir.join(SCENE_DIR);
    fs::create_dir_all(&scene_dir)
        .map_err(|error| format!("建 {} 失败: {error}", scene_dir.display()))?;
    write_sync(&scene_dir.join(SCENE_RECORD), &bytes)
}

fn write_sync(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file =
        fs::File::create(path).map_err(|error| format!("建 {} 失败: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("写 {} 失败: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abort_with(defects: Vec<FrameDefect>) -> HealthAbort {
        HealthAbort::new(9_625, 10_223, 300, defects, 300, 9_800, vec![9_625])
    }

    #[test]
    fn the_annotator_text_stays_free_of_internal_jargon() {
        let all = [
            vec![FrameDefect::RawUniform],
            vec![FrameDefect::RawAllNonFinite],
            vec![FrameDefect::StoredUniform],
            vec![FrameDefect::StoredAllNonFinite],
            vec![FrameDefect::StoredQuantizationCollapse {
                raw_unique_sampled: 7_258,
                stored_unique_sampled: 185,
            }],
            Vec::new(),
        ];
        for defects in all {
            let message = abort_with(defects).annotator_message();
            assert!(!message.is_empty());
            for jargon in [
                "DSV", "probe", "fp16", "binding", "streak", "seq", "profile", "Z 约定",
            ] {
                assert!(
                    !message.contains(jargon),
                    "给采集员的文案不该出现内部术语 `{jargon}`：{message}"
                );
            }
        }
    }

    /// 采集员文案必须**劝停**，不能劝重试——重启只会再废一次并冲掉现场。
    #[test]
    fn the_annotator_text_does_not_invite_another_burnt_recording() {
        let message = abort_with(vec![FrameDefect::RawUniform]).annotator_message();
        assert!(message.contains("联系工程"), "{message}");
        assert!(message.contains("不要继续录制"), "{message}");
        for retry in ["重试", "重新开始", "再试"] {
            assert!(
                !message.contains(retry),
                "不该劝采集员重来：{message}（命中 `{retry}`）"
            );
        }
    }

    /// 诊断这一路必须给出 130 号 §6.7 的四元组，缺一条就查不动。
    #[test]
    fn the_diagnostic_carries_symptom_cause_evidence_and_next_step() {
        let diagnostic = abort_with(vec![FrameDefect::RawUniform]).diagnostic();
        assert!(diagnostic.contains("defect=raw_uniform"), "{diagnostic}");
        assert!(diagnostic.contains("streak=300"), "{diagnostic}");
        assert!(diagnostic.contains("first_seq=9625"), "{diagnostic}");
        assert!(diagnostic.contains("abort_seq=10223"), "{diagnostic}");
        assert!(diagnostic.contains("suspected_cause="), "{diagnostic}");
        // 证据要指到具体文件，不是「看日志」
        assert!(
            diagnostic.contains("health-abort/probe.log"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("raw/000000009625.exr"), "{diagnostic}");
        assert!(diagnostic.contains("next="), "{diagnostic}");
    }

    /// 两种缺陷的下一步完全不同——写成同一句等于没写。
    #[test]
    fn different_defects_get_different_next_steps() {
        let uniform = abort_with(vec![FrameDefect::RawUniform]).diagnostic();
        let collapse = abort_with(vec![FrameDefect::StoredQuantizationCollapse {
            raw_unique_sampled: 7_258,
            stored_unique_sampled: 185,
        }])
        .diagnostic();
        assert!(uniform.contains("DSV"), "{uniform}");
        assert!(collapse.contains("94 号"), "{collapse}");
        assert_ne!(uniform, collapse);
    }

    /// 现场缺件必须在诊断里说出来，不能让工程拿着半个包当全部。
    #[test]
    fn an_incomplete_scene_says_so() {
        let mut abort = abort_with(vec![FrameDefect::RawUniform]);
        assert!(!abort.diagnostic().contains("scene_incomplete"));
        abort
            .scene_errors
            .push("copy probe log failed: 拒绝访问".into());
        assert!(
            abort
                .diagnostic()
                .contains("scene_incomplete=copy probe log failed"),
            "{}",
            abort.diagnostic()
        );
    }

    #[test]
    fn the_record_round_trips_and_rejects_a_contradictory_one() {
        let abort = abort_with(vec![FrameDefect::RawUniform]);
        abort.validate().unwrap();
        let text = serde_json::to_string(&abort).unwrap();
        let back: HealthAbort = serde_json::from_str(&text).unwrap();
        assert_eq!(back, abort);

        // 段长比整个 run 的异常帧数还多 ⇒ 自相矛盾
        let mut bad = abort.clone();
        bad.defective_frames = 12;
        assert!(bad.validate().is_err());
        // 起点在终点之后
        let mut backwards = abort.clone();
        backwards.first_present_seq = backwards.abort_present_seq + 1;
        assert!(backwards.validate().is_err());
    }

    /// 窗口以异常段第一帧为中心，`before` 侧装的是最后那几帧**正常**画面。
    #[test]
    fn the_window_is_centred_on_the_onset_not_on_the_abort() {
        let mut window = SceneWindow::new();
        // 30 fps 采 15 Hz：序号步长 2，窗口必须按实际写盘的号来，不能算术推
        for i in 0..40u64 {
            window.record(false, 9_000 + i * 2);
        }
        assert!(window.present_seqs().is_empty(), "没有异常段就没有现场");
        window.record(true, 9_080);
        for i in 1..300u64 {
            window.record(true, 9_080 + i * 2);
        }
        let seqs = window.present_seqs();
        assert_eq!(seqs.len(), SCENE_HALF_WINDOW * 2 + 1);
        assert_eq!(seqs[SCENE_HALF_WINDOW], 9_080, "中心是异常段的第一帧");
        // 前 10 帧是正常画面，后 10 帧是坏画面的开头——都不是「停录前 20 帧」
        assert_eq!(seqs.first(), Some(&9_060));
        assert_eq!(seqs.last(), Some(&9_100));
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    }

    /// 恢复过的那一段不是中止的那一段，现场不能串。
    #[test]
    fn a_recovered_streak_does_not_leave_its_scene_behind() {
        let mut window = SceneWindow::new();
        for seq in 0..20u64 {
            window.record(false, seq);
        }
        for seq in 20..25u64 {
            window.record(true, seq);
        }
        window.record(false, 25);
        assert!(window.present_seqs().is_empty(), "已恢复的段不该留下现场");
        window.record(true, 26);
        let seqs = window.present_seqs();
        assert_eq!(seqs.last(), Some(&26));
        assert_eq!(
            seqs[seqs.len() - 1],
            26,
            "新段的中心是新段自己的第一帧：{seqs:?}"
        );
        // 恢复的那一帧（25）算正常画面，应当进 before 侧
        assert!(seqs.contains(&25), "{seqs:?}");
        // 上一段的坏帧不该出现在 after 侧
        assert!(!seqs.iter().any(|seq| (20..25).contains(seq)), "{seqs:?}");
    }

    /// 开局就坏：`before` 侧本来就没有帧，窗口该短就短，不许编。
    #[test]
    fn a_run_that_is_broken_from_the_first_frame_still_yields_a_scene() {
        let mut window = SceneWindow::new();
        window.record(true, 7);
        window.record(true, 9);
        assert_eq!(window.present_seqs(), vec![7, 9]);
    }

    // --- 留存现场 ---

    /// 造一个像真 run 一样的输出目录：`raw/*.exr` + `depth-index.jsonl`。
    fn fake_run(tag: &str, seqs: &[u64]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dm-health-abort-{tag}-{}-{:p}",
            std::process::id(),
            &tag
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("raw")).unwrap();
        let mut index = String::new();
        for &seq in seqs {
            fs::write(
                root.join("raw").join(raw_frame_name(seq)),
                format!("exr-bytes-{seq}"),
            )
            .unwrap();
            index.push_str(&format!(
                "{{\"present_seq\":{seq},\"health\":{{\"raw\":{{\"uniform\":true}}}}}}\n"
            ));
        }
        fs::write(root.join("depth-index.jsonl"), index).unwrap();
        root
    }

    /// 现场必须是**自包含**的：日志、窗口帧、窗口索引行都在一个目录里，
    /// 采集员整个打包发出来就够工程用。
    #[test]
    fn the_preserved_scene_is_self_contained() {
        let seqs: Vec<u64> = (0..60).map(|i| 9_000 + i * 2).collect();
        let root = fake_run("selfcontained", &seqs);
        let log = root.join("outside-the-run.log");
        fs::write(&log, "line one\nline two\n").unwrap();

        let window: Vec<u64> = seqs[20..41].to_vec();
        let mut abort = HealthAbort::new(
            window[SCENE_HALF_WINDOW],
            *window.last().unwrap(),
            300,
            vec![FrameDefect::RawUniform],
            300,
            600,
            window.clone(),
        );
        abort.scene_errors = preserve_scene(&root, Some(&log), &abort);
        assert!(abort.scene_errors.is_empty(), "{:?}", abort.scene_errors);
        write_record(&root, &abort).unwrap();

        let scene = root.join(SCENE_DIR);
        assert_eq!(
            fs::read_to_string(scene.join(SCENE_PROBE_LOG)).unwrap(),
            "line one\nline two\n"
        );
        for &seq in &window {
            let name = raw_frame_name(seq);
            assert_eq!(
                fs::read_to_string(scene.join("raw").join(&name)).unwrap(),
                format!("exr-bytes-{seq}"),
                "现场缺帧 {name}"
            );
        }
        // 窗口外的帧不该被拷进来——诊断包不是整个 run 的副本
        assert!(!scene.join("raw").join(raw_frame_name(seqs[0])).exists());

        let index = fs::read_to_string(scene.join(SCENE_INDEX_WINDOW)).unwrap();
        assert_eq!(index.lines().count(), window.len());
        assert!(index.contains(&format!("\"present_seq\":{}", window[0])));
        assert!(!index.contains(&format!("\"present_seq\":{}", seqs[0])));

        let back: HealthAbort =
            serde_json::from_str(&fs::read_to_string(scene.join(SCENE_RECORD)).unwrap()).unwrap();
        assert_eq!(back, abort);
        back.validate().unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    /// 现场缺件不许静默：拷不动的每一件都要落进 `scene_errors`，
    /// 否则工程拿到半个包却以为那就是全部（130 号 §6.4 的教训）。
    #[test]
    fn a_missing_piece_of_the_scene_is_reported_rather_than_swallowed() {
        let root = fake_run("missing", &[10, 12]);
        // 窗口里有一帧盘上根本没有（例如它落盘失败），日志路径也解析不出来
        let abort = HealthAbort::new(
            10,
            14,
            300,
            vec![FrameDefect::RawUniform],
            300,
            600,
            vec![10, 12, 14],
        );
        let errors = preserve_scene(&root, None, &abort);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("probe 日志")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains(&raw_frame_name(14))),
            "{errors:?}"
        );
        // 拿得到的那两帧仍然要留下来——一件失败不该带走整份现场
        for seq in [10u64, 12] {
            assert!(
                root.join(SCENE_DIR)
                    .join("raw")
                    .join(raw_frame_name(seq))
                    .exists(),
                "seq {seq} 本来拷得动却没留下"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// 索引尾部半行（进程被拆时很常见）不该把整份现场带走。
    #[test]
    fn a_half_written_index_line_does_not_lose_the_whole_scene() {
        let root = fake_run("halfline", &[10, 12]);
        let mut index = fs::read_to_string(root.join("depth-index.jsonl")).unwrap();
        index.push_str("{\"present_seq\":14,\"heal");
        fs::write(root.join("depth-index.jsonl"), index).unwrap();
        let abort = HealthAbort::new(
            10,
            12,
            300,
            vec![FrameDefect::RawUniform],
            300,
            600,
            vec![10, 12],
        );
        assert!(
            preserve_scene(&root, None, &abort)
                .iter()
                .all(|error| error.contains("probe 日志"))
        );
        let window = fs::read_to_string(root.join(SCENE_DIR).join(SCENE_INDEX_WINDOW)).unwrap();
        assert_eq!(window.lines().count(), 2, "{window}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 把 writer 线程那几行**原样重演一遍**：同一个 `defective` 同时喂
    /// [`crate::frame_health::DefectStreak`] 与 [`SceneWindow`]，到线时用两者
    /// 拼出记录。
    ///
    /// # 为什么值得单独钉
    ///
    /// 真正的组装点在 `probe.rs` 的 writer 线程里，而那是 Windows-only、
    /// macOS 上连交叉编译测试都跑不了的地方。两个状态机各自的测试都绿、
    /// 拼起来却对不上（起点取错、streak 与累计数矛盾导致 `validate` 拒收），
    /// 是这种分工天然的盲区。这条测试就是把那几行搬到能跑的地方跑一遍。
    #[test]
    fn the_writer_side_composition_yields_a_record_that_validates() {
        use crate::frame_health::{DefectStreak, StreakVerdict};

        let mut streak = DefectStreak::default();
        let mut window = SceneWindow::new();
        let mut record = None;
        // #518 的形状：先正常跑一阵，再从某一帧起连续空白直到到线。
        // 序号步长 2（30 fps 采 15 Hz），刻意不连续。
        for index in 0..2_000u64 {
            let present_seq = 9_000 + index * 2;
            let defective = index >= 312;
            window.record(defective, present_seq);
            if let StreakVerdict::Abort(streak_len) = streak.record(defective, present_seq) {
                let (defective_frames, total) = streak.totals();
                record = Some(HealthAbort::new(
                    streak.first_defective_seq().expect("异常段正在进行"),
                    present_seq,
                    streak_len,
                    vec![FrameDefect::RawUniform],
                    defective_frames,
                    total,
                    window.present_seqs(),
                ));
                break;
            }
        }

        let record = record.expect("连续 300 帧必须到线");
        record.validate().expect("writer 拼出来的记录必须自洽");
        assert_eq!(record.streak, 300);
        // 起点是第 312 帧（seq 9624），到线是它之后第 299 帧（seq 10222）
        assert_eq!(record.first_present_seq, 9_624);
        assert_eq!(record.abort_present_seq, 10_222);
        assert_eq!(record.defective_frames, 300);
        assert_eq!(record.total_frames, 612);
        // 现场围着起点，不是围着停录点
        assert_eq!(record.scene_present_seqs.len(), SCENE_HALF_WINDOW * 2 + 1);
        assert_eq!(record.scene_present_seqs[SCENE_HALF_WINDOW], 9_624);
        assert_eq!(record.scene_present_seqs.first(), Some(&9_604));
        assert_eq!(record.scene_present_seqs.last(), Some(&9_644));
        // 诊断要能直接指到盘上那个文件名
        assert!(
            record.diagnostic().contains("raw/000000009624.exr"),
            "{}",
            record.diagnostic()
        );
    }

    /// 记录自身不自洽时必须拒写：一份说不通的现场记录比没有更糟。
    #[test]
    fn an_inconsistent_record_is_refused_before_it_reaches_the_scene() {
        let root = fake_run("inconsistent", &[10]);
        let mut abort = HealthAbort::new(
            10,
            12,
            300,
            vec![FrameDefect::RawUniform],
            300,
            600,
            vec![10],
        );
        abort.streak = 0;
        assert!(write_record(&root, &abort).is_err());
        assert!(!root.join(SCENE_DIR).join(SCENE_RECORD).exists());
        let _ = fs::remove_dir_all(&root);
    }
}
