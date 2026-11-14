//! `--view-scale` 合成相机场景（docs/46 §3 / C3）：已知真值的可转动全景，
//! 供标定向导与人工多圈对表在无真实游戏的环境下端到端演练。
//!
//! 本模块只含纯逻辑与纯数学（相机状态、方位角→屏幕投影、HUD 文案），
//! 不碰任何 Win32 API——单元测试无头可跑（capture-lab 整体依赖 windows
//! crate，故仍需在 Windows 侧执行）；GDI 绘制与 Raw Input 接线在
//! `synthetic.rs`。
//!
//! 投影采用"角度线性"映射（相对方位角 ∝ 屏幕横坐标）而非透视 tan 投影：
//! 对"转整圈数格子"这一标定目标，参照柱只要方位稳定即可，线性映射更
//! 简单且无边缘拉伸；FOV 反解类 QA 不使用本场景。

use view_scale::sim::CameraModel;

/// 场景配置，由 CLI 旗标构造。
#[derive(Debug, Clone)]
pub struct ViewScaleConfig {
    /// 真值：转满 360° 需要的计数。
    pub counts_per_360: f64,
    /// 加速劣化增益（`--simulate-acceleration`），0 = 关。
    pub accel_gain: f64,
    /// EMA 平滑劣化（`--simulate-smoothing`），None = 关。
    pub smoothing_alpha: Option<f64>,
    /// 水平视场角（度），决定一屏能看到多宽的方位范围。
    pub hfov_deg: f64,
}

/// 可转动全景场景：消费真实 Raw Input 相对计数，维护相机方位角。
pub struct ViewScaleScene {
    camera: CameraModel,
    config: ViewScaleConfig,
    qpc_frequency: i64,
    last_event_qpc: Option<i64>,
    event_count: u64,
    total_counts: i64,
}

impl ViewScaleScene {
    pub fn new(config: ViewScaleConfig, qpc_frequency: i64) -> Self {
        let mut camera = CameraModel::new(config.counts_per_360);
        camera = camera.with_acceleration(config.accel_gain);
        if let Some(alpha) = config.smoothing_alpha {
            camera = camera.with_smoothing(alpha);
        }
        Self {
            camera,
            config,
            qpc_frequency,
            last_event_qpc: None,
            event_count: 0,
            total_counts: 0,
        }
    }

    /// 摄入一包真实鼠标相对计数；速率由相邻事件的 QPC 间隔推出。
    pub fn ingest_relative(&mut self, qpc: i64, dx: i32) {
        let dt_seconds = match self.last_event_qpc {
            Some(last) if qpc > last => (qpc - last) as f64 / self.qpc_frequency as f64,
            _ => 0.0,
        };
        self.last_event_qpc = Some(qpc);
        self.event_count += 1;
        self.total_counts += i64::from(dx);
        self.camera.ingest_timed(dx, dt_seconds);
    }

    pub fn yaw_deg(&self) -> f64 {
        self.camera.yaw_deg()
    }

    pub fn hfov_deg(&self) -> f64 {
        self.config.hfov_deg
    }

    /// HUD 顶行：真值与劣化开关——实验窗口必须自报身份，防止把带劣化的
    /// 场景误当干净真值用。
    pub fn hud_config_line(&self) -> String {
        let smoothing = match self.config.smoothing_alpha {
            Some(alpha) => format!("{alpha:.2}"),
            None => "off".to_string(),
        };
        format!(
            "VIEW-SCALE TRUTH counts_per_360={:.0} hfov={:.0} accel={} smoothing={}",
            self.config.counts_per_360,
            self.config.hfov_deg,
            if self.config.accel_gain > 0.0 {
                format!("{:.4}", self.config.accel_gain)
            } else {
                "off".to_string()
            },
            smoothing,
        )
    }

    /// HUD 状态行：当前方位角与累计事实（供人工核对与录屏取证）。
    pub fn hud_status_line(&self) -> String {
        format!(
            "yaw={:+9.3} deg  turns={:+.4}  events={}  counts={:+}",
            wrap_deg(self.camera.yaw_deg()),
            self.camera.yaw_deg() / 360.0,
            self.event_count,
            self.total_counts,
        )
    }
}

/// 归一化到 [-180, 180)。
pub fn wrap_deg(angle: f64) -> f64 {
    (angle % 360.0 + 540.0) % 360.0 - 180.0
}

