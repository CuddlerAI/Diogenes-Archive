// docs/83 D1：68 号单条 SFM clip 全链（在 Megacity 采集世界里跑真几何）。
//
// 与 McClipCaptureTest 同一套世界搭建，只换采集形态：1 路视频 + 1 路深度。
// 产物落 clip 目录后由 engine_qc --mode sfm 终审（depth.zip 打包在编排侧完成）。

using System;
using System.Collections;
using System.IO;
using Diogenes.EngineCapture;
using NUnit.Framework;
using Unity.Entities;
using UnityEngine;
using UnityEngine.TestTools;

namespace Diogenes.Capture.Tests
{
    public class SfmClipCaptureTest
    {
        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        [UnityTest]
        [Timeout(7200000)]
        public IEnumerator Sfm_CaptureClip_FromPlan()
        {
            string planDir = Env("DIOGENES_SFM_PLAN", "/tmp/sfmplan");
            string outDir = Env("DIOGENES_SFM_OUT", "/tmp/sfmclips");
            string clipName = Env("DIOGENES_SFM_CLIP", "sfm-d1");
            string[] off = Env("DIOGENES_WORLD_OFFSET", "60,138,220").Split(',');
            var worldOffset = new Vector3(float.Parse(off[0]), float.Parse(off[1]), float.Parse(off[2]));
            int warmup = int.Parse(Env("DIOGENES_WARMUP", "90"));

            Assert.IsTrue(File.Exists(Path.Combine(planDir, "action_camera_30fps.json")),
                          "plan dir invalid: " + planDir);
            Directory.CreateDirectory(outDir);

            World world = null;
            var result = new Megacity.SfmCaptureResult();
            try
            {
                world = Megacity.CaptureWorldBuilder.CreateWorld();
                Megacity.CaptureWorldBuilder.HoldTrafficSetup(world);
                var loads = Megacity.CaptureWorldBuilder.LoadCaptureSubScenes(world, includeAudio: true);
                float deadline = Time.realtimeSinceStartup + 3000f;
                while (!Megacity.CaptureWorldBuilder.AllScenesLoaded(world, loads)
                       && Time.realtimeSinceStartup < deadline)
                    yield return null;
                Assert.IsTrue(Megacity.CaptureWorldBuilder.AllScenesLoaded(world, loads),
                              "subscene load timeout");

                Megacity.CaptureWorldBuilder.EnsureTrafficEnabled(world);
                Megacity.CaptureWorldBuilder.ReleaseTrafficSetup(world, 15f, 60f);

                var road = Megacity.CaptureWorldBuilder.NearestRoadSegment(world, worldOffset);
                if (road.HasValue) worldOffset.y = road.Value.Midpoint.y - 4.5f;

                var options = new Megacity.SfmCaptureOptions
                {
                    PlanDir = planDir,
                    OutDir = outDir,
                    ClipName = clipName,
                    WorldOffset = worldOffset,
                    WorldYawDeg = road.HasValue ? road.Value.AzimuthDeg : 0f,
                    WarmupTicks = warmup,
                };
                yield return Megacity.SfmClipCapture.Run(options, result);

                Debug.Log("[sfm-capture] ok=" + result.Ok + " err='" + result.Error +
                          "' frames=" + result.FramesVerified + " depth=" + result.DepthFrames +
                          " tick=" + result.AvgTickMs.ToString("F1") + "ms dir=" + result.ClipDir);
            }
            finally
            {
                if (world != null && world.IsCreated) world.Dispose();
            }

            Assert.IsTrue(result.Ok, "capture failed: " + result.Error);
            Assert.That(result.DepthFrames, Is.EqualTo(900),
                        "60s@30fps 的偶数帧应出 900 张 depth");
            Assert.That(result.FramesVerified, Is.EqualTo(1800L),
                        "构造自检应覆盖每一帧（1 条 pose lane × 1800 帧）");
            foreach (var f in new[] { "video.mp4", "action_camera_30fps.json",
                                      "action_camera_15fps.json", "systeminfo.json" })
                Assert.IsTrue(File.Exists(Path.Combine(result.ClipDir, f)), "缺 " + f);
            Assert.That(Directory.GetFiles(Path.Combine(result.ClipDir, "depth"), "*.exr").Length,
                        Is.EqualTo(900));
        }
    }
}
