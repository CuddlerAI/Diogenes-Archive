// Pure C# (no UnityEngine dependency) so the formatting rules can be unit-tested
// outside Unity.
//
// Serializer choice (docs/70): a HAND-WRITTEN writer, not JsonUtility / Newtonsoft.
// Rationale:
//   * JsonUtility cannot emit the verbatim key "camera_Follow Offset" (space in key).
//   * Newtonsoft would work via [JsonProperty] but adds a package dependency
//     (com.unity.nuget.newtonsoft-json) and gives less byte-level control.
//   * Delivery keys are contractually verbatim (engine-data/README.md quirk table),
//     so we keep full control of key bytes, key order and array layout here.
//
// Number policy: invariant culture, shortest round-trip ("R"), with a trailing ".0"
// forced for integral doubles (samples write 1080.0, 30.0, 1.0). The demand-side
// samples mix fixed-4 and round-trip precision between fields; precision is NOT
// normative (schemas validate types only) - key names/order and array layout ARE.

using System;
using System.Globalization;
using System.Text;

namespace Diogenes.EngineCapture
{
    /// <summary>Low-level JSON token helpers shared by all capture writers.</summary>
    public static class JsonPrimitives
    {
        /// <summary>
        /// Format a double: invariant culture, round-trip precision, always with a
        /// decimal point (integral values get ".0", matching the demand-side samples).
        /// NaN/Infinity are rejected - invalid in JSON; depth invalids are encoded in
        /// the EXR payload, never in action_camera json.
        /// </summary>
        public static string Number(double value)
        {
            if (double.IsNaN(value) || double.IsInfinity(value))
                throw new ArgumentException("NaN/Infinity cannot be serialized to JSON", nameof(value));
            string s = value.ToString("R", CultureInfo.InvariantCulture);
            if (s.IndexOf('.') < 0 && s.IndexOf('e') < 0 && s.IndexOf('E') < 0)
                s += ".0";
            return s;
        }

        /// <summary>Format an integer (invariant culture).</summary>
        public static string Number(int value)
        {
            return value.ToString(CultureInfo.InvariantCulture);
        }

        /// <summary>Append a JSON string literal with standard escaping.</summary>
        public static void AppendString(StringBuilder sb, string value)
        {
            sb.Append('"');
            foreach (char c in value)
            {
                switch (c)
                {
                    case '"': sb.Append("\\\""); break;
                    case '\\': sb.Append("\\\\"); break;
                    case '\b': sb.Append("\\b"); break;
                    case '\f': sb.Append("\\f"); break;
                    case '\n': sb.Append("\\n"); break;
                    case '\r': sb.Append("\\r"); break;
                    case '\t': sb.Append("\\t"); break;
                    default:
                        if (c < 0x20)
                            sb.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                        else
                            sb.Append(c);
                        break;
                }
            }
            sb.Append('"');
        }

        /// <summary>Append an inline 3-vector: [x, y, z] (single line, ", " separator).</summary>
        public static void AppendVec3(StringBuilder sb, in Vec3 v)
        {
            sb.Append('[')
              .Append(Number(v.X)).Append(", ")
              .Append(Number(v.Y)).Append(", ")
              .Append(Number(v.Z)).Append(']');
        }

        /// <summary>Append an inline quaternion: [x, y, z, w].</summary>
        public static void AppendQuat(StringBuilder sb, in Quat q)
        {
            sb.Append('[')
              .Append(Number(q.X)).Append(", ")
              .Append(Number(q.Y)).Append(", ")
              .Append(Number(q.Z)).Append(", ")
              .Append(Number(q.W)).Append(']');
        }

        /// <summary>Append inline intrinsics: {"fx": ..., "fy": ..., "cx": ..., "cy": ...}.</summary>
        public static void AppendIntrinsics(StringBuilder sb, in Intrinsics k)
        {
            sb.Append("{\"fx\": ").Append(Number(k.Fx))
              .Append(", \"fy\": ").Append(Number(k.Fy))
              .Append(", \"cx\": ").Append(Number(k.Cx))
              .Append(", \"cy\": ").Append(Number(k.Cy))
              .Append('}');
        }

        /// <summary>Append an inline int array: [87] / [87, 32] / [].</summary>
        public static void AppendIntArray(StringBuilder sb, int[] values)
        {
            sb.Append('[');
            if (values != null)
            {
                for (int i = 0; i < values.Length; i++)
                {
                    if (i > 0) sb.Append(", ");
                    sb.Append(Number(values[i]));
                }
            }
            sb.Append(']');
        }
    }
}
