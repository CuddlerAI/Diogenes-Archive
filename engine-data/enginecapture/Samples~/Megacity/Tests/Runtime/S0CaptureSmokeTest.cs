using System.Collections;
using System.IO;
using Diogenes.EngineCapture;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;

namespace Diogenes.Capture.Tests
{
    /// <summary>docs/78 S0：静态合成场景、4 路 10s、SDK 采集链首次行为验证（渲染→回读→编码）。</summary>
    public class S0CaptureSmokeTest
    {
        const int Frames = 300; // 10 s @ 30 fps
        static readonly string OutDir = "/tmp/diogenes-s0";

        [UnityTest]
        public IEnumerator S0_FourLane_StaticScene_ProducesFourMp4()
        {
            if (Directory.Exists(OutDir)) Directory.Delete(OutDir, true);
            Directory.CreateDirectory(OutDir);

            // --- 极简内容：地面 + 立方体环 + 平行光（只为让画面非空） ---
            var light = new GameObject("SunS0").AddComponent<Light>();
            light.type = LightType.Directional;
            light.transform.rotation = Quaternion.Euler(50, -30, 0);
            GameObject.CreatePrimitive(PrimitiveType.Plane).transform.localScale = new Vector3(20, 1, 20);
            for (int i = 0; i < 12; i++)
            {
                var c = GameObject.CreatePrimitive(PrimitiveType.Cube);
                float a = i * Mathf.PI * 2f / 12f;
                c.transform.position = new Vector3(Mathf.Cos(a) * 8f, 1f, Mathf.Sin(a) * 8f);
                c.transform.localScale = new Vector3(1.5f, 2f + (i % 3), 1.5f);
            }

            // --- rig：4 路聚簇（两两 forward 夹角 ≈0°，满足 ≤15° 初始约束） ---
            var rigGo = new GameObject("CaptureRig");
            var rig = rigGo.AddComponent<MultiCamRig>();
            int[] focals = { 18, 24, 35, 50 };
            string[] aspects = { "16:9", "3:4", "4:3", "9:16" };
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var camGo = new GameObject("cam" + (i + 1));
                camGo.transform.SetParent(rigGo.transform, false);
                camGo.transform.position = new Vector3(-14f + i * 0.3f, 3f, -14f);
                camGo.transform.LookAt(new Vector3(0f, 1.5f, 0f));
                var cam = camGo.AddComponent<Camera>();
                rig.Lanes[i] = new CameraLane
                {
                    Camera = cam, FocalLengthMm = focals[i], Aspect = aspects[i], ExclusiveLayer = 24 + i
                };
            }
            rig.Initialize();

            var clock = rigGo.AddComponent<FrameClock>(); // 序列化默认 Offline

            var ringGo = new GameObject("Ring");
            ringGo.SetActive(false);
            var ring = ringGo.AddComponent<ReadbackRing>();
            ring.Clock = clock;
            ring.Rig = rig;
            var pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            for (int i = 0; i < pipes.Length; i++)
            {
                pipes[i] = new FfmpegPipe(new FfmpegPipeConfig
                {
                    FfmpegPath = "/opt/homebrew/bin/ffmpeg", Codec = "libx264", Preset = "veryfast",
                    BitrateKbps = 12000
                });
                var s = rig.Lanes[i].Size;
                pipes[i].Start(s.Width, s.Height, Path.Combine(OutDir, "video_" + (i + 1) + ".mp4"));
            }
            ring.AttachPipes(pipes);
            ringGo.SetActive(true);

            bool aborted = false; string abortInfo = "";
            clock.OnClipAborted += r => { aborted = true; abortInfo = r.Code + ":" + r.Detail; };

            clock.BeginClip();
            int safety = Frames * 20;
            while (clock.CurrentTick < Frames - 1 && !aborted && safety-- > 0)
                yield return null;
            clock.EndClip();
            ring.WaitAllRequests();
            yield return null;

            Assert.IsFalse(aborted, "clip aborted: " + abortInfo);
            for (int i = 0; i < pipes.Length; i++)
            {
                Assert.IsTrue(pipes[i].Finish(60000), "ffmpeg finish failed lane " + (i + 1) + ": " + pipes[i].FailReason);
                var f = new FileInfo(Path.Combine(OutDir, "video_" + (i + 1) + ".mp4"));
                Assert.IsTrue(f.Exists && f.Length > 100_000, "video_" + (i + 1) + " missing/too small: " + (f.Exists ? f.Length : 0));
            }
            Debug.Log("S0 OK: 4 mp4 written to " + OutDir);
        }
    }
}
