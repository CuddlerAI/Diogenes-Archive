// docs/78 G'4: per-tick dynamic-object visibility metering (docs/71 §9 "动态物体
// ≥80% 时长" probe). The SDK side is engine-agnostic: a game-side IDynObjectSource
// feeds world-space bounding spheres of the scene's dynamic objects (vehicles,
// blimps, ...) each tick; this component does per-lane frustum counting plus an
// approximate pixel-share estimate (bounding-sphere projection), and writes
// dyn_metrics.json at clip end. QC's dynamic-object gate reads that sidecar.

using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>World-space bounding sphere of one dynamic object.</summary>
    public struct DynSphere
    {
        public Vector3 Center;
        public float Radius;

        public DynSphere(Vector3 center, float radius)
        {
            Center = center;
            Radius = radius;
        }
    }

    /// <summary>Game-side supplier of dynamic-object bounds, polled once per tick.
    /// Implementations append to <paramref name="buffer"/> (cleared by the caller).</summary>
    public interface IDynObjectSource
    {
        void CollectSpheres(List<DynSphere> buffer);
    }

    /// <summary>
    /// Per-lane, per-tick dynamic object metrics:
    ///   count       = spheres intersecting the lane frustum;
    ///   pixel_share = Σ min(1, π·r_px²/(W·H)) over visible spheres, clamped to 1,
    ///                 with r_px = fy · r / max(z_view, r) (bounding-sphere projection).
    /// A frame is "active" when count > 0. Summary reports per-lane active fraction
    /// against <see cref="ActiveFracThreshold"/> (docs/69: ≥80% of clip duration).
    /// </summary>
    [DisallowMultipleComponent]
    public sealed class DynObjectProbe : MonoBehaviour
    {
        [SerializeField] FrameClock _clock;
        [SerializeField] MultiCamRig _rig;

        [Tooltip("Active-frame fraction each lane must reach (docs/69: 0.8).")]
        [SerializeField] double _activeFracThreshold = 0.8;

        [Tooltip("Objects farther than this from the camera do not count (0 = unlimited). Keeps sub-pixel skyline traffic from trivially satisfying the gate.")]
        [SerializeField] float _maxDistance = 0f;

        public FrameClock Clock { get => _clock; set => _clock = value; }
        public MultiCamRig Rig { get => _rig; set => _rig = value; }
        public IDynObjectSource Source { get; set; }
        public double ActiveFracThreshold { get => _activeFracThreshold; set => _activeFracThreshold = value; }
        public float MaxDistance { get => _maxDistance; set => _maxDistance = value; }

        sealed class LaneSeries
        {
            public readonly List<int> Counts = new List<int>(2048);
            public readonly List<double> PixelShares = new List<double>(2048);
        }

        readonly LaneSeries[] _series = new LaneSeries[MultiCamRig.LaneCount];
        readonly List<DynSphere> _buffer = new List<DynSphere>(4096);
        readonly Plane[] _planes = new Plane[6];
        bool _recording;

        public void BeginClip()
        {
            for (int i = 0; i < MultiCamRig.LaneCount; i++)
                _series[i] = new LaneSeries();
            _recording = true;
        }

        public void EndClip() => _recording = false;

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
            if (!_recording || Source == null || _rig == null || !_rig.IsInitialized) return;

            _buffer.Clear();
            Source.CollectSpheres(_buffer);

            for (int lane = 0; lane < MultiCamRig.LaneCount; lane++)
            {
                var cam = _rig.Lanes[lane].Camera;
                var size = _rig.Lanes[lane].Size;
                GeometryUtility.CalculateFrustumPlanes(cam, _planes);
                double fy = (size.Height * 0.5) / Math.Tan(cam.fieldOfView * 0.5 * Mathf.Deg2Rad);
                Vector3 camPos = cam.transform.position;
                Vector3 camFwd = cam.transform.forward;

                int count = 0;
                double share = 0.0;
                for (int i = 0; i < _buffer.Count; i++)
                {
                    var s = _buffer[i];
                    if (_maxDistance > 0f
                        && (s.Center - camPos).sqrMagnitude > _maxDistance * _maxDistance)
                        continue;
                    if (!SphereInFrustum(_planes, s.Center, s.Radius)) continue;
                    count++;
                    float zView = Vector3.Dot(s.Center - camPos, camFwd);
                    double rPx = fy * s.Radius / Math.Max(zView, s.Radius);
                    double a = Math.PI * rPx * rPx / ((double)size.Width * size.Height);
                    share += Math.Min(1.0, a);
                }
                _series[lane].Counts.Add(count);
                _series[lane].PixelShares.Add(Math.Min(1.0, share));
            }
        }

        static bool SphereInFrustum(Plane[] planes, Vector3 center, float radius)
        {
            for (int i = 0; i < 6; i++)
                if (planes[i].GetDistanceToPoint(center) < -radius)
                    return false;
            return true;
        }

        /// <summary>Write dyn_metrics.json. Returns the summary's overall pass flag.</summary>
        public bool WriteJson(string path)
        {
            var sb = new StringBuilder(1 << 20);
            sb.Append("{\n");
            sb.Append("  \"fps\": ").Append(JsonPrimitives.Number(CaptureFormats.FrameRate)).Append(",\n");
            sb.Append("  \"active_frac_threshold\": ")
              .Append(JsonPrimitives.Number(_activeFracThreshold)).Append(",\n");
            sb.Append("  \"max_distance_m\": ")
              .Append(JsonPrimitives.Number(_maxDistance)).Append(",\n");
            sb.Append("  \"lanes\": [\n");

            double minActive = 1.0;
            for (int lane = 0; lane < MultiCamRig.LaneCount; lane++)
            {
                var s = _series[lane] ?? new LaneSeries();
                int n = s.Counts.Count;
                int active = 0;
                double meanCount = 0, meanShare = 0;
                for (int i = 0; i < n; i++)
                {
                    if (s.Counts[i] > 0) active++;
                    meanCount += s.Counts[i];
                    meanShare += s.PixelShares[i];
                }
                double activeFrac = n > 0 ? (double)active / n : 0.0;
                minActive = Math.Min(minActive, activeFrac);
                meanCount = n > 0 ? meanCount / n : 0.0;
                meanShare = n > 0 ? meanShare / n : 0.0;

                sb.Append("    {\n");
                sb.Append("      \"cam\": ").Append(JsonPrimitives.Number(lane + 1)).Append(",\n");
                sb.Append("      \"frames\": ").Append(JsonPrimitives.Number(n)).Append(",\n");
                sb.Append("      \"active_frames\": ").Append(JsonPrimitives.Number(active)).Append(",\n");
                sb.Append("      \"active_frac\": ").Append(JsonPrimitives.Number(Math.Round(activeFrac, 4))).Append(",\n");
                sb.Append("      \"mean_count\": ").Append(JsonPrimitives.Number(Math.Round(meanCount, 2))).Append(",\n");
                sb.Append("      \"mean_pixel_share\": ").Append(JsonPrimitives.Number(Math.Round(meanShare, 4))).Append(",\n");
                sb.Append("      \"count_series\": [");
                for (int i = 0; i < n; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(JsonPrimitives.Number(s.Counts[i]));
                }
                sb.Append("],\n");
                sb.Append("      \"pixel_share_series\": [");
                for (int i = 0; i < n; i++)
                {
                    if (i > 0) sb.Append(',');
                    sb.Append(JsonPrimitives.Number(Math.Round(s.PixelShares[i], 4)));
                }
                sb.Append("]\n");
                sb.Append(lane == MultiCamRig.LaneCount - 1 ? "    }\n" : "    },\n");
            }

            bool pass = minActive >= _activeFracThreshold;
            sb.Append("  ],\n");
            sb.Append("  \"summary\": {\n");
            sb.Append("    \"min_active_frac\": ").Append(JsonPrimitives.Number(Math.Round(minActive, 4))).Append(",\n");
            sb.Append("    \"pass\": ").Append(pass ? "true" : "false").Append("\n");
            sb.Append("  }\n");
            sb.Append("}\n");

            File.WriteAllText(path, sb.ToString(), new UTF8Encoding(false));
            return pass;
        }
    }
}
