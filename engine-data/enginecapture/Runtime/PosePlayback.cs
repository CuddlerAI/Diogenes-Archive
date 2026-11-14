// docs/78 G'1: baked-pose playback (辩题十裁决: 离线烘焙回放).
// The ONLY runtime pose logic is `transform = baked[frame]` - no interpolation, no
// smoothing, no look-at correction. The baked json (trajgen output) is ground truth;
// the engine only renders it and re-exports for the construction self-check.

using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>One baked camera track loaded from an action_camera_N.json file.</summary>
    public sealed class BakedPoseTrack
    {
        public double[][] Positions;   // [frame][3], generator frame (before world offset)
        public double[][] Quaternions; // [frame][4], [x, y, z, w]
        /// <summary>Baked input stream, cam1 only ([frame] = x, y, dx, dy); null when the
        /// plan carries no input fields. The planner derives these from the camera's own
        /// orientation deltas, so they are what makes the delivered mouse_dx/dy line up
        /// with on-screen camera motion (69 号 §8.2 json 验收第 4 条).</summary>
        public double[][] Mouse;
        public int[][] KeyCodes;
        public int FrameCount => Positions.Length;

        /// <summary>Load camera_position / camera_rotation_quaternion of every frame,
        /// plus the optional baked input stream (present on the cam1 plan only).</summary>
        public static BakedPoseTrack Load(string path)
        {
            var root = MiniJson.ParseFile(path) as List<object>
                ?? throw new InvalidDataException(path + ": top level is not a JSON array");
            var pos = new double[root.Count][];
            var quat = new double[root.Count][];
            double[][] mouse = null;
            int[][] keys = null;
            for (int i = 0; i < root.Count; i++)
            {
                var f = root[i] as Dictionary<string, object>
                    ?? throw new InvalidDataException(path + "[" + i + "]: frame is not an object");
                pos[i] = Nums(f, "camera_position", 3, path, i);
                quat[i] = Nums(f, "camera_rotation_quaternion", 4, path, i);
                double frameNo = f.TryGetValue("frame", out var fr) && fr is double d ? d : -1;
                if ((int)frameNo != i)
                    throw new InvalidDataException(path + "[" + i + "]: frame=" + frameNo + " != " + i);
                if (f.ContainsKey("mouse_x"))
                {
                    if (mouse == null) mouse = new double[root.Count][];
                    mouse[i] = new[] { Num(f, "mouse_x"), Num(f, "mouse_y"),
                                       Num(f, "mouse_dx"), Num(f, "mouse_dy") };
                }
                if (f.TryGetValue("keyCode", out var kv) && kv is List<object> kl)
                {
                    if (keys == null) keys = new int[root.Count][];
                    var arr = new int[kl.Count];
                    for (int k = 0; k < kl.Count; k++) arr[k] = (int)(double)kl[k];
                    keys[i] = arr;
                }
            }
            return new BakedPoseTrack
            {
                Positions = pos, Quaternions = quat, Mouse = mouse, KeyCodes = keys
            };
        }

        static double Num(Dictionary<string, object> f, string key)
            => f.TryGetValue(key, out var v) && v is double d ? d : 0.0;

        static double[] Nums(Dictionary<string, object> f, string key, int n, string path, int idx)
        {
            if (!f.TryGetValue(key, out var v) || !(v is List<object> list) || list.Count != n)
                throw new InvalidDataException(path + "[" + idx + "]: bad field '" + key + "'");
            var arr = new double[n];
            for (int i = 0; i < n; i++)
                arr[i] = (double)list[i];
            return arr;
        }

        /// <summary>Dominant horizontal view azimuth of this track (degrees, Unity
        /// convention: atan2(x, z) of the mean forward direction). Used by placement to
        /// align a lane's gaze with a content corridor.</summary>
        public double MeanForwardAzimuthDeg()
        {
            double sx = 0, sz = 0;
            for (int i = 0; i < Quaternions.Length; i++)
            {
                double[] q = Quaternions[i];
                double x = q[0], y = q[1], z = q[2], w = q[3];
                sx += 2.0 * (x * z + w * y);   // forward.x of q*(0,0,1)
                sz += 1.0 - 2.0 * (x * x + y * y); // forward.z
            }
            return Math.Atan2(sx, sz) * (180.0 / Math.PI);
        }
    }

    /// <summary>
    /// Per-tick verbatim playback of baked camera poses onto the capture rig lanes,
    /// plus the construction self-check (docs/78 §4.2 step 5): the PoseRecorder's
    /// re-exported pose must equal the applied pose within <see cref="ToleranceMeters"/>
    /// (1e-5 by default), else the clip aborts on the spot - a playback/clock bug must
    /// explode inside the engine, never flow to QC.
    ///
    /// World offset: baked plans live in the generator's local frame; WorldOffset
    /// translates them into the content scene (docs/78 S1 interim placement). The
    /// expected value for the self-check is computed in float32 exactly as applied
    /// (expected = (float)(baked + offset)), so the check is quantization-free.
    ///
    /// Wiring order contract: enable/subscribe PosePlayback BEFORE PoseRecorder so the
    /// pose for tick t is applied before the recorder samples tick t (FrameClock.OnTick
    /// invokes subscribers in subscription order).
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class PosePlayback : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] CaptureRigBase _rig;

        [Tooltip("Translation applied to every baked position (baked frame -> world frame).")]
        [SerializeField] Vector3 _worldOffset = Vector3.zero;

        [Tooltip("Yaw (degrees, about +Y through the plan origin) applied BEFORE the translation. Placement dof: aligns the plan's dominant gaze with a content corridor.")]
        [SerializeField] float _worldYawDeg = 0f;

        [Tooltip("Abort tolerance for |recorded - applied| per position axis (metres).")]
        [SerializeField] double _toleranceMeters = 1e-5;

        BakedPoseTrack[] _tracks;
        Vector3[] _appliedPos;                 // 惰性：长度取自 rig
        Quaternion[] _appliedRot;
        int _appliedTick = -1;
        long _framesVerified;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public CaptureRigBase Rig { get => _rig; set => _rig = value; }
        public Vector3 WorldOffset { get => _worldOffset; set => _worldOffset = value; }
        public float WorldYawDeg { get => _worldYawDeg; set => _worldYawDeg = value; }
        public double ToleranceMeters { get => _toleranceMeters; set => _toleranceMeters = value; }

        /// <summary>Frames available in the shortest loaded track.</summary>
        public int FrameCount { get; private set; }

        /// <summary>Loaded baked track for a lane (0-based), null before loading.</summary>
        public BakedPoseTrack Track(int laneIndex) => _tracks?[laneIndex];

        /// <summary>Total lane-frames that passed the construction self-check.</summary>
        public long FramesVerified => _framesVerified;

        /// <summary>Load action_camera_{1..4}.json from a trajgen plan directory.</summary>
        public void LoadPlanDir(string planDir)
        {
            var tracks = new BakedPoseTrack[_rig.PoseLaneCount];
            for (int i = 0; i < _rig.PoseLaneCount; i++)
            {
                string path = Path.Combine(planDir, "action_camera_" + (i + 1) + ".json");
                tracks[i] = BakedPoseTrack.Load(path);
            }
            SetTracks(tracks);
        }

        /// <summary>Inject already-loaded tracks (tests / partial-lane setups).</summary>
        public void SetTracks(BakedPoseTrack[] tracks)
        {
            if (tracks == null || tracks.Length != _rig.PoseLaneCount)
                throw new ArgumentException("Expected " + _rig.PoseLaneCount + " tracks", nameof(tracks));
            _appliedPos = new Vector3[_rig.PoseLaneCount];
            _appliedRot = new Quaternion[_rig.PoseLaneCount];
            int frames = int.MaxValue;
            for (int i = 0; i < tracks.Length; i++)
            {
                if (tracks[i] == null) throw new ArgumentException("track " + (i + 1) + " is null");
                frames = Math.Min(frames, tracks[i].FrameCount);
            }
            _tracks = tracks;
            FrameCount = frames;
        }

        /// <summary>Apply frame 0 immediately (pre-roll: cameras must sit on the first
        /// baked pose during warmup so tick 0 renders the correct view).</summary>
        public void ApplyFirstFrame() => ApplyFrame(0);

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
            if (_tracks == null) return;
            if (tick >= FrameCount)
            {
                _clock.AbortClip("pose_overrun",
                    "tick " + tick + " >= baked frame count " + FrameCount);
                return;
            }
            ApplyFrame(tick);
        }

        void ApplyFrame(int frame)
        {
            double yawRad = _worldYawDeg * (Math.PI / 180.0);
            double cy = Math.Cos(yawRad), sy = Math.Sin(yawRad);
            var yawQuat = Quaternion.AngleAxis(_worldYawDeg, Vector3.up);
            // 只驱动 pose lane：深度路 parent 在视频相机下，位姿自动跟随，
            // 不该也不能有自己的轨道（SFM 交付只有一份 action_camera）。
            for (int lane = 0; lane < _rig.PoseLaneCount; lane++)
            {
                var cam = _rig.GetLane(lane).Camera;
                if (cam == null) continue;
                double[] p = _tracks[lane].Positions[frame];
                double[] q = _tracks[lane].Quaternions[frame];
                // R_y(θ) in Unity's left-handed frame, then translate.
                double rx = p[0] * cy + p[2] * sy;
                double rz = -p[0] * sy + p[2] * cy;
                var pos = new Vector3(
                    (float)(rx + _worldOffset.x),
                    (float)(p[1] + _worldOffset.y),
                    (float)(rz + _worldOffset.z));
                var rot = yawQuat * new Quaternion((float)q[0], (float)q[1], (float)q[2], (float)q[3]);
                cam.transform.SetPositionAndRotation(pos, rot);
                _appliedPos[lane] = pos;
                _appliedRot[lane] = rot;
            }
            _appliedTick = frame;
        }

        /// <summary>
        /// Construction self-check. Subscribe to PoseRecorder.OnLaneFrame with this.
        /// Compares the recorder's sampled pose against the pose applied this tick.
        /// </summary>
        public void VerifyLaneFrame(int laneIndex, ActionCameraFrame f)
        {
            if (_tracks == null || _appliedTick < 0) return;
            if (f.Frame != _appliedTick)
            {
                Abort(laneIndex, f.Frame, "recorder frame " + f.Frame + " != applied tick " + _appliedTick);
                return;
            }
            Vector3 ap = _appliedPos[laneIndex];
            Quaternion aq = _appliedRot[laneIndex];
            double dx = Math.Abs(f.CameraPosition.X - ap.x);
            double dy = Math.Abs(f.CameraPosition.Y - ap.y);
            double dz = Math.Abs(f.CameraPosition.Z - ap.z);
            if (dx > _toleranceMeters || dy > _toleranceMeters || dz > _toleranceMeters)
            {
                Abort(laneIndex, f.Frame, "position drift (" + dx + ", " + dy + ", " + dz + ")");
                return;
            }
            double qx = Math.Abs(f.CameraRotationQuaternion.X - aq.x);
            double qy = Math.Abs(f.CameraRotationQuaternion.Y - aq.y);
            double qz = Math.Abs(f.CameraRotationQuaternion.Z - aq.z);
            double qw = Math.Abs(f.CameraRotationQuaternion.W - aq.w);
            if (qx > _toleranceMeters || qy > _toleranceMeters || qz > _toleranceMeters || qw > _toleranceMeters)
            {
                Abort(laneIndex, f.Frame, "rotation drift (" + qx + ", " + qy + ", " + qz + ", " + qw + ")");
                return;
            }
            _framesVerified++;
        }

        void Abort(int laneIndex, int frame, string detail)
        {
            _clock.AbortClip("pose_mismatch",
                "lane " + (laneIndex + 1) + " frame " + frame + ": " + detail);
        }
    }
}
