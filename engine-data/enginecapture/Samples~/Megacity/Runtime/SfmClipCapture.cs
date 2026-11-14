// docs/83 D1：68 号单条 SFM clip 的完整采集编排（对应 MC 侧的 McClipCapture）。
//
// 与 MC 的差别只有形态，纪律完全相同：离线定帧、烘焙回放、构造自检、同 tick 侧录。
//   MC：4 路视频，4 份 action_camera
//   SFM：1 路视频（1920×1080）+ 1 路深度（1280×720，偶数 tick），
//        1 份 30fps json + 1 份 15fps json（30fps 的偶数帧投影）+ depth EXR 序列
//
// 「depth 与 rgb 帧、camera pose 严格对齐」（68 号 §6.5 的单独质检项）在这里是
// **构造性的**：深度相机 parent 在视频相机下、同一个 FrameClock tick、同一个
// ReadbackRing——不存在需要事后对齐的环节。

using System;
using System.Collections;
using System.Diagnostics;
using System.IO;
using Diogenes.EngineCapture;
using UnityEngine;
using Debug = UnityEngine.Debug;

namespace Diogenes.Capture.Megacity
{
    public sealed class SfmCaptureOptions
    {
        /// <summary>trajgen 出的计划目录（含 action_camera_30fps.json 作为烘焙位姿）。</summary>
        public string PlanDir;
        public string OutDir;
        public string ClipName;
        public Vector3 WorldOffset;
        public float WorldYawDeg;
        public bool RenderVideo = true;
        public bool CaptureAudio = true;
        public int WarmupTicks = 30;
        public float NearClip = 0.3f;
        public float FarClip = 500f;
        public float VerticalFovDeg = 58.72f;
        public string FfmpegPath = "/opt/homebrew/bin/ffmpeg";
        public string Codec = "libx264";
        public string Preset = "veryfast";
        public int BitrateKbps = 16000;
        public string GameProcessName = "DiogenesEngineScenes";
        public string SceneAssetId = "megacity-metro@6000.1.0f1";
        public double PoseTolerance = 1e-5;
    }

    public sealed class SfmCaptureResult
    {
        public bool Ok;
        public string Error = "";
        public string ClipDir = "";
        public long FramesVerified;
        public int DepthFrames;
        public double AvgTickMs;
        public bool AudioMuxed;
    }

