// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using System.IO;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Packages one MC clip into the docs/69 delivery layout (mirrored by
    /// engine-data/delivery/README.md): a per-clip folder ("{scene}-{traj}-{seed}-{ts}",
    /// globally unique) holding the 12 delivery files
    ///     video_N.mp4 + systeminfo_N.json + action_camera_N.json   (N = 1..4)
    /// plus provenance.json, OUR 13th audit sidecar (docs/70): scene asset id, license
    /// id, plan reference, seed, SDK version, calibration result, interim-convention
    /// notes. provenance.json stays out of the demand-side file count.
    ///
    /// Json serialization uses the hand-written writers (see JsonPrimitives header for
    /// the rationale) - delivery key names, including "camera_Follow Offset", are
    /// byte-verbatim per engine-data/fixtures. systeminfo width/height are filled
    /// truthfully with the lane's RT size (vertical aspects report e.g. 1080x1920).
    ///
    /// Orchestration per clip: Begin -> (FrameClock ticks: PoseRecorder feeds writers,
    /// ReadbackRing feeds pipes) -> End (offline: WaitAllRequests barrier) -> sidecars.
    /// Any abort (frame watchdog, readback backlog, ffmpeg failure) discards the whole
    /// clip folder - docs/70 rules a damaged clip is re-recorded, never patched.
    ///
    /// SFM clips (docs/68: depth EXR + single camera) are NOT packaged here yet - see
    /// DepthExporter (skeleton) for the depth side; SFM packaging lands with it.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class ClipPackager : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] MultiCamRig _rig;
        [SerializeField] PoseRecorder _pose;
        [SerializeField] ReadbackRing _readback;
        [SerializeField] AudioCapture _audio;

        [Tooltip("Value written to systeminfo gameProcessName.")]
        [SerializeField] string _gameProcessName = "unity";

        [SerializeField] FfmpegPipeConfig _ffmpegConfig = new FfmpegPipeConfig();

        /// <summary>Audit sidecar; fill SceneAssetId/LicenseId/PlanReference/Seed before
        /// BeginClip. Standard interim-convention notes are appended automatically.</summary>
        public ProvenanceInfo Provenance { get; set; } = new ProvenanceInfo();

        public string ClipDirectory { get; private set; }
        public bool IsRecording { get; private set; }

        FfmpegPipe[] _pipes;
        ActionCameraJsonWriter[] _jsonWriters;

        void OnEnable()
        {
            if (_clock != null)
            {
                _clock.OnTick += HandlePostTick;
                _clock.OnClipAborted += HandleAbort;
            }
            if (_pose != null) _pose.OnLaneFrame += HandleLaneFrame;
        }

        void OnDisable()
        {
            if (_clock != null)
            {
                _clock.OnTick -= HandlePostTick;
                _clock.OnClipAborted -= HandleAbort;
            }
            if (_pose != null) _pose.OnLaneFrame -= HandleLaneFrame;
        }

        /// <summary>
        /// Start recording one clip. clipName must follow the delivery convention
        /// "{scene}-{traj}-{seed}-{ts}" (uniqueness is the caller's contract).
        /// </summary>
        public void BeginClip(string outputRoot, string clipName)
        {
            if (IsRecording) throw new InvalidOperationException("ClipPackager: clip already running");
            if (_rig == null || !_rig.IsInitialized)
                throw new InvalidOperationException("ClipPackager: rig not initialized");

            WarnIfDriverTooOld();

            ClipDirectory = Path.Combine(outputRoot, clipName);
            Directory.CreateDirectory(ClipDirectory);

            _pipes = new FfmpegPipe[MultiCamRig.LaneCount];
            _jsonWriters = new ActionCameraJsonWriter[MultiCamRig.LaneCount];
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                var size = _rig.Lanes[i].Size;
                _pipes[i] = new FfmpegPipe(_ffmpegConfig);
                _pipes[i].Start(size.Width, size.Height,
                    Path.Combine(ClipDirectory, "video_" + (i + 1) + ".mp4"));
                _jsonWriters[i] = ActionCameraJsonWriter.CreateFile(
                    Path.Combine(ClipDirectory, "action_camera_" + (i + 1) + ".json"),
                    i == 0 ? ActionCameraVariant.McCam1 : ActionCameraVariant.McCamN);
            }
            _readback?.AttachPipes(_pipes);

            IsRecording = true;
            _audio?.BeginCapture();
            _clock.BeginClip();
        }

        /// <summary>Finish the clip: barrier the readbacks, close encoders, write sidecars.</summary>
        public bool EndClip()
        {
            if (!IsRecording) return false;
            _clock.EndClip();
            _audio?.EndCapture();

            // Offline determinism barrier; harmless (drains faster) in realtime mode.
            _readback?.WaitAllRequests();

            bool ok = true;
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                ok &= _pipes[i].Finish();
                _jsonWriters[i].Finish();
                _jsonWriters[i].Dispose();

                var info = CaptureSystemInfo.ForRenderTarget(_gameProcessName, _rig.Lanes[i].Size);
                SidecarJson.WriteUtf8File(
                    Path.Combine(ClipDirectory, "systeminfo_" + (i + 1) + ".json"),
                    SidecarJson.SystemInfoJson(in info));
            }

            AppendStandardProvenanceNotes();
            SidecarJson.WriteUtf8File(
                Path.Combine(ClipDirectory, "provenance.json"),
                SidecarJson.ProvenanceJson(Provenance));

            _audio?.WriteWav(Path.Combine(ClipDirectory, "audio_mix.wav"));

            DisposePipes();
            _jsonWriters = null;
            IsRecording = false;
            if (!ok)
                Debug.LogError("EngineCapture ClipPackager: encoder failure in " + ClipDirectory);
            return ok;
        }

        // Post-tick health poll (runs after recorders on the same tick event; order
        // within the tick does not matter - failure aborts before the NEXT frame).
        void HandlePostTick(int tick)
        {
            if (!IsRecording || _pipes == null) return;
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                if (_pipes[i] != null && _pipes[i].HasFailed)
                {
                    _clock.AbortClip("ffmpeg_exit",
                        "lane " + (i + 1) + ": " + _pipes[i].FailReason);
                    return;
                }
            }
        }

        void HandleLaneFrame(int laneIndex, ActionCameraFrame frame)
        {
            if (!IsRecording || _jsonWriters == null) return;
            _jsonWriters[laneIndex].WriteFrame(in frame);
        }

        void HandleAbort(ClipAbortReason reason)
        {
            if (!IsRecording) return;
            IsRecording = false;
            _audio?.EndCapture();

            for (int i = 0; i < MultiCamRig.LaneCount; i++)
            {
                _pipes?[i]?.Abort();
                _jsonWriters?[i]?.Dispose();
            }
            DisposePipes();
            _jsonWriters = null;

            try
            {
                if (!string.IsNullOrEmpty(ClipDirectory) && Directory.Exists(ClipDirectory))
                    Directory.Delete(ClipDirectory, true);
            }
            catch (Exception exception) when (
                exception is IOException || exception is UnauthorizedAccessException)
            {
                Debug.LogWarning("EngineCapture ClipPackager: could not delete aborted clip dir "
                    + ClipDirectory + ": " + exception.Message);
            }

            Debug.LogWarning("EngineCapture clip aborted [" + reason.Code + "] at tick "
                + reason.Tick + ": " + reason.Detail);
        }

        void AppendStandardProvenanceNotes()
        {
            Provenance.AddNote("euler_order",
                "Unity Transform.eulerAngles convention; demand-side rotation order unconfirmed (docs/72 C3)");
            Provenance.AddNote("mouse_delta_normalization",
                "both axes divided by screen WIDTH (docs/71 interim); raw pixel deltas preserved by InputRecorder");
            Provenance.AddNote("mouse_y_origin",
                "Unity bottom-left origin; flip pending demand-side confirmation");
            Provenance.AddNote("systeminfo_resolution",
                "render-target size, truthfully reported (not desktop resolution)");
        }

        void WarnIfDriverTooOld()
        {
            // Windows-only production concern (docs/70): NVENC needs driver >= 551.76.
            if (FfmpegPipe.TryGetNvidiaDriverVersion(out var v) && v < FfmpegPipe.MinNvidiaDriver)
                Debug.LogWarning("EngineCapture: NVIDIA driver " + v + " < required "
                    + FfmpegPipe.MinNvidiaDriver + " for h264_nvenc");
        }

        void DisposePipes()
        {
            if (_pipes == null) return;
            for (int i = 0; i < _pipes.Length; i++) _pipes[i]?.Dispose();
            _pipes = null;
        }
    }
}
