// Pure C# (no UnityEngine dependency) so the serialization layer can be unit-tested
// outside Unity. Spec source: docs/68 (SFM) / docs/69 (MC) delivery json, mirrored by
// engine-data/schema + engine-data/fixtures. Key-name quirks are normative - see
// engine-data/README.md "schema 键名怪癖表".

using System;

namespace Diogenes.EngineCapture
{
    /// <summary>Double-precision 3-vector for serialization ([x, y, z]).</summary>
    public readonly struct Vec3
    {
        public readonly double X;
        public readonly double Y;
        public readonly double Z;

        public Vec3(double x, double y, double z)
        {
            X = x;
            Y = y;
            Z = z;
        }
    }

    /// <summary>
    /// Quaternion for serialization. Serialized order is [x, y, z, w]
    /// (matches the demand-side samples, e.g. [0.0, 0.707, 0.0, 0.707]).
    /// </summary>
    public readonly struct Quat
    {
        public readonly double X;
        public readonly double Y;
        public readonly double Z;
        public readonly double W;

        public Quat(double x, double y, double z, double w)
        {
            X = x;
            Y = y;
            Z = z;
            W = w;
        }
    }

    /// <summary>Which action_camera json flavour a frame is serialized as.</summary>
    public enum ActionCameraVariant
    {
        /// <summary>docs/69 MC main camera (cam1): blue + white fields, snake_case mouse_x,
        /// "camera_Follow Offset" (space + capital, verbatim), euler *_rotation_oula.</summary>
        McCam1,

        /// <summary>docs/69 MC cam2-4: blue fields only.</summary>
        McCamN,

        /// <summary>docs/68 SFM 15fps: lowercase camera_follow_offset, camelCase mouseX/mouseY,
        /// depth_name present, frame steps by 2 (even frames).</summary>
        Sfm15,

        /// <summary>docs/68 SFM 30fps: same keys as Sfm15 minus depth_name, frame contiguous.</summary>
        Sfm30
    }

    /// <summary>
    /// One recorded frame, superset of all variants. Which fields are emitted (and under
    /// which exact key names) is decided by <see cref="ActionCameraJsonWriter"/> per
    /// <see cref="ActionCameraVariant"/>. Fields not applicable to a variant are ignored.
    /// </summary>
    public struct ActionCameraFrame
    {
        /// <summary>Wall-clock timestamp, "yyyy-MM-dd HH:mm:ss.fff" (matches samples).</summary>
        public string Time;

        /// <summary>Measured fps (realtime) or exact 30.0 (offline).</summary>
        public double Fps;

        /// <summary>Frame index (tick). SFM 15fps: even numbers stepping by 2.</summary>
        public int Frame;

        /// <summary>SFM 15fps only: "depth_{frame}" linking to the depth EXR.</summary>
        public string DepthName;

        public Vec3 CameraPosition;

        /// <summary>MC cam1 only ("camera_rotation_oula"). Unity Transform.eulerAngles
        /// (degrees, [x, y, z]); rotation order convention NOT confirmed by the demand
        /// side (docs/72 C3 pending) - recorded as-is with a provenance note.</summary>
        public Vec3 CameraRotationEuler;

        public Quat CameraRotationQuaternion;

        /// <summary>Follow offset. MC key is "camera_Follow Offset" (verbatim, with
        /// space + capital F); SFM key is "camera_follow_offset".</summary>
        public Vec3 CameraFollowOffset;

        public Intrinsics CameraIntrinsics;

        /// <summary>Positional velocity: (pos - prevPos) * 30, world units/s (Vector3).</summary>
        public Vec3 CameraSpeed;

        public Vec3 PlayerPosition;

        /// <summary>MC cam1 only ("player_rotation_oula"), same caveat as camera euler.</summary>
        public Vec3 PlayerRotationEuler;

        public Quat PlayerRotationQuaternion;
        public Vec3 PlayerSpeed;

        /// <summary>Cursor position normalized to [0,1] (MC keys mouse_x/mouse_y; SFM keys
        /// mouseX/mouseY).</summary>
        public double MouseX;
        public double MouseY;

        /// <summary>Mouse delta normalized to [-1,1]; denominator is the screen WIDTH for
        /// both axes for now (docs/71 interim decision) - raw pixel deltas are preserved
        /// in provenance, so this normalization is losslessly revisable.</summary>
        public double MouseDx;
        public double MouseDy;

        /// <summary>Held keys as Windows Virtual-Key codes (see VirtualKeys). May be empty,
        /// never null when serialized.</summary>
        public int[] KeyCodes;

        /// <summary>World-to-metric scale; 1.0 when scene units are metres.</summary>
        public double MetricScale;
    }
}