    public static class SfmClipCapture
    {
        public static IEnumerator Run(SfmCaptureOptions o, SfmCaptureResult r)
        {
            string planPose = Path.Combine(o.PlanDir, "action_camera_30fps.json");
            if (!File.Exists(planPose)) { r.Error = "缺 " + planPose; yield break; }

            var track = BakedPoseTrack.Load(planPose);
            int frames = track.FrameCount;
            if (frames <= 0) { r.Error = "烘焙位姿为空"; yield break; }

            string clipName = string.IsNullOrEmpty(o.ClipName)
                ? "sfm-" + DateTime.Now.ToString("yyyyMMddHHmmss") : o.ClipName;
            string clipDir = Path.Combine(o.OutDir, clipName);
            Directory.CreateDirectory(clipDir);
            string depthDir = Path.Combine(clipDir, "depth");
            r.ClipDir = clipDir;

            // ---- rig ----
            var rigGo = new GameObject("DiogenesSfmRig");
            var videoGo = new GameObject("video");
            videoGo.transform.SetParent(rigGo.transform, false);
            var videoCam = videoGo.AddComponent<Camera>();
            videoCam.nearClipPlane = o.NearClip;
            videoCam.farClipPlane = o.FarClip;

            var depthGo = new GameObject("depth");
            depthGo.transform.SetParent(rigGo.transform, false);
            var depthCam = depthGo.AddComponent<Camera>();
            depthCam.nearClipPlane = o.NearClip;
            depthCam.farClipPlane = o.FarClip;

            var rig = rigGo.AddComponent<SfmRig>();
            rig.VideoCamera = videoCam;
            rig.DepthCamera = depthCam;
            rig.VerticalFovDeg = o.VerticalFovDeg;

            var clock = rigGo.AddComponent<FrameClock>();   // 序列化默认 Offline

            // 位姿回放：先失活挂件、赋值后再激活（OnEnable 会当场触发）
            var playbackGo = new GameObject("PosePlayback");
            playbackGo.SetActive(false);
            var playback = playbackGo.AddComponent<PosePlayback>();
            playback.Clock = clock;
            playback.Rig = rig;
            playback.WorldOffset = o.WorldOffset;
            playback.WorldYawDeg = o.WorldYawDeg;
            playback.ToleranceMeters = o.PoseTolerance;
            // 顺序要紧：SetTracks 会问 rig.PoseLaneCount，而 lane 是 Initialize() 才建的
            rig.Initialize();
            playback.SetTracks(new[] { track });            // SFM 只有 1 条 pose lane
            playback.ApplyFirstFrame();
            playbackGo.SetActive(true);

            // ---- 深度线性化通道 ----
            var blitGo = new GameObject("DepthPassBlitter");
            blitGo.SetActive(false);
            var blitter = blitGo.AddComponent<DepthPassBlitter>();
            blitter.DepthCamera = depthCam;
            blitter.Target = rig.DepthLane.TargetTexture;
            blitGo.SetActive(true);
            if (blitter.FailReason != null) { r.Error = blitter.FailReason; yield break; }

            // ---- 回读：视频路 → ffmpeg，深度路 → EXR ----
            ReadbackRing ring = null;
            FfmpegPipe videoPipe = null;
            DepthSink depthSink = null;
            if (o.RenderVideo)
            {
                var ringGo = new GameObject("ReadbackRing");
                ringGo.SetActive(false);
                ring = ringGo.AddComponent<ReadbackRing>();
                ring.Clock = clock;
                ring.Rig = rig;

                videoPipe = new FfmpegPipe(new FfmpegPipeConfig
                {
                    FfmpegPath = o.FfmpegPath, Codec = o.Codec,
                    Preset = o.Preset, BitrateKbps = o.BitrateKbps
                });
                var vs = rig.GetLane(0).Size;
                videoPipe.Start(vs.Width, vs.Height, Path.Combine(clipDir, "video.mp4"));

                depthSink = DepthExporter.CreateSink(depthDir, o.FarClip);
                ring.AttachSinks(new IFrameSink[]
                {
                    new VideoSink(videoPipe, vs.Width * vs.Height * 4),
                    depthSink,
                });
                ringGo.SetActive(true);
            }

            AudioCapture audio = null;
            if (o.CaptureAudio)
            {
                videoCam.gameObject.AddComponent<AudioListener>();
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
            recorderGo.SetActive(true);

            // ---- json 侧车：30fps 全帧 + 15fps 偶数帧（同一条流的投影，构造性对齐）----
            var w30 = ActionCameraJsonWriter.CreateFile(
                Path.Combine(clipDir, "action_camera_30fps.json"), ActionCameraVariant.Sfm30);
            var w15 = ActionCameraJsonWriter.CreateFile(
                Path.Combine(clipDir, "action_camera_15fps.json"), ActionCameraVariant.Sfm15);

            recorder.OnLaneFrame += playback.VerifyLaneFrame;      // 构造自检先于写盘
            recorder.OnLaneFrame += (lane, frame) =>
            {
                if (lane != 0) return;
                w30.WriteFrame(in frame);
                if (DepthEncoding.IsExportFrame(frame.Frame))
                {
                    var f = frame;
                    f.DepthName = DepthEncoding.DepthName(frame.Frame);
                    w15.WriteFrame(in f);
                }
            };

            bool aborted = false;
            string abortInfo = "";
            clock.OnClipAborted += reason =>
            {
                aborted = true;
                abortInfo = reason.Code + " @tick " + reason.Tick + ": " + reason.Detail;
            };

            // ---- 预热 + 采集 ----
            float prevCapture = Time.captureDeltaTime;
            Time.captureDeltaTime = (float)CaptureFormats.FrameIntervalSeconds;
            for (int i = 0; i < o.WarmupTicks; i++) yield return null;

            var sw = Stopwatch.StartNew();
            audio?.BeginCapture();
            clock.BeginClip();
            int safety = frames * 20;
            while (clock.CurrentTick < frames - 1 && !aborted && safety-- > 0) yield return null;
            clock.EndClip();
            sw.Stop();

            // 末帧的回读请求在本帧渲染后才发出，先让它渲完再设栅栏
            yield return null;
            ring?.WaitAllRequests();
            yield return null;

            r.AvgTickMs = sw.Elapsed.TotalMilliseconds / Math.Max(1, frames);
            r.FramesVerified = playback.FramesVerified;
            r.DepthFrames = depthSink != null ? depthSink.FramesWritten : 0;

            // ---- 收尾 ----
            w30.Finish(); w30.Dispose();
            w15.Finish(); w15.Dispose();

            bool ok = !aborted;
            string err = abortInfo;
            if (videoPipe != null && !videoPipe.Finish(180000))
            {
                ok = false;
                err += " ffmpeg: " + videoPipe.FailReason;
            }
            if (depthSink != null && depthSink.FailReason != null)
            {
                ok = false;
                err += " depth: " + depthSink.FailReason;
            }

            var info = CaptureSystemInfo.ForRenderTarget(o.GameProcessName, rig.GetLane(0).Size);
            SidecarJson.WriteUtf8File(Path.Combine(clipDir, "systeminfo.json"),
                                      SidecarJson.SystemInfoJson(in info));

            // provenance：UCL 红线硬标记（docs/78 R'5）。**必写**——编排器的上传闸门
            // 拿 license 字段做白名单校验，缺了它这条 clip 既进不了 internal-validation/，
            // 也失去「这是 UCL 场景产物、永不对外交付」的可追溯凭据。
            var prov = new ProvenanceInfo
            {
                SceneAssetId = o.SceneAssetId,
                LicenseId = "unity-companion-license",
                License = "UCL-internal-validation-only",
                PlanReference = o.PlanDir,
                CalibrationResult = "pose_selfcheck_frames=" + playback.FramesVerified,
            };
            prov.AddNote("mode", "sfm (docs/68): 1 video lane 1920x1080 + 1 depth lane 1280x720");
            prov.AddNote("depth", "view-space linear eye depth, metres, trunc(d*100)/100, "
                                + "single-channel Y half EXR, NaN beyond far=" + o.FarClip);
            prov.AddNote("depth_intrinsics",
                "depth intrinsics = video intrinsics scaled by resolution ratio "
                + "(same vFOV / principal point); docs/75 Q10 fixed convention");
            prov.AddNote("depth_frames", r.DepthFrames + " (even ticks only, 15 fps)");
            prov.AddNote("euler_order",
                "Unity Transform.eulerAngles convention; demand-side rotation order unconfirmed (docs/72 C3)");
            prov.AddNote("player_fields",
                "no player character: player_* mirrors the camera pose (PoseRecorder interim rule)");
            prov.AddNote("encoder", o.Codec + " preset=" + o.Preset + " (mac validation profile)");
            prov.AddNote("world_offset", o.WorldOffset.x + "," + o.WorldOffset.y + "," + o.WorldOffset.z);
            prov.AddNote("world_yaw_deg", o.WorldYawDeg.ToString("F2"));
            SidecarJson.WriteUtf8File(Path.Combine(clipDir, "provenance.json"),
                                      SidecarJson.ProvenanceJson(prov));

            audio?.EndCapture();
            if (audio != null && ok && o.RenderVideo)
            {
                string wav = Path.Combine(clipDir, "audio_mix.wav");
                if (audio.WriteWav(wav))
                    r.AudioMuxed = MuxAudio(o.FfmpegPath, clipDir, "video.mp4", wav, out err);
            }

            Time.captureDeltaTime = prevCapture;
            Cleanup(rigGo, playbackGo, blitGo, ring, audio, recorderGo);

            r.Ok = ok;
            r.Error = ok ? "" : err;
            Debug.Log(string.Format(
                "SFM clip {0}: frames={1} depth={2} verified={3} tick={4:F1}ms ok={5} {6}",
                clipName, frames, r.DepthFrames, r.FramesVerified, r.AvgTickMs, r.Ok, r.Error));
        }

        static bool MuxAudio(string ffmpeg, string dir, string video, string wav, out string err)
        {
            err = "";
            string tmp = Path.Combine(dir, "muxed.mp4");
            var psi = new ProcessStartInfo(ffmpeg)
            {
                // 不要 -shortest：它按较短的音轨截视频，实测把 1800 帧截成 1799
                // （59.989s）。与 McClipCapture 的 mux 参数保持一致。
                Arguments = "-hide_banner -loglevel error -y -i \"" + Path.Combine(dir, video) +
                            "\" -i \"" + wav + "\" -map 0:v:0 -map 1:a:0 " +
                            "-c:v copy -c:a aac -b:a 192k \"" + tmp + "\"",
                UseShellExecute = false, RedirectStandardError = true, CreateNoWindow = true
            };
            using (var p = Process.Start(psi))
            {
                string tail = p.StandardError.ReadToEnd();
                p.WaitForExit(180000);
                if (p.ExitCode != 0 || !File.Exists(tmp))
                {
                    err = "mux 失败：" + tail.Substring(Math.Max(0, tail.Length - 300));
                    return false;
                }
            }
            File.Delete(Path.Combine(dir, video));
            File.Move(tmp, Path.Combine(dir, video));
            return true;
        }

        static void Cleanup(GameObject rig, GameObject playback, GameObject blit,
                            ReadbackRing ring, AudioCapture audio, GameObject recorder)
        {
            if (ring != null) UnityEngine.Object.Destroy(ring.gameObject);
            if (audio != null) UnityEngine.Object.Destroy(audio.gameObject);
            if (recorder != null) UnityEngine.Object.Destroy(recorder);
            if (blit != null) UnityEngine.Object.Destroy(blit);
            if (playback != null) UnityEngine.Object.Destroy(playback);
            if (rig != null) UnityEngine.Object.Destroy(rig);
        }
    }
}
