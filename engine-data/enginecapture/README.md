# com.diogenes.enginecapture · EngineCapture SDK（UPM）

> ## ⚠️ 未经 Unity 编译验证
> 本包在无 Unity 工具链的环境中编写（对照 Unity 2022.3 LTS API 文档），**尚未在任何
> Unity 编辑器中编译或运行过**。首次导入请按 docs/70 M0 清单联调：预期会有少量
> API 笔误级修正。各组件的实际验证状态见下方矩阵，不要把本包当作已验收产物。

服务 docs/68（SFM-Depth）与 docs/69（Multi-Camera 二期）两单的引擎侧采集。
总方案 docs/70，MC 详设 docs/71，对外确认清单 docs/72。交付 json 的键名/键序
以 `engine-data/schema` + `engine-data/fixtures` 为唯一权威（含 `camera_Follow Offset`
空格键名这类**逐字**怪癖，勿"修正"）。

## 验证状态矩阵（诚实申报）

| 部分 | 编译 | 逻辑验证 | 说明 |
|---|---|---|---|
| Runtime/Core 纯 C#（writer/换算/VK/深度量化） | ❌ 无 dotnet/mono，未编译 | ⚠️ 间接：键序+schema 覆盖+数字格式规则已用 Python 等价实现对 fixtures 全量核对通过（见 PR 记录） | 验证的是**格式规格**，不是 C# 代码本身 |
| Runtime Unity 组件（FrameClock 等 9 个） | ❌ | ❌ 仅人工对照 2022.3 API 文档 | M0 联调项 |
| Editor（PurityCheck / Validate Rig） | ❌ | ❌ | M0 联调项 |
| Tests/Editor（EditMode，NUnit） | ❌ | ❌ 未跑过 Test Runner | 导入后第一件事就是跑它 |
| package.json / asmdef 结构 | n/a | ⚠️ 按 UPM 规范人工核对 | — |

## 包结构

```
Runtime/            Diogenes.EngineCapture（asmdef，无第三方依赖）
  Core/             纯 C#（无 UnityEngine 引用，可独立编译测试）：
                    CaptureFormats（画幅表/焦距→FOV/intrinsics）、VirtualKeys、
                    JsonPrimitives + ActionCameraJsonWriter（手写 writer）、
                    SidecarJson（systeminfo/provenance）、DepthEncoding
  FrameClock        权威节拍器：实时=fixedDeltaTime 锁 1/30+帧看门狗；
                    离线=captureDeltaTime=1/30（tick=帧号）；OnTick(int)
  MultiCamRig       4 路 Base Camera + RT（16:9→1920×1080 / 3:4→1440×1080 /
                    4:3→1080×1440 / 9:16→1080×1920，逐字按 docs/71）；
                    焦距→vFOV=2·atan(12/f)（全画幅 36×24 底片）；专属 Layer 互斥；
                    初始夹角 ≤15° 断言
  PoseRecorder      逐 tick 位姿/intrinsics/速度；cam1 白字段（player_*/欧拉角）
  InputRecorder     旧 Input Manager 后端（见下）；VK 表；mouse 归一化+原始 delta
  ReadbackRing      AsyncGPUReadback FIFO + 双缓冲 + 专线程消费；WaitAllRequests()；
                    积压看门狗→abort
  FfmpegPipe        每路 ffmpeg 子进程（h264_nvenc 30fps CFR）；驱动 ≥551.76 检查
  AudioCapture      实时=OnAudioFilterRead 混音直采；离线=AudioRenderer 逐帧攒 PCM；
                    4 路共用同一混音（已裁决）
  ClipPackager      69 打包 12 文件 + provenance.json（第 13 个我方侧车）
  DepthExporter     SFM 深度骨架：720p、LinearEyeDepth→米、trunc(d*100)/100→half、
                    偶数帧；GPU pass 与单通道 Y EXR 容器为 TODO(M0)
Editor/             PurityCheck（CI 红线：相机子树 Renderer/Canvas/ParticleSystem
                    报错；unscaled*/realtimeSinceStartup 扫描告警）+
                    菜单 Diogenes/EngineCapture/Validate Rig
Tests/Editor/       EditMode 测试：焦距→FOV 表、intrinsics 公式、VK 抽验、
                    writer 与 fixtures 键序对齐 + 本包布局字节金样
```

## 集成步骤（Unity 2022.3 LTS）

1. `Packages/manifest.json` 加入本地包，并把测试装配加入 testables：
   ```json
   {
     "dependencies": {
       "com.diogenes.enginecapture": "file:../../engine-data/enginecapture"
     },
     "testables": ["com.diogenes.enginecapture"]
   }
   ```
2. **先跑测试**：Window → General → Test Runner → EditMode 全绿再继续
   （fixtures 键序测试要求包以仓库内路径引入，否则自动 Ignore）。
