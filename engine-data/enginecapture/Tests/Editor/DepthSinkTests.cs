// DepthSink is the piece that turns read-back RFloat metres into the docs/68 §6.4
// deliverable. It is testable without a GPU: feed it the byte buffer the ring would
// hand over and read the EXR back.

using System;
using System.IO;
using Diogenes.EngineCapture;
using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    public class DepthSinkTests
    {
        const int W = 8, H = 4;

        static byte[] Buffer(Func<int, int, float> f)
        {
            var b = new byte[W * H * 4];
            for (int y = 0; y < H; y++)
                for (int x = 0; x < W; x++)
                    Array.Copy(BitConverter.GetBytes(f(x, y)), 0, b, (y * W + x) * 4, 4);
            return b;
        }

        string _dir;

        [SetUp]
        public void SetUp()
        {
            _dir = Path.Combine(Path.GetTempPath(), "diog_depthsink_" + Guid.NewGuid().ToString("N"));
        }

        [TearDown]
        public void TearDown()
        {
            if (Directory.Exists(_dir)) Directory.Delete(_dir, true);
        }

        [Test]
        public void ExportsEvenTicksOnly()
        {
            var sink = new DepthSink(W, H, 100f, _dir);
            Assert.That(sink.WantsTick(0), Is.True);
            Assert.That(sink.WantsTick(1), Is.False, "odd ticks carry no depth (docs/68 §6.5)");
            Assert.That(sink.WantsTick(2), Is.True);
            Assert.That(sink.WantsTick(1799), Is.False);
        }

        [Test]
        public void RequestsRFloatReadback()
        {
            var sink = new DepthSink(W, H, 100f, _dir);
            Assert.That(sink.ReadbackFormat, Is.EqualTo(UnityEngine.TextureFormat.RFloat));
            Assert.That(sink.FrameBytes, Is.EqualTo(W * H * 4));
        }

        [Test]
        public void WritesTruncatedCentimetres()
        {
            var sink = new DepthSink(W, H, 1000f, _dir);
            var buf = Buffer((x, y) => 3.678f);
            sink.Write(4, buf, buf.Length);

            string path = Path.Combine(_dir, "depth_4.exr");
            Assert.That(File.Exists(path), Is.True, "file name must be depth_{tick}");
            float v = ReadFirstPixel(path);
            // 3.678 -> trunc to 3.67 -> nearest half
            Assert.That(v, Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(3.67f))));
        }

        [Test]
        public void MarksSkyAndBeyondFarPlaneAsNaN()
        {
            // far plane comes from the CAMERA, not a constant: the old skeleton hard-coded
            // 500 m, which in Megacity's 4000 m frustum would have erased real geometry.
            var sink = new DepthSink(W, H, 500f, _dir);
            var buf = Buffer((x, y) => x == 0 ? 0f          // nothing written
                                     : x == 1 ? float.NaN   // already invalid
                                     : x == 2 ? 600f        // beyond far plane
                                              : 12.34f);
            sink.Write(0, buf, buf.Length);

            var px = ReadAllPixels(Path.Combine(_dir, "depth_0.exr"));
            Assert.That(float.IsNaN(px[0]), Is.True, "depth 0 -> NaN");
            Assert.That(float.IsNaN(px[1]), Is.True, "NaN passes through");
            Assert.That(float.IsNaN(px[2]), Is.True, "beyond far plane -> NaN");
            Assert.That(px[3], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(12.34f))));
        }

        [Test]
        public void FlipsRowsToMatchVideoLaneOrientation()
        {
            // Readback rows arrive bottom-up (same GPU->CPU path as the video lane,
            // which corrects via ffmpeg "vflip"). Input row 0 = image BOTTOM, so the
            // EXR's top scanline (y=0, lineOrder INCREASING_Y) must hold the LAST
            // input row - the sfm-d1-20260805 flip defect is exactly this inverted.
            var sink = new DepthSink(W, H, 100f, _dir);
            var buf = Buffer((x, y) => y + 1f);      // input row y carries (y+1) metres
            sink.Write(0, buf, buf.Length);

            var px = ReadAllPixels(Path.Combine(_dir, "depth_0.exr"));
            Assert.That(px[0], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(H))),
                "EXR top scanline must come from the last (topmost) readback row");
            Assert.That(px[(H - 1) * W], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(1f))),
                "EXR bottom scanline must come from the first (bottommost) readback row");
        }

        [Test]
        public void FlipCanBeDisabledForTopDownReadbackPlatforms()
        {
            // Escape hatch mirroring FfmpegPipeConfig.VideoFilter="": a platform whose
            // readback is already top-down clears BOTH, never one.
            var sink = new DepthSink(W, H, 100f, _dir, ExrCompression.Zip, flipRows: false);
            var buf = Buffer((x, y) => y + 1f);
            sink.Write(0, buf, buf.Length);

            var px = ReadAllPixels(Path.Combine(_dir, "depth_0.exr"));
            Assert.That(px[0], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(1f))));
            Assert.That(px[(H - 1) * W], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(H))));
        }

        [Test]
        public void CountsFramesAndSurvivesReuse()
        {
            var sink = new DepthSink(W, H, 100f, _dir);
            var buf = Buffer((x, y) => 5f + x);
            sink.Write(0, buf, buf.Length);
            sink.Write(2, buf, buf.Length);
            Assert.That(sink.FramesWritten, Is.EqualTo(2));
            Assert.That(sink.FailReason, Is.Null);
            Assert.That(sink.Finish(1000), Is.True);
        }

        [Test]
        public void FailureIsReportedNotThrown()
        {
            // writing into a path that cannot exist must surface via FailReason so the
            // clip aborts for re-record - the writer thread must never crash the process
            var sink = new DepthSink(W, H, 100f, _dir);
            Directory.Delete(_dir, true);
            File.WriteAllText(_dir, "not a directory");
            var buf = Buffer((x, y) => 1f);
            Assert.DoesNotThrow(() => sink.Write(0, buf, buf.Length));
            Assert.That(sink.FailReason, Is.Not.Null);
            Assert.That(sink.Finish(1000), Is.False);
            File.Delete(_dir);
            Directory.CreateDirectory(_dir);
        }

        // ---- helpers: minimal EXR read-back (uncompressed path is enough here) ----

        static float[] ReadAllPixels(string path)
        {
            byte[] data = File.ReadAllBytes(path);
            var w = new ExrWriter(W, H, "Y", ExrCompression.Zip);   // same block layout
            int p = 8;
            while (true)
            {
                int s = p;
                while (data[p] != 0) p++;
                if (p == s) { p++; break; }
                p++;
                while (data[p] != 0) p++;
                p++;
                int size = BitConverter.ToInt32(data, p);
                p += 4 + size;
            }
            int lpb = ExrWriter.LinesPerBlock(ExrCompression.Zip);
            int blocks = (H + lpb - 1) / lpb;
            var offsets = new long[blocks];
            for (int i = 0; i < blocks; i++) { offsets[i] = BitConverter.ToInt64(data, p); p += 8; }

            var px = new float[W * H];
            foreach (long off in offsets)
            {
                int o = (int)off;
                int y0 = BitConverter.ToInt32(data, o); o += 4;
                int sz = BitConverter.ToInt32(data, o); o += 4;
                int lines = Math.Min(lpb, H - y0);
                int expect = lines * W * 2;
                byte[] raw;
                if (sz == expect)
                {
                    raw = new byte[expect];
                    System.Buffer.BlockCopy(data, o, raw, 0, expect);
                }
                else
                {
                    var z = new byte[sz];
                    System.Buffer.BlockCopy(data, o, z, 0, sz);
                    using (var ms = new MemoryStream(z, 2, z.Length - 6))
                    using (var ds = new System.IO.Compression.DeflateStream(
                               ms, System.IO.Compression.CompressionMode.Decompress))
                    {
                        var inflated = new byte[expect];
                        int read = 0, n;
                        while (read < expect && (n = ds.Read(inflated, read, expect - read)) > 0) read += n;
                        raw = new byte[expect];
                        ExrWriter.UnpredictAndUnreorder(inflated, expect, raw);
                    }
                }
                for (int i = 0; i < lines; i++)
                    for (int x = 0; x < W; x++)
                    {
                        int idx = (i * W + x) * 2;
                        px[(y0 + i) * W + x] =
                            HalfFloat.ToFloat((ushort)(raw[idx] | (raw[idx + 1] << 8)));
                    }
            }
            return px;
        }

        static float ReadFirstPixel(string path) => ReadAllPixels(path)[0];
    }

    public class LaneKindTests
    {
        [Test]
        public void VideoSinkWantsEveryTick()
        {
            var sink = new VideoSink(new FfmpegPipe(new FfmpegPipeConfig()), 4);
            Assert.That(sink.WantsTick(0), Is.True);
            Assert.That(sink.WantsTick(1), Is.True);
            Assert.That(sink.ReadbackFormat, Is.EqualTo(UnityEngine.TextureFormat.RGBA32));
        }
    }
}
