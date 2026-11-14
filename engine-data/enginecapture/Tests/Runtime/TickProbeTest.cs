// tick 键控帧探针（PlayMode 回归测试）—— BRINGUP-MEGACITY.md §2/§7-2 事故的固化防线。
//
// 事故史：AsyncGPUReadback 曾在 Update 阶段发请求，GPU 拷贝排在本帧渲染之前，
// 视频恒滞后 1 tick。json 与视频各自全绿、只是错开一帧，QC 无跨模态检查抓不到；
// 当年靠一次性探针（tick 编码成纯色帧）抓出，但探针没入仓，此后换编码器/驱动/
// 图形 API/RT 格式/tick 循环都无防线。本测试把它固化：每 tick 渲染一张 tick 键控
// 纯色帧，走 ReadbackRing→FfmpegPipe 真实链路（非 mock）出 mp4，再由
// Tools~/tick_probe_decode.py 抽帧反解 tick，逐帧断言 shift==0（-1=视频滞后
// 1 tick=事故形态），任何非零偏移 FAIL 并打印偏移直方图。
//
// 什么时候必须重跑：换编码器或 ffmpeg / 换显卡驱动 / 换图形 API / 换 RT 格式 /
// 动 FrameClock tick 循环或 ReadbackRing 请求时机之后（BRINGUP-MEGACITY.md §11）。
//
// 位置裁决：放包内 Tests/Runtime 而非 Samples~/Megacity/Tests——探针只依赖包
// Runtime（零 Megacity/Entities 依赖），回归对象是包级不变量（回读时机 × 编码器 ×
// RT 格式），消费工程经 manifest "testables" 直接拾取，免 Samples~→Assets 手工
// 同步的漂移（S1 探针正是死在那条同步链上没入仓）。

using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Text;
using NUnit.Framework;
using UnityEngine;
using UnityEngine.TestTools;
using Debug = UnityEngine.Debug;

namespace Diogenes.EngineCapture.Tests
{
    public class TickProbeTest
    {
        /// <summary>90 帧 = 3 s @ 30 fps；(R,G) 联合码唯一区 256 tick ≫ 90，
        /// 每帧可反解出绝对 tick（滞后/超前/丢帧/重复帧全部逐帧可见）。</summary>
        const int Frames = 90;

        // ---------------------------------------------------------------- 编码档矩阵

        [UnityTest]
        [Timeout(600000)]
        public IEnumerator TickProbe_Libx264_ShiftZero()
        {
            // 验证档（mac/linux 与历史 S1 复现档）。libx264 不可用视为环境损坏：红灯。
            yield return RunProbe("libx264", "veryfast", requireCodec: true);
        }

        [UnityTest]
        [Timeout(600000)]
        public IEnumerator TickProbe_H264Nvenc_ShiftZero()
        {
            // 生产档（docs/70：Windows h264_nvenc）。无 NVENC 的开发机默认 Ignore；
            // 采集机必须设 DIOGENES_REQUIRE_NVENC=1，让「驱动升级悄悄弄坏 NVENC」变红灯。
            yield return RunProbe("h264_nvenc", "p4",
                requireCodec: Env("DIOGENES_REQUIRE_NVENC", "0") == "1");
        }

        [UnityTest]
        [Timeout(600000)]
        public IEnumerator TickProbe_EnvProfile_ShiftZero()
        {
            // 换编码档前的先行探针：DIOGENES_CODEC/DIOGENES_PRESET 指定待验证档
            // （与 McClipCaptureTest 同一环境变量通道）。未设置则跳过。
            string codec = Env("DIOGENES_CODEC", "");
            if (codec.Length == 0)
                Assert.Ignore("DIOGENES_CODEC 未设置：固定矩阵（libx264/h264_nvenc）已覆盖");
            string preset = Env("DIOGENES_PRESET", "");
            if (preset.Length == 0)
                Assert.Fail("DIOGENES_PRESET 必须与 DIOGENES_CODEC 成对设置（预设名不跨编码器通用）");
            yield return RunProbe(codec, preset, requireCodec: true);
        }

        // ---------------------------------------------------------------- 探针本体

