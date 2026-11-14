// Pure C# (no UnityEngine dependency) so the math can be unit-tested outside Unity.
// Spec source: docs/70 (engine-data line overall plan) + docs/71 (multi-camera detail).

using System;
using System.Collections.Generic;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Aspect-ratio labels used by the plan format (engine-data/schema/plan.schema.json).
    /// The label strings and their pixel sizes follow the demand-side wording verbatim
    /// (docs/71): "16:9"=1920x1080, "3:4"=1440x1080, "4:3"=1080x1440, "9:16"=1080x1920.
    /// NOTE: the "3:4"/"4:3" labels are intentionally kept exactly as decreed even though
    /// they look swapped relative to the common width:height reading - do NOT "fix" them.
    /// </summary>
    public enum AspectLabel
    {
        A16x9,
        A3x4,
        A4x3,
        A9x16
    }

    /// <summary>Immutable render-target size in pixels.</summary>
    public readonly struct RtSize
    {
        public readonly int Width;
        public readonly int Height;

        public RtSize(int width, int height)
        {
            Width = width;
            Height = height;
        }

        public override string ToString() => Width + "x" + Height;
    }

    /// <summary>
    /// Focal-length -> field-of-view and intrinsics math for the capture line.
    ///
    /// Conversion basis (docs/71): focal lengths are interpreted against a full-frame
    /// 36x24 mm film back. Vertical FOV therefore is
    ///     vFOV = 2 * atan((24/2) / f_mm) = 2 * atan(12 / f_mm)
    /// i.e. half the film-back HEIGHT (12 mm) over the focal length. Unity's
    /// Camera.fieldOfView is the vertical FOV in degrees, so this value is assigned
    /// directly regardless of the render-target aspect ratio.
    ///
    /// Intrinsics (docs/70; matches the demand-side samples, e.g. 1920x1080 @ vFOV
    /// 53.13 deg -> fx=fy=1080, cx=960, cy=540):
    ///     fy = (H/2) / tan(vFOV/2),  fx = fy  (square pixels),
    ///     cx = W/2,  cy = H/2.
    /// </summary>
    public static class CaptureFormats
    {
        /// <summary>Authoritative capture frame rate (docs/70): 30 fps CFR.</summary>
        public const int FrameRate = 30;

        /// <summary>Frame interval in seconds (1/30).</summary>
        public const double FrameIntervalSeconds = 1.0 / FrameRate;

        /// <summary>Full-frame film-back half height in millimetres (24 mm / 2).</summary>
        public const double FilmBackHalfHeightMm = 12.0;

        /// <summary>Focal lengths allowed by the plan schema (mm).</summary>
        public static readonly int[] AllowedFocalLengthsMm = { 18, 24, 35, 50 };

        /// <summary>RT sizes per aspect label, verbatim from docs/71 (see AspectLabel).</summary>
        public static readonly IReadOnlyDictionary<AspectLabel, RtSize> RtSizes =
            new Dictionary<AspectLabel, RtSize>
            {
                { AspectLabel.A16x9, new RtSize(1920, 1080) },
                { AspectLabel.A3x4,  new RtSize(1440, 1080) },
                { AspectLabel.A4x3,  new RtSize(1080, 1440) },
                { AspectLabel.A9x16, new RtSize(1080, 1920) }
            };

        /// <summary>Parse a plan aspect string ("16:9" | "3:4" | "4:3" | "9:16").</summary>
        public static AspectLabel ParseAspect(string label)
        {
            switch (label)
            {
                case "16:9": return AspectLabel.A16x9;
                case "3:4": return AspectLabel.A3x4;
                case "4:3": return AspectLabel.A4x3;
                case "9:16": return AspectLabel.A9x16;
                default:
                    throw new ArgumentException("Unknown aspect label: " + label, nameof(label));
            }
        }

        /// <summary>
        /// Vertical FOV in degrees for a focal length in mm on a full-frame 36x24 mm back:
        /// vFOV = 2 * atan(12 / f_mm).
        /// </summary>
        public static double VerticalFovDegrees(double focalLengthMm)
        {
            if (focalLengthMm <= 0.0)
                throw new ArgumentOutOfRangeException(nameof(focalLengthMm));
            return 2.0 * Math.Atan(FilmBackHalfHeightMm / focalLengthMm) * (180.0 / Math.PI);
        }

        /// <summary>
        /// Pinhole intrinsics from RT size + vertical FOV (degrees):
        /// fx = fy = (H/2)/tan(vFOV/2), cx = W/2, cy = H/2.
        /// </summary>
        public static Intrinsics IntrinsicsFor(int width, int height, double verticalFovDegrees)
        {
            if (width <= 0 || height <= 0)
                throw new ArgumentOutOfRangeException(nameof(width), "RT size must be positive");
            double halfFovRad = verticalFovDegrees * 0.5 * (Math.PI / 180.0);
            double fy = (height * 0.5) / Math.Tan(halfFovRad);
            return new Intrinsics(fy, fy, width * 0.5, height * 0.5);
        }
    }

    /// <summary>Pinhole intrinsics (pixels). Serialized as {"fx","fy","cx","cy"}.</summary>
    public readonly struct Intrinsics
    {
        public readonly double Fx;
        public readonly double Fy;
        public readonly double Cx;
        public readonly double Cy;

        public Intrinsics(double fx, double fy, double cx, double cy)
        {
            Fx = fx;
            Fy = fy;
            Cx = cx;
            Cy = cy;
        }
    }
}
