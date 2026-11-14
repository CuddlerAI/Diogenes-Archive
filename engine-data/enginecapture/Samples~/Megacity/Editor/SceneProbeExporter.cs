// docs/78 §4.1 SceneProbeExporter 的 batchmode -executeMethod 入口（一次性导出）：
//
//   Unity -batchmode -projectPath <megacity-metro> \
//         -executeMethod Diogenes.Bringup.SceneProbeExporter.Run -logFile <log>
//   环境变量：DIOGENES_SCENE_OUT   输出目录（默认 ~/Documents/diogenes-capture-runs/scene-export）
//             DIOGENES_TRAFFIC_MIN/MAX  车距区间（默认 15/60，与编排器一致）
//             DIOGENES_WARMUP     预热 tick 数（默认 150，让车流铺满走廊）
//
// 不进 play mode：手动泵浦采集世界（world.Update()）。edit mode 下 UnityEngine.Time
// 不前进，所以禁用 UpdateWorldTimeSystem 并逐拍 World.SetTime——FixedStep 组
// （Unity.Physics BuildPhysicsWorld 所在）按 ElapsedTime 推进，物理照常烘焙。
// PresentationSystemGroup 全程禁用（导出不渲染，避开 batchmode 下 entities.graphics
// 的帧上下文问题）。若此路径在某版本 editor 失效，等价兜底是 PlayMode 测试
// SceneProbeExportTest（同一 SceneProbe.ExportAll 核心）。

using System;
using System.Diagnostics;
using System.IO;
using Diogenes.Capture.Megacity;
using Unity.Core;
using Unity.Entities;
using UnityEditor;
using UnityEditorInternal;
using Debug = UnityEngine.Debug;

namespace Diogenes.Bringup
{
    public static class SceneProbeExporter
    {
        const float Dt = 1f / 30f;

        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        public static void Run()
        {
            int code = 1;
            try
            {
                code = RunInner();
            }
            catch (Exception e)
            {
                Debug.LogError("[scene-probe] fatal: " + e);
            }
            finally
            {
                if (InternalEditorUtility.inBatchMode)
                    EditorApplication.Exit(code);
            }
        }

        static int RunInner()
        {
            string outDir = Env("DIOGENES_SCENE_OUT",
                Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                             "Documents/diogenes-capture-runs/scene-export"));
            float trafficMin = float.Parse(Env("DIOGENES_TRAFFIC_MIN", "15"));
            float trafficMax = float.Parse(Env("DIOGENES_TRAFFIC_MAX", "60"));
            int warmup = int.Parse(Env("DIOGENES_WARMUP", "150"));
            Directory.CreateDirectory(outDir);

            World world = null;
            try
            {
                world = CaptureWorldBuilder.CreateWorld("DiogenesSceneProbeWorld",
                                                        appendToPlayerLoop: false);
                var timeSys = world.GetExistingSystemManaged<UpdateWorldTimeSystem>();
                if (timeSys != null) timeSys.Enabled = false;
                var presentation = world.GetExistingSystemManaged<PresentationSystemGroup>();
                if (presentation != null) presentation.Enabled = false;

                CaptureWorldBuilder.HoldTrafficSetup(world);
                var loads = CaptureWorldBuilder.LoadCaptureSubScenes(world, includeAudio: false);

                double elapsed = 0;
                void Pump()
                {
                    elapsed += Dt;
                    world.SetTime(new TimeData(elapsed, Dt));
                    world.Update();
                }

                var sw = Stopwatch.StartNew();
                while (!CaptureWorldBuilder.AllScenesLoaded(world, loads)
                       && sw.Elapsed.TotalSeconds < 1800)
                    Pump();
                if (!CaptureWorldBuilder.AllScenesLoaded(world, loads))
                {
                    Debug.LogError("[scene-probe] subscene load timeout: "
                                   + CaptureWorldBuilder.SceneLoadStatus(world, loads));
                    return 2;
                }
                Debug.Log("[scene-probe] subscenes loaded in "
                          + sw.Elapsed.TotalSeconds.ToString("F1") + "s");

                string trafficSource = CaptureWorldBuilder.EnsureTrafficEnabled(world);
                string density = CaptureWorldBuilder.ReleaseTrafficSetup(world, trafficMin, trafficMax);
                Debug.Log("[scene-probe] TrafficEnabled=" + trafficSource + " density=" + density);

                for (int i = 0; i < warmup; i++) Pump();

                var res = SceneProbe.ExportAll(world, outDir);
                if (!res.Ok)
                {
                    Debug.LogError("[scene-probe] export failed: " + res.Error);
                    return 3;
                }
                var best = res.Corridors[res.ChosenIndex];
                Debug.Log("[scene-probe] OK corridors=" + res.Corridors.Count
                          + " chosen(road=" + best.RoadIndex + " axis=" + best.Axis
                          + " len=" + best.Length.ToString("F0")
                          + "m inBand=" + best.VehiclesInBand + " open=" + best.OpenFrac.ToString("F2")
                          + ") offset=" + res.Volume.Origin
                          + " boxes=" + res.BoxCount
                          + " → " + res.SceneJsonPath);
                return 0;
            }
            finally
            {
                if (world != null && world.IsCreated)
                    CaptureWorldBuilder.DestroyWorld(world);
            }
        }
    }
}