        IEnumerator RunProbe(string codec, string preset, bool requireCodec)
        {
            if (!Application.isEditor)
                Assert.Ignore("探针依赖编辑器环境（python 解码器 / PackageInfo 路径解析）");
            string ffmpeg = Env("DIOGENES_FFMPEG", "ffmpeg");

            // 前置闸门先行（编码器可用性 / python / 解码脚本），采集不白跑。
            string encErr = ProbeEncoder(ffmpeg, codec, preset);
            if (encErr != null)
            {
                if (requireCodec)
                    Assert.Fail("编码器 " + codec + " (preset " + preset + ") 不可用: " + encErr);
                Assert.Ignore("跳过 " + codec + "（本机不可用；采集机应设 DIOGENES_REQUIRE_NVENC=1 使其变红）: " + encErr);
            }
            string python = ResolvePython();
            string decoder = ResolveDecoder();

            string outDir = Path.Combine(Path.GetTempPath(), "diogenes-tick-probe", codec);
            if (Directory.Exists(outDir)) Directory.Delete(outDir, true);
            Directory.CreateDirectory(outDir);

            // --- 探针场景：生产同款 4 路 rig（真实 RT 几何/格式），纯色清屏，无几何体 ---
            var rigGo = new GameObject("TickProbeRig");
            var rig = rigGo.AddComponent<MultiCamRig>();
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var camGo = new GameObject("tickcam" + (i + 1));
                camGo.transform.SetParent(rigGo.transform, false);
                var cam = camGo.AddComponent<Camera>();
                cam.clearFlags = CameraClearFlags.SolidColor;
                cam.cullingMask = 0;
                cam.backgroundColor = Color.black;
                rig.Lanes[i] = new CameraLane
                {
                    Camera = cam, FocalLengthMm = 24, Aspect = "16:9", ExclusiveLayer = 24 + i
                };
            }
            rig.Initialize();
            var clock = rigGo.AddComponent<FrameClock>(); // 序列化默认 Offline：tick==帧号

            var driverGo = new GameObject("TickColorDriver");
            driverGo.SetActive(false);
            var driver = driverGo.AddComponent<TickColorDriver>();
            driver.Clock = clock;
            driver.Rig = rig;
            driverGo.SetActive(true);

            var ringGo = new GameObject("TickProbeRing");
            ringGo.SetActive(false);
            var ring = ringGo.AddComponent<ReadbackRing>();
            ring.Clock = clock;
            ring.Rig = rig;
            var pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            var mp4s = new string[MultiCamRig.LaneCount];
            for (int i = 0; i < pipes.Length; i++)
            {
                pipes[i] = new FfmpegPipe(new FfmpegPipeConfig
                {
                    FfmpegPath = ffmpeg, Codec = codec, Preset = preset, BitrateKbps = 12000
                });
                var s = rig.Lanes[i].Size;
                mp4s[i] = Path.Combine(outDir, "tick_" + (i + 1) + ".mp4");
                pipes[i].Start(s.Width, s.Height, mp4s[i]);
            }
            ring.AttachPipes(pipes);
            ringGo.SetActive(true);

            bool aborted = false;
            string abortInfo = "";
            clock.OnClipAborted += r =>
            {
                aborted = true;
                abortInfo = r.Code + " @tick " + r.Tick + ": " + r.Detail;
            };

            try
            {
                clock.BeginClip();
                int safety = Frames * 20;
                while (clock.CurrentTick < Frames - 1 && !aborted && safety-- > 0)
                    yield return null;
                clock.EndClip();
                // 末 tick 的回读请求要等本帧渲染完（endCameraRendering）才发出：
                // 先让这一帧走完，再做回读栅栏（BRINGUP §2 的调用侧配套）。
                yield return null;
                ring.WaitAllRequests();
                yield return null;

                Assert.IsFalse(aborted, "clip aborted: " + abortInfo);
                Assert.AreEqual(Frames - 1, clock.CurrentTick, "tick 循环未跑满");
                for (int i = 0; i < pipes.Length; i++)
                    Assert.IsTrue(pipes[i].Finish(60000),
                        "ffmpeg lane " + (i + 1) + " 收尾失败: " + pipes[i].FailReason);
            }
            finally
            {
                foreach (var p in pipes)
                {
                    if (p == null) continue;
                    if (aborted) p.Abort();
                    p.Dispose();
                }
                UnityEngine.Object.Destroy(ringGo);
                UnityEngine.Object.Destroy(driverGo);
                UnityEngine.Object.Destroy(rigGo);
                Time.captureDeltaTime = 0f;
            }

            // --- 解码断言：逐帧 shift==0 + 残差闸门（详见 Tools~/tick_probe_decode.py 文件头）---
            RunDecoder(python, decoder, ffmpeg, mp4s, codec);
        }

        // ---------------------------------------------------------------- tick 编码

