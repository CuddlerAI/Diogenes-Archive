using System;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using Diogenes.Capture.Megacity;
using Diogenes.EngineCapture;
using NUnit.Framework;
using Unity.Entities;
using UnityEngine;
using UnityEngine.TestTools;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Tests
{
    /// <summary>
    /// docs/78 S1/S2 采集入口（S3 编排器逐条驱动同一入口）：
    /// LocalSimulation 世界 + 子场景 + 车流 → McClipCapture.Run 一条 MC clip。
    /// 时长由计划的 `duration_s`×`fps` 决定（60s 交付档与 300s 能力档同一入口）。
    /// 参数全部走环境变量（编排器传参通道）：
    ///   DIOGENES_PLAN_DIR    trajgen 计划目录（默认 seed1000 样例）
    ///   DIOGENES_OUT_DIR     clip 输出根目录
    ///   DIOGENES_CLIP_NAME   clip 目录名（缺省 megacity-mc-{seed}-{ts}）
    ///   DIOGENES_WORLD_OFFSET "x,y,z" 世界偏移（缺省用勘察表推荐值）
    ///   DIOGENES_RENDER / DIOGENES_AUDIO / DIOGENES_DYN  "0" 关闭对应链路
    ///   DIOGENES_WARMUP      预热 tick 数（默认 120）
    /// </summary>
    public class McClipCaptureTest
    {
        const string DefaultPlanDir = "/Users/example/Documents/diogenes-capture-runs/plans/seed1000";
        const string DefaultOutDir = "/Users/example/Documents/diogenes-capture-runs/clips";
        // 勘察表定稿值（probe-report.md：近原点车流最密格；y=走廊平均高度-3，让 50mm
        // 窄镜头也能把车流收进视锥——S1 实测 -8m 时 active_frac 随焦距单调塌陷；
        // |x|,|z|≤220 是 float32 量化护栏——全程坐标 <256 ⇒ 回读偏差 ≤7.6e-6）
        const string DefaultOffset = "60,138,220";

        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        /// <summary>
        /// 期望 lane-frame 数 = 计划声明的帧数 × 路数，口径与 engine_qc 的
        /// `--expect-duration-s` 同源（帧数 = round(duration_s × fps)）。
        /// 不能写死 1800（60s×30fps）：能力档 300s 的 clip 是 9000 帧，写死会让
        /// 采集全部成功的 run 仍判 Failed——0804 clip-011 即此例，断言报
        /// 「Expected 7200 / But was 36000」，而同一 run 的 McClipCapture 自报
        /// `ok=True verifiedLaneFrames=36000`，产物 12 文件齐全、4 路各 300.00s。
        /// </summary>
        static long ExpectedLaneFrames(string planDir)
        {
            var plan = MiniJson.ParseFile(Path.Combine(planDir, "plan.json"))
                as Dictionary<string, object>;
            Assert.IsNotNull(plan, "plan.json unreadable: " + planDir);
            double duration = plan.TryGetValue("duration_s", out var dv) && dv is double d ? d : 0.0;
            double fps = plan.TryGetValue("fps", out var fv) && fv is double f ? f : 0.0;
            Assert.Greater(duration, 0.0, "plan.json missing/invalid duration_s");
            Assert.Greater(fps, 0.0, "plan.json missing/invalid fps");
            return (long)Math.Round(duration * fps) * MultiCamRig.LaneCount;
        }

        [UnityTest]
        [Timeout(7200000)]
        public IEnumerator Mc_CaptureClip_FromPlan()
        {
            string planDir = Env("DIOGENES_PLAN_DIR", DefaultPlanDir);
            string outDir = Env("DIOGENES_OUT_DIR", DefaultOutDir);
            string clipName = Env("DIOGENES_CLIP_NAME", "");
            string[] off = Env("DIOGENES_WORLD_OFFSET", DefaultOffset).Split(',');
            var worldOffset = new Vector3(
                float.Parse(off[0]), float.Parse(off[1]), float.Parse(off[2]));
            bool render = Env("DIOGENES_RENDER", "1") != "0";
            bool audioOn = Env("DIOGENES_AUDIO", "1") != "0";
            bool dynOn = Env("DIOGENES_DYN", "1") != "0";
            int warmup = int.Parse(Env("DIOGENES_WARMUP", "120"));
            float trafficMin = float.Parse(Env("DIOGENES_TRAFFIC_MIN", "15"));
            float trafficMax = float.Parse(Env("DIOGENES_TRAFFIC_MAX", "60"));

            Assert.IsTrue(File.Exists(Path.Combine(planDir, "plan.json")),
                "plan dir invalid: " + planDir);
            Directory.CreateDirectory(outDir);

            World world = null;
            var result = new McCaptureResult();
            try
            {
                world = CaptureWorldBuilder.CreateWorld();
                CaptureWorldBuilder.HoldTrafficSetup(world); // 先挂起铺车，等改完车距再放行
                var loads = CaptureWorldBuilder.LoadCaptureSubScenes(world, includeAudio: audioOn);
                float deadline = Time.realtimeSinceStartup + 3000f;
                while (!CaptureWorldBuilder.AllScenesLoaded(world, loads)
                       && Time.realtimeSinceStartup < deadline)
                    yield return null;
                Assert.IsTrue(CaptureWorldBuilder.AllScenesLoaded(world, loads),
                    "subscene load timeout: " + CaptureWorldBuilder.SceneLoadStatus(world, loads));

                string trafficSource = CaptureWorldBuilder.EnsureTrafficEnabled(world);
                string density = CaptureWorldBuilder.ReleaseTrafficSetup(world, trafficMin, trafficMax);
                Debug.Log("[mc-capture] TrafficEnabled=" + trafficSource + " density=" + density);

                string yawEnv = Env("DIOGENES_YAW", "auto");
                bool snapY = Env("DIOGENES_SNAP_Y", "1") != "0";
                var road = CaptureWorldBuilder.NearestRoadSegment(world, worldOffset);
                if (snapY && road.HasValue)
                    // 机位面吸附到走廊高度：相机 ≈ 走廊 -3m（车流稳在各焦距视锥中带）
                    worldOffset.y = road.Value.Midpoint.y - 4.5f;
                var options = new McCaptureOptions
                {
                    PlanDir = planDir,
                    OutDir = outDir,
                    ClipName = clipName,
                    WorldOffset = worldOffset,
                    WorldYawDeg = yawEnv == "auto" ? float.NaN : float.Parse(yawEnv),
                    AutoYawAzimuthDeg = yawEnv == "auto" && road.HasValue
                        ? road.Value.AzimuthDeg
                        : (float?)null,
                    RenderVideo = render,
                    CaptureAudio = audioOn,
                    // 编码档三件套走环境变量，与其余参数同一通道。不传则用
                    // McCaptureOptions 的平台默认（Windows=h264_nvenc / 其余=libx264）。
                    FfmpegPath = Env("DIOGENES_FFMPEG", new McCaptureOptions().FfmpegPath),
                    Codec = Env("DIOGENES_CODEC", new McCaptureOptions().Codec),
                    Preset = Env("DIOGENES_PRESET", new McCaptureOptions().Preset),
                    BitrateKbps = int.Parse(Env("DIOGENES_BITRATE",
                        new McCaptureOptions().BitrateKbps.ToString())),
                    DynSource = dynOn ? new EcsDynObjectSource(world) : null,
                    WarmupTicks = warmup
                };
                Debug.Log("[mc-capture] yaw=" + yawEnv + " road=" +
                          (road.HasValue ? road.Value.AzimuthDeg + "deg@" + road.Value.Midpoint : "none") +
                          " offset=" + worldOffset);
                yield return McClipCapture.Run(options, result);

                Debug.Log("[mc-capture] ok=" + result.Ok + " err='" + result.Error +
                          "' clip=" + result.ClipDir +
                          " verifiedLaneFrames=" + result.LaneFramesVerified +
                          " avgTickMs=" + result.AvgTickMs.ToString("F0") +
                          " audioCaptured=" + result.AudioCaptured +
                          " audioMuxed=" + result.AudioMuxed +
                          " dynPass=" + result.DynMetricsPass);

                Assert.IsTrue(result.Ok, "capture failed: " + result.Error);
                Assert.AreEqual(ExpectedLaneFrames(planDir), result.LaneFramesVerified,
                    "construction self-check did not cover every lane-frame");
            }
            finally
            {
                if (world != null && world.IsCreated)
                    CaptureWorldBuilder.DestroyWorld(world);
                Time.captureDeltaTime = 0f;
            }
        }
    }
}
