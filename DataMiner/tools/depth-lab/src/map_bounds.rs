//! 世界空间边界：从「cell 网格索引」换算到 68 号 §6.1 要的**4 个角点**
//! （[82 号 §6.1 D-1](../../../docs/82-depth-map-bounds-public-data-survey.md) 的承接结构）。
//!
//! # 为什么这段值得写成代码而不是手算
//!
//! Creation Kit 的 `World > World Spaces` 面板给出的是**四个 cell 索引**
//! （`NWCellX / NWCellY / SECellX / SECellY`，对应 ESM 里 `WRLD` 记录的 `MNAM` 子记录），
//! 不是坐标。换算成坐标要踩两个坑，而且两个都是**静默出错**：
//!
//! 1. **远侧的 +1**。cell 索引标的是格子，不是格线。东边界不是 `SECellX × 4096`
//!    而是 `(SECellX + 1) × 4096`——忘了这个 +1，单格世界会算出 0×0 的边界，
//!    大世界则一律少一格（[`far_edge_includes_the_whole_se_cell`]）。
//! 2. **北是 +Y**。Bethesda 系的 NW 角是「X 最小、**Y 最大**」，SE 角是「X 最大、Y 最小」。
//!    按「左上角 = 最小值」的直觉写就会把南北整个翻过来
//!    （[`north_is_max_y_not_min_y`]）。
//!
//! 两个坑都不会让程序崩，只会让交付出去的边界看着像模像样地错。
//!
//! # 单位
//!
//! [`CellGrid::to_bounds`] 的输出是**游戏单位**——68 号 §6.1 要的正是游戏自身坐标系下的坐标。
//! 换算到米需要 `metric_scale`（[`WorldBounds::to_metres`]），而 Fallout 4 的 `metric_scale`
//! **目前仍是待定值**：两个公开来源相差 12.5%，见
//! [`crate::calibration`] 的 `fallout4_public_scale_sources_disagree_beyond_tolerance`。
//! 所以本模块不内置任何游戏的比例常数——比例永远由调用方显式传入。

use serde::{Deserialize, Serialize};

/// Bethesda Creation Engine 的外部 cell 边长（游戏单位）。
///
/// 4096 单位见方，是 Creation Kit 文档与整个 mod 生态的一致口径 \[MED\]。
/// 它是**格子尺寸**，与「1 单位等于多少米」是两件独立的事——后者才是有争议的那个。
pub const BETHESDA_CELL_UNITS: f64 = 4096.0;

/// 世界空间的 cell 网格范围，逐字对应 CK `World > World Spaces` 面板里的四个输入框
/// （= ESM `WRLD` 记录 `MNAM` 子记录的后四个字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellGrid {
    /// 西北角所在 cell 的 X（**最小** X）。
    pub nw_x: i32,
    /// 西北角所在 cell 的 Y（**最大** Y——北是 +Y）。
    pub nw_y: i32,
    /// 东南角所在 cell 的 X（**最大** X）。
    pub se_x: i32,
    /// 东南角所在 cell 的 Y（**最小** Y）。
    pub se_y: i32,
}

/// 世界空间边界（轴对齐），单位由构造它的调用方决定。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

/// 换算失败的原因。都对应「继续算下去会得到看着合理的错边界」。
///
/// 刻意**不实现 `Copy`**：`UnparsableSectorName` 要带上出错的那个名字，
/// 没有它，「扇区列表里有一个名字不对」这句报错在几千个文件里等于没说。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapBoundsError {
    /// `se_x < nw_x`——东西反了，多半是把两个角点填反了。
    InvertedEastWest { nw_x: i32, se_x: i32 },
    /// `nw_y < se_y`——南北反了。注意**北是 +Y**，`nw_y` 应当 ≥ `se_y`。
    InvertedNorthSouth { nw_y: i32, se_y: i32 },
    /// cell 边长非正。
    NonPositiveCellSize,
    /// 扇区文件名解析不了。名字对不上只可能是拿错目录或列表被截断，两者都必须暴露。
    UnparsableSectorName(String),
    /// 一个扇区都没有。空列表算不出边界，绝不能返回一个「零尺寸地图」。
    NoSectors,
}

