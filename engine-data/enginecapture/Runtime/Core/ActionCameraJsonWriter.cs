// Pure C# (no UnityEngine dependency) so key order/layout can be unit-tested outside
// Unity. Spec: docs/68 / docs/69 delivery json via engine-data/schema + fixtures.
//
// Layout follows the demand-side cam1 sample (engine-data/fixtures/
// mc_cam1_action_camera.sample.json): top-level JSON array, one frame object per
// element, one key per line at 4-space depth, vectors/quaternions/intrinsics/keyCode
// inline on a single line. Key ORDER below is byte-for-byte the fixture order.

using System;
using System.IO;
using System.Text;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// Streaming writer for action_camera_N.json. Frames are appended one at a time
    /// (constant memory), so a 60s/1800-frame clip never holds the full document.
    /// Usage: ctor -> WriteFrame xN -> Finish -> Dispose (Dispose without Finish
    /// discards cleanly for abort paths).
    /// </summary>
    public sealed class ActionCameraJsonWriter : IDisposable
    {
        const string FrameIndent = "  ";      // depth of "{" / "}"
        const string KeyIndent = "    ";      // depth of keys inside a frame

        readonly TextWriter _out;
        readonly ActionCameraVariant _variant;
        readonly StringBuilder _sb = new StringBuilder(1024);
        bool _first = true;
        bool _finished;

        public ActionCameraJsonWriter(TextWriter output, ActionCameraVariant variant)
        {
            _out = output ?? throw new ArgumentNullException(nameof(output));
            _variant = variant;
            _out.Write("[\n");
        }

        /// <summary>Open a writer over a UTF-8 (no BOM) file stream.</summary>
        public static ActionCameraJsonWriter CreateFile(string path, ActionCameraVariant variant)
        {
            var stream = new StreamWriter(path, false, new UTF8Encoding(false));
            return new ActionCameraJsonWriter(stream, variant);
        }

        public void WriteFrame(in ActionCameraFrame f)
        {
            if (_finished) throw new InvalidOperationException("Writer already finished");
            _sb.Length = 0;
            if (!_first) _sb.Append(",\n");
            _first = false;
            _sb.Append(FrameIndent).Append("{\n");

            switch (_variant)
            {
                case ActionCameraVariant.McCam1: AppendMcCam1(in f); break;
                case ActionCameraVariant.McCamN: AppendMcCamN(in f); break;
                case ActionCameraVariant.Sfm15: AppendSfm(in f, withDepthName: true); break;
                case ActionCameraVariant.Sfm30: AppendSfm(in f, withDepthName: false); break;
                default: throw new InvalidOperationException("Unknown variant");
            }

            TrimTrailingComma();
            _sb.Append(FrameIndent).Append('}');
            _out.Write(_sb.ToString());
        }

        /// <summary>Close the JSON array. Must be called exactly once on the success path.</summary>
        public void Finish()
        {
            if (_finished) return;
            _finished = true;
            _out.Write("\n]\n");
            _out.Flush();
        }

        public void Dispose() => _out.Dispose();

        // ---- variant layouts (key order is normative, from fixtures) ----

        void AppendMcCam1(in ActionCameraFrame f)
        {
            Str("time", f.Time);
            Num("fps", f.Fps);
            Int("frame", f.Frame);
            V3("camera_position", f.CameraPosition);
            V3("camera_rotation_oula", f.CameraRotationEuler);
            Q4("camera_rotation_quaternion", f.CameraRotationQuaternion);
            V3("camera_Follow Offset", f.CameraFollowOffset); // verbatim key, do NOT "fix"
            K("camera_intrinsics", f.CameraIntrinsics);
            V3("camera_speed", f.CameraSpeed);
            V3("player_position", f.PlayerPosition);
            Num("mouse_x", f.MouseX);
            Num("mouse_y", f.MouseY);
            Num("mouse_dx", f.MouseDx);
            Num("mouse_dy", f.MouseDy);
            Ints("keyCode", f.KeyCodes);
            V3("player_rotation_oula", f.PlayerRotationEuler);
            Q4("player_rotation_quaternion", f.PlayerRotationQuaternion);
            V3("player_speed", f.PlayerSpeed);
            Num("metric_scale", f.MetricScale);
        }

        void AppendMcCamN(in ActionCameraFrame f)
        {
            Str("time", f.Time);
            Num("fps", f.Fps);
            Int("frame", f.Frame);
            V3("camera_position", f.CameraPosition);
            Q4("camera_rotation_quaternion", f.CameraRotationQuaternion);
            V3("camera_Follow Offset", f.CameraFollowOffset); // verbatim key, do NOT "fix"
            K("camera_intrinsics", f.CameraIntrinsics);
            V3("camera_speed", f.CameraSpeed);
        }

        void AppendSfm(in ActionCameraFrame f, bool withDepthName)
        {
            Str("time", f.Time);
            Num("fps", f.Fps);
            if (withDepthName)
                Str("depth_name", f.DepthName ?? ("depth_" + JsonPrimitives.Number(f.Frame)));
            Int("frame", f.Frame);
            V3("camera_position", f.CameraPosition);
            Q4("camera_rotation_quaternion", f.CameraRotationQuaternion);
            V3("camera_follow_offset", f.CameraFollowOffset); // SFM: lowercase key
            K("camera_intrinsics", f.CameraIntrinsics);
            V3("camera_speed", f.CameraSpeed);
            V3("player_position", f.PlayerPosition);
            Num("mouseX", f.MouseX); // SFM: camelCase, verbatim
            Num("mouseY", f.MouseY);
            Num("mouse_dx", f.MouseDx);
            Num("mouse_dy", f.MouseDy);
            Ints("keyCode", f.KeyCodes);
            Q4("player_rotation_quaternion", f.PlayerRotationQuaternion);
            V3("player_speed", f.PlayerSpeed);
            Num("metric_scale", f.MetricScale);
        }

        // ---- line helpers ----

        void Key(string name)
        {
            _sb.Append(KeyIndent);
            JsonPrimitives.AppendString(_sb, name);
            _sb.Append(": ");
        }

        void EndLine() => _sb.Append(",\n");

        void Str(string k, string v)
        {
            Key(k);
            JsonPrimitives.AppendString(_sb, v ?? "");
            EndLine();
        }

        void Num(string k, double v)
        {
            Key(k);
            _sb.Append(JsonPrimitives.Number(v));
            EndLine();
        }

        void Int(string k, int v)
        {
            Key(k);
            _sb.Append(JsonPrimitives.Number(v));
            EndLine();
        }

        void V3(string k, in Vec3 v)
        {
            Key(k);
            JsonPrimitives.AppendVec3(_sb, in v);
            EndLine();
        }

        void Q4(string k, in Quat q)
        {
            Key(k);
            JsonPrimitives.AppendQuat(_sb, in q);
            EndLine();
        }

        void K(string k, in Intrinsics intr)
        {
            Key(k);
            JsonPrimitives.AppendIntrinsics(_sb, in intr);
            EndLine();
        }

        void Ints(string k, int[] values)
        {
            Key(k);
            JsonPrimitives.AppendIntArray(_sb, values);
            EndLine();
        }

        void TrimTrailingComma()
        {
            // last appended line ends ",\n" -> "\n"
            if (_sb.Length >= 2 && _sb[_sb.Length - 2] == ',')
                _sb.Remove(_sb.Length - 2, 1);
        }
    }
}
