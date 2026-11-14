using System;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using Diogenes.Capture.Megacity;
using Diogenes.EngineCapture;
using NUnit.Framework;
using Unity.Entities;
using Unity.Mathematics;
using UnityEngine;
using UnityEngine.TestTools;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Tests
{
    /// <summary>
    /// docs/78 §4.1 SceneProbeExporter 的 PlayMode 兜底通道：与 Editor -executeMethod
    /// 入口（Diogenes.Bringup.SceneProbeExporter.Run）共用同一 SceneProbe.ExportAll 核心，
    /// 差别只在世界泵浦方式（play mode 由 player loop 驱动，时间真实前进）。
    /// -executeMethod 手动泵浦路径若在某 editor 版本失效，用本测试导出：
    ///   -runTests -testPlatform PlayMode -testFilter SceneProbeExportTest
    /// 环境变量与 Editor 入口一致（DIOGENES_SCENE_OUT / DIOGENES_TRAFFIC_MIN/MAX /
    /// DIOGENES_WARMUP）。
    /// </summary>
    public class SceneProbeExportTest
    {
        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        static void WithEnv(string key, string value, TestDelegate action)
        {
            string previous = Environment.GetEnvironmentVariable(key);
            try
            {
                Environment.SetEnvironmentVariable(key, value);
                action();
            }
            finally
            {
                Environment.SetEnvironmentVariable(key, previous);
            }
        }

        [Test]
        public void CrossHalfM_Uses_Valid_Invariant_Override()
        {
            WithEnv("DIOGENES_CROSS_HALF_M", "160", () =>
                Assert.AreEqual(160f, SceneProbe.CrossHalfM));
        }

        [TestCase(null)]
        [TestCase("")]
        [TestCase("0")]
        [TestCase("251")]
        [TestCase("not-a-number")]
        public void CrossHalfM_Missing_Or_Invalid_Override_Falls_Back_To_Default(string value)
        {
            WithEnv("DIOGENES_CROSS_HALF_M", value, () =>
                Assert.AreEqual(80f, SceneProbe.CrossHalfM));
        }

        [Test]
        public void WriteSceneJson_Records_Effective_Cross_Half_Width()
        {
            string outDir = Path.Combine(Path.GetTempPath(),
                "diogenes-scene-probe-" + Guid.NewGuid().ToString("N"));
            Directory.CreateDirectory(outDir);
            try
            {
                WithEnv("DIOGENES_CROSS_HALF_M", "160", () =>
                {
                    var res = new SceneProbeResult
                    {
                        ChosenIndex = 0,
                        Dims = new int3(640, 92, 360),
                        Volume = new VolumeSpec
                        {
                            LoW = new float3(-160f, 0f, -90f),
                            HiW = new float3(160f, 46f, 90f),
                            Origin = float3.zero,
                            Axis = 'z',
                            CorridorYLocal = 4.5f,
                        },
                    };
                    res.Corridors.Add(new CorridorCandidate
                    {
                        RoadIndex = 8,
                        Start = new float3(0f, 4.5f, -90f),
                        End = new float3(0f, 4.5f, 90f),
                        Length = 180f,
                        MeanY = 4.5f,
                        Axis = 'z',
                    });

                    string path = Path.Combine(outDir, "scene.json");
                    SceneProbe.WriteSceneJson(path, "test-scene", res,
                        new List<(float3 lo, float3 hi)>());

                    StringAssert.Contains("\"cross_half_m\": 160", File.ReadAllText(path));
                });
            }
            finally
            {
                Directory.Delete(outDir, true);
            }
        }

        [UnityTest]
        [Timeout(3600000)]
        public IEnumerator Export_SceneFile_From_CollisionWorld()
        {
            string outDir = Env("DIOGENES_SCENE_OUT",
                Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                             "Documents/diogenes-capture-runs/scene-export"));
            float trafficMin = float.Parse(Env("DIOGENES_TRAFFIC_MIN", "15"));
            float trafficMax = float.Parse(Env("DIOGENES_TRAFFIC_MAX", "60"));
            int warmup = int.Parse(Env("DIOGENES_WARMUP", "150"));
            string sceneName = Env("DIOGENES_SCENE_NAME", "megacity-corridor");
            Directory.CreateDirectory(outDir);

            World world = null;
            try
            {
                world = CaptureWorldBuilder.CreateWorld("DiogenesSceneProbeWorld");
                CaptureWorldBuilder.HoldTrafficSetup(world);
                var loads = CaptureWorldBuilder.LoadCaptureSubScenes(world, includeAudio: false);
                float deadline = Time.realtimeSinceStartup + 3000f;
                while (!CaptureWorldBuilder.AllScenesLoaded(world, loads)
                       && Time.realtimeSinceStartup < deadline)
                    yield return null;
                Assert.IsTrue(CaptureWorldBuilder.AllScenesLoaded(world, loads),
                    "subscene load timeout: " + CaptureWorldBuilder.SceneLoadStatus(world, loads));

                string trafficSource = CaptureWorldBuilder.EnsureTrafficEnabled(world);
                string density = CaptureWorldBuilder.ReleaseTrafficSetup(world, trafficMin, trafficMax);
                Debug.Log("[scene-probe] TrafficEnabled=" + trafficSource + " density=" + density);

                Time.captureDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;
                for (int i = 0; i < warmup; i++) yield return null;

                var res = SceneProbe.ExportAll(world, outDir, sceneName);
                Assert.IsTrue(res.Ok, "scene probe export failed: " + res.Error);
                var best = res.Corridors[res.ChosenIndex];
                Debug.Log("[scene-probe] OK corridors=" + res.Corridors.Count
                          + " chosen(road=" + best.RoadIndex + " axis=" + best.Axis
                          + " len=" + best.Length.ToString("F0")
                          + "m meanY=" + best.MeanY.ToString("F1")
                          + " inBand=" + best.VehiclesInBand
                          + " open=" + best.OpenFrac.ToString("F2")
                          + ") selection=" + res.SelectionMode
                          + (string.IsNullOrEmpty(res.SelectionNote)
                              ? "" : "(" + res.SelectionNote + ")")
                          + " offset=" + res.Volume.Origin
                          + " boxes=" + res.BoxCount
                          + " → " + res.SceneJsonPath);
                // 缺省选择（打分序）下零占据 = 物理坏了（低走廊必有峡谷墙体）。
                // 显式换景开关选中的高走廊可能是合法的全空气体积（楼面退到横向半宽外）：
                // 物理确认存活（前置 numStatic 检查 + 车辆普查非零）时放行并留警告。
                if (res.SelectionMode == "score")
                    Assert.Greater(res.BoxCount, 0, "empty occupancy: no static geometry in volume?");
                else if (res.BoxCount == 0)
                {
                    Assert.Greater(res.VehiclesTotal, 0,
                        "empty occupancy AND no vehicles: physics world looks dead");
                    Debug.LogWarning("[scene-probe] chosen high corridor volume is open air "
                        + "(0 static voxels) — legitimate under selection switch "
                        + res.SelectionMode + "; trajgen 侧共视回落到虚拟地面探针。");
                }
            }
            finally
            {
                Time.captureDeltaTime = 0f;
                if (world != null && world.IsCreated)
                    CaptureWorldBuilder.DestroyWorld(world);
            }
        }
    }
}