impl core::fmt::Display for MapBoundsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvertedEastWest { nw_x, se_x } => write!(
                f,
                "se_x={se_x} < nw_x={nw_x}：东西向反了（NW 应为 X 最小侧）"
            ),
            Self::InvertedNorthSouth { nw_y, se_y } => write!(
                f,
                "nw_y={nw_y} < se_y={se_y}：南北向反了（北是 +Y，NW 应为 Y 最大侧）"
            ),
            Self::NonPositiveCellSize => write!(f, "cell 边长必须为正"),
            Self::UnparsableSectorName(name) => {
                write!(f, "扇区文件名无法解析：{name}（期望 sec±XXXX±YYYY）")
            }
            Self::NoSectors => write!(f, "扇区列表为空，算不出边界"),
        }
    }
}

impl std::error::Error for MapBoundsError {}

impl CellGrid {
    #[must_use]
    pub fn new(nw_x: i32, nw_y: i32, se_x: i32, se_y: i32) -> Self {
        Self {
            nw_x,
            nw_y,
            se_x,
            se_y,
        }
    }

    /// 网格跨了多少个 cell（含首尾）。
    #[must_use]
    pub fn cell_extent(&self) -> (i64, i64) {
        (
            i64::from(self.se_x) - i64::from(self.nw_x) + 1,
            i64::from(self.nw_y) - i64::from(self.se_y) + 1,
        )
    }

    /// 换算成世界坐标边界（游戏单位）。
    ///
    /// **远侧含整格**：东边界取 `(se_x + 1) × cell`，北边界取 `(nw_y + 1) × cell`。
    ///
    /// # Errors
    ///
    /// 网格方向反了或 cell 边长非正时返回 [`MapBoundsError`]。
    pub fn to_bounds(&self, cell_units: f64) -> Result<WorldBounds, MapBoundsError> {
        if !cell_units.is_finite() || cell_units <= 0.0 {
            return Err(MapBoundsError::NonPositiveCellSize);
        }
        if self.se_x < self.nw_x {
            return Err(MapBoundsError::InvertedEastWest {
                nw_x: self.nw_x,
                se_x: self.se_x,
            });
        }
        if self.nw_y < self.se_y {
            return Err(MapBoundsError::InvertedNorthSouth {
                nw_y: self.nw_y,
                se_y: self.se_y,
            });
        }
        Ok(WorldBounds {
            west: f64::from(self.nw_x) * cell_units,
            east: (f64::from(self.se_x) + 1.0) * cell_units,
            south: f64::from(self.se_y) * cell_units,
            north: (f64::from(self.nw_y) + 1.0) * cell_units,
        })
    }
}

impl WorldBounds {
    /// 68 号 §6.1 要的四个角点，**按 xz 平面、逆时针从西南角起**：
    /// `[(west,south), (east,south), (east,north), (west,north)]`。
    ///
    /// 元组是 `(x, z)`——68 号 §6.1 第 2 条定的左手系里 y 是高度，
    /// 地面平面是 xz，所以「4 个坐标」落在这两轴上。
    #[must_use]
    pub fn corners_xz(&self) -> [(f64, f64); 4] {
        [
            (self.west, self.south),
            (self.east, self.south),
            (self.east, self.north),
            (self.west, self.north),
        ]
    }

    /// 东西 / 南北跨度。
    #[must_use]
    pub fn size(&self) -> (f64, f64) {
        (self.east - self.west, self.north - self.south)
    }

    /// 乘 `metric_scale` 换算到米。
    ///
    /// **比例必须显式传入**：本模块不内置任何游戏的常数，理由见模块头。
    #[must_use]
    pub fn to_metres(&self, metric_scale: f64) -> Self {
        Self {
            west: self.west * metric_scale,
            east: self.east * metric_scale,
            south: self.south * metric_scale,
            north: self.north * metric_scale,
        }
    }

    /// 某个世界坐标是否落在本边界内（闭区间）。
    ///
    /// 这是 [82 号 §5](../../../docs/82-depth-map-bounds-public-data-survey.md) 那条
    /// **落域检查**的判据：拿一段实采的 `camera_position` 逐帧问一遍，
    /// 有落在外面的就说明边界值或原点约定是错的，**值作废**，而不是把点裁进来。
    #[must_use]
    pub fn contains_xz(&self, x: f64, z: f64) -> bool {
        x >= self.west && x <= self.east && z >= self.south && z <= self.north
    }
}

/// SCS Prism3D（ETS2 / ATS）的扇区边长，单位米。
///
/// 官方地图编辑器教程原话："A single sector has a length of 4000 meters with a height of
/// 4000 meters"，原点扇区是 `sec+0000+0000` \[MED\]。
/// Prism3D 的世界单位就是米，所以这里不再乘 `metric_scale`
/// （**尤其不要乘 1:20**——那是里程压缩比，见 `ets2-player-telemetry` 的 `ETS2_METRES_PER_GAME_UNIT`）。
pub const SCS_SECTOR_METRES: f64 = 4000.0;

