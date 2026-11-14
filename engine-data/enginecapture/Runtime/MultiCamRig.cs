// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>Per-camera lane configuration + runtime state.</summary>
    [Serializable]
    public sealed class CameraLane
    {
        [Tooltip("Base Camera for this lane (URP: keep it a Base camera, no overlay stack).")]
        public Camera Camera;

        [Tooltip("Focal length in mm on a full-frame 36x24 back (plan enum: 18/24/35/50).")]
        public int FocalLengthMm = 24;

        [Tooltip("Aspect label per plan schema: 16:9 | 3:4 | 4:3 | 9:16 (sizes verbatim per docs/71).")]
        public string Aspect = "16:9";

        [Tooltip("Layer index reserved EXCLUSIVELY for this lane's helper objects. Other lanes cull it. Default lanes use layers 24-27.")]
        public int ExclusiveLayer = 24;

        [Tooltip("Video = colour lane encoded to H.264; Depth = 720p linear-eye-depth lane exported as EXR on even ticks (docs/68).")]
        public LaneKind Kind = LaneKind.Video;

        [Tooltip("Explicit pixel size; when width/height are 0 the size comes from Aspect (MC lanes).")]
        public int OverrideWidth;
        public int OverrideHeight;

        /// <summary>Allocated render target (docs/71 sizes). Null until Initialize().</summary>
        [NonSerialized] public RenderTexture TargetTexture;

        /// <summary>Vertical FOV in degrees derived from FocalLengthMm.</summary>
        [NonSerialized] public double VerticalFovDeg;

        /// <summary>Pixel size of the render target.</summary>
        [NonSerialized] public RtSize Size;
    }

    /// <summary>
    /// 4-camera capture rig (docs/71). Owns:
    ///   * one RenderTexture per lane, sized by aspect label
    ///     (16:9=1920x1080, 3:4=1440x1080, 4:3=1080x1440, 9:16=1080x1920 - verbatim);
    ///   * focal-length -> vertical FOV assignment (full-frame 36x24 mm back:
    ///     vFOV = 2*atan(12/f_mm), see CaptureFormats);
    ///   * lane layer isolation: each lane has an exclusive layer, and every lane's
    ///     cullingMask excludes the other lanes' exclusive layers (a lane's helper
    ///     geometry must never leak into another lane's video);
    ///   * initial-pose sanity: pairwise forward-axis angle must be <= 15 deg at
    ///     Initialize() (docs/71 rig geometry constraint), else initialization fails.
    /// Camera purity (no Renderer/Canvas/ParticleSystem under camera subtrees) is
    /// enforced separately by Editor/PurityCheck (CI red line).
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class MultiCamRig : CaptureRigBase
    {
        /// <summary>Number of lanes fixed by docs/69: 4.</summary>
        public const int LaneCount = 4;

        /// <summary>Max pairwise forward-angle between lanes at init (degrees, docs/71).</summary>
        public const float MaxInitialPairAngleDeg = 15f;

        [SerializeField] CameraLane[] _lanes = new CameraLane[LaneCount];

        /// <summary>Lanes in delivery order: index 0 == cam1 (main camera, white fields).</summary>
        public CameraLane[] Lanes => _lanes;

        /// <inheritdoc/>
        public override int Count => LaneCount;

        /// <inheritdoc/>
        public override CameraLane GetLane(int index) => _lanes[index];

        /// <summary>
        /// Validate config, allocate RTs, assign FOVs and culling masks.
        /// Throws InvalidOperationException on any rig-geometry violation - a broken rig
        /// must fail loudly before a single frame is recorded.
        /// </summary>
        public override void Initialize()
        {
            if (IsInitialized)
                throw new InvalidOperationException("MultiCamRig: already initialized");
            if (_lanes == null || _lanes.Length != LaneCount)
                throw new InvalidOperationException("MultiCamRig: exactly " + LaneCount + " lanes required");

            for (int i = 0; i < LaneCount; i++)
            {
                var lane = _lanes[i];
                if (lane == null || lane.Camera == null)
                    throw new InvalidOperationException("MultiCamRig: lane " + (i + 1) + " has no camera");
                if (lane.ExclusiveLayer < 0 || lane.ExclusiveLayer > 31)
                    throw new InvalidOperationException("MultiCamRig: lane " + (i + 1) + " layer out of range");
                for (int j = 0; j < i; j++)
                    if (_lanes[j].ExclusiveLayer == lane.ExclusiveLayer)
                        throw new InvalidOperationException(
                            "MultiCamRig: lanes " + (j + 1) + " and " + (i + 1) + " share layer " + lane.ExclusiveLayer);
            }

            AssertInitialPoseAngles();

            int allExclusiveMask = 0;
            for (int i = 0; i < LaneCount; i++)
                allExclusiveMask |= 1 << _lanes[i].ExclusiveLayer;

            for (int i = 0; i < LaneCount; i++)
            {
                var lane = _lanes[i];
                var size = CaptureFormats.RtSizes[CaptureFormats.ParseAspect(lane.Aspect)];
                lane.Size = size;
                // 24-bit depth, sRGB per project color space; format left default so URP
                // picks its preferred color format.
                // Metal: 默认 B8G8R8A8_SRGB 不支持 AsyncGPUReadback 的 RGBA32 转换读取
                // （S0 实测 "doesn't support ReadPixels usage"）。显式锁 R8G8B8A8_SRGB，
                // 与 ReadbackRing 的 RGBA32 请求端到端一致，全平台可回读。
                var desc = new RenderTextureDescriptor(size.Width, size.Height,
                    UnityEngine.Experimental.Rendering.GraphicsFormat.R8G8B8A8_SRGB, 24);
                lane.TargetTexture = new RenderTexture(desc)
                {
                    name = "EngineCapture_cam" + (i + 1),
                    antiAliasing = 1
                };
                lane.TargetTexture.Create();
                lane.Camera.targetTexture = lane.TargetTexture;

                lane.VerticalFovDeg = CaptureFormats.VerticalFovDegrees(lane.FocalLengthMm);
                // Unity Camera.fieldOfView is the VERTICAL fov in degrees; the RT's own
                // width/height fixes the aspect, so vertical fov fully determines intrinsics.
                lane.Camera.fieldOfView = (float)lane.VerticalFovDeg;

                // Own exclusive layer visible, other lanes' exclusive layers culled.
                int mask = lane.Camera.cullingMask;
                mask &= ~allExclusiveMask;
                mask |= 1 << lane.ExclusiveLayer;
                lane.Camera.cullingMask = mask;
            }

            IsInitialized = true;
        }

        /// <summary>Release RTs and detach cameras.</summary>
        public override void Shutdown()
        {
            if (!IsInitialized) return;
            for (int i = 0; i < LaneCount; i++)
            {
                var lane = _lanes[i];
                if (lane?.Camera != null) lane.Camera.targetTexture = null;
                if (lane?.TargetTexture != null)
                {
                    lane.TargetTexture.Release();
                    UnityEngine.Object.Destroy(lane.TargetTexture);
                    lane.TargetTexture = null;
                }
            }
            IsInitialized = false;
        }

        void OnDestroy() => Shutdown();

        /// <summary>Lanes exempt from the pairwise initial-angle gate. Set to {4} for the
        /// director mode's neutral overview camera: by design (docs/78 §10) cam4 stands off
        /// the carrier orbit and looks at the whole flight path, so its initial forward
        /// direction necessarily differs from the carrier-mounted lanes. The three carrier
        /// lanes are still checked against each other.</summary>
        public int[] InitialAngleExemptLanes { get; set; } = new int[0];

        bool ExemptLane(int laneIndex1Based)
        {
            foreach (var l in InitialAngleExemptLanes)
                if (l == laneIndex1Based) return true;
            return false;
        }

        void AssertInitialPoseAngles()
        {
            for (int i = 0; i < LaneCount; i++)
            {
                if (ExemptLane(i + 1)) continue;
                for (int j = i + 1; j < LaneCount; j++)
                {
                    if (ExemptLane(j + 1)) continue;
                    float angle = Vector3.Angle(
                        _lanes[i].Camera.transform.forward,
                        _lanes[j].Camera.transform.forward);
                    if (angle > MaxInitialPairAngleDeg)
                        throw new InvalidOperationException(
                            "MultiCamRig: initial forward angle between lane " + (i + 1) +
                            " and lane " + (j + 1) + " is " + angle.ToString("F2") +
                            " deg (max " + MaxInitialPairAngleDeg + ")");
                }
            }
        }
    }
}
