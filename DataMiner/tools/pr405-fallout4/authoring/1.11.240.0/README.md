# Fallout4 1.11.240.0 候选配置

这是供代码审查、复现和后续正式化使用的候选，不是线上目录，也不是性能证书。
`camera-binding.json` 和 `depth-profile.json` 保留已实测的 pass3/draw0、近裁面约 15、
同帧 view/projection 和米制约定；客户端不会从这个 authoring 目录自动加载配置。

## 已验证与尚未验证

在同一 exact EXE、同一探针和原几何门槛下，三个约 30 秒的独立短段通过：

| 场景 | 改善占比 | 改善倍数中位数 | 视频重复帧 |
|---|---:|---:|---:|
| 室外收枪 | 95.83% | 3.7950 | 238 / 918 |
| 室外持枪 | 89.62% | 2.5968 | 149 / 920 |
| 室内持枪 | 81.03% | 2.0300 | 10 / 918 |

详情与原报告哈希见 `geometry-evidence.json`。失败的短段同样保留；没有删除样本、
放宽门槛或把原始 sweep 的 Ambiguous 改写为 Selected。室内实测投影 A/B 随 far plane
改变，仍使用每帧观测值，不能把 profile 中室外的 A/B 当跨场景常量。

**这些是 trial 几何通过，不是正式放行。** 仍需正式绑定审查、Player 语义验收、
录制节奏的长稳/延迟验收以及最终配置的性能验收。没有第三人称、瞄准、开火、
全地图或其他 EXE 的覆盖承诺；不新增旧版本兼容。

Player 旋转反号已由精确 EXE 原生运算与分开的 W/S 位移对照定位为 column block 被误当行。
解码修复及回归依据见 [Player 旋转证据](player-rotation-evidence.md)。本次没有修复后探针的
新实录，历史 Player 行不会被改写；位置/速度内部一致性和 Camera 的 trial E-geo 不能替代该验收。

随后仅更换含 CFR 排队取帧修复的本地 helper，探针、binding、profile 保持原始实测字节不变：
室外持枪短段的重复帧为 1/915（前次旧 helper 为 149/920），精确配对 458/458，
原门槛 trial E-geo 通过（改善占比 88.89%）。摘要将其单列为 `recording_regression`，
不冒充前三段使用同一 helper。游戏已重启且动作/时钟相位不相同，这是短段前后对照，
不是严格 A/B/A 因果实验或性能认证；部分延迟中位数未改善，仍须最终性能验收。

## 文件身份与整理差异

原始实测 CameraBinding SHA-256：
`0c1780f1171822bed1043fe16bf911878e3e85abee5e6f28991ec7e7db544b9a`。
原始实测 DepthProfile SHA-256：
`c782f6ee400f8e2acbc9d82a572e8ca6dd26b4795d51a074a8e09e734f56c927`。

本目录配置使用仓库规范的格式。CameraBinding 的 JSON 字段和值与实测文件一致；
DepthProfile 只将 `dsv.evidence_path` 从本地取证位置改成这里的脱敏证据摘要。
这个引用调整和格式整理会改变文件哈希，不得把新文件哈希写回旧实测报告或证书。
米制比例沿用有日期的引擎约定/历史交叉校验，并非此次新测量。

正式化时对最终配置重新生成身份和匹配键，按现有审核/性能流程放行；不得复制 trial
manifest 然后把 `production_binding_authorized` 改成 true。这里不携带原始视频、EXR、
设备身份、路径、证书、runtime allow-list 或可用于交付的授权。
