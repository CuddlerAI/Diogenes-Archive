// NOT yet compiled in a Unity editor (this repo has no Unity toolchain) -
// APIs are written against Unity 2022.3 LTS; first import must follow the
// docs/70 M0 bring-up checklist.
//
// Uses System.Diagnostics.Process. Project prerequisite (README): if the project's
// Api Compatibility Level strips Process, switch it to ".NET Framework". Capture rigs
// are Windows boxes (docs/70 production environment), where this is the default ask.

using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;

namespace Diogenes.EngineCapture
{
    /// <summary>Encoder settings; defaults follow docs/70 (h264_nvenc, 30 fps CFR).</summary>
    [Serializable]
    public sealed class FfmpegPipeConfig
    {
        public string FfmpegPath = "ffmpeg";
        public string Codec = "h264_nvenc";
        public string Preset = "p5";
        public int BitrateKbps = 20000;

        /// <summary>Filter graph applied to the raw input. "vflip" by default: GPU
        /// readback rows are bottom-up on D3D - verify per platform during M0.
        /// Must stay paired with DepthSink's flipRows (docs/88): the depth lane reads
        /// back with the SAME row order, so changing one without the other splits the
        /// two lanes' orientation - exactly the sfm-d1-20260805 defect.</summary>
        public string VideoFilter = "vflip";

        /// <summary>Escape hatch appended verbatim before the output path.</summary>
        public string ExtraArgs = "";
    }

    /// <summary>
    /// One ffmpeg child process per camera lane (docs/70 encode stage): raw RGBA frames
    /// are piped into stdin, ffmpeg encodes h264_nvenc at 30 fps CFR into an .mp4.
    /// WriteFrame is called from the ReadbackRing consumer thread; Start/Finish/Abort
    /// from the main thread. Unexpected process exit sets HasFailed - ClipPackager polls
    /// it and aborts the clip (a partially encoded video is re-recorded, not repaired).
    ///
    /// NVENC prerequisite (docs/70): NVIDIA driver >= 551.76. Checked best-effort via
    /// nvidia-smi (Windows-only production concern; probe failure only warns - the
    /// definitive test is ffmpeg refusing h264_nvenc, surfaced as HasFailed).
    /// </summary>
    public sealed class FfmpegPipe : IDisposable
    {
        /// <summary>Minimum NVIDIA driver for the NVENC feature set used (docs/70).</summary>
        public static readonly Version MinNvidiaDriver = new Version(551, 76);

        readonly FfmpegPipeConfig _config;
        Process _process;
        Stream _stdin;
        volatile bool _failed;
        string _failReason = "";
        volatile string _lastStderrLine = "";
        bool _finished;

        public bool HasFailed => _failed;
        public string FailReason => string.IsNullOrEmpty(_lastStderrLine)
            ? _failReason
            : _failReason + " | stderr: " + _lastStderrLine;
        public string OutputPath { get; private set; }

        public FfmpegPipe(FfmpegPipeConfig config)
        {
            _config = config ?? new FfmpegPipeConfig();
        }

        /// <summary>Spawn ffmpeg for one lane. Raw input format is fixed by the readback
        /// stage: rgba, widthxheight, 30 fps.</summary>
        public void Start(int width, int height, string outputPath)
        {
            if (_process != null) throw new InvalidOperationException("FfmpegPipe already started");
            OutputPath = outputPath;

            string filter = string.IsNullOrEmpty(_config.VideoFilter)
                ? ""
                : " -vf " + _config.VideoFilter;
            string args = string.Format(CultureInfo.InvariantCulture,
                "-hide_banner -loglevel error -y" +
                " -f rawvideo -pix_fmt rgba -s {0}x{1} -r {2} -i -" +
                "{3} -c:v {4} -preset {5} -b:v {6}k -pix_fmt yuv420p -r {2} {7} \"{8}\"",
                width, height, CaptureFormats.FrameRate, filter,
                _config.Codec, _config.Preset, _config.BitrateKbps,
                _config.ExtraArgs, outputPath);

            var psi = new ProcessStartInfo
            {
                FileName = _config.FfmpegPath,
                Arguments = args,
                UseShellExecute = false,
                RedirectStandardInput = true,
                RedirectStandardError = true,
                CreateNoWindow = true
            };

            _process = new Process { StartInfo = psi, EnableRaisingEvents = true };
            _process.Exited += (s, e) =>
            {
                if (!_finished)
                    Fail("ffmpeg exited early (code " + SafeExitCode() + ")");
            };
            // Drain stderr asynchronously; an un-read stderr pipe can deadlock ffmpeg.
            _process.ErrorDataReceived += (s, e) =>
            {
                if (!string.IsNullOrEmpty(e.Data)) _lastStderrLine = e.Data;
            };
            try
            {
                _process.Start();
                _process.BeginErrorReadLine();
            }
            catch (Exception ex)
            {
                _process = null;
                Fail("ffmpeg failed to start: " + ex.Message);
                throw;
            }
            _stdin = _process.StandardInput.BaseStream;
        }

