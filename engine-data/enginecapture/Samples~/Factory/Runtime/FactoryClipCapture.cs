// 宿主场景的 MC clip 采集编排（Factory Training 起家，现为通用室内宿主场景版）——
// 移植自 engine-data/enginecapture/Samples~/Megacity/Runtime/McClipCapture.cs，
// 去掉 Megacity 专属部分（ECS 世界/车流、载机 spawner、走廊自动 yaw）。
//
// **许可标记不在本文件里**（docs/70 R1 / docs/106）：它是场景属性，由调用方从
// engine-data/scenes/manifest.json 的登记注入。本文件从前硬编码了 Asset Store EULA
// 标记，工程整份复制到 Robotics-Warehouse（Apache-2.0）时标记跟着复制，把干净场景
// 的数据标成了 EULA。改法是让采集层没有默认许可可用：没被告知就拒绝出片。

using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using Diogenes.EngineCapture;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Factory
{
    public sealed class FactoryCaptureOptions
    {
        public string PlanDir;
        public string OutDir;
        public string ClipName;
        public Vector3 WorldOffset;
        public float WorldYawDeg = 0f;
        public bool RenderVideo = true;
        public bool CaptureAudio = true;
        public IDynObjectSource DynSource;   // 显式给源时用它；否则按 Crowd 配置自建
        /// <summary>非 null 则铺设 NpcCrowd 并用作 dyn 源（docs/70 §4.3 动态物体系统）。</summary>
        public NpcCrowdConfig Crowd;
        public GameObject CrowdHumanoidPrefab;
        public GameObject CrowdVehiclePrefab;
        /// <summary>场景占据图 json（SceneOccupancyExporter 产物）——NPC 用它防穿模，
        /// 与相机轨迹用的是同一张图。</summary>
        public string SceneFile;
        public float DynMaxDistance = 300f;
        public int WarmupTicks = 60;
        public string FfmpegPath = "/opt/homebrew/bin/ffmpeg";
        public string Codec = "libx264";     // mac 验证档; Windows 生产档 h264_nvenc
        public string Preset = "veryfast";
        public int BitrateKbps = 12000;
        public string GameProcessName = "FactoryTraining";
        public string SceneAssetId = "factory-training@assetstore-344832";
        /// <summary>许可标记三件套，来自 scenes/manifest.json 的场景登记，**没有默认值**：
        /// 留空 Run 直接拒绝出片（fail closed）。LicenseId 是 SPDX/EULA 标识，
        /// License 是上传闸校验的「许可+处置」令牌，Attribution 是随交付传递的署名文本
        /// （无署名义务时为空串，此时不写这条 note）。</summary>
        public string LicenseId = "";
        public string License = "";
        public string Attribution = "";
        public double PoseTolerance = 1e-5;
    }

    public sealed class FactoryCaptureResult
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

    public static class FactoryClipCapture
    {
        public static IEnumerator Run(FactoryCaptureOptions o, FactoryCaptureResult r)
        {
            // 许可先于一切：宁可这条 clip 不存在，也不留一条许可未知的数据。
            // 在写任何盘之前判，免得留下半条无主 clip。
            if (string.IsNullOrEmpty(o.License) || string.IsNullOrEmpty(o.LicenseId))
            {
                r.Error = "license marker missing: 采集层不再自带许可默认值，"
                        + "请由 scenes/manifest.json 的场景登记注入"
                        + "（DIOGENES_LICENSE / DIOGENES_LICENSE_ID）";
                yield break;
            }

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
                ? "factory-mc-" + seed + "-" + DateTime.Now.ToString("yyyyMMddHHmmss")
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
                cam.nearClipPlane = 0.1f;    // 室内近景比 Megacity 城市近
                cam.farClipPlane = 1000f;
                rig.Lanes[i] = new CameraLane
                {
                    Camera = cam,
                    FocalLengthMm = focals[i] > 0 ? focals[i] : 24,
                    Aspect = aspect,
                    ExclusiveLayer = 24 + i
                };
            }

            var clock = rigGo.AddComponent<FrameClock>(); // 序列化默认 Offline

            var playbackGo = new GameObject("PosePlayback");
            playbackGo.SetActive(false);
            var playback = playbackGo.AddComponent<PosePlayback>();
            playback.Clock = clock;
            playback.Rig = rig;
            playback.WorldOffset = o.WorldOffset;
            playback.ToleranceMeters = o.PoseTolerance;
            playback.LoadPlanDir(o.PlanDir);
            playback.WorldYawDeg = o.WorldYawDeg;

            playback.ApplyFirstFrame();
            rig.Initialize();
            playbackGo.SetActive(true);

            // 动态物体（docs/69「画面含较多动态物体 ≥80% 时长」）：绕锚点区游走的人群/载具。
            // 铺设在 rig.Initialize 之后、采集之前；随后作为 DynObjectProbe 的源。
            NpcCrowd crowd = null;
            if (o.Crowd != null)
            {
                // 窄画幅要把人群收近才入画（见 NpcCrowd.FovRadiusScale）
                float fSum = 0f, fN = 0f;
                foreach (var f in focals) if (f > 0) { fSum += f; fN++; }
                float scale = NpcCrowd.FovRadiusScale(aspect, fN > 0 ? fSum / fN : 24f);
                o.Crowd.RingRadiusMin *= scale;
                o.Crowd.RingRadiusMax *= scale;
                o.Crowd.VehicleRingRadius *= scale;

                var crowdGo = new GameObject("NpcCrowd");
                crowdGo.SetActive(false);
                crowd = crowdGo.AddComponent<NpcCrowd>();
                crowd.Clock = clock;
                crowd.Rig = rig;              // 相机 keepout 需要（防贴脸遮挡）
                crowd.LoadAnchors(o.PlanDir, o.WorldOffset, o.WorldYawDeg);
                crowd.LoadOccupancy(o.SceneFile);
                crowd.Build(o.Crowd, o.CrowdHumanoidPrefab, o.CrowdVehiclePrefab);
                crowdGo.SetActive(true);
                if (o.DynSource == null) o.DynSource = crowd;
                Debug.Log("[Diogenes] npc crowd: " + crowd.QuotaNote());
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
                    rig.Lanes[i].Camera.enabled = false;
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
            clock.OnTick += tick =>
            {
                if (pipes == null) return;
                for (int i = 0; i < pipes.Length; i++)
                    if (pipes[i] != null && pipes[i].HasFailed)
                        clock.AbortClip("ffmpeg_exit", "lane " + (i + 1) + ": " + pipes[i].FailReason);
            };

            // ---- 预热，固定步长 ----
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

            // 末 tick 回读请求在本帧渲染后才发出——先渲染完这一帧再栅栏
            yield return null;
            ring?.WaitAllRequests();
            yield return null;

            if (aborted)
            {
                foreach (var w in writers) w?.Dispose();
                foreach (var p in pipes) p?.Abort();
                audio?.EndCapture();
                TryDeleteDir(clipDir);
                Cleanup(rigGo, playbackGo, ring, audio, recorderGo, probe, crowd);
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

            File.Copy(Path.Combine(o.PlanDir, "plan.json"), Path.Combine(clipDir, "plan.json"), true);
            string covisSrc = Path.Combine(o.PlanDir, "covis.json");
            if (File.Exists(covisSrc))
                File.Copy(covisSrc, Path.Combine(clipDir, "covis.json"), true);

            if (probe != null)
                r.DynMetricsPass = probe.WriteJson(Path.Combine(clipDir, "dyn_metrics.json"));

            // provenance：身份与许可全部如实转写调用方给的值（来源见文件头）。
            // 编排器 upload 闸拿 license 标记在场景登记里反查处置：
            // `*-internal-validation-only` 只进 internal-validation/ 前缀，
            // `*-deliverable*` 才进交付面。
            var prov = new ProvenanceInfo
            {
                SceneAssetId = o.SceneAssetId,
                LicenseId = o.LicenseId,
                License = o.License,
                PlanReference = o.PlanDir,
                Seed = seed,
                CalibrationResult = "pose_selfcheck_lane_frames=" + playback.FramesVerified
            };
            if (!string.IsNullOrEmpty(o.Attribution))
                prov.AddNote("attribution", o.Attribution);
            prov.AddNote("euler_order",
                "Unity Transform.eulerAngles convention; demand-side rotation order unconfirmed (docs/72 C3)");
            prov.AddNote("player_fields",
                "no player character: player_* mirrors cam1 camera pose (PoseRecorder interim rule)");
            prov.AddNote("audio",
                audio == null ? "audio capture disabled"
                    : (r.AudioCaptured ? "engine mix via AudioRenderer, muxed into all four lanes"
                                       : "AudioRenderer yielded 0 samples, no track muxed"));
            prov.AddNote("encoder", o.Codec + " preset=" + o.Preset + " (mac validation profile)");
            prov.AddNote("world_offset", o.WorldOffset.x + "," + o.WorldOffset.y + "," + o.WorldOffset.z);
            prov.AddNote("world_yaw_deg", o.WorldYawDeg.ToString("F2"));
            prov.AddNote("dyn_objects",
                crowd != null
                    ? "procedural NpcCrowd orbiting plan anchors (not native scene actors): " + crowd.QuotaNote()
                      + (string.IsNullOrEmpty(o.SceneFile) ? "; clip-collision=raycast" : "; clip-collision=occupancy-grid")
                    : o.DynSource == null
                        ? "no dyn source (host scene static); dyn_metrics omitted"
                        : "host-supplied dyn source");
            SidecarJson.WriteUtf8File(Path.Combine(clipDir, "provenance.json"),
                SidecarJson.ProvenanceJson(prov));

            Cleanup(rigGo, playbackGo, ring, audio, recorderGo, probe, crowd);
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
            AudioCapture audio, GameObject recorderGo, DynObjectProbe probe, NpcCrowd crowd)
        {
            if (crowd != null) UnityEngine.Object.Destroy(crowd.gameObject);
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
    }
}