        /// <summary>tick → 纯色帧（解码端 Tools~/tick_probe_decode.py 同一公式）。
        /// 步长 16 按实测噪声标定，不是拍的：ffmpeg 7.1.1 下纯 swscale
        /// RGB→YUV420(BT.601 limited)→RGB 往返对饱和色 worst=3，libx264/h264_nvenc
        /// 全链 worst=4.0；步长 8 的判决半径恰为 4（零余量），步长 16 判决半径 8
        /// = 2 倍噪声上限。B=0 是常数哨兵：rgba/bgra 通道错位、映射漂移立即显形。</summary>
        internal static Color32 TickColor(int tick)
        {
            return new Color32(
                (byte)((tick * 16) & 255),        // tick 低 4 位
                (byte)(((tick >> 4) * 16) & 255), // tick 位 4..7 → (R,G) 唯一区 256 tick
                0,
                255);
        }

        sealed class TickColorDriver : MonoBehaviour
        {
            public FrameClock Clock;
            public CaptureRigBase Rig;

            void OnEnable() { Clock.OnTick += Handle; }
            void OnDisable() { Clock.OnTick -= Handle; }

            void Handle(int tick)
            {
                // OnTick（Update 阶段）设 tick 色，本帧相机随后才渲染；ReadbackRing 的
                // 回读请求也在本帧渲染后（endCameraRendering）发出。若它退回 Update 期
                // 发请求（0802 事故形态），抓到的是上一帧 ⇒ 解码端逐帧 shift=-1。
                Color c = TickColor(tick);
                for (int i = 0; i < Rig.Count; i++)
                    Rig.GetLane(i).Camera.backgroundColor = c;
            }
        }

        // ---------------------------------------------------------------- 解码器调用

        void RunDecoder(string python, string decoder, string ffmpeg, string[] mp4s, string codec)
        {
            var args = new StringBuilder();
            args.Append('"').Append(decoder).Append('"')
                .Append(" --ffmpeg \"").Append(ffmpeg).Append('"')
                .Append(" --expect-frames ").Append(Frames);
            foreach (var m in mp4s)
                args.Append(" \"").Append(m).Append('"');

            var psi = new ProcessStartInfo
            {
                FileName = python,
                Arguments = args.ToString(),
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                CreateNoWindow = true,
                StandardOutputEncoding = Encoding.UTF8,
                StandardErrorEncoding = Encoding.UTF8
            };
            psi.EnvironmentVariables["PYTHONIOENCODING"] = "utf-8";

            var stdout = new StringBuilder();
            var stderr = new StringBuilder();
            using (var p = new Process { StartInfo = psi })
            {
                p.OutputDataReceived += (s, e) => { if (e.Data != null) stdout.AppendLine(e.Data); };
                p.ErrorDataReceived += (s, e) => { if (e.Data != null) stderr.AppendLine(e.Data); };
                p.Start();
                p.BeginOutputReadLine();
                p.BeginErrorReadLine();
                if (!p.WaitForExit(300000))
                {
                    try { p.Kill(); } catch (Exception) { }
                    Assert.Fail("解码器超时（300s）\n" + stdout + "\n" + stderr);
                }
                p.WaitForExit(); // 无参 WaitForExit：等异步流 flush 完

                string all = stdout.ToString();
                Debug.Log("[tick-probe " + codec + "] 产物 " + Path.GetDirectoryName(mp4s[0])
                          + "\n" + all);

                // 偏移直方图与残差明细都在解码器 stdout 里，随断言消息完整落到测试日志。
                Assert.AreEqual(0, p.ExitCode,
                    "tick 探针判定失败（exit " + p.ExitCode + "：2=存在非零 shift，3=解码完整性）\n"
                    + all + "\n" + stderr);

                // 双保险：机读行显式断言逐帧 shift==0（防解码器退出码语义漂移）。
                string json = ExtractJsonLine(all);
                Assert.IsNotNull(json, "解码器输出缺 TICKPROBE_JSON 行\n" + all);
                var root = MiniJson.Parse(json) as Dictionary<string, object>;
                Assert.IsNotNull(root, "TICKPROBE_JSON 不可解析: " + json);
                Assert.IsTrue(root["ok"] is bool ok && ok, "TICKPROBE_JSON.ok != true\n" + all);
                var lanes = root["lanes"] as List<object>;
                Assert.IsNotNull(lanes, "TICKPROBE_JSON.lanes 缺失: " + json);
                Assert.AreEqual(mp4s.Length, lanes.Count, "lane 数不符\n" + all);
                for (int i = 0; i < lanes.Count; i++)
                {
                    var lane = lanes[i] as Dictionary<string, object>;
                    Assert.IsNotNull(lane, "lane " + (i + 1) + " 不可解析");
                    Assert.IsTrue(lane["shift_zero"] is bool sz && sz,
                        "lane " + (i + 1) + " 存在非零 shift（直方图见上方解码器输出）\n" + all);
                    Assert.IsTrue(lane["decode_ok"] is bool dk && dk,
                        "lane " + (i + 1) + " 解码完整性失败\n" + all);
                }
            }
        }

