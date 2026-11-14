// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using System.Globalization;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Records per-camera pose/intrinsics/speed every FrameClock tick (docs/70 recorder
    /// set; field semantics per docs/68/69 delivery json):
    ///   * camera_position / camera_rotation_quaternion ([x, y, z, w] order) from
    ///     Transform.position / Transform.rotation;
    ///   * camera_intrinsics: fx = fy = (H/2)/tan(vFOV/2), cx = W/2, cy = H/2
    ///     (see CaptureFormats; matches demand-side samples);
    ///   * camera_speed: positional finite difference * 30 (Vector3, world units/s);
    ///   * follow offset: camera position expressed in the player anchor's local frame
    ///     (player.InverseTransformPoint(camera.position)); zero when no anchor;
    ///   * cam1 (main lane) extras (docs/69 white fields): player_* (copies the camera
    ///     pose when no player anchor exists) and euler angles via Transform.eulerAngles.
    ///     Euler rotation ORDER convention is not confirmed by the demand side
    ///     (docs/72 C3) - recorded under Unity's convention with a provenance note.
    /// Mouse/key fields for cam1 are pulled from the optional InputRecorder.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class PoseRecorder : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] CaptureRigBase _rig;

        [Tooltip("Optional player/character anchor. cam1 player_* fields copy the camera pose when unset (docs/69 interim rule for characterless scenes).")]
        [SerializeField] Transform _playerAnchor;

        [Tooltip("Optional input recorder feeding cam1 mouse/key fields.")]
        [SerializeField] InputRecorder _input;

        /// <summary>Baked cam1 input stream, used when no live InputRecorder exists
        /// (the programmatic capture path). Without it the delivered mouse_x/y/dx/dy are
        /// all zero while the camera visibly turns, which fails 69 号 §8.2 json 验收
        /// 第 4 条（dx/dy 要与画面镜头移动对齐）and 第 11 条.</summary>
        public BakedPoseTrack BakedInput { get; set; }

        [Tooltip("World-to-metric scale written as metric_scale (1.0 when scene units are metres).")]
        [SerializeField] double _metricScale = 1.0;

        /// <summary>Per tick, one frame per lane, lane index 0-based (0 == cam1).</summary>
        public event Action<int, ActionCameraFrame> OnLaneFrame;

        Vector3[] _prevCamPos;
        Vector3 _prevPlayerPos;
        bool _hasPrev;
        DateTime _clipStartWallClock;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public CaptureRigBase Rig { get => _rig; set => _rig = value; }
        public Transform PlayerAnchor { get => _playerAnchor; set => _playerAnchor = value; }
        public InputRecorder Input { get => _input; set => _input = value; }

        void OnEnable()
        {
            if (_clock != null) _clock.OnTick += HandleTick;
        }

        void OnDisable()
        {
            if (_clock != null) _clock.OnTick -= HandleTick;
        }

        void HandleTick(int tick)
        {
            if (_rig == null || !_rig.IsInitialized) return;

            if (tick == 0 || !_hasPrev)
            {
                _prevCamPos = new Vector3[_rig.PoseLaneCount];
                for (int i = 0; i < _rig.PoseLaneCount; i++)
                    _prevCamPos[i] = _rig.GetLane(i).Camera.transform.position;
                _prevPlayerPos = _playerAnchor != null
                    ? _playerAnchor.position
                    : _rig.MainLane.Camera.transform.position;
                _hasPrev = true;
                // Wall clock is only DATA (the delivery "time" string), never a timing
                // source; offline mode synthesizes deterministic timestamps from it.
                _clipStartWallClock = DateTime.Now;
            }

            string time = TimestampFor(tick);
            double fps = _clock.Mode == FrameClockMode.Offline
                ? CaptureFormats.FrameRate
                : (Time.deltaTime > 0f ? 1.0 / Time.deltaTime : CaptureFormats.FrameRate);

            // 深度路不出 json（它与视频路同位姿），只记 pose lane
            for (int i = 0; i < _rig.PoseLaneCount; i++)
            {
                var frame = BuildLaneFrame(i, tick, time, fps);
                OnLaneFrame?.Invoke(i, frame);
            }

            for (int i = 0; i < _rig.PoseLaneCount; i++)
                _prevCamPos[i] = _rig.GetLane(i).Camera.transform.position;
            // Must mirror the tick==0 seeding above, INCLUDING the no-anchor fallback:
            // when _playerAnchor is null, FillMainLaneExtras reads the cam1 transform,
            // so the baseline has to advance too. Guarding this on `_playerAnchor != null`
            // froze the baseline at frame 0 and made player_speed accumulate as
            // (pos[k] - pos[0])*fps -- 98% of frames then exceeded the 6 m/s cap.
            _prevPlayerPos = _playerAnchor != null
                ? _playerAnchor.position
                : _rig.MainLane.Camera.transform.position;
        }

        ActionCameraFrame BuildLaneFrame(int laneIndex, int tick, string time, double fps)
        {
            var lane = _rig.GetLane(laneIndex);
            Transform ct = lane.Camera.transform;
            Vector3 pos = ct.position;
            Quaternion rot = ct.rotation;
            Vector3 speed = tick == 0 ? Vector3.zero
                : (pos - _prevCamPos[laneIndex]) * CaptureFormats.FrameRate;
            Vector3 follow = _playerAnchor != null
                ? _playerAnchor.InverseTransformPoint(pos)
                : Vector3.zero;

            var frame = new ActionCameraFrame
            {
                Time = time,
                Fps = fps,
                Frame = tick,
                CameraPosition = ToVec3(pos),
                CameraRotationQuaternion = ToQuat(rot),
                CameraFollowOffset = ToVec3(follow),
                CameraIntrinsics = _rig.IntrinsicsForLane(laneIndex),
                CameraSpeed = ToVec3(speed),
                MetricScale = _metricScale,
                KeyCodes = Array.Empty<int>()
            };

            if (laneIndex == 0)
                FillMainLaneExtras(ref frame, ct, tick);

            return frame;
        }

        void FillMainLaneExtras(ref ActionCameraFrame frame, Transform cameraTransform, int tick)
        {
            // Euler angles per Unity convention; rotation order pending demand-side
            // confirmation (docs/72 C3) - see provenance note in ClipPackager.
            frame.CameraRotationEuler = ToVec3(cameraTransform.eulerAngles);

            Transform pt = _playerAnchor != null ? _playerAnchor : cameraTransform;
            Vector3 playerPos = pt.position;
            Vector3 playerSpeed = tick == 0 ? Vector3.zero
                : (playerPos - _prevPlayerPos) * CaptureFormats.FrameRate;

            frame.PlayerPosition = ToVec3(playerPos);
            frame.PlayerRotationEuler = ToVec3(pt.eulerAngles);
            frame.PlayerRotationQuaternion = ToQuat(pt.rotation);
            frame.PlayerSpeed = ToVec3(playerSpeed);

            if (_input != null)
            {
                var sample = _input.SampleForTick(tick);
                frame.MouseX = sample.MouseX;
                frame.MouseY = sample.MouseY;
                frame.MouseDx = sample.MouseDxNormalized;
                frame.MouseDy = sample.MouseDyNormalized;
                frame.KeyCodes = sample.HeldVirtualKeys ?? Array.Empty<int>();
            }
            else if (BakedInput != null)
            {
                // Programmatic capture: replay the planner's input stream verbatim.
                var m = BakedInput.Mouse;
                if (m != null && tick < m.Length && m[tick] != null)
                {
                    frame.MouseX = m[tick][0];
                    frame.MouseY = m[tick][1];
                    frame.MouseDx = m[tick][2];
                    frame.MouseDy = m[tick][3];
                }
                var k = BakedInput.KeyCodes;
                if (k != null && tick < k.Length && k[tick] != null)
                    frame.KeyCodes = k[tick];
            }
        }

        string TimestampFor(int tick)
        {
            // Delivery format: "yyyy-MM-dd HH:mm:ss.fff" (per demand-side samples).
            DateTime t = _clock.Mode == FrameClockMode.Offline
                ? _clipStartWallClock.AddSeconds(tick * CaptureFormats.FrameIntervalSeconds)
                : DateTime.Now;
            return t.ToString("yyyy-MM-dd HH:mm:ss.fff", CultureInfo.InvariantCulture);
        }

        static Vec3 ToVec3(in Vector3 v) => new Vec3(v.x, v.y, v.z);
        static Quat ToQuat(in Quaternion q) => new Quat(q.x, q.y, q.z, q.w);
    }
}
