// docs/78 §4.2 CaptureRunner 的 Megacity 实装：一条 MC clip 的完整采集编排。
// 离线定帧（FrameClock Offline）+ 烘焙回放（PosePlayback）+ 构造自检 + 4 路渲染回读
// 编码 + PoseRecorder 重导出 + DynObjectProbe + AudioCapture(离线 mux) + 12 文件打包
// （plan.json / covis.json 透传，provenance 带 UCL-internal-validation-only 硬标记）。

using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using Diogenes.EngineCapture;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.Universal;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Megacity
{
    public sealed class McCaptureOptions
    {
        public string PlanDir;
        public string OutDir;
        public string ClipName;
        public Vector3 WorldOffset;
        /// <summary>Yaw placement (degrees). NaN = auto-align: rotate the plan so cam4's
        /// dominant gaze runs along the nearest road corridor (needs AutoYawAzimuthDeg).</summary>
        public float WorldYawDeg = float.NaN;
        /// <summary>Corridor azimuth supplied by the world side for auto yaw (degrees).</summary>
        public float? AutoYawAzimuthDeg;
        public bool RenderVideo = true;
        public bool CaptureAudio = true;
        public IDynObjectSource DynSource;
        public float DynMaxDistance = 300f;
        public int WarmupTicks = 60;
        // 默认走 PATH 查找 + 按平台选编码档。曾经写死 "/opt/homebrew/bin/ffmpeg"
        // + libx264（mac 验证档），Windows 上直接 Win32Exception「系统找不到指定的文件」——
        // 注释当时已写明「Windows 生产档 h264_nvenc」，但没有任何覆盖通道，
        // 等于 SDK 在非 mac 机器上跑不起来。调用侧可用 DIOGENES_FFMPEG /
        // DIOGENES_CODEC / DIOGENES_PRESET 覆盖（见 McClipCaptureTest）。
        public string FfmpegPath = "ffmpeg";
#if UNITY_EDITOR_WIN || UNITY_STANDALONE_WIN
        public string Codec = "h264_nvenc";   // Windows 生产档（NVENC，驱动 ≥551.76）
        public string Preset = "p4";          // NVENC 预设，libx264 的 veryfast 在此非法
#else
        public string Codec = "libx264";      // mac/linux 无 NVENC
        public string Preset = "veryfast";
#endif
        public int BitrateKbps = 12000;
        public string GameProcessName = "MegacityMetro";
        public string SceneAssetId = "megacity-metro@6000.1.0f1";
        public double PoseTolerance = 1e-5;
    }

    public sealed class McCaptureResult
    {
        public bool Ok;
        public string Error = "";
        public string ClipDir = "";
        public long LaneFramesVerified;
        public double AvgTickMs;
        public bool DynMetricsPass;
        public bool AudioCaptured;
        public bool AudioMuxed;
    }

    /// <summary>One-clip capture pipeline. Drive with a coroutine runner (PlayMode test
    /// or an -executeMethod host).</summary>
    public static class McClipCapture
    {
        public static IEnumerator Run(McCaptureOptions o, McCaptureResult r)
        {
            // ---- plan 元数据 ----
            var plan = MiniJson.ParseFile(Path.Combine(o.PlanDir, "plan.json"))
                as Dictionary<string, object>;
            if (plan == null) { r.Error = "plan.json unreadable"; yield break; }
            string aspect = plan.TryGetValue("aspect", out var av) ? av as string : "16:9";
            long seed = plan.TryGetValue("seed", out var sv) && sv is double sd ? (long)sd : -1;
            var focals = new int[MultiCamRig.LaneCount];
            if (plan.TryGetValue("cameras", out var cams) && cams is List<object> camList)
            {
                foreach (var c in camList)
                {
                    if (c is Dictionary<string, object> cd
                        && cd.TryGetValue("id", out var idv) && idv is double idd
                        && cd.TryGetValue("focal_mm", out var fv) && fv is double fd)
                    {
                        int idx = (int)idd - 1;
                        if (idx >= 0 && idx < focals.Length) focals[idx] = (int)fd;
                    }
                }
            }

            string clipName = string.IsNullOrEmpty(o.ClipName)
                ? "megacity-mc-" + seed + "-" + DateTime.Now.ToString("yyyyMMddHHmmss")
                : o.ClipName;
            string clipDir = Path.Combine(o.OutDir, clipName);
            Directory.CreateDirectory(clipDir);
            r.ClipDir = clipDir;

            // ---- 采集链组装（激活顺序 == OnTick 订阅顺序，回放必须最先）----
            var rigGo = new GameObject("DiogenesCaptureRig");
            var rig = rigGo.AddComponent<MultiCamRig>();
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var camGo = new GameObject("cam" + (i + 1));
                camGo.transform.SetParent(rigGo.transform, false);
                var cam = camGo.AddComponent<Camera>();
                cam.nearClipPlane = 0.3f;
                cam.farClipPlane = 4000f;
                // 画质：程序化创建的 Camera 在 URP 下 renderPostProcessing 默认 **false**，
                // 场景全局 Volume（tonemapping / bloom / 曝光）完全不作用 ⇒ 采集画面
                // 发灰发平，与游戏本体观感差一个档（standalone 截图 vs 采集帧的差距
                // 即此，非编码码率问题）。开后处理 + SMAA 空间抗锯齿；刻意不用 TAA——
                // 时域累积会拖影，depth 车道明令禁 TAA（docs/83），RGB 保持同口径。
                var urpData = cam.GetUniversalAdditionalCameraData();
                urpData.renderPostProcessing = true;
                urpData.antialiasing = AntialiasingMode.SubpixelMorphologicalAntiAliasing;
                urpData.antialiasingQuality = AntialiasingQuality.High;
                rig.Lanes[i] = new CameraLane
                {
                    Camera = cam,
                    FocalLengthMm = focals[i] > 0 ? focals[i] : 24,
                    Aspect = aspect,
                    ExclusiveLayer = 24 + i
                };
            }

            // 开了场景 Volume 就得设防：Megacity 的全局 profile 里若带运动模糊/景深/
            // 镜头畸变，会直接踩交付红线（69 §8.1 画面模糊；docs/83 G-b：RGB 带畸变
            // 而 depth 是理想针孔 ⇒ 边缘系统性错位）。挂一个最高优先级的本地 Volume
            // 把这些几何/时域效果强制归零——只留 tonemapping/bloom/曝光这类不动
            // 几何的画质项。晕影/胶片颗粒一并归零（§8.1 禁黑边、禁噪点样式）。
            var purityGo = new GameObject("DiogenesCapturePurityVolume");
            purityGo.transform.SetParent(rigGo.transform, false);
            var purityVol = purityGo.AddComponent<Volume>();
            purityVol.isGlobal = true;
            purityVol.priority = 1000f;
            var purityProf = ScriptableObject.CreateInstance<VolumeProfile>();
            purityProf.Add<MotionBlur>(true).intensity.Override(0f);
            purityProf.Add<DepthOfField>(true).mode.Override(DepthOfFieldMode.Off);
            purityProf.Add<LensDistortion>(true).intensity.Override(0f);
            purityProf.Add<ChromaticAberration>(true).intensity.Override(0f);
            purityProf.Add<Vignette>(true).intensity.Override(0f);
            purityProf.Add<FilmGrain>(true).intensity.Override(0f);
            purityVol.profile = purityProf;

            var clock = rigGo.AddComponent<FrameClock>(); // 序列化默认 Offline

            var playbackGo = new GameObject("PosePlayback");
            playbackGo.SetActive(false);
            var playback = playbackGo.AddComponent<PosePlayback>();
            playback.Clock = clock;
            playback.Rig = rig;
            playback.WorldOffset = o.WorldOffset;
            playback.ToleranceMeters = o.PoseTolerance;
            playback.LoadPlanDir(o.PlanDir);

            // 摆位 yaw：显式给值用给值；NaN 且世界侧给了走廊方位 → 把 cam4 主导视轴
            // 转到走廊方向（DynObjectProbe「画面里有动态物体」的内容前提，docs/78 R'6）
            float yawDeg = 0f;
            if (!float.IsNaN(o.WorldYawDeg))
            {
                yawDeg = o.WorldYawDeg;
            }
            else if (o.AutoYawAzimuthDeg.HasValue)
            {
                double az4 = playback.Track(3).MeanForwardAzimuthDeg();
                float delta = Mathf.DeltaAngle((float)az4, o.AutoYawAzimuthDeg.Value);
                // 沿走廊两个方向等价，取转角小的那个
                if (Mathf.Abs(delta) > 90f) delta = Mathf.DeltaAngle((float)az4, o.AutoYawAzimuthDeg.Value + 180f);
                yawDeg = delta;
            }
            playback.WorldYawDeg = yawDeg;

            // 初始位姿先落位（预热期间 4 路就位于首帧位姿）再 Initialize（rig 首帧几何校验）
            playback.ApplyFirstFrame();
            // 导演模式：cam4 是统览位（docs/78 §10），初始朝向按设计与载机不同 ⇒ 豁免
            if (File.Exists(Path.Combine(o.PlanDir, "vehicle_plan.json")))
                rig.InitialAngleExemptLanes = new[] { 4 };
            rig.Initialize();
            playbackGo.SetActive(true);

            // 载机车体（docs/78 §10 修补）：三辆「玩家车」必须在画面里真实存在，
            // 否则 cam4 的出幅断言（几何投影）全过、画面里却空无一物。
            CarrierVehicleSpawner spawner = null;
            if (File.Exists(Path.Combine(o.PlanDir, "vehicle_plan.json")))
            {
                var spawnGo = new GameObject("CarrierVehicles");
                spawnGo.SetActive(false);
                spawner = spawnGo.AddComponent<CarrierVehicleSpawner>();
                spawner.Clock = clock;
                spawner.Rig = rig;
                spawner.WorldOffset = o.WorldOffset;
                spawner.WorldYawDeg = yawDeg;
                // Build 内含首帧挂载自检（车体↔本路相机距离必须 ≈ |mount_offset|）：
                // 坐标系错配在这里就爆，不进预热、不开编码器、不产出半成品 clip。
                string spawnError = null;
                try
                {
                    spawner.LoadPlan(o.PlanDir);
                    spawner.Build(LoadCarrierPrefab());
                }
                catch (Exception e)
                {
                    spawnError = e.Message;
                }
                if (spawnError != null)
                {
                    Cleanup(rigGo, playbackGo, null, null, null, null, spawner);
                    TryDeleteDir(clipDir);
                    r.Error = "clip aborted: carrier_spawn: " + spawnError;
                    yield break;
                }
                spawnGo.SetActive(true);
                Debug.Log("[Diogenes] carrier bodies spawned: " + spawner.SpawnedCount);
            }

            ReadbackRing ring = null;
            var pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            if (o.RenderVideo)
            {
                var ringGo = new GameObject("ReadbackRing");
                ringGo.SetActive(false);
                ring = ringGo.AddComponent<ReadbackRing>();
                ring.Clock = clock;
                ring.Rig = rig;
                for (int i = 0; i < pipes.Length; i++)
                {
                    pipes[i] = new FfmpegPipe(new FfmpegPipeConfig
                    {
                        FfmpegPath = o.FfmpegPath,
                        Codec = o.Codec,
                        Preset = o.Preset,
                        BitrateKbps = o.BitrateKbps
                    });
                    var s = rig.Lanes[i].Size;
                    pipes[i].Start(s.Width, s.Height,
                        Path.Combine(clipDir, "video_" + (i + 1) + ".mp4"));
                }
                ring.AttachPipes(pipes);
                ringGo.SetActive(true);
            }
            else
            {
                for (int i = 0; i < MultiCamRig.LaneCount; i++)
                    rig.Lanes[i].Camera.enabled = false; // 不渲染，只回放位姿
            }

            AudioCapture audio = null;
            if (o.CaptureAudio)
            {
                rig.MainLane.Camera.gameObject.AddComponent<AudioListener>();
                var audioGo = new GameObject("AudioCapture");
                audioGo.SetActive(false);
                audio = audioGo.AddComponent<AudioCapture>();
                audio.Clock = clock;
                audioGo.SetActive(true);
            }

            var recorderGo = new GameObject("PoseRecorder");
            recorderGo.SetActive(false);
            var recorder = recorderGo.AddComponent<PoseRecorder>();
            recorder.Clock = clock;
            recorder.Rig = rig;
            // No live input device on the programmatic path: replay the planner's baked
            // cam1 mouse/key stream so the delivered mouse_dx/dy track the camera's own
            // orientation deltas instead of being flat zero (69 号 §8.2 第 4/11 条).
            recorder.BakedInput = playback.Track(0);
            recorderGo.SetActive(true);

            DynObjectProbe probe = null;
            if (o.DynSource != null)
            {
                var probeGo = new GameObject("DynObjectProbe");
                probeGo.SetActive(false);
                probe = probeGo.AddComponent<DynObjectProbe>();
                probe.Clock = clock;
                probe.Rig = rig;
                probe.Source = o.DynSource;
                probe.MaxDistance = o.DynMaxDistance;
                probeGo.SetActive(true);
            }

            // ---- json 侧车：构造自检订阅在写盘之前 ----
            var writers = new ActionCameraJsonWriter[MultiCamRig.LaneCount];
            for (int i = 0; i < writers.Length; i++)
                writers[i] = ActionCameraJsonWriter.CreateFile(
                    Path.Combine(clipDir, "action_camera_" + (i + 1) + ".json"),
                    i == 0 ? ActionCameraVariant.McCam1 : ActionCameraVariant.McCamN);
            recorder.OnLaneFrame += playback.VerifyLaneFrame;
            recorder.OnLaneFrame += (lane, frame) =>
            {
                if (writers[lane] != null) writers[lane].WriteFrame(in frame);
            };

            bool aborted = false;
            string abortInfo = "";
            clock.OnClipAborted += reason =>
            {
                aborted = true;
                abortInfo = reason.Code + " @tick " + reason.Tick + ": " + reason.Detail;
            };
            // 编码器健康巡检（订阅在最后：本 tick 全部工作完成后再查错）
            clock.OnTick += tick =>
            {
                if (pipes == null) return;
                for (int i = 0; i < pipes.Length; i++)
                    if (pipes[i] != null && pipes[i].HasFailed)
                        clock.AbortClip("ffmpeg_exit", "lane " + (i + 1) + ": " + pipes[i].FailReason);
            };

            // ---- 预热（车流铺满 + 流送稳定），固定步长 ----
            float prevCapture = Time.captureDeltaTime;
            Time.captureDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;
            for (int i = 0; i < o.WarmupTicks; i++) yield return null;

            // ---- 采集 ----
            int frames = playback.FrameCount;
            var sw = Stopwatch.StartNew();
            probe?.BeginClip();
            audio?.BeginCapture();
            clock.BeginClip();
            int safety = frames * 20;
            while (clock.CurrentTick < frames - 1 && !aborted && safety-- > 0)
                yield return null;
            clock.EndClip();
            sw.Stop();
            probe?.EndClip();

            // 最后一个 tick 的回读请求在本帧各相机渲染后（endCameraRendering）才发出——
            // 必须先让这一帧渲染完，再做回读栅栏。
            yield return null;
            ring?.WaitAllRequests();
            yield return null;

            if (aborted)
            {
                foreach (var w in writers) w?.Dispose();
                foreach (var p in pipes) p?.Abort();
                audio?.EndCapture();
                TryDeleteDir(clipDir);
                Cleanup(rigGo, playbackGo, ring, audio, recorderGo, probe, spawner);
                Time.captureDeltaTime = prevCapture;
                r.Error = "clip aborted: " + abortInfo;
                yield break;
            }

            r.AvgTickMs = sw.Elapsed.TotalMilliseconds / frames;
            r.LaneFramesVerified = playback.FramesVerified;

            // ---- 收尾落盘 ----
            bool ok = true;
            string err = "";
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                if (pipes[i] != null && !pipes[i].Finish(120000))
                {
                    ok = false;
                    err += "ffmpeg lane " + (i + 1) + ": " + pipes[i].FailReason + "; ";
                }
                writers[i].Finish();
                writers[i].Dispose();
                var info = CaptureSystemInfo.ForRenderTarget(o.GameProcessName, rig.Lanes[i].Size);
                SidecarJson.WriteUtf8File(
                    Path.Combine(clipDir, "systeminfo_" + (i + 1) + ".json"),
                    SidecarJson.SystemInfoJson(in info));
            }

            // 音频：引擎混音 wav + 离线 mux 进 4 路（同一混音，已裁决口径）
            audio?.EndCapture();
            if (audio != null)
            {
                string wav = Path.Combine(clipDir, "audio_mix.wav");
                r.AudioCaptured = audio.WriteWav(wav);
                if (r.AudioCaptured && o.RenderVideo && ok)
                {
                    r.AudioMuxed = true;
                    for (int i = 1; i <= MultiCamRig.LaneCount; i++)
                        r.AudioMuxed &= MuxAudio(o.FfmpegPath, clipDir, "video_" + i + ".mp4", wav, out err);
                }
            }

            // plan / covis 透传（QC cam4 组与 covis.ratio 的输入）
            File.Copy(Path.Combine(o.PlanDir, "plan.json"), Path.Combine(clipDir, "plan.json"), true);
            string covisSrc = Path.Combine(o.PlanDir, "covis.json");
            if (File.Exists(covisSrc))
                File.Copy(covisSrc, Path.Combine(clipDir, "covis.json"), true);

            if (probe != null)
                r.DynMetricsPass = probe.WriteJson(Path.Combine(clipDir, "dyn_metrics.json"));

            // provenance：UCL 红线硬标记（docs/78 R'5）
            var prov = new ProvenanceInfo
            {
                SceneAssetId = o.SceneAssetId,
                LicenseId = "unity-companion-license",
                License = "UCL-internal-validation-only",
                PlanReference = o.PlanDir,
                Seed = seed,
                CalibrationResult = "pose_selfcheck_lane_frames=" + playback.FramesVerified
            };
            prov.AddNote("euler_order",
                "Unity Transform.eulerAngles convention; demand-side rotation order unconfirmed (docs/72 C3)");
            prov.AddNote("player_fields",
                "no player character: player_* mirrors cam1 camera pose (PoseRecorder interim rule)");
            prov.AddNote("audio",
                audio == null ? "audio capture disabled"
                    : (r.AudioCaptured ? "engine mix via AudioRenderer, muxed into all four lanes"
                                       : "AudioRenderer yielded 0 samples (batchmode audio off?), no track muxed"));
            // 曾写死 "(mac validation profile)"，与 o.Codec 无关——换成 NVENC 跑时
            // provenance 仍自称 mac 档，是交付元数据层面的假话。改为如实报运行平台。
            prov.AddNote("encoder", o.Codec + " preset=" + o.Preset
                         + " (" + Application.platform + ")");
            prov.AddNote("post_processing",
                "URP scene volumes + SMAA(high); purity volume forces off: "
                + "motion-blur / DoF / lens-distortion / chromatic-aberration / vignette / film-grain");
            prov.AddNote("world_offset", o.WorldOffset.x + "," + o.WorldOffset.y + "," + o.WorldOffset.z);
            prov.AddNote("world_yaw_deg", yawDeg.ToString("F2"));
            if (spawner != null)
                // 记录 ≠ 校验：这条是**已执行**自检的覆盖面（超差当场 AbortClip），不是观测量
                prov.AddNote("carrier_mount",
                    "vehicle_plan v" + spawner.PlanVersion + " frames=" + spawner.PlanFramesFrame
                    + "; mount selfcheck carrier_frames=" + spawner.MountFramesVerified
                    + " worst_err=" + spawner.WorstMountErrorMeters.ToString("F4") + "m"
                    + " (expect |mount_offset|=" + spawner.MountDistanceMeters.ToString("F3")
                    + "m ±" + spawner.MountToleranceMeters.ToString("F2") + "m)");
            SidecarJson.WriteUtf8File(Path.Combine(clipDir, "provenance.json"),
                SidecarJson.ProvenanceJson(prov));

            Cleanup(rigGo, playbackGo, ring, audio, recorderGo, probe, spawner);
            Time.captureDeltaTime = prevCapture;

            r.Ok = ok;
            r.Error = err;
        }

        static bool MuxAudio(string ffmpeg, string dir, string video, string wav, out string err)
        {
            err = "";
            string src = Path.Combine(dir, video);
            string tmp = src + ".mux.mp4";
            var psi = new ProcessStartInfo
            {
                FileName = ffmpeg,
                Arguments = "-hide_banner -loglevel error -y -i \"" + src + "\" -i \"" + wav +
                            "\" -map 0:v:0 -map 1:a:0 -c:v copy -c:a aac -b:a 192k \"" + tmp + "\"",
                UseShellExecute = false,
                RedirectStandardError = true,
                CreateNoWindow = true
            };
            try
            {
                using (var p = Process.Start(psi))
                {
                    string stderr = p.StandardError.ReadToEnd();
                    p.WaitForExit(120000);
                    if (p.ExitCode != 0)
                    {
                        err = "mux failed: " + stderr;
                        File.Delete(tmp);
                        return false;
                    }
                }
                File.Delete(src);
                File.Move(tmp, src);
                return true;
            }
            catch (Exception e)
            {
                err = "mux exception: " + e.Message;
                return false;
            }
        }

        static void Cleanup(GameObject rigGo, GameObject playbackGo, ReadbackRing ring,
            AudioCapture audio, GameObject recorderGo, DynObjectProbe probe,
            CarrierVehicleSpawner spawner)
        {
            // 车体是 Instantiate 出来的独立根物体，不随 spawner 一起销毁 ⇒ 显式清
            if (spawner != null)
            {
                spawner.DestroyBodies();
                UnityEngine.Object.Destroy(spawner.gameObject);
            }
            if (probe != null) UnityEngine.Object.Destroy(probe.gameObject);
            if (recorderGo != null) UnityEngine.Object.Destroy(recorderGo);
            if (audio != null) UnityEngine.Object.Destroy(audio.gameObject);
            if (ring != null) UnityEngine.Object.Destroy(ring.gameObject);
            if (playbackGo != null) UnityEngine.Object.Destroy(playbackGo);
            if (rigGo != null) UnityEngine.Object.Destroy(rigGo);
        }

        static void TryDeleteDir(string dir)
        {
            try { if (Directory.Exists(dir)) Directory.Delete(dir, true); }
            catch (IOException e) { Debug.LogWarning("could not delete aborted clip dir: " + e.Message); }
        }

        /// <summary>取 Megacity 自带 HoverCar 预制体作为载机车体；取不到返回 null
        /// （Spawner 会退化为等尺寸程序化车体，画面里仍有可见主体）。</summary>
        static GameObject LoadCarrierPrefab()
        {
#if UNITY_EDITOR
            string[] candidates =
            {
                "Assets/Prefabs/Vehicles/Traffic/Vehicle_HoverCar_A_Variant.prefab",
                "Assets/Prefabs/Vehicles/Traffic/Vehicle_HoverCar_B Variant.prefab",
            };
            foreach (var path in candidates)
            {
                var go = UnityEditor.AssetDatabase.LoadAssetAtPath<GameObject>(path);
                if (go != null) return go;
            }
#endif
            return null;
        }

    }
}
