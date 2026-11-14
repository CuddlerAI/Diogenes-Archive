// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.

using System.Collections.Generic;
using System.IO;
using System.Text.RegularExpressions;
using UnityEditor;
using UnityEngine;

namespace Diogenes.EngineCapture.Editor
{
    /// <summary>Result of a purity scan.</summary>
    public sealed class PurityReport
    {
        /// <summary>Red-line violations (docs/70 camera purity): fail CI.</summary>
        public readonly List<string> Errors = new List<string>();

        /// <summary>Advisories (clock-source scan): reported, do not fail CI.</summary>
        public readonly List<string> Warnings = new List<string>();

        public bool Passed => Errors.Count == 0;
    }

    /// <summary>
    /// CI gate for capture-scene hygiene (docs/70):
    ///
    /// 1. Camera purity RED LINE - nothing may render from a capture camera's own
    ///    subtree. Any Renderer, Canvas or ParticleSystem under one of the 4 rig
    ///    cameras (inactive included) is an ERROR: overlays/gizmos/FX attached to
    ///    cameras would contaminate delivered video.
    ///
    /// 2. Clock-source scan - project scripts referencing unscaledTime,
    ///    unscaledDeltaTime or realtimeSinceStartup are listed as WARNINGS: capture
    ///    logic must follow FrameClock ticks only (docs/70 single time authority).
    ///    Only Assets/ is scanned; packages (including this SDK, whose FrameClock
    ///    watchdog legitimately reads realtimeSinceStartup) are exempt.
    ///
    /// CI usage (docs/70 M0):
    ///   Unity -batchmode -quit -projectPath ... -executeMethod
    ///       Diogenes.EngineCapture.Editor.PurityCheck.RunCi
    /// Exit code 0 = pass, 1 = red-line violation.
    /// </summary>
    public static class PurityCheck
    {
        static readonly Regex ForbiddenClockRegex = new Regex(
            @"\b(unscaledTime|unscaledDeltaTime|realtimeSinceStartup)\b",
            RegexOptions.Compiled);

        /// <summary>Run the full scan against every MultiCamRig in open scenes.</summary>
        public static PurityReport Run()
        {
            var report = new PurityReport();

#if UNITY_2021_3_OR_NEWER
            MultiCamRig[] rigs = Object.FindObjectsOfType<MultiCamRig>(true);
#else
            MultiCamRig[] rigs = Object.FindObjectsOfType<MultiCamRig>();
#endif
            if (rigs.Length == 0)
                report.Warnings.Add("No MultiCamRig found in open scenes; camera purity not checked.");
            foreach (var rig in rigs)
                CheckRigCameraPurity(rig, report);

            ScanProjectScriptsForForbiddenClocks(report);
            return report;
        }

        /// <summary>Batch-mode entry point; exits the editor with a CI status code.</summary>
        public static void RunCi()
        {
            PurityReport report = Run();
            LogReport(report);
            EditorApplication.Exit(report.Passed ? 0 : 1);
        }

        /// <summary>Camera purity red line for one rig.</summary>
        public static void CheckRigCameraPurity(MultiCamRig rig, PurityReport report)
        {
            if (rig.Lanes == null) return;
            for (int i = 0; i < rig.Lanes.Length; i++)
            {
                var lane = rig.Lanes[i];
                if (lane?.Camera == null)
                {
                    report.Errors.Add("Rig '" + rig.name + "' lane " + (i + 1) + " has no camera assigned.");
                    continue;
                }
                Transform root = lane.Camera.transform;
                AddSubtreeViolations<Renderer>(root, i, report);
                AddSubtreeViolations<Canvas>(root, i, report);
                AddSubtreeViolations<ParticleSystem>(root, i, report);
            }
        }

        static void AddSubtreeViolations<T>(Transform cameraRoot, int laneIndex, PurityReport report)
            where T : Component
        {
            foreach (T c in cameraRoot.GetComponentsInChildren<T>(true))
            {
                report.Errors.Add(
                    "Camera purity violation (lane " + (laneIndex + 1) + "): " +
                    typeof(T).Name + " on '" + FullPath(c.transform) + "'");
            }
        }

        /// <summary>Advisory scan of Assets/ scripts for non-FrameClock time sources.</summary>
        public static void ScanProjectScriptsForForbiddenClocks(PurityReport report)
        {
            string[] guids = AssetDatabase.FindAssets("t:MonoScript", new[] { "Assets" });
            foreach (string guid in guids)
            {
                string path = AssetDatabase.GUIDToAssetPath(guid);
                if (!path.EndsWith(".cs")) continue;
                string text;
                try { text = File.ReadAllText(path); }
                catch (IOException) { continue; }

                string[] lines = text.Split('\n');
                for (int i = 0; i < lines.Length; i++)
                {
                    Match m = ForbiddenClockRegex.Match(lines[i]);
                    if (m.Success)
                        report.Warnings.Add(
                            "Clock-source advisory: " + path + ":" + (i + 1) + " uses " + m.Value);
                }
            }
        }

        internal static void LogReport(PurityReport report)
        {
            foreach (string e in report.Errors) Debug.LogError("[EngineCapture PurityCheck] " + e);
            foreach (string w in report.Warnings) Debug.LogWarning("[EngineCapture PurityCheck] " + w);
            Debug.Log("[EngineCapture PurityCheck] " + (report.Passed ? "PASS" : "FAIL") +
                " (" + report.Errors.Count + " errors, " + report.Warnings.Count + " warnings)");
        }

        static string FullPath(Transform t)
        {
            string path = t.name;
            while (t.parent != null)
            {
                t = t.parent;
                path = t.name + "/" + path;
            }
            return path;
        }
    }
}
