using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Text;
using System.Threading;
using SCSSdkClient;
using SCSSdkClient.Object;

internal static class TelemetryRecorder
{
    private static readonly object Gate = new object();
    private static StreamWriter writer;
    private static long rows;

    private static int Main(string[] args)
    {
        if (args.Length < 1)
        {
            Console.Error.WriteLine("usage: TelemetryRecorder.exe <output.jsonl> [duration_seconds]");
            return 2;
        }

        int durationSeconds = 0;
        if (args.Length > 1 && !int.TryParse(args[1], out durationSeconds))
        {
            Console.Error.WriteLine("duration_seconds must be an integer");
            return 2;
        }

        string fullPath = Path.GetFullPath(args[0]);
        Directory.CreateDirectory(Path.GetDirectoryName(fullPath));
        writer = new StreamWriter(fullPath, false, new UTF8Encoding(false), 65536);
        writer.AutoFlush = true;

        using (var telemetry = new SCSSdkTelemetry(10))
        {
            if (telemetry.Error != null)
            {
                Console.Error.WriteLine("telemetry connect failed: " + telemetry.Error);
                writer.Dispose();
                return 3;
            }

            telemetry.Data += OnData;
            Console.WriteLine("telemetry connected: Local\\SCSTelemetry");
            DateTime deadline = durationSeconds > 0
                ? DateTime.UtcNow.AddSeconds(durationSeconds)
                : DateTime.MaxValue;
            while (DateTime.UtcNow < deadline)
                Thread.Sleep(100);
            telemetry.Data -= OnData;
        }

        lock (Gate)
        {
            writer.Dispose();
            writer = null;
        }
        Console.WriteLine("telemetry rows: " + rows.ToString(CultureInfo.InvariantCulture));
        return rows > 0 ? 0 : 4;
    }

    private static void OnData(SCSTelemetry data, bool changed)
    {
        if (!changed || data == null || !data.SdkActive)
            return;

        var current = data.TruckValues.CurrentValues;
        var placement = current.PositionValue;
        if (placement == null || placement.Position == null || placement.Orientation == null)
            return;

        var p = placement.Position;
        var r = placement.Orientation;
        var positioning = data.TruckValues.Positioning;
        var headWorld = positioning.HeadPositionInWorldSpace;
        var cabinWorld = positioning.CabinPositionInWorlSpace;
        var headVehicle = positioning.HeadPositionInVehicleSpace;
        var headOffset = positioning.HeadOffset;
        var cabinOffset = positioning.CabinOffset;
        var velocity = current.AccelerationValues.LinearVelocity;
        var constants = data.TruckValues.ConstantsValues;
        long qpc = Stopwatch.GetTimestamp();
        string line = string.Format(
            CultureInfo.InvariantCulture,
            "{{\"schema\":\"ets2-telemetry-qpc/v2\",\"utc\":\"{0}\",\"qpc\":{1},\"qpc_frequency\":{2},\"telemetry_timestamp\":{3},\"simulation_timestamp\":{4},\"render_timestamp\":{5},\"paused\":{6},\"game\":\"{7}\",\"dll_version\":{8},\"player_position\":[{9:R},{10:R},{11:R}],\"player_orientation_turns\":[{12:R},{13:R},{14:R}],\"speed_mps\":{15:R},\"truck_id\":\"{16}\",\"truck_name\":\"{17}\",\"head_position_world\":[{18:R},{19:R},{20:R}],\"cabin_position_world\":[{21:R},{22:R},{23:R}],\"head_offset_position\":[{24:R},{25:R},{26:R}],\"head_offset_orientation_turns\":[{27:R},{28:R},{29:R}],\"cabin_offset_position\":[{30:R},{31:R},{32:R}],\"cabin_offset_orientation_turns\":[{33:R},{34:R},{35:R}],\"head_position_vehicle\":[{36:R},{37:R},{38:R}],\"linear_velocity\":[{39:R},{40:R},{41:R}]}}",
            DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture),
            qpc,
            Stopwatch.Frequency,
            data.Timestamp,
            data.SimulationTimestamp,
            data.RenderTimestamp,
            data.Paused ? "true" : "false",
            data.Game,
            data.DllVersion,
            p.X, p.Y, p.Z,
            r.Heading, r.Pitch, r.Roll,
            current.DashboardValues.Speed.Value,
            EscapeJson(constants.Id), EscapeJson(constants.Name),
            headWorld.X, headWorld.Y, headWorld.Z,
            cabinWorld.X, cabinWorld.Y, cabinWorld.Z,
            headOffset.Position.X, headOffset.Position.Y, headOffset.Position.Z,
            headOffset.Orientation.Heading, headOffset.Orientation.Pitch, headOffset.Orientation.Roll,
            cabinOffset.Position.X, cabinOffset.Position.Y, cabinOffset.Position.Z,
            cabinOffset.Orientation.Heading, cabinOffset.Orientation.Pitch, cabinOffset.Orientation.Roll,
            headVehicle.X, headVehicle.Y, headVehicle.Z,
            velocity.X, velocity.Y, velocity.Z);

        lock (Gate)
        {
            if (writer == null)
                return;
            writer.WriteLine(line);
            rows++;
        }
    }

    private static string EscapeJson(string value)
    {
        if (value == null)
            return string.Empty;
        return value.Replace("\\", "\\\\").Replace("\"", "\\\"")
            .Replace("\r", "\\r").Replace("\n", "\\n").Replace("\t", "\\t");
    }
}
