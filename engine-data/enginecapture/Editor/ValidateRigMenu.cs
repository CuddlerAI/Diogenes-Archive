// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using UnityEditor;
using UnityEngine;

namespace Diogenes.EngineCapture.Editor
{
    /// <summary>
    /// "Diogenes/EngineCapture/Validate Rig": one-click pre-flight for the operator
    /// (docs/70 M0). Runs the PurityCheck scan plus static rig-config validation
    /// (lane count, aspect labels, focal enum, layer uniqueness, initial pose angles)
    /// WITHOUT allocating render textures or touching play mode.
    /// </summary>
    public static class ValidateRigMenu
    {
        [MenuItem("Diogenes/EngineCapture/Validate Rig")]
        public static void ValidateRig()
        {
            PurityReport report = PurityCheck.Run();

#if UNITY_2021_3_OR_NEWER
            MultiCamRig[] rigs = Object.FindObjectsOfType<MultiCamRig>(true);
#else
            MultiCamRig[] rigs = Object.FindObjectsOfType<MultiCamRig>();
#endif
            foreach (var rig in rigs)
                ValidateConfig(rig, report);

            PurityCheck.LogReport(report);
            EditorUtility.DisplayDialog(
                "EngineCapture Rig Validation",
                (report.Passed ? "PASS" : "FAIL") + "\n\nErrors: " + report.Errors.Count +
                "\nWarnings: " + report.Warnings.Count + "\n\nDetails in the Console.",
                "OK");
        }

        static void ValidateConfig(MultiCamRig rig, PurityReport report)
        {
            var lanes = rig.Lanes;
            if (lanes == null || lanes.Length != MultiCamRig.LaneCount)
            {
                report.Errors.Add("Rig '" + rig.name + "': expected " + MultiCamRig.LaneCount + " lanes.");
                return;
            }

            for (int i = 0; i < lanes.Length; i++)
            {
                var lane = lanes[i];
                if (lane?.Camera == null) continue; // already reported by PurityCheck

                try
                {
                    CaptureFormats.ParseAspect(lane.Aspect);
                }
                catch (System.ArgumentException)
                {
                    report.Errors.Add("Rig '" + rig.name + "' lane " + (i + 1) +
                        ": unknown aspect label '" + lane.Aspect + "'");
                }

                if (System.Array.IndexOf(CaptureFormats.AllowedFocalLengthsMm, lane.FocalLengthMm) < 0)
                    report.Errors.Add("Rig '" + rig.name + "' lane " + (i + 1) +
                        ": focal " + lane.FocalLengthMm + " mm not in plan enum {18,24,35,50}");

                for (int j = 0; j < i; j++)
                {
                    if (lanes[j]?.Camera == null) continue;
                    if (lanes[j].ExclusiveLayer == lane.ExclusiveLayer)
                        report.Errors.Add("Rig '" + rig.name + "': lanes " + (j + 1) + " and " +
                            (i + 1) + " share exclusive layer " + lane.ExclusiveLayer);

                    float angle = Vector3.Angle(
                        lanes[j].Camera.transform.forward, lane.Camera.transform.forward);
                    if (angle > MultiCamRig.MaxInitialPairAngleDeg)
                        report.Errors.Add("Rig '" + rig.name + "': lanes " + (j + 1) + " and " +
                            (i + 1) + " initial forward angle " + angle.ToString("F2") +
                            " deg exceeds " + MultiCamRig.MaxInitialPairAngleDeg + " deg");
                }
            }
        }
    }
}