/// 一个 SCS 扇区的网格索引，来自扇区文件名 `sec±XXXX±YYYY`。
///
/// 两个数是**水平面**上的格子索引：第一个沿世界 X（东正），第二个沿世界 Z（**南正**）。
/// 官方教程把它们称作「X and Y axis」，那是编辑器俯视图的叫法，
/// 与 SDK 头文件里的世界系（X 东 / Y 上 / Z 南）不是一回事——
/// 把编辑器的「Y」当成世界的高度轴，会把整张图的南北读成上下。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SectorIndex {
    /// 沿世界 X（东为正）。
    pub x: i32,
    /// 沿世界 Z（**南为正**）。
    pub z: i32,
}

impl SectorIndex {
    /// 解析扇区文件名。接受 `sec+0001-0002`，也接受带扩展名的 `sec+0001-0002.base`。
    ///
    /// **严格解析**：符号必须显式（官方命名恒带 `+`/`-`），数字必须正好 4 位。
    /// 宽松解析在这里没有好处——名字对不上只可能是拿错了目录或文件列表被截断，
    /// 两种情况都应该让调用方看见，而不是悄悄少算几个扇区把地图缩小。
    #[must_use]
    pub fn parse(file_name: &str) -> Option<Self> {
        let stem = file_name.split('.').next()?;
        let rest = stem
            .strip_prefix("sec")
            .or_else(|| stem.strip_prefix("SEC"))?;
        if rest.len() != 10 {
            return None;
        }
        let (first, second) = rest.split_at(5);
        Some(Self {
            x: parse_signed_four(first)?,
            z: parse_signed_four(second)?,
        })
    }
}

fn parse_signed_four(token: &str) -> Option<i32> {
    let (sign, digits) = token.split_at(1);
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude: i32 = digits.parse().ok()?;
    match sign {
        "+" => Some(magnitude),
        "-" => Some(-magnitude),
        _ => None,
    }
}

/// 一整张 SCS 地图占用的扇区范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectorGrid {
    pub min_x: i32,
    pub max_x: i32,
    pub min_z: i32,
    pub max_z: i32,
}

