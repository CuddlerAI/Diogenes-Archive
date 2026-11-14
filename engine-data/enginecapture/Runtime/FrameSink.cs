// Where a lane's read-back pixels go. Splitting this out is what lets the depth lane
// reuse ReadbackRing instead of growing a second GPU->CPU path: the ring already paid
// for the expensive lesson that an AsyncGPUReadback request issued during Update
// captures the PREVIOUS frame (docs/78 §9 correction 2, measured on Metal), plus the
// offline zero-drop barrier and the backlog watchdog. A parallel path would have to
// re-learn all three (docs/83 debate 17).

using System;
using System.IO;

namespace Diogenes.EngineCapture
{
    /// <summary>Consumer of one lane's read-back frames. Called on the writer thread.</summary>
    public interface IFrameSink
    {
        /// <summary>Pixel format the ring must request for this lane.</summary>
        UnityEngine.TextureFormat ReadbackFormat { get; }

        /// <summary>Bytes per frame the ring should pool for this lane.</summary>
        int FrameBytes { get; }

        /// <summary>True when this lane should be read back on the given tick.
        /// Depth lanes export EVEN ticks only (docs/68 15 fps out of a 30 fps timeline).</summary>
        bool WantsTick(int tick);

        /// <summary>Consume one frame. Runs off the Unity main thread — must not touch the engine API.</summary>
        void Write(int tick, byte[] data, int length);

        /// <summary>Non-null once the sink has failed; the clip is aborted for re-record.</summary>
        string FailReason { get; }

        /// <summary>Flush and close. Returns false on failure.</summary>
        bool Finish(int timeoutMs);
    }

    /// <summary>Colour lane sink: raw RGBA32 straight into an ffmpeg pipe.</summary>
    public sealed class VideoSink : IFrameSink
    {
        readonly FfmpegPipe _pipe;

        public VideoSink(FfmpegPipe pipe, int frameBytes)
        {
            _pipe = pipe ?? throw new ArgumentNullException(nameof(pipe));
            FrameBytes = frameBytes;
        }

        public FfmpegPipe Pipe => _pipe;
        public UnityEngine.TextureFormat ReadbackFormat => UnityEngine.TextureFormat.RGBA32;
        public int FrameBytes { get; }
        public bool WantsTick(int tick) => true;
        public string FailReason => _pipe.HasFailed ? _pipe.FailReason : null;
        public void Write(int tick, byte[] data, int length) => _pipe.WriteFrame(data);
        public bool Finish(int timeoutMs) => _pipe.Finish(timeoutMs);
    }

    /// <summary>
    /// Depth lane sink (docs/68 §6.4): RFloat linear eye depth in metres ->
    /// trunc(d*100)/100 -> single-channel half EXR named depth_{tick}.
    ///
    /// Invalid pixels (sky / nothing written / at-or-beyond the far plane) become NaN.
    /// The far cutoff is the CAMERA's far plane, passed in by the rig — an arbitrary
    /// constant would silently NaN out real geometry (the skeleton's hard-coded 500 m
    /// would have erased everything past 500 m in a 4000 m Megacity frustum).
    ///
    /// Row order: both lanes ride the same ReadbackRing, so their readback row order is
    /// the same graphics-API property (bottom-up on D3D). The video lane corrects it in
    /// the ffmpeg graph (FfmpegPipeConfig.VideoFilter, "vflip" by default); this sink
    /// applies the SAME correction while filling the scratch buffer (flipRows, default
    /// true), which is what keeps video and depth upright together on every platform —
    /// one shared row-order fact, one correction per lane, decided at the same layer.
    /// The depth pass does NOT pre-flip: sfm-d1-20260805 proved it (all 900 EXRs came
    /// out mirrored, sky NaN in the bottom rows — docs/88). If a platform ever reads
    /// back top-down, clear VideoFilter AND pass flipRows=false together.
    /// </summary>
    public sealed class DepthSink : IFrameSink
    {
        readonly int _width;
        readonly int _height;
        readonly float _farMeters;
        readonly string _outDir;
        readonly ExrWriter _exr;
        readonly float[] _scratch;
        readonly bool _flipRows;

        public DepthSink(int width, int height, float farMeters, string outputDirectory,
                         ExrCompression compression = ExrCompression.Zip,
                         bool flipRows = true)
        {
            _width = width;
            _height = height;
            _farMeters = farMeters;
            _outDir = outputDirectory;
            _flipRows = flipRows;
            Directory.CreateDirectory(outputDirectory);
            _exr = new ExrWriter(width, height, "Y", compression);
            _scratch = new float[width * height];
        }

        public UnityEngine.TextureFormat ReadbackFormat => UnityEngine.TextureFormat.RFloat;
        public int FrameBytes => _width * _height * 4;
        public bool WantsTick(int tick) => DepthEncoding.IsExportFrame(tick);
        public string FailReason { get; private set; }

        /// <summary>Files written so far (drives the depth.zip manifest).</summary>
        public int FramesWritten { get; private set; }

        public void Write(int tick, byte[] data, int length)
        {
            if (FailReason != null) return;
            try
            {
                int n = Math.Min(_scratch.Length, length / 4);
                // Readback source row y is the IMAGE BOTTOM at y=0 (see class summary);
                // ExrWriter expects y=0 = top scanline, so mirror rows on the way in.
                for (int i = 0; i < n; i++)
                {
                    float d = BitConverter.ToSingle(data, i * 4);
                    int y = i / _width;
                    int dst = _flipRows ? (_height - 1 - y) * _width + (i - y * _width) : i;
                    _scratch[dst] = (d <= 0f || float.IsNaN(d) || d >= _farMeters)
                        ? float.NaN
                        : (float)DepthEncoding.QuantizeMeters(d);
                }
                _exr.WriteFile(Path.Combine(_outDir, DepthEncoding.DepthName(tick) + ".exr"),
                               _scratch);
                FramesWritten++;
            }
            catch (Exception e)
            {
                FailReason = e.GetType().Name + ": " + e.Message;
            }
        }

        public bool Finish(int timeoutMs) => FailReason == null;
    }
}
