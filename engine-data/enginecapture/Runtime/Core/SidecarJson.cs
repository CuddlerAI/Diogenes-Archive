// Pure C# (no UnityEngine dependency). Sidecar json for a clip:
//   * systeminfo_N.json - demand-side format (engine-data/schema/systeminfo.schema.json);
//     resolution is filled TRUTHFULLY with the render-target size (docs/70: no
//     pretending a desktop resolution; vertical aspects report e.g. 1080x1920).
//   * provenance.json - our own sidecar (13th file, not part of the 12-file delivery
//     set; docs/70 audit trail): scene asset id, license id, plan reference, seed,
//     SDK version, calibration results, plus free-form notes.

using System;
using System.Collections.Generic;
using System.IO;
using System.Text;

namespace Diogenes.EngineCapture
{
    /// <summary>Payload of systeminfo_N.json (keys verbatim per schema/fixture).
    /// Named CaptureSystemInfo to avoid clashing with UnityEngine.SystemInfo.</summary>
    public struct CaptureSystemInfo
    {
        public string GameProcessName;
        public int X;
        public int Y;
        public int Width;
        public int Height;
        public double RecordDpi;

        public static CaptureSystemInfo ForRenderTarget(string gameProcessName, in RtSize size)
        {
            return new CaptureSystemInfo
            {
                GameProcessName = gameProcessName,
                X = 0,
                Y = 0,
                Width = size.Width,
                Height = size.Height,
                RecordDpi = 1.0
            };
        }
    }

    /// <summary>Our-side audit sidecar (docs/70). Serialized to provenance.json.</summary>
    public sealed class ProvenanceInfo
    {
        public string SdkVersion = CaptureSdk.Version;
        public string SceneAssetId = "";
        public string LicenseId = "";

        /// <summary>Usage-restriction marker serialized as "license". docs/78 R'5: clips
        /// captured from UCL-licensed content MUST carry "UCL-internal-validation-only"
        /// so the delivery tooling can physically refuse to ship them.</summary>
        public string License = "";
        public string PlanReference = "";
        public long Seed;
        public string CalibrationResult = "";

        /// <summary>Free-form notes, serialized in insertion order. Used for interim
        /// conventions that await demand-side confirmation, e.g. euler rotation order
        /// (docs/72 C3), mouse-delta normalization denominator, raw mouse deltas.</summary>
        public readonly List<KeyValuePair<string, string>> Notes =
            new List<KeyValuePair<string, string>>();

        public void AddNote(string key, string value)
        {
            Notes.Add(new KeyValuePair<string, string>(key, value));
        }
    }

    /// <summary>SDK identity constants.</summary>
    public static class CaptureSdk
    {
        /// <summary>Keep in sync with package.json.</summary>
        public const string Version = "0.1.0";
        public const string PackageName = "com.diogenes.enginecapture";
    }

    /// <summary>Writers for the sidecar json files.</summary>
    public static class SidecarJson
    {
        /// <summary>Serialize systeminfo json (single object, 2-space indent).</summary>
        public static string SystemInfoJson(in CaptureSystemInfo info)
        {
            var sb = new StringBuilder(160);
            sb.Append("{\n");
            sb.Append("  \"gameProcessName\": ");
            JsonPrimitives.AppendString(sb, info.GameProcessName ?? "");
            sb.Append(",\n");
            sb.Append("  \"x\": ").Append(JsonPrimitives.Number(info.X)).Append(",\n");
            sb.Append("  \"y\": ").Append(JsonPrimitives.Number(info.Y)).Append(",\n");
            sb.Append("  \"width\": ").Append(JsonPrimitives.Number(info.Width)).Append(",\n");
            sb.Append("  \"height\": ").Append(JsonPrimitives.Number(info.Height)).Append(",\n");
            sb.Append("  \"recordDpi\": ").Append(JsonPrimitives.Number(info.RecordDpi)).Append('\n');
            sb.Append("}\n");
            return sb.ToString();
        }

        /// <summary>Serialize provenance.json.</summary>
        public static string ProvenanceJson(ProvenanceInfo p)
        {
            if (p == null) throw new ArgumentNullException(nameof(p));
            var sb = new StringBuilder(512);
            sb.Append("{\n");
            AppendStringField(sb, "sdk_version", p.SdkVersion, true);
            AppendStringField(sb, "scene_asset_id", p.SceneAssetId, true);
            AppendStringField(sb, "license_id", p.LicenseId, true);
            if (!string.IsNullOrEmpty(p.License))
                AppendStringField(sb, "license", p.License, true);
            AppendStringField(sb, "plan_reference", p.PlanReference, true);
            sb.Append("  \"seed\": ").Append(p.Seed.ToString(System.Globalization.CultureInfo.InvariantCulture)).Append(",\n");
            AppendStringField(sb, "calibration_result", p.CalibrationResult, p.Notes.Count > 0);
            if (p.Notes.Count > 0)
            {
                sb.Append("  \"notes\": {\n");
                for (int i = 0; i < p.Notes.Count; i++)
                {
                    sb.Append("    ");
                    JsonPrimitives.AppendString(sb, p.Notes[i].Key);
                    sb.Append(": ");
                    JsonPrimitives.AppendString(sb, p.Notes[i].Value ?? "");
                    sb.Append(i + 1 < p.Notes.Count ? ",\n" : "\n");
                }
                sb.Append("  }\n");
            }
            sb.Append("}\n");
            return sb.ToString();
        }

        /// <summary>Write text as UTF-8 without BOM (delivery json must be BOM-free).</summary>
        public static void WriteUtf8File(string path, string content)
        {
            File.WriteAllText(path, content, new UTF8Encoding(false));
        }

        static void AppendStringField(StringBuilder sb, string key, string value, bool comma)
        {
            sb.Append("  ");
            JsonPrimitives.AppendString(sb, key);
            sb.Append(": ");
            JsonPrimitives.AppendString(sb, value ?? "");
            sb.Append(comma ? ",\n" : "\n");
        }
    }
}
