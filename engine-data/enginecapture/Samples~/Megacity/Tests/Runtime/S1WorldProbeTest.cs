using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using Diogenes.Capture.Megacity;
using Diogenes.EngineCapture;
using NUnit.Framework;
using Unity.Collections;
using Unity.Entities;
using Unity.MegacityMetro.Traffic;
using Unity.Transforms;
using UnityEngine;
using UnityEngine.TestTools;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Tests
{
    /// <summary>
    /// docs/78 S1 前置勘察（迭代 A）：
    ///  1. LocalSimulation 采集世界能建起来、子场景能载入、车流/飞艇活着（勘察表落盘）；
    ///  2. 路网统计 → 给出世界偏移候选（车流走廊上的空地）；
    ///  3. ReadbackRing 的 tick↔视频帧对齐实测（tick 键控纯色帧）；
    ///  4. 城市视角 4 路渲染的单 tick 耗时实测（R'2 产能公式输入）。
    /// 报告写 /tmp/diogenes-capture-logs/probe-report.md。
    /// </summary>
    public class S1WorldProbeTest
    {
        const string LogDir = "/tmp/diogenes-capture-logs";

        [UnityTest]
        [Timeout(3600000)]
        public IEnumerator Probe_World_Traffic_Alignment_RenderCost()
        {
            Directory.CreateDirectory(LogDir);
            var report = new StringBuilder();
            report.Append("# S1 勘察报告（自动生成）\n\n");
            World world = null;
            try
            {
                // ---- 1. 采集世界 + 子场景 ----
                var swLoad = Stopwatch.StartNew();
                world = CaptureWorldBuilder.CreateWorld();
                var loads = CaptureWorldBuilder.LoadCaptureSubScenes(world, includeAudio: false);
                float timeout = Time.realtimeSinceStartup + 3000f;
                while (!CaptureWorldBuilder.AllScenesLoaded(world, loads)
                       && Time.realtimeSinceStartup < timeout)
                    yield return null;
                swLoad.Stop();
                string status = CaptureWorldBuilder.SceneLoadStatus(world, loads);
                report.Append("子场景加载: ").Append(status)
                      .Append(" 用时 ").Append(swLoad.Elapsed.TotalSeconds.ToString("F1")).Append("s\n\n");
                Debug.Log("[probe] scene load " + swLoad.Elapsed.TotalSeconds.ToString("F1") + "s: " + status);
                Assert.IsTrue(CaptureWorldBuilder.AllScenesLoaded(world, loads),
                    "subscene load timed out: " + status);

                string trafficSource = CaptureWorldBuilder.EnsureTrafficEnabled(world);
                report.Append("TrafficEnabled 来源: ").Append(trafficSource).Append("\n");

                // ---- 2. 预热 + 动态物体计数 ----
                Time.captureDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;
                for (int i = 0; i < 120; i++) yield return null;

                var src = new EcsDynObjectSource(world);
                int vehicles = src.VehicleCount;
                int blimps = src.BlimpCount;
                report.Append("预热 120 tick 后: 车辆=").Append(vehicles)
                      .Append(" 飞艇=").Append(blimps).Append("\n\n");
                Debug.Log("[probe] vehicles=" + vehicles + " blimps=" + blimps);
                Assert.Greater(vehicles, 100, "traffic did not spawn");

                // ---- 勘察表 ----
                SystemSurvey.WriteReport(world, Path.Combine(LogDir, "system-survey.md"));

                // ---- 3. 路网 + 车流密度统计 → 偏移候选 ----
                report.Append(RoadStats(world, out Vector3 roadCandidate));
                report.Append(VehicleStats(world, out Vector3 candidate, out bool hasVehicleCell));
                if (!hasVehicleCell) candidate = roadCandidate;
                Debug.Log("[probe] placement candidate = " + candidate);

                // ---- 4. 对齐实测（tick 键控纯色）----
                report.Append(AlignmentHeader());
                yield return AlignmentProbe(report);

                // ---- 5. 城市渲染成本（4 路 @候选点）----
                yield return RenderCostProbe(report, candidate, world);
            }
            finally
            {
                File.WriteAllText(Path.Combine(LogDir, "probe-report.md"), report.ToString());
                Time.captureDeltaTime = 0f;
                if (world != null && world.IsCreated)
                    CaptureWorldBuilder.DestroyWorld(world);
            }
        }

        static string AlignmentHeader() =>
            "\n## ReadbackRing tick↔帧 对齐实测\n\n";

        // -------------------------------------------------- 路网统计
        static string RoadStats(World world, out Vector3 candidate)
        {
            var sb = new StringBuilder("## 路网统计（RoadPoint+RoadOffset，烘焙自 Traffic 子场景）\n\n");
            var em = world.EntityManager;
            var cells = new Dictionary<(int, int), (int n, double sumY, double sumAbs)>();
            int totalNodes = 0;
            Vector3 min = Vector3.positiveInfinity, max = Vector3.negativeInfinity;
            using (var q = em.CreateEntityQuery(
                ComponentType.ReadOnly<RoadPoint>(), ComponentType.ReadOnly<RoadOffset>()))
            using (var ents = q.ToEntityArray(Allocator.Temp))
            {
                for (int e = 0; e < ents.Length; e++)
                {
                    var off = em.GetComponentData<RoadOffset>(ents[e]).Value;
                    var buf = em.GetBuffer<RoadPoint>(ents[e], true);
                    for (int i = 0; i < buf.Length; i++)
                    {
                        var p = buf[i].Value + off;
                        var v = new Vector3(p.x, p.y, p.z);
                        min = Vector3.Min(min, v);
                        max = Vector3.Max(max, v);
                        totalNodes++;
                        var key = (Mathf.FloorToInt(p.x / 40f), Mathf.FloorToInt(p.z / 40f));
                        cells.TryGetValue(key, out var c);
                        cells[key] = (c.n + 1, c.sumY + p.y, c.sumAbs + Math.Abs(p.x) + Math.Abs(p.z));
                    }
                }
                sb.Append("道路实体 ").Append(ents.Length).Append(" 条, 节点 ").Append(totalNodes).Append('\n');
            }
            sb.Append("节点包围盒 min=").Append(min.ToString("F0"))
              .Append(" max=").Append(max.ToString("F0")).Append("\n\n");

            // 近原点（|x|,|z|≤600, float32 量化噪声上限）且节点密的格子
            var ranked = cells
                .Where(kv => Math.Abs(kv.Key.Item1 * 40 + 20) <= 600 && Math.Abs(kv.Key.Item2 * 40 + 20) <= 600)
                .OrderByDescending(kv => kv.Value.n)
                .Take(10)
                .ToList();
            sb.Append("| cell中心x | cell中心z | 节点数 | 平均y |\n|---|---|---|---|\n");
            foreach (var kv in ranked)
                sb.Append("| ").Append(kv.Key.Item1 * 40 + 20)
                  .Append(" | ").Append(kv.Key.Item2 * 40 + 20)
                  .Append(" | ").Append(kv.Value.n)
                  .Append(" | ").Append((kv.Value.sumY / kv.Value.n).ToString("F1"))
                  .Append(" |\n");

            if (ranked.Count > 0)
            {
                var best = ranked[0];
                float y = (float)(best.Value.sumY / best.Value.n);
                candidate = new Vector3(best.Key.Item1 * 40 + 20, y - 8f, best.Key.Item2 * 40 + 20);
            }
            else
            {
                candidate = new Vector3(0, 40, 0);
            }
            sb.Append("\n候选世界偏移（走廊平均高度-8m）: ").Append(candidate.ToString("F1")).Append("\n");
            return sb.ToString();
        }

        // -------------------------------------------------- 车流密度统计
        static string VehicleStats(World world, out Vector3 candidate, out bool found)
        {
            var sb = new StringBuilder("\n## 车流密度统计（预热后 VehiclePathing×LocalToWorld）\n\n");
            var em = world.EntityManager;
            var cells = new Dictionary<(int, int), (int n, double sumY)>();
            using (var q = em.CreateEntityQuery(
                ComponentType.ReadOnly<VehiclePathing>(),
                ComponentType.ReadOnly<LocalToWorld>()))
            using (var l2w = q.ToComponentDataArray<LocalToWorld>(Allocator.Temp))
            {
                for (int i = 0; i < l2w.Length; i++)
                {
                    var p = l2w[i].Position;
                    var key = (Mathf.FloorToInt(p.x / 40f), Mathf.FloorToInt(p.z / 40f));
                    cells.TryGetValue(key, out var c);
                    cells[key] = (c.n + 1, c.sumY + p.y);
                }
                sb.Append("采样车辆 ").Append(l2w.Length).Append(" 台\n\n");
            }
            // float32 量化护栏：|x|,|z|≤220 ⇒ 全程坐标 <256，双精度回读偏差 ≤7.6e-6（<1e-5）
            var ranked = cells
                .Where(kv => Math.Abs(kv.Key.Item1 * 40 + 20) <= 220
                          && Math.Abs(kv.Key.Item2 * 40 + 20) <= 220)
                .OrderByDescending(kv => kv.Value.n)
                .Take(10)
                .ToList();
            sb.Append("| cell中心x | cell中心z | 车辆数 | 平均y |\n|---|---|---|---|\n");
            foreach (var kv in ranked)
                sb.Append("| ").Append(kv.Key.Item1 * 40 + 20)
                  .Append(" | ").Append(kv.Key.Item2 * 40 + 20)
                  .Append(" | ").Append(kv.Value.n)
                  .Append(" | ").Append((kv.Value.sumY / kv.Value.n).ToString("F1"))
                  .Append(" |\n");

            found = ranked.Count > 0;
            if (found)
            {
                var best = ranked[0];
                float y = (float)(best.Value.sumY / best.Value.n);
                candidate = new Vector3(best.Key.Item1 * 40 + 20, y - 8f, best.Key.Item2 * 40 + 20);
                sb.Append("\n候选世界偏移（车流最密近原点格, 平均高度-8m）: ")
                  .Append(candidate.ToString("F1")).Append('\n');
            }
            else
            {
                candidate = Vector3.zero;
                sb.Append("\n近原点 220m 内无车流格，退回路网候选。\n");
            }
            return sb.ToString();
        }

        // -------------------------------------------------- 对齐实测
        sealed class TickColorDriver : MonoBehaviour
        {
            public FrameClock Clock;
            public Camera[] Cams;
            public int LastTick = -1;

            void OnEnable() { Clock.OnTick += Handle; }
            void OnDisable() { Clock.OnTick -= Handle; }

            void Handle(int tick)
            {
                LastTick = tick;
                float v = ((tick % 8) * 32 + 16) / 255f;
                foreach (var c in Cams)
                    c.backgroundColor = new Color(v, v, v, 1f);
            }
        }

        IEnumerator AlignmentProbe(StringBuilder report)
        {
            const int frames = 64;
            string outDir = Path.Combine(LogDir, "align-probe");
            if (Directory.Exists(outDir)) Directory.Delete(outDir, true);
            Directory.CreateDirectory(outDir);

            var rigGo = new GameObject("AlignRig");
            var rig = rigGo.AddComponent<MultiCamRig>();
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var camGo = new GameObject("acam" + (i + 1));
                camGo.transform.SetParent(rigGo.transform, false);
                var cam = camGo.AddComponent<Camera>();
                cam.clearFlags = CameraClearFlags.SolidColor;
                cam.cullingMask = 0;
                cam.backgroundColor = Color.black;
                rig.Lanes[i] = new CameraLane
                { Camera = cam, FocalLengthMm = 24, Aspect = "16:9", ExclusiveLayer = 24 + i };
            }
            rig.Initialize();
            var clock = rigGo.AddComponent<FrameClock>();

            var driverGo = new GameObject("TickColor");
            driverGo.SetActive(false);
            var driver = driverGo.AddComponent<TickColorDriver>();
            driver.Clock = clock;
            driver.Cams = new[] { rig.Lanes[0].Camera, rig.Lanes[1].Camera, rig.Lanes[2].Camera, rig.Lanes[3].Camera };
            driverGo.SetActive(true);

            var ringGo = new GameObject("AlignRing");
            ringGo.SetActive(false);
            var ring = ringGo.AddComponent<ReadbackRing>();
            ring.Clock = clock;
            ring.Rig = rig;
            var pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            for (int i = 0; i < pipes.Length; i++)
            {
                pipes[i] = new FfmpegPipe(new FfmpegPipeConfig
                { FfmpegPath = "/opt/homebrew/bin/ffmpeg", Codec = "libx264", Preset = "veryfast", BitrateKbps = 8000 });
                var s = rig.Lanes[i].Size;
                pipes[i].Start(s.Width, s.Height, Path.Combine(outDir, "align_" + (i + 1) + ".mp4"));
            }
            ring.AttachPipes(pipes);
            ringGo.SetActive(true);

            clock.BeginClip();
            int safety = frames * 20;
            while (clock.CurrentTick < frames - 1 && safety-- > 0) yield return null;
            clock.EndClip();
            yield return null; // 让最后一帧渲染完成，endCameraRendering 发出末 tick 回读
            ring.WaitAllRequests();
            yield return null;
            for (int i = 0; i < pipes.Length; i++)
                Assert.IsTrue(pipes[i].Finish(60000), "align pipe " + i + " failed: " + pipes[i].FailReason);

            UnityEngine.Object.Destroy(ringGo);
            UnityEngine.Object.Destroy(driverGo);
            UnityEngine.Object.Destroy(rigGo);

            // ffmpeg 解回 gray 序列，测 shift
            int shift = MeasureShift(Path.Combine(outDir, "align_1.mp4"), frames, out string detail);
            report.Append(detail).Append('\n');
            Debug.Log("[probe] readback alignment shift = " + shift + " (" + detail + ")");
            Assert.AreEqual(0, shift,
                "tick↔video frame alignment broken (ReadbackRing must capture the tick's own render): " + detail);
        }

        /// <summary>返回视频帧序列相对 tick 序列的最优偏移（0=对齐, +1=视频滞后一帧）。</summary>
        static int MeasureShift(string mp4, int frames, out string detail)
        {
            string raw = mp4 + ".gray";
            var psi = new ProcessStartInfo
            {
                FileName = "/opt/homebrew/bin/ffmpeg",
                Arguments = "-hide_banner -loglevel error -y -i \"" + mp4 +
                            "\" -vf scale=16:16 -f rawvideo -pix_fmt gray \"" + raw + "\"",
                UseShellExecute = false,
                CreateNoWindow = true
            };
            using (var p = Process.Start(psi)) p.WaitForExit(60000);
            byte[] data = File.ReadAllBytes(raw);
            int frameBytes = 16 * 16;
            int n = data.Length / frameBytes;
            var mean = new double[n];
            for (int f = 0; f < n; f++)
            {
                double s = 0;
                for (int i = 0; i < frameBytes; i++) s += data[f * frameBytes + i];
                mean[f] = s / frameBytes;
            }
            double Expected(int tick) => (tick % 8) * 32 + 16;
            int bestShift = 99;
            double bestErr = double.MaxValue;
            var errs = new StringBuilder();
            for (int shift = -3; shift <= 3; shift++)
            {
                double err = 0;
                int cnt = 0;
                for (int f = 0; f < n; f++)
                {
                    int tick = f - shift;
                    if (tick < 0 || tick >= frames) continue;
                    // 视频亮度经 yuv 限幅缩放，用仿射拟合前先粗归一：直接比较序列形状（相关）
                    err += Math.Abs(mean[f] - Expected(tick) * (219.0 / 255.0) - 16.0);
                    cnt++;
                }
                err /= Math.Max(1, cnt);
                errs.Append("shift=").Append(shift).Append(":").Append(err.ToString("F1")).Append(" ");
                if (err < bestErr) { bestErr = err; bestShift = shift; }
            }
            detail = "视频帧数=" + n + "（期望 " + frames + "），最优 shift=" + bestShift +
                     "（+1=视频滞后 1 tick）; 残差表 " + errs;
            return bestShift;
        }

        // -------------------------------------------------- 城市渲染成本
        IEnumerator RenderCostProbe(StringBuilder report, Vector3 candidate, World world)
        {
            const int frames = 90;
            string outDir = Path.Combine(LogDir, "render-probe");
            if (Directory.Exists(outDir)) Directory.Delete(outDir, true);
            Directory.CreateDirectory(outDir);

            var rigGo = new GameObject("CostRig");
            var rig = rigGo.AddComponent<MultiCamRig>();
            int[] focals = { 24, 18, 35, 50 };
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var camGo = new GameObject("ccam" + (i + 1));
                camGo.transform.SetParent(rigGo.transform, false);
                camGo.transform.position = candidate + new Vector3(-10f + i * 0.4f, 1.6f, -10f);
                camGo.transform.LookAt(candidate + new Vector3(0, 1.6f, 0));
                var cam = camGo.AddComponent<Camera>();
                cam.nearClipPlane = 0.3f;
                cam.farClipPlane = 4000f;
                rig.Lanes[i] = new CameraLane
                { Camera = cam, FocalLengthMm = focals[i], Aspect = "16:9", ExclusiveLayer = 24 + i };
            }
            rig.Initialize();
            var clock = rigGo.AddComponent<FrameClock>();

            var ringGo = new GameObject("CostRing");
            ringGo.SetActive(false);
            var ring = ringGo.AddComponent<ReadbackRing>();
            ring.Clock = clock;
            ring.Rig = rig;
            var pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            for (int i = 0; i < pipes.Length; i++)
            {
                pipes[i] = new FfmpegPipe(new FfmpegPipeConfig
                { FfmpegPath = "/opt/homebrew/bin/ffmpeg", Codec = "libx264", Preset = "veryfast", BitrateKbps = 12000 });
                var s = rig.Lanes[i].Size;
                pipes[i].Start(s.Width, s.Height, Path.Combine(outDir, "city_" + (i + 1) + ".mp4"));
            }
            ring.AttachPipes(pipes);
            ringGo.SetActive(true);

            var sw = Stopwatch.StartNew();
            clock.BeginClip();
            int safety = frames * 40;
            while (clock.CurrentTick < frames - 1 && safety-- > 0) yield return null;
            clock.EndClip();
            sw.Stop();
            yield return null; // 末 tick 回读在渲染后发出
            ring.WaitAllRequests();
            yield return null;
            bool pipesOk = true;
            for (int i = 0; i < pipes.Length; i++) pipesOk &= pipes[i].Finish(60000);

            double msPerTick = sw.Elapsed.TotalMilliseconds / frames;
            report.Append("\n## 城市 4 路渲染成本（候选点 ").Append(candidate.ToString("F1")).Append("）\n\n");
            report.Append("90 tick 用时 ").Append(sw.Elapsed.TotalSeconds.ToString("F1"))
                  .Append("s → ").Append(msPerTick.ToString("F0")).Append(" ms/tick → 60s clip ≈ ")
                  .Append((msPerTick * 1800 / 60000).ToString("F1")).Append(" 分钟\n");
            report.Append("pipes ok=").Append(pipesOk).Append("\n");
            Debug.Log("[probe] render cost " + msPerTick.ToString("F0") + " ms/tick, pipesOk=" + pipesOk);

            UnityEngine.Object.Destroy(ringGo);
            UnityEngine.Object.Destroy(rigGo);
        }
    }
}
