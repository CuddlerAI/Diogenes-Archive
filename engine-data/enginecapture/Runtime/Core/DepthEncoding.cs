// Pure C# (no UnityEngine dependency) so the quantization rule can be unit-tested
// outside Unity. Spec source: docs/68 (SFM-Depth delivery).

using System;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Depth value encoding rules (docs/68):
    ///   * linear eye depth in METRES (LinearEyeDepth output, metric_scale applied);
    ///   * quantized to centimetre precision by TRUNCATION: trunc(d * 100) / 100
    ///     (floor for the positive depths we emit - not rounding, per spec);
    ///   * stored as IEEE half in a single-channel Y EXR;
    ///   * invalid pixels (sky / no hit / out of range) become NaN.
    /// </summary>
    public static class DepthEncoding
    {
        /// <summary>Sentinel for invalid depth (serialized as half NaN in the EXR).</summary>
        public static readonly float Invalid = float.NaN;

        /// <summary>trunc(d*100)/100 - centimetre truncation, NaN passes through.</summary>
        public static double QuantizeMeters(double depthMeters)
        {
            if (double.IsNaN(depthMeters)) return double.NaN;
            if (depthMeters < 0.0) throw new ArgumentOutOfRangeException(nameof(depthMeters));
            return Math.Truncate(depthMeters * 100.0) / 100.0;
        }

        /// <summary>SFM 15 fps exports EVEN frames only (frame steps by 2, docs/68).</summary>
        public static bool IsExportFrame(int frame) => (frame & 1) == 0;

        /// <summary>Depth file base name for a frame: "depth_{frame}".</summary>
        public static string DepthName(int frame) =>
            "depth_" + frame.ToString(System.Globalization.CultureInfo.InvariantCulture);
    }
}
