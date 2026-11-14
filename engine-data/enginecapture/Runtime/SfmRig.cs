// SFM-Depth rig (docs/68): one 1080p colour lane + one 720p depth lane sharing a
// single transform, so depth and video are the SAME camera at two resolutions.
//
// Why a dedicated depth camera instead of downsampling the colour camera's depth
// texture (docs/83 debate 14): 1920x1080 -> 1280x720 is 1.5x, a NON-integer ratio.
// Any resampling kernel has to be chosen and justified - bilinear across a depth
// discontinuity manufactures "flying pixels", which is exactly the ghosting badcase
// docs/68 §7.2 names. With a native 720p pass at the same vFOV, the depth pixel at
// normalised (u,v) and the colour pixel at (u,v) are the SAME ray by construction:
// no kernel, no calibration, nothing to explain to the demand side.

using System;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Two-lane rig for docs/68 delivery: lane 0 = video (1920x1080), lane 1 = depth
    /// (1280x720, RFloat linear eye depth in metres).
    ///
    /// Both lanes carry the same pose every tick - PosePlayback drives lane 0 and the
    /// depth camera is parented to it, so "depth and pose came from the same tick" is a
    /// structural property rather than something QC has to verify after the fact.
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class SfmRig : CaptureRigBase
    {
        public const int VideoWidth = 1920;
        public const int VideoHeight = 1080;
        public const int DepthWidth = DepthExporter.DepthWidth;    // 1280
        public const int DepthHeight = DepthExporter.DepthHeight;  // 720

        [SerializeField] Camera _videoCamera;
        [SerializeField] Camera _depthCamera;

        [Tooltip("Vertical FOV in degrees applied to BOTH lanes. Equal vFOV + equal aspect is what makes the depth intrinsics a pure resolution scale of the video intrinsics (docs/75 Q10).")]
        [SerializeField] float _verticalFovDeg = 58.72f;   // matches the demand-side 15fps sample (hFOV 90 deg)

        readonly CameraLane[] _lanes = new CameraLane[2];
        RenderTexture _depthColorScratch;   // 深度相机的颜色去处，见 Initialize

        public override int Count => 2;
        public override CameraLane GetLane(int index) => _lanes[index];

        public Camera VideoCamera { get => _videoCamera; set => _videoCamera = value; }
        public Camera DepthCamera { get => _depthCamera; set => _depthCamera = value; }
        public float VerticalFovDeg { get => _verticalFovDeg; set => _verticalFovDeg = value; }

        /// <summary>Depth lane (index 1).</summary>
        public CameraLane DepthLane => _lanes[1];

        public override void Initialize()
        {
            if (IsInitialized) throw new InvalidOperationException("SfmRig: already initialized");
            if (_videoCamera == null) throw new InvalidOperationException("SfmRig: video camera not set");
            if (_depthCamera == null) throw new InvalidOperationException("SfmRig: depth camera not set");

            // Aspect must match, otherwise the depth intrinsics are NOT a pure scale of
            // the video intrinsics and docs/75 Q10's fixed convention silently breaks.
            double va = VideoWidth / (double)VideoHeight;
            double da = DepthWidth / (double)DepthHeight;
            if (Math.Abs(va - da) > 1e-9)
                throw new InvalidOperationException(
                    "SfmRig: video aspect " + va + " != depth aspect " + da +
                    " - depth intrinsics would no longer be a pure resolution scale (docs/75 Q10)");

            _lanes[0] = MakeLane(_videoCamera, LaneKind.Video, VideoWidth, VideoHeight,
                                 UnityEngine.Experimental.Rendering.GraphicsFormat.R8G8B8A8_SRGB, 24);
            _lanes[1] = MakeLane(_depthCamera, LaneKind.Depth, DepthWidth, DepthHeight,
                                 UnityEngine.Experimental.Rendering.GraphicsFormat.R32_SFloat, 24);

            // 深度路的 TargetTexture 是**线性深度**的落点（DepthPassBlitter 写、
            // ReadbackRing 读），不能同时当深度相机的颜色目标——URP 会在帧末把相机颜色
            // blit 上去，把深度盖掉（D0 实测：读回 0.053 m，低于 near 根本不可能是深度）。
            // 所以给深度相机另开一张丢弃用的颜色 RT。
            _depthColorScratch = new RenderTexture(new RenderTextureDescriptor(
                DepthWidth, DepthHeight,
                UnityEngine.Experimental.Rendering.GraphicsFormat.R8G8B8A8_SRGB, 24))
            {
                name = "EngineCapture_depth_color_scratch",
                antiAliasing = 1
            };
            _depthColorScratch.Create();
            _depthCamera.targetTexture = _depthColorScratch;

            // The depth camera rides the video camera: one transform, no drift.
            _depthCamera.transform.SetParent(_videoCamera.transform, false);
            _depthCamera.transform.localPosition = Vector3.zero;
            _depthCamera.transform.localRotation = Quaternion.identity;
            _depthCamera.nearClipPlane = _videoCamera.nearClipPlane;
            _depthCamera.farClipPlane = _videoCamera.farClipPlane;

            IsInitialized = true;
        }

        CameraLane MakeLane(Camera cam, LaneKind kind, int w, int h,
                            UnityEngine.Experimental.Rendering.GraphicsFormat fmt, int depthBits)
        {
            var lane = new CameraLane
            {
                Camera = cam,
                Kind = kind,
                Aspect = "16:9",
                OverrideWidth = w,
                OverrideHeight = h,
                Size = new RtSize(w, h),
                VerticalFovDeg = _verticalFovDeg,
            };
            var desc = new RenderTextureDescriptor(w, h, fmt, depthBits);
            lane.TargetTexture = new RenderTexture(desc)
            {
                name = "EngineCapture_" + kind.ToString().ToLowerInvariant(),
                antiAliasing = 1
            };
            lane.TargetTexture.Create();
            // 深度路的相机目标在 Initialize 里另设为 scratch；视频路直接用本路 RT
            if (kind == LaneKind.Video) cam.targetTexture = lane.TargetTexture;
            cam.fieldOfView = _verticalFovDeg;
            return lane;
        }

        public override void Shutdown()
        {
            if (!IsInitialized) return;
            for (int i = 0; i < _lanes.Length; i++)
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
            if (_depthColorScratch != null)
            {
                _depthColorScratch.Release();
                UnityEngine.Object.Destroy(_depthColorScratch);
                _depthColorScratch = null;
            }
            IsInitialized = false;
        }

        /// <summary>Depth lane renders on EVEN ticks only; toggled from the clock tick,
        /// which runs in Update and therefore lands before the frame is rendered.</summary>
        public void SetDepthEnabledForTick(int tick)
        {
            if (_depthCamera != null)
                _depthCamera.enabled = DepthEncoding.IsExportFrame(tick);
        }

        void OnDestroy() => Shutdown();
    }
}