impl SectorGrid {
    /// 由一批扇区文件名求包围范围。无法解析的名字**一律报错**，不跳过。
    ///
    /// # Errors
    ///
    /// 空列表或有名字解析不了时返回 [`MapBoundsError`]。
    pub fn from_sector_names<'a, I>(names: I) -> Result<Self, MapBoundsError>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut grid: Option<Self> = None;
        for name in names {
            let sector = SectorIndex::parse(name)
                .ok_or_else(|| MapBoundsError::UnparsableSectorName(name.to_owned()))?;
            grid = Some(match grid {
                None => Self {
                    min_x: sector.x,
                    max_x: sector.x,
                    min_z: sector.z,
                    max_z: sector.z,
                },
                Some(g) => Self {
                    min_x: g.min_x.min(sector.x),
                    max_x: g.max_x.max(sector.x),
                    min_z: g.min_z.min(sector.z),
                    max_z: g.max_z.max(sector.z),
                },
            });
        }
        grid.ok_or(MapBoundsError::NoSectors)
    }

    /// 跨了多少个扇区（含首尾）。
    #[must_use]
    pub fn sector_extent(&self) -> (i64, i64) {
        (
            i64::from(self.max_x) - i64::from(self.min_x) + 1,
            i64::from(self.max_z) - i64::from(self.min_z) + 1,
        )
    }

    /// 换算成**交付世界系**（68 号 §6.1：X 右/东、Z 前/北）的米制边界。
    ///
    /// 两处换算都容易错，各自对应一个测试：
    ///
    /// 1. **远侧含整格**（同 [`CellGrid::to_bounds`] 的坑 1）：
    ///    东边界是 `(max_x + 1) × 4000` 而不是 `max_x × 4000`。
    /// 2. **Z 轴要翻号**：SCS 世界的 +Z 指**南**，交付世界的 +Z 指**北**。
    ///    所以扇区索引最小的那一侧（`min_z`）是**北**边，最大的那一侧是南边，
    ///    且两者都要取负。照抄 Bethesda 那套「大的是北」会让整张图南北颠倒——
    ///    而颠倒后的边界数值范围看上去完全正常，是典型的静默错误。
    ///
    /// 输出与 `ets2-player-telemetry` 的 `player_position` 同一个坐标系，
    /// 所以可以直接用 [`WorldBounds::contains_xz`] 做落域闸。
    #[must_use]
    pub fn to_delivery_bounds_m(&self) -> WorldBounds {
        WorldBounds {
            west: f64::from(self.min_x) * SCS_SECTOR_METRES,
            east: (f64::from(self.max_x) + 1.0) * SCS_SECTOR_METRES,
            // SCS 的 z 越大越南；交付系 z 越大越北 ⇒ 取负后大小对调。
            south: -(f64::from(self.max_z) + 1.0) * SCS_SECTOR_METRES,
            north: -f64::from(self.min_z) * SCS_SECTOR_METRES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 坑 1：cell 索引标的是格子不是格线，远侧必须含整格。
    ///
    /// 单格世界 (0,0)-(0,0) 应当是 4096×4096 的一格，不是 0×0 的一个点。
    #[test]
    fn far_edge_includes_the_whole_se_cell() {
        let one = CellGrid::new(0, 0, 0, 0)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        assert_eq!(one.size(), (4096.0, 4096.0), "单格世界必须是一整格");
        assert_eq!((one.west, one.east), (0.0, 4096.0));
        assert_eq!((one.south, one.north), (0.0, 4096.0));

        // 跨 3×3 格：索引差 2，实际跨度 3 格。
        let three = CellGrid::new(-1, 1, 1, -1)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        assert_eq!(three.size(), (3.0 * 4096.0, 3.0 * 4096.0));
        assert_eq!(CellGrid::new(-1, 1, 1, -1).cell_extent(), (3, 3));
    }

    /// 坑 2：北是 +Y，NW 角是 Y 最大侧而不是最小侧。
    #[test]
    fn north_is_max_y_not_min_y() {
        let b = CellGrid::new(-10, 20, 30, -40)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        assert!(b.north > b.south, "北必须大于南");
        assert_eq!(b.north, 21.0 * 4096.0, "北边界 = (nw_y + 1) × cell");
        assert_eq!(b.south, -40.0 * 4096.0, "南边界 = se_y × cell");
        assert_eq!(b.west, -10.0 * 4096.0);
        assert_eq!(b.east, 31.0 * 4096.0);
    }

    /// 填反了必须报错，而不是算出一个负尺寸的「边界」。
    #[test]
    fn inverted_grid_is_rejected_not_silently_flipped() {
        assert_eq!(
            CellGrid::new(30, 0, -10, 0)
                .to_bounds(BETHESDA_CELL_UNITS)
                .unwrap_err(),
            MapBoundsError::InvertedEastWest {
                nw_x: 30,
                se_x: -10
            }
        );
        // 南北：nw_y 必须 ≥ se_y。把「NW 是左上角所以 y 最小」的直觉写进去就会撞这条。
        assert_eq!(
            CellGrid::new(0, -40, 0, 20)
                .to_bounds(BETHESDA_CELL_UNITS)
                .unwrap_err(),
            MapBoundsError::InvertedNorthSouth {
                nw_y: -40,
                se_y: 20
            }
        );
        assert_eq!(
            CellGrid::new(0, 0, 0, 0).to_bounds(0.0).unwrap_err(),
            MapBoundsError::NonPositiveCellSize
        );
    }

    /// 交付形态：四个角点按 xz 逆时针给出。
    #[test]
    fn corners_are_the_four_points_the_spec_asks_for() {
        let b = CellGrid::new(0, 0, 1, -1)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        // nw=(0,0) se=(1,-1) → west=0、east=(1+1)×4096、south=-1×4096、north=(0+1)×4096
        let c = b.corners_xz();
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], (0.0, -4096.0)); // 西南
        assert_eq!(c[1], (8192.0, -4096.0)); // 东南
        assert_eq!(c[2], (8192.0, 4096.0)); // 东北
        assert_eq!(c[3], (0.0, 4096.0)); // 西北
        assert_eq!(b.size(), (8192.0, 8192.0), "2×2 格");
    }

    /// 2026-08-04 在 Fallout 4 Commonwealth 的两次独立进程中实采的相机位置。
    ///
    /// 测试直接读取交付中的原始 JSONL，让每个稳定样本都实际经过 `contains_xz`；
    /// 不能只测范围端点后就在证据里声称“逐点验证”。
    #[test]
    fn fallout4_runtime_camera_samples_are_inside_commonwealth() {
        let commonwealth = CellGrid::new(-33, 25, 28, -36)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        let captures = [
            (
                include_str!(
                    "../../../docs/evidence/issue-457/fallout4-camera-candidates-static-2026-08-04.jsonl"
                ),
                0,
            ),
            (
                include_str!(
                    "../../../docs/evidence/issue-457/fallout4-camera-candidates-runtime-2026-08-04.jsonl"
                ),
                380,
            ),
        ];
        let mut checked = 0;

        for (jsonl, first_stable_seq) in captures {
            for line in jsonl.lines() {
                let record: serde_json::Value = serde_json::from_str(line).unwrap();
                if record["stage"] != "vs"
                    || record["slot"] != 9
                    || record["offset"] != 0
                    || record["present_seq"].as_u64().unwrap() < first_stable_seq
                {
                    continue;
                }
                let position = record["pose"]["position"].as_array().unwrap();
                let x = position[0].as_f64().unwrap();
                let z = position[2].as_f64().unwrap();
                assert!(
                    commonwealth.contains_xz(x, z),
                    "Fallout 4 camera sample ({x}, {z}) must remain inside Commonwealth"
                );
                checked += 1;
            }
        }

        assert_eq!(checked, 384, "every stable captured sample must be checked");
    }

    /// 比例只在换米这一步进来，且必须由调用方给。
    ///
    /// 这里用 1.0 与 0.014_28 两个值各算一遍，是为了说明**换算是线性的、比例是外部输入**——
    /// Fallout 4 真实该用哪个数目前仍未定（两个公开来源差 12.5%，见 calibration 模块）。
    #[test]
    fn metre_conversion_is_linear_in_the_supplied_scale() {
        let b = CellGrid::new(0, 0, 0, 0)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        assert_eq!(b.to_metres(1.0).size(), (4096.0, 4096.0));
        let m = b.to_metres(0.014_28);
        assert!((m.size().0 - 58.49).abs() < 0.01, "实得 {}", m.size().0);
        // 换一个比例，结果按比例走——没有藏在别处的常数。
        let m2 = b.to_metres(0.0127);
        assert!((m2.size().0 / m.size().0 - 0.0127 / 0.014_28).abs() < 1e-12);
    }

    /// 82 号 §5 的落域检查：边界值上线前必须拿实采位姿验一遍。
    #[test]
    fn contains_is_the_runtime_sanity_gate() {
        let b = CellGrid::new(-2, 2, 2, -2)
            .to_bounds(BETHESDA_CELL_UNITS)
            .unwrap();
        assert!(b.contains_xz(0.0, 0.0));
        assert!(b.contains_xz(b.west, b.north), "闭区间，边上算落域内");
        assert!(!b.contains_xz(b.east + 1.0, 0.0));
        assert!(!b.contains_xz(0.0, b.south - 1.0));
    }

    /// Issue #457：CK 与 xEdit 对 Commonwealth 的 MNAM 独立读数一致。
    /// 这里固定用生产函数换算，避免交付时手算漏掉远侧的 +1。
    #[test]
    fn fallout4_commonwealth_ck_grid_converts_to_expected_game_unit_corners() {
        let grid = CellGrid::new(-33, 25, 28, -36);
        assert_eq!(grid.cell_extent(), (62, 62));

        let bounds = grid.to_bounds(BETHESDA_CELL_UNITS).unwrap();
        assert_eq!(bounds.size(), (253_952.0, 253_952.0));
        assert_eq!(
            bounds.corners_xz(),
            [
                (-135_168.0, -147_456.0),
                (118_784.0, -147_456.0),
                (118_784.0, 106_496.0),
                (-135_168.0, 106_496.0),
            ]
        );
    }
}