3. 工程设置：
   - Active Input Handling 含 "Input Manager (Old)"（或 Both）——InputRecorder
     选择了旧 Input 后端：零包依赖、可在任何工程导入；Input-System-only 工程
     会编译通过但只发一次告警并输出零样本；
   - 若 Api Compatibility Level 下 `System.Diagnostics.Process` 缺失，切到
     .NET Framework（FfmpegPipe/驱动探测需要）；
   - PATH 可寻 `ffmpeg`（NVENC 构建），NVIDIA 驱动 ≥551.76（Windows 采集机）。
4. 场景搭建：空物体挂 FrameClock + MultiCamRig + PoseRecorder + InputRecorder +
   ReadbackRing + AudioCapture（与 AudioListener 同物体）+ ClipPackager，四路相机
   连入 MultiCamRig（层默认 24–27 专属互斥），序列化引用连好。
5. 菜单 **Diogenes/EngineCapture/Validate Rig** 过一遍（纯净性红线 + 配置校验）。
6. 录制：`rig.Initialize()` → 填 `packager.Provenance` → `packager.BeginClip(root,
   "{scene}-{traj}-{seed}-{ts}")` → 到时长后 `packager.EndClip()`。abort（帧超时/
   回读积压/ffmpeg 退出）会整条丢弃重录，不修补。
7. CI：`-batchmode -quit -executeMethod Diogenes.EngineCapture.Editor.PurityCheck.RunCi`
   （退出码 0/1）。

## Megacity Metro 适配指南（bring-up 场景）

目标场景 Megacity Metro：**Unity 6 + URP + DOTS/ECS + Netcode**。要点：

- **许可红线**：Megacity Metro 按 Unity Companion License 仅限内部技术验证，
  **渲染输出不得对外交付**——bring-up 只用来打通管线与自检，不产出交付数据。
- **相机可挂**：Megacity Metro 的相机仍是 GameObject（DOTS 场景不 bake 相机），
  MultiCamRig/PoseRecorder 原样可用；URP 下保持 4 路 Base Camera（不要 overlay
  stack），RT 绑定路径一致。
- **Unity 6 API 差异**：本包 Runtime 主路径只用 2022.3 API。Unity 6 独有改动
  （如 `FindObjectsByType` 全面替代、RenderGraph 强制化）若触发弃用告警，用
  `#if UNITY_6000_0_OR_NEWER` 在工程侧适配层处理，不改包内主路径。
- **动态物体位姿导出走 EntityQuery**：DOTS 动态物体（车流/人群）没有 Transform，
  用 SystemBase 查 `LocalToWorld` 导出。示例（放 bring-up 工程，不放本包；
  writer 复用本包 JsonPrimitives）：

  ```csharp
  // bring-up 工程内的示例：每 tick 导出动态实体位姿为 JSON 行
  using System.Text;
  using Unity.Entities;
  using Unity.Transforms;
  using Diogenes.EngineCapture;

  [DisableAutoCreation] // 由采集编排器在 FrameClock.OnTick 后手动 Update
  public partial class DynamicPoseExportSystem : SystemBase
  {
      public StringBuilder Output = new StringBuilder();
      public int CurrentTick;

      protected override void OnUpdate()
      {
          var sb = Output;
          int tick = CurrentTick;
          Entities
              .WithAll<LocalToWorld>()
              .ForEach((Entity e, in LocalToWorld ltw) =>
              {
                  var p = ltw.Position;
                  sb.Append("{\"tick\": ").Append(JsonPrimitives.Number(tick))
                    .Append(", \"entity\": ").Append(JsonPrimitives.Number(e.Index))
                    .Append(", \"position\": [")
                    .Append(JsonPrimitives.Number(p.x)).Append(", ")
                    .Append(JsonPrimitives.Number(p.y)).Append(", ")
                    .Append(JsonPrimitives.Number(p.z)).Append("]}\n");
              })
              .WithoutBurst() // StringBuilder 捕获不可 Burst；量大时改 NativeList+后处理
              .Run();
      }
  }
  ```
- **Netcode 与 captureDeltaTime 相容性 = 待实测项**：Netcode for Entities 的
  tick 驱动与 `Time.captureDeltaTime` 离线时钟是否打架（预测/插值回退、
  ServerTick 漂移）没有现成结论——M0 实测；保底方案是 bring-up 用单机模式
  （关 Netcode）先打通采集面。
- **实测顺序建议**：单机 URP 场景 4 路 RT 出片 → PurityCheck 过红线 →
  离线模式 WaitAllRequests 确定性验证 → 再讨论 Netcode 共存。

## 关键口径备注

- 画幅标签→RT 尺寸映射**逐字**按 docs/71（"3:4"→1440×1080、"4:3"→1080×1440，
  标签与常见宽高比记法相反也照写，QC 以此为准）。
- 欧拉角顺规（docs/72 C3）、mouse_y 原点、mouse delta 分母（暂 = 屏宽）均为
  待需求方确认的暂定口径——ClipPackager 自动写入 provenance notes，原始值保留。
- systeminfo 分辨率如实填 RT 尺寸（竖屏 1080×1920），不伪装桌面分辨率。
- provenance.json 是我方第 13 个侧车文件，不计入 69 的 12 文件交付面。
