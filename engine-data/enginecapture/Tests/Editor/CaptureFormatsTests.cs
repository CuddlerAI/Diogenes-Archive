// EditMode tests (Unity Test Runner). NOT yet executed - this repo has no Unity
// toolchain; run via Window > General > Test Runner (EditMode) after import
// (remember to add this package to the project manifest "testables" list).

using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    public class CaptureFormatsTests
    {
        // Focal length -> vertical FOV, full-frame 36x24 back: vFOV = 2*atan(12/f).
        // Expected values computed independently (python, 6 decimals).
        [TestCase(18, 67.380135)]
        [TestCase(24, 53.130102)]
        [TestCase(35, 37.849289)]
        [TestCase(50, 26.991467)]
        public void VerticalFov_MatchesFullFrameFormula(int focalMm, double expectedDeg)
        {
            Assert.That(CaptureFormats.VerticalFovDegrees(focalMm),
                Is.EqualTo(expectedDeg).Within(1e-5));
        }

        [Test]
        public void Intrinsics_MatchDemandSideSample_1920x1080()
        {
            // Demand-side cam1 sample: 1920x1080 -> fx=fy=1080, cx=960, cy=540
            // at vFOV = 53.130102 deg (24 mm).
            double vfov = CaptureFormats.VerticalFovDegrees(24);
            Intrinsics k = CaptureFormats.IntrinsicsFor(1920, 1080, vfov);
            Assert.That(k.Fx, Is.EqualTo(1080.0).Within(1e-6));
            Assert.That(k.Fy, Is.EqualTo(1080.0).Within(1e-6));
            Assert.That(k.Cx, Is.EqualTo(960.0).Within(0.0));
            Assert.That(k.Cy, Is.EqualTo(540.0).Within(0.0));
        }

        [Test]
        public void Intrinsics_FxEqualsFy_ForAllAspects()
        {
            foreach (var kv in CaptureFormats.RtSizes)
            {
                double vfov = CaptureFormats.VerticalFovDegrees(35);
                Intrinsics k = CaptureFormats.IntrinsicsFor(kv.Value.Width, kv.Value.Height, vfov);
                Assert.That(k.Fx, Is.EqualTo(k.Fy), "fx==fy (square pixels) for " + kv.Key);
                Assert.That(k.Cx, Is.EqualTo(kv.Value.Width * 0.5));
                Assert.That(k.Cy, Is.EqualTo(kv.Value.Height * 0.5));
            }
        }

        // RT sizes verbatim per docs/71 (labels intentionally as decreed - do not "fix").
        [TestCase("16:9", 1920, 1080)]
        [TestCase("3:4", 1440, 1080)]
        [TestCase("4:3", 1080, 1440)]
        [TestCase("9:16", 1080, 1920)]
        public void RtSizes_MatchDocs71(string label, int width, int height)
        {
            RtSize size = CaptureFormats.RtSizes[CaptureFormats.ParseAspect(label)];
            Assert.That(size.Width, Is.EqualTo(width));
            Assert.That(size.Height, Is.EqualTo(height));
        }

        [Test]
        public void ParseAspect_RejectsUnknownLabel()
        {
            Assert.Throws<System.ArgumentException>(() => CaptureFormats.ParseAspect("21:9"));
        }
    }

    public class DepthEncodingTests
    {
        [TestCase(1.239, 1.23)]
        [TestCase(0.999, 0.99)]
        [TestCase(12.3456, 12.34)]
        [TestCase(0.0, 0.0)]
        public void QuantizeMeters_TruncatesToCentimetres(double input, double expected)
        {
            Assert.That(DepthEncoding.QuantizeMeters(input), Is.EqualTo(expected).Within(1e-9));
        }

        [Test]
        public void QuantizeMeters_NaNPassesThrough()
        {
            Assert.That(double.IsNaN(DepthEncoding.QuantizeMeters(double.NaN)), Is.True);
        }

        [Test]
        public void ExportFrames_AreEvenOnly()
        {
            Assert.That(DepthEncoding.IsExportFrame(0), Is.True);
            Assert.That(DepthEncoding.IsExportFrame(1), Is.False);
            Assert.That(DepthEncoding.IsExportFrame(2), Is.True);
            Assert.That(DepthEncoding.DepthName(4), Is.EqualTo("depth_4"));
        }
    }
}