#[cfg(test)]
mod scs_sector_tests {
    use super::*;

    /// 官方命名的严格解析：符号显式、4 位数字，带不带扩展名都行。
    #[test]
    fn sector_names_parse_with_explicit_signs() {
        assert_eq!(
            SectorIndex::parse("sec+0000+0000"),
            Some(SectorIndex { x: 0, z: 0 }),
            "原点扇区"
        );
        assert_eq!(
            SectorIndex::parse("sec+0001-0002.base"),
            Some(SectorIndex { x: 1, z: -2 }),
            "带扩展名"
        );
        assert_eq!(
            SectorIndex::parse("sec-0012+0034"),
            Some(SectorIndex { x: -12, z: 34 })
        );
        assert_eq!(
            SectorIndex::parse("SEC+0009+0009"),
            Some(SectorIndex { x: 9, z: 9 }),
            "大小写都收"
        );
    }

    /// 名字不规范一律拒绝，绝不「尽力而为」地少算扇区。
    /// 少算的后果是地图边界悄悄缩小，而缩小后的数字看着完全正常。
    #[test]
    fn malformed_sector_names_are_rejected() {
        for bad in [
            "sec00000000",    // 无符号
            "sec+001+0002",   // 位数不对
            "sec+0001",       // 只有一个坐标
            "road+0001+0002", // 前缀不对
            "sec+0001+000a",  // 非数字
            "",
        ] {
            assert!(SectorIndex::parse(bad).is_none(), "{bad} 不该被接受");
        }
        assert_eq!(
            SectorGrid::from_sector_names(["sec+0000+0000", "junk"]).unwrap_err(),
            MapBoundsError::UnparsableSectorName("junk".to_owned())
        );
    }