        static string ExtractJsonLine(string stdout)
        {
            const string tag = "TICKPROBE_JSON ";
            foreach (var line in stdout.Split('\n'))
            {
                string t = line.Trim();
                if (t.StartsWith(tag, StringComparison.Ordinal))
                    return t.Substring(tag.Length);
            }
            return null;
        }

        // ---------------------------------------------------------------- 环境解析

        static string Env(string key, string fallback)
        {
            string v = Environment.GetEnvironmentVariable(key);
            return string.IsNullOrEmpty(v) ? fallback : v;
        }

        /// <summary>lavfi 黑场 3 帧真实开一次编码器；返回 null=可用，否则失败详情。</summary>
        static string ProbeEncoder(string ffmpeg, string codec, string preset)
        {
            var psi = new ProcessStartInfo
            {
                FileName = ffmpeg,
                Arguments = "-hide_banner -v error -f lavfi -i color=c=black:s=256x256:r=30:d=0.1"
                            + " -frames:v 3 -c:v " + codec + " -preset " + preset + " -f null -",
                UseShellExecute = false,
                RedirectStandardError = true,
                CreateNoWindow = true
            };
            try
            {
                using (var p = Process.Start(psi))
                {
                    string err = p.StandardError.ReadToEnd();
                    if (!p.WaitForExit(30000))
                    {
                        try { p.Kill(); } catch (Exception) { }
                        return "编码器探测超时";
                    }
                    return p.ExitCode == 0 ? null : "exit " + p.ExitCode + ": " + Tail(err, 300);
                }
            }
            catch (Exception e)
            {
                return "ffmpeg 启动失败（设 DIOGENES_FFMPEG）: " + e.Message;
            }
        }

        static string ResolvePython()
        {
            var candidates = new List<string>();
            string env = Env("DIOGENES_PYTHON", "");
            if (env.Length > 0) candidates.Add(env);
            candidates.Add("python");
            candidates.Add("python3");
            foreach (var c in candidates)
            {
                try
                {
                    var psi = new ProcessStartInfo
                    {
                        FileName = c, Arguments = "--version", UseShellExecute = false,
                        RedirectStandardOutput = true, RedirectStandardError = true,
                        CreateNoWindow = true
                    };
                    using (var p = Process.Start(psi))
                    {
                        string o = p.StandardOutput.ReadToEnd() + p.StandardError.ReadToEnd();
                        // Windows 商店占位 stub 会假装存在：必须验证真输出 Python 版本号
                        if (p.WaitForExit(10000) && p.ExitCode == 0 && o.Contains("Python"))
                            return c;
                    }
                }
                catch (Exception) { /* 下一个候选 */ }
            }
            Assert.Fail("找不到 Python 3（候选: " + string.Join(", ", candidates.ToArray())
                        + "）；请设 DIOGENES_PYTHON");
            return null;
        }

        static string ResolveDecoder()
        {
            string env = Env("DIOGENES_TICKPROBE_DECODER", "");
            if (env.Length > 0)
            {
                Assert.IsTrue(File.Exists(env), "DIOGENES_TICKPROBE_DECODER 不存在: " + env);
                return env;
            }
#if UNITY_EDITOR
            var info = UnityEditor.PackageManager.PackageInfo.FindForAssembly(
                typeof(TickProbeTest).Assembly);
            if (info != null)
            {
                string p = Path.Combine(info.resolvedPath, "Tools~", "tick_probe_decode.py");
                Assert.IsTrue(File.Exists(p), "包内解码脚本缺失: " + p);
                return p;
            }
#endif
            Assert.Fail("定位不到 Tools~/tick_probe_decode.py（非 UPM 包环境请设 DIOGENES_TICKPROBE_DECODER）");
            return null;
        }

        static string Tail(string s, int n)
        {
            if (string.IsNullOrEmpty(s)) return "";
            s = s.Trim();
            return s.Length <= n ? s : s.Substring(s.Length - n);
        }
    }
}
