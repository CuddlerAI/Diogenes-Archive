// NOT yet compiled in a Unity editor when written - APIs target Unity 2022.3 LTS.
//
// Why an abstract MonoBehaviour rather than an interface: Unity cannot serialize an
// interface-typed field, and the recorders need their rig assignable in the inspector.
// A shared base keeps ONE implementation of the tick/readback machinery for both the
// 4-lane MC rig (docs/69) and the 2-lane SFM rig (docs/68) - docs/83 debate 16 ruled
// against a parallel single-lane path precisely because two implementations drift
// (the #431 finite-difference split and the docs/71 R10 lesson).

using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>What a lane is for; decides readback format and cadence.</summary>
    public enum LaneKind
    {
        /// <summary>Colour lane: RGBA32 readback every tick, encoded to H.264.</summary>
        Video,

        /// <summary>Depth lane: RFloat readback on EVEN ticks only (docs/68 15 fps),
        /// quantised to cm and written as single-channel half EXR.</summary>
        Depth
    }

    /// <summary>
    /// Common surface every capture rig exposes to the shared recorders
    /// (PosePlayback / PoseRecorder / ReadbackRing).
    /// </summary>
    public abstract class CaptureRigBase : MonoBehaviour
    {
        /// <summary>Number of lanes this rig owns.</summary>
        public abstract int Count { get; }

        /// <summary>Lane by 0-based index, in delivery order.</summary>
        public abstract CameraLane GetLane(int index);

        /// <summary>True once render targets are allocated and geometry validated.</summary>
        public bool IsInitialized { get; protected set; }

        /// <summary>Allocate render targets and validate rig geometry. Throws on violation.</summary>
        public abstract void Initialize();

        /// <summary>Release render targets and detach cameras.</summary>
        public abstract void Shutdown();

        /// <summary>Main lane (index 0): carries player_* / mouse / key fields.</summary>
        public CameraLane MainLane => Count > 0 ? GetLane(0) : null;

        /// <summary>Intrinsics for a lane (fx=fy=(H/2)/tan(vFOV/2), cx=W/2, cy=H/2).</summary>
        public Intrinsics IntrinsicsForLane(int index)
        {
            var lane = GetLane(index);
            return CaptureFormats.IntrinsicsFor(lane.Size.Width, lane.Size.Height, lane.VerticalFovDeg);
        }

        /// <summary>Lanes carrying pose/action json (i.e. not depth mirrors).
        /// CONTRACT: video lanes occupy indices [0, PoseLaneCount) — the recorders index
        /// tracks by lane, so a depth lane must never sit before a video lane.</summary>
        public virtual int PoseLaneCount
        {
            get
            {
                int n = 0;
                for (int i = 0; i < Count; i++)
                {
                    var lane = GetLane(i);
                    if (lane == null) continue;   // Initialize() 之前 lane 尚未建立
                    if (lane.Kind == LaneKind.Video) n++;
                }
                return n;
            }
        }
    }
}