    /// 空列表不产出「零尺寸地图」。
    #[test]
    fn empty_sector_list_is_an_error_not_a_zero_map() {
        assert_eq!(
            SectorGrid::from_sector_names([]).unwrap_err(),
            MapBoundsError::NoSectors
        );
    }

    /// 坑 1（与 Bethesda 同形）：远侧含整格。
    /// 单扇区地图是 4000×4000，不是一个点。
    #[test]
    fn single_sector_map_is_one_whole_sector() {
        let grid = SectorGrid::from_sector_names(["sec+0000+0000"]).unwrap();
        let b = grid.to_delivery_bounds_m();
        assert_eq!(b.size(), (4000.0, 4000.0));
        assert_eq!(grid.sector_extent(), (1, 1));
    }

    /// 坑 2：SCS 的 +Z 指南，交付系的 +Z 指北——Z 必须翻号。
    ///
    /// 取 z 索引 0..=1（即世界 Z 的 0..8000 m，都在原点以南）：
    /// 交付系里它应当整体落在 **负 Z**（原点以南）一侧。
    /// 若漏了翻号，这段图会跑到原点以北去，而数值范围看着一样合理。
    #[test]
    fn south_positive_scs_z_becomes_north_positive_delivery_z() {
        let grid = SectorGrid::from_sector_names(["sec+0000+0000", "sec+0000+0001"]).unwrap();
        let b = grid.to_delivery_bounds_m();
        assert_eq!(b.north, 0.0, "最北边是 z 索引最小侧，且取负");
        assert_eq!(b.south, -8000.0, "最南边是 (max_z + 1) 取负");
        assert!(b.north > b.south, "north 必须仍然大于 south");
        assert!(b.south < 0.0, "全在原点以南");
    }

    /// 东西向不翻号：SCS 的 +X 就是东，与交付系一致。
    #[test]
    fn east_west_is_not_flipped() {
        let grid = SectorGrid::from_sector_names(["sec+0002+0000", "sec-0001+0000"]).unwrap();
        let b = grid.to_delivery_bounds_m();
        assert_eq!(b.west, -4000.0, "min_x = −1 ⇒ 西边界 −4000");
        assert_eq!(b.east, 12000.0, "max_x = 2 ⇒ 东边界 (2+1)×4000");
        assert_eq!(grid.sector_extent().0, 4);
    }

    /// 换算结果与 `ets2-player-telemetry` 的 `player_position` 同系，
    /// 所以落域闸可以直接用。这里模拟一段「原点扇区内行驶」的采样。
    #[test]
    fn delivery_bounds_accept_positions_from_the_same_space() {
        let grid = SectorGrid::from_sector_names(["sec+0000+0000"]).unwrap();
        let b = grid.to_delivery_bounds_m();
        // SCS 世界坐标 (1000 东, 2000 南) → 交付系 (1000, −2000)。
        assert!(b.contains_xz(1000.0, -2000.0));
        // 漏翻号会把它算成 (1000, +2000)，落在边界外——落域闸正是这样抓错的。
        assert!(!b.contains_xz(1000.0, 2000.0));
    }

    /// 4000 m 是官方值，别被改成 4096（那是 Bethesda 的 cell 边长）。
    #[test]
    fn sector_size_is_four_thousand_not_the_bethesda_cell_size() {
        assert!((SCS_SECTOR_METRES - 4000.0).abs() < f64::EPSILON);
        assert!((SCS_SECTOR_METRES - BETHESDA_CELL_UNITS).abs() > 1.0);
    }
}
