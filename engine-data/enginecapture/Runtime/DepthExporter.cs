// Spec source: docs/68 §6.4/§6.5 (SFM-Depth delivery).
//
// This file used to carry a skeleton that read the depth RT back inside HandleTick,
// i.e. during Update. That is the SAME defect ReadbackRing was fixed for: an
// AsyncGPUReadback/ReadPixels issued during Update captures the PREVIOUS frame
// (docs/78 §9 correction 2, measured on Metal). On the depth path the consequence is
// that every depth map lags its json row by one export frame - and the QC of the day
// only reconciled FILE NAMES, so nothing would have caught it before delivery, where
// docs/68 §7.2 says "find one misaligned and the whole batch goes back".
//
// The readback now goes through ReadbackRing like every other lane (docs/83 debate 17),
// so the timing lives in exactly one place. What remains here is the constant set and
// the quantisation rule, both unit-tested.

using UnityEngine;

namespace Diogenes.EngineCapture
{
    /// <summary>
    /// SFM depth constants (docs/68 §6.4): 720p, view-space linear depth in metres,
    /// centimetre truncation, single-channel half EXR, NaN for invalid pixels,
    /// EVEN frames only (15 fps out of the 30 fps timeline).
    ///
    /// The pixels themselves are produced by <see cref="SfmRig"/>'s depth lane and
    /// consumed by <see cref="DepthSink"/>; this type is the shared vocabulary.
    /// </summary>
    public static class DepthExporter
    {
        public const int DepthWidth = 1280;
        public const int DepthHeight = 720;

        /// <summary>Single-channel EXR channel name (docs/68 recommends Y or Z).</summary>
        public const string ChannelName = "Y";

        /// <summary>Build the sink for a clip's depth lane. flipRows must stay paired
        /// with the video lane's FfmpegPipeConfig.VideoFilter: both lanes read back with
        /// the same row order, so they need the same correction (docs/88 — defaults
        /// match: "vflip" / true for the D3D production rigs).</summary>
        public static DepthSink CreateSink(string outputDirectory, float farPlaneMeters,
                                           ExrCompression compression = ExrCompression.Zip,
                                           bool flipRows = true)
        {
            return new DepthSink(DepthWidth, DepthHeight, farPlaneMeters,
                                 outputDirectory, compression, flipRows);
        }
    }
}