/// 计算可见参照柱的位置：每 `spacing_deg` 一根（0° 起），返回按屏幕
/// 横坐标升序的 `(x_px, 方位角标签)`。相对方位角线性映射到 [0, width)。
pub fn pillar_screen_positions(
    yaw_deg: f64,
    hfov_deg: f64,
    width: i32,
    spacing_deg: f64,
) -> Vec<(i32, i32)> {
    debug_assert!(hfov_deg > 0.0 && spacing_deg > 0.0 && width > 0);
    let mut out = Vec::new();
    let mut azimuth = 0.0_f64;
    while azimuth < 360.0 {
        let relative = wrap_deg(azimuth - yaw_deg);
        if relative.abs() <= hfov_deg / 2.0 {
            let x = ((relative / hfov_deg + 0.5) * f64::from(width)).round() as i32;
            out.push((x.clamp(0, width - 1), azimuth as i32));
        }
        azimuth += spacing_deg;
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_deg_covers_edges() {
        assert_eq!(wrap_deg(0.0), 0.0);
        assert_eq!(wrap_deg(360.0), 0.0);
        assert_eq!(wrap_deg(-360.0), 0.0);
        assert_eq!(wrap_deg(180.0), -180.0);
        assert_eq!(wrap_deg(190.0), -170.0);
        assert_eq!(wrap_deg(-190.0), 170.0);
        assert_eq!(wrap_deg(720.0 + 45.0), 45.0);
    }

    #[test]
    fn pillars_center_and_wrap() {
        // yaw=0、hfov=90、宽 900：可见 330/0/30 三根，居中的 0° 在正中。
        let pillars = pillar_screen_positions(0.0, 90.0, 900, 30.0);
        assert_eq!(pillars, vec![(150, 330), (450, 0), (750, 30)]);

        // yaw=345：跨 0° 环绕——300/30 两根恰压左右边界（rel=±45），
        // 330 在左半屏、0 在右偏，共四根。
        let pillars = pillar_screen_positions(345.0, 90.0, 900, 30.0);
        assert_eq!(pillars, vec![(0, 300), (300, 330), (600, 0), (899, 30)]);
    }

    #[test]
    fn pillar_positions_are_sorted_and_bounded() {
        for yaw in [-720.0, -33.3, 0.0, 123.4, 359.9, 1080.0] {
            let pillars = pillar_screen_positions(yaw, 103.0, 1920, 30.0);
            assert!(!pillars.is_empty());
            let mut last_x = -1;
            for (x, azimuth) in pillars {
                assert!(x > last_x, "pillar x must be strictly increasing");
                assert!((0..1920).contains(&x));
                assert!((0..360).contains(&azimuth));
                last_x = x;
            }
        }
    }

    #[test]
    fn scene_turns_full_circle_on_exact_counts() {
        let config = ViewScaleConfig {
            counts_per_360: 18_000.0,
            accel_gain: 0.0,
            smoothing_alpha: None,
            hfov_deg: 90.0,
        };
        let mut scene = ViewScaleScene::new(config, 10_000_000);
        let mut qpc = 1_000;
        for _ in 0..1_800 {
            qpc += 10_000; // 1ms 间隔 ≈ 1000Hz
            scene.ingest_relative(qpc, 10);
        }
        assert!((scene.yaw_deg() - 360.0).abs() < 1e-9);
        assert!(scene.hud_status_line().contains("counts=+18000"));
        assert!(scene.hud_config_line().contains("accel=off"));
    }

    #[test]
    fn timed_ingest_applies_acceleration_by_rate() {
        let mut fast = CameraModel::new(18_000.0).with_acceleration(0.002);
        let mut slow = fast.clone();
        // 同样 3000 计数：快速（30 格/1ms = 30000 格/秒）应比慢速
        // （3 格/1ms = 3000 格/秒）转出更大的角度。
        for _ in 0..100 {
            fast.ingest_timed(30, 0.001);
        }
        for _ in 0..1000 {
            slow.ingest_timed(3, 0.001);
        }
        assert!(fast.yaw_deg() > slow.yaw_deg() + 1.0);
        // dt<=0 全程按增益 1：恰好一圈。
        let mut neutral = CameraModel::new(18_000.0).with_acceleration(0.002);
        neutral.ingest_timed(18_000, 0.0);
        assert!((neutral.yaw_deg() - 360.0).abs() < 1e-9);
    }
}
