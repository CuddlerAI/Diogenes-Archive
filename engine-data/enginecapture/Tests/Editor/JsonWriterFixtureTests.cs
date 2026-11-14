// EditMode tests (Unity Test Runner). NOT yet executed - this repo has no Unity
// toolchain; run via Window > General > Test Runner (EditMode) after import.
//
// Fixture alignment: these tests locate engine-data/fixtures RELATIVE to the package
// (works when the package lives in the Diogenes repo and is added to a project from
// disk / as embedded). If the fixtures folder is absent (package consumed standalone),
// fixture tests Assert.Ignore instead of silently passing.

using System.Collections.Generic;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    public class JsonWriterFixtureTests
    {
        static readonly Regex KeyRegex = new Regex("\"([^\"]+)\"\\s*:", RegexOptions.Compiled);

        static string FixturesDir
        {
            get
            {
                // Resolve the package's on-disk folder (works for file:/embedded
                // packages); in this repo that is engine-data/enginecapture, so the
                // fixtures live one level up.
                var info = UnityEditor.PackageManager.PackageInfo.FindForAssembly(
                    typeof(CaptureSdk).Assembly);
                string pkg = info != null
                    ? info.resolvedPath
                    : Path.GetFullPath("Packages/" + CaptureSdk.PackageName);
                return Path.Combine(pkg, "..", "fixtures");
            }
        }

        static string LoadFixture(string name)
        {
            string path = Path.Combine(FixturesDir, name);
            if (!File.Exists(path))
                Assert.Ignore("Fixture not reachable from package location: " + path);
            return File.ReadAllText(path);
        }

        static List<string> ExtractKeys(string json)
        {
            var keys = new List<string>();
            foreach (Match m in KeyRegex.Matches(json))
                keys.Add(m.Groups[1].Value);
            return keys;
        }

        static ActionCameraFrame SampleFrame()
        {
            return new ActionCameraFrame
            {
                Time = "2026-03-20 16:25:29.860",
                Fps = 29.99895003674871,
                Frame = 0,
                CameraPosition = new Vec3(-28.5, 1.691, -7.149),
                CameraRotationEuler = new Vec3(0.0, 76.0219, 0.0),
                CameraRotationQuaternion = new Quat(0.0, 0.707, 0.0, 0.707),
                CameraFollowOffset = new Vec3(0.0, 2.5, -5.0),
                CameraIntrinsics = new Intrinsics(1080.0, 1080.0, 960.0, 540.0),
                CameraSpeed = new Vec3(1.25, 0.0, -3.5),
                PlayerPosition = new Vec3(-28.5, 0.251, -7.149),
                PlayerRotationEuler = new Vec3(0.0, 76.0219, 0.0),
                PlayerRotationQuaternion = new Quat(0.0, 0.707, 0.0, 0.707),
                PlayerSpeed = new Vec3(0.707, 0.0, 0.707),
                MouseX = 0.5,
                MouseY = 0.5,
                MouseDx = 0.026,
                MouseDy = 0.01,
                KeyCodes = new[] { 87 },
                MetricScale = 1.0,
                DepthName = "depth_0"
            };
        }

        static string Serialize(ActionCameraVariant variant)
        {
            var sb = new StringBuilder();
            using (var sw = new StringWriter(sb))
            {
                var writer = new ActionCameraJsonWriter(sw, variant);
                var frame = SampleFrame();
                writer.WriteFrame(in frame);
                writer.Finish();
            }
            return sb.ToString();
        }

        // ---- key order vs demand-side fixtures (names byte-verbatim, order normative) ----

        [TestCase(ActionCameraVariant.McCam1, "mc_cam1_action_camera.sample.json")]
        [TestCase(ActionCameraVariant.McCamN, "mc_camN_action_camera.sample.json")]
        [TestCase(ActionCameraVariant.Sfm15, "sfm_action_camera_15fps.sample.json")]
        public void KeyOrder_MatchesFixture(ActionCameraVariant variant, string fixtureName)
        {
            List<string> ours = ExtractKeys(Serialize(variant));
            List<string> fixtureKeys = ExtractKeys(LoadFixture(fixtureName));

            Assert.That(fixtureKeys.Count, Is.GreaterThanOrEqualTo(ours.Count),
                "fixture has fewer keys than our output");
            // Fixtures may contain several frames; the first frame must match ours 1:1.
            for (int i = 0; i < ours.Count; i++)
                Assert.That(ours[i], Is.EqualTo(fixtureKeys[i]),
                    "key #" + i + " mismatch for " + variant);
        }

        [Test]
        public void QuirkKeys_AreVerbatim()
        {
            string cam1 = Serialize(ActionCameraVariant.McCam1);
            StringAssert.Contains("\"camera_Follow Offset\": [", cam1); // space + capital F
            StringAssert.Contains("\"mouse_x\": ", cam1);               // MC: snake_case
            StringAssert.Contains("\"camera_rotation_oula\": [", cam1);

            string sfm = Serialize(ActionCameraVariant.Sfm15);
            StringAssert.Contains("\"camera_follow_offset\": [", sfm);  // SFM: lowercase
            StringAssert.Contains("\"mouseX\": ", sfm);                 // SFM: camelCase
            StringAssert.Contains("\"depth_name\": \"depth_0\"", sfm);
            StringAssert.DoesNotContain("mouse_x\"", sfm);

            string camN = Serialize(ActionCameraVariant.McCamN);
            StringAssert.DoesNotContain("player_position", camN);       // blue fields only
            StringAssert.DoesNotContain("keyCode", camN);
        }

        // ---- byte-level golden for our canonical layout (locks layout regressions) ----

        [Test]
        public void McCamN_ByteGolden()
        {
            const string expected =
                "[\n" +
                "  {\n" +
                "    \"time\": \"2026-03-20 16:25:29.860\",\n" +
                "    \"fps\": 29.99895003674871,\n" +
                "    \"frame\": 0,\n" +
                "    \"camera_position\": [-28.5, 1.691, -7.149],\n" +
                "    \"camera_rotation_quaternion\": [0.0, 0.707, 0.0, 0.707],\n" +
                "    \"camera_Follow Offset\": [0.0, 2.5, -5.0],\n" +
                "    \"camera_intrinsics\": {\"fx\": 1080.0, \"fy\": 1080.0, \"cx\": 960.0, \"cy\": 540.0},\n" +
                "    \"camera_speed\": [1.25, 0.0, -3.5]\n" +
                "  }\n" +
                "]\n";
            Assert.That(Serialize(ActionCameraVariant.McCamN), Is.EqualTo(expected));
        }

        // ---- number formatting policy ----

        [TestCase(1080.0, "1080.0")]
        [TestCase(30.0, "30.0")]
        [TestCase(0.5, "0.5")]
        [TestCase(0.707, "0.707")]
        [TestCase(-3.5, "-3.5")]
        [TestCase(29.99895003674871, "29.99895003674871")]
        public void NumberFormat_RoundTripWithForcedDecimalPoint(double value, string expected)
        {
            Assert.That(JsonPrimitives.Number(value), Is.EqualTo(expected));
        }

        [Test]
        public void NumberFormat_RejectsNaNAndInfinity()
        {
            Assert.Throws<System.ArgumentException>(() => JsonPrimitives.Number(double.NaN));
            Assert.Throws<System.ArgumentException>(() => JsonPrimitives.Number(double.PositiveInfinity));
        }

        // ---- systeminfo ----

        [Test]
        public void SystemInfo_KeyOrderMatchesFixture()
        {
            var info = CaptureSystemInfo.ForRenderTarget("ds", new RtSize(1920, 1080));
            List<string> ours = ExtractKeys(SidecarJson.SystemInfoJson(in info));
            List<string> fixtureKeys = ExtractKeys(LoadFixture("systeminfo.sample.json"));
            Assert.That(ours, Is.EqualTo(fixtureKeys));
        }

        [Test]
        public void SystemInfo_ReportsRtSizeTruthfully()
        {
            // Vertical aspect: resolution is the RT size, NOT a desktop resolution.
            var info = CaptureSystemInfo.ForRenderTarget("unity", new RtSize(1080, 1920));
            string json = SidecarJson.SystemInfoJson(in info);
            StringAssert.Contains("\"width\": 1080", json);
            StringAssert.Contains("\"height\": 1920", json);
            StringAssert.Contains("\"recordDpi\": 1.0", json);
        }
    }
}