        /// <summary>Write one raw RGBA frame. Called from the readback consumer thread.</summary>
        public void WriteFrame(byte[] rgba)
        {
            if (_failed || _stdin == null) return;
            try
            {
                _stdin.Write(rgba, 0, rgba.Length);
            }
            catch (IOException ex)
            {
                Fail("pipe write failed: " + ex.Message);
            }
        }

        /// <summary>Close stdin and wait for ffmpeg to finalize the mp4.</summary>
        public bool Finish(int timeoutMs = 30000)
        {
            if (_process == null) return !_failed;
            _finished = true;
            try
            {
                _stdin?.Flush();
                _stdin?.Close();
                if (!_process.WaitForExit(timeoutMs))
                {
                    Fail("ffmpeg did not exit within " + timeoutMs + " ms");
                    _process.Kill();
                    return false;
                }
                if (_process.ExitCode != 0)
                {
                    Fail("ffmpeg exit code " + _process.ExitCode);
                    return false;
                }
                return !_failed;
            }
            catch (Exception ex)
            {
                Fail("finish failed: " + ex.Message);
                return false;
            }
        }

        /// <summary>Abort path: kill the process and delete the partial output.</summary>
        public void Abort()
        {
            _finished = true;
            try
            {
                _stdin?.Close();
                if (_process != null && !_process.HasExited)
                {
                    _process.Kill();
                    // Kill is asynchronous on Windows. Wait for ffmpeg to release its
                    // MP4 handle before ClipPackager removes the aborted clip directory.
                    _process.WaitForExit(3000);
                }
            }
            catch (Exception) { /* already gone */ }
            try
            {
                if (!string.IsNullOrEmpty(OutputPath) && File.Exists(OutputPath))
                    File.Delete(OutputPath);
            }
            catch (IOException) { /* leave the partial file for manual cleanup */ }
        }

        public void Dispose()
        {
            try { _stdin?.Dispose(); } catch (Exception) { }
            try { _process?.Dispose(); } catch (Exception) { }
            _stdin = null;
            _process = null;
        }

        void Fail(string reason)
        {
            _failed = true;
            _failReason = reason;
        }

        int SafeExitCode()
        {
            try { return _process.ExitCode; } catch (Exception) { return -1; }
        }

        /// <summary>
        /// Best-effort NVIDIA driver probe via nvidia-smi (production rigs are Windows;
        /// on other platforms or without nvidia-smi this returns false and the caller
        /// should only warn). Returns true when a version was parsed.
        /// </summary>
        public static bool TryGetNvidiaDriverVersion(out Version version)
        {
            version = null;
            try
            {
                var psi = new ProcessStartInfo
                {
                    FileName = "nvidia-smi",
                    Arguments = "--query-gpu=driver_version --format=csv,noheader",
                    UseShellExecute = false,
                    RedirectStandardOutput = true,
                    CreateNoWindow = true
                };
                using (var p = Process.Start(psi))
                {
                    string line = p.StandardOutput.ReadLine();
                    p.WaitForExit(3000);
                    if (string.IsNullOrEmpty(line)) return false;
                    return Version.TryParse(line.Trim(), out version);
                }
            }
            catch (Exception)
            {
                return false;
            }
        }
    }
}
