// Golden vectors come from a Python reference implementation of the same algorithm
// that was cross-checked byte-for-byte against ffmpeg's independent OpenEXR decoder
// (NONE / ZIPS / ZIP, 64x40 and 1280x720, 0 pixel mismatches). See docs/83 §3.4.
//
// The compressed bytes themselves are deliberately NOT asserted: two conforming
// deflate implementations may emit different (equally valid) streams, so pinning
// them would test zlib rather than the EXR contract. What IS pinned:
//   * the header bytes (no compression involved);
//   * ReorderAndPredict, the deterministic zlib-independent half of ZIP;
//   * half conversion, including round-to-nearest-even and NaN;
//   * a full decode round-trip through a reader written independently in this file.

using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using Diogenes.EngineCapture;
using NUnit.Framework;

namespace Diogenes.EngineCapture.Tests
{
    public class HalfFloatTests
    {
        [TestCase(0.0f, (ushort)0x0000)]
        [TestCase(1.0f, (ushort)0x3C00)]
        [TestCase(-2.0f, (ushort)0xC000)]
        [TestCase(3.67f, (ushort)0x4357)]     // docs/68 §6.4 worked example, 3.678 -> 3.67
        [TestCase(0.01f, (ushort)0x211F)]
        [TestCase(65504.0f, (ushort)0x7BFF)]  // largest finite half
        [TestCase(6.1035156e-05f, (ushort)0x0400)]  // smallest normal
        [TestCase(5.9604645e-08f, (ushort)0x0001)]  // smallest subnormal
        [TestCase(1e-9f, (ushort)0x0000)]     // underflow -> +0
        [TestCase(1e9f, (ushort)0x7C00)]      // overflow -> +Inf
        public void FromFloat_MatchesGolden(float input, ushort expected)
        {
            Assert.That(HalfFloat.FromFloat(input), Is.EqualTo(expected));
        }

        [TestCase(2049.0f, 2048.0f)]   // exact tie -> even
        [TestCase(2051.0f, 2052.0f)]   // exact tie -> even (upward)
        public void FromFloat_RoundsToNearestEven(float input, float expected)
        {
            Assert.That(HalfFloat.ToFloat(HalfFloat.FromFloat(input)), Is.EqualTo(expected));
        }

        [Test]
        public void NaN_IsCanonicalisedRegardlessOfSign()
        {
            // .NET's float.NaN has the sign bit SET; a sign-preserving conversion would
            // emit 0xFE00 and make the delivered bytes depend on which NaN the caller
            // held. docs/68 §6.4 wants invalid pixels "uniformly marked NaN".
            foreach (float nan in new[] { float.NaN, -float.NaN, 0f / 0f })
            {
                ushort h = HalfFloat.FromFloat(nan);
                Assert.That(h, Is.EqualTo(HalfFloat.NaNBits), "NaN must canonicalise to 0x7E00");
                Assert.That(HalfFloat.IsNaN(h), Is.True);
                Assert.That(float.IsNaN(HalfFloat.ToFloat(h)), Is.True);
            }
        }

        [Test]
        public void NegativeInfinity_KeepsItsSign()
        {
            Assert.That(HalfFloat.FromFloat(float.NegativeInfinity), Is.EqualTo((ushort)0xFC00));
        }

        [Test]
        public void Infinity_IsNotFlaggedAsNaN()
        {
            ushort h = HalfFloat.FromFloat(float.PositiveInfinity);
            Assert.That(h, Is.EqualTo((ushort)0x7C00));
            Assert.That(HalfFloat.IsNaN(h), Is.False);
        }

        [Test]
        public void RoundTrip_IsIdempotentOverRepresentableValues()
        {
            // every finite half must survive half -> float -> half unchanged
            for (int bits = 0; bits <= 0xFFFF; bits++)
            {
                ushort h = (ushort)bits;
                if (HalfFloat.IsNaN(h)) continue;                 // NaN payloads are not preserved
                float f = HalfFloat.ToFloat(h);
                if (float.IsInfinity(f)) continue;
                Assert.That(HalfFloat.FromFloat(f), Is.EqualTo(h), "bits 0x" + bits.ToString("X4"));
            }
        }
    }

    public class ExrZipTransformTests
    {
        [Test]
        public void ReorderAndPredict_MatchesGoldenVector()
        {
            // src[i] = (i*37 + 11) & 0xFF, length 37 (odd -> exercises the tail branch)
            var src = new byte[37];
            for (int i = 0; i < src.Length; i++) src[i] = (byte)((i * 37 + 11) & 0xFF);

            var golden = new byte[]
            {
                11, 202, 202, 202, 202, 202, 202, 202, 202, 202, 202, 202, 202, 202,
                202, 202, 202, 202, 202, 113, 202, 202, 202, 202, 202, 202, 202, 202,
                202, 202, 202, 202, 202, 202, 202, 202, 202
            };

            var tmp = new byte[src.Length];
            ExrWriter.ReorderAndPredict(src, src.Length, tmp);
            Assert.That(tmp, Is.EqualTo(golden));
        }

        [Test]
        public void ReorderAndPredict_IsInvertible()
        {
            var rng = new System.Random(7);
            foreach (int len in new[] { 0, 1, 2, 3, 37, 256, 40960 })
            {
                var src = new byte[len];
                rng.NextBytes(src);
                var tmp = new byte[len];
                var back = new byte[len];
                ExrWriter.ReorderAndPredict(src, len, tmp);
                ExrWriter.UnpredictAndUnreorder(tmp, len, back);
                Assert.That(back, Is.EqualTo(src), "len=" + len);
            }
        }

        [Test]
        public void Adler32_MatchesKnownValue()
        {
            // zlib.adler32(b"abc") == 0x024D0127
            byte[] abc = Encoding.ASCII.GetBytes("abc");
            Assert.That(ExrWriter.Adler32(abc, abc.Length), Is.EqualTo(0x024D0127u));
        }
    }

    public class ExrWriterTests
    {
        /// <summary>Header for 1280x720 single-channel "Y" half ZIP, from the reference impl.</summary>
        const string GoldenHeader1280x720Zip =
            "762f3101020000006368616e6e656c730063686c697374001300000059000100000000000000" +
            "010000000100000000636f6d7072657373696f6e00636f6d7072657373696f6e000100000003" +
            "6461746157696e646f7700626f78326900100000000000000000000000ff040000cf020000" +
            "646973706c617957696e646f7700626f78326900100000000000000000000000ff040000cf020000" +
            "6c696e654f72646572006c696e654f7264657200010000000070697865" +
            "6c417370656374526174696f00666c6f617400040000000000803f" +
            "73637265656e57696e646f7743656e746572007632660008000000000000000000000073" +
            "637265656e57696e646f77576964746800666c6f617400040000000000803f00";

        static string Hex(byte[] b, int len)
        {
            var sb = new StringBuilder(len * 2);
            for (int i = 0; i < len; i++) sb.Append(b[i].ToString("x2"));
            return sb.ToString();
        }

        [Test]
        public void Header_MatchesGoldenBytes()
        {
            var w = new ExrWriter(1280, 720, "Y", ExrCompression.Zip);
            byte[] exr = w.Encode(new float[1280 * 720]);
            string expect = GoldenHeader1280x720Zip;
            Assert.That(Hex(exr, expect.Length / 2), Is.EqualTo(expect));
        }

        [Test]
        public void Encode_RejectsShortPixelBuffer()
        {
            var w = new ExrWriter(8, 8);
            Assert.Throws<ArgumentException>(() => w.Encode(new float[63]));
        }

        [TestCase(ExrCompression.None)]
        [TestCase(ExrCompression.Zips)]
        [TestCase(ExrCompression.Zip)]
        public void RoundTrip_PreservesEveryPixel(ExrCompression compression)
        {
            // 64x40: height is not a multiple of 16, so the last ZIP block is partial.
            const int W = 64, H = 40;
            var px = new float[W * H];
            for (int y = 0; y < H; y++)
                for (int x = 0; x < W; x++)
                    px[y * W + x] = y < 3
                        ? float.NaN                          // sky band (docs/68 invalid marker)
                        : (float)Math.Truncate((0.5 + x * 0.37 + y * 1.1) * 100.0) / 100f;

            var w = new ExrWriter(W, H, "Y", compression);
            var img = MiniExrReader.Read(w.Encode(px));

            Assert.That(img.Width, Is.EqualTo(W));
            Assert.That(img.Height, Is.EqualTo(H));
            Assert.That(img.Channel, Is.EqualTo("Y"));
            Assert.That(img.PixelType, Is.EqualTo(1), "HALF");
            Assert.That((int)img.Compression, Is.EqualTo((int)compression));

            for (int i = 0; i < px.Length; i++)
            {
                float expect = HalfFloat.ToFloat(HalfFloat.FromFloat(px[i]));
                if (float.IsNaN(expect)) Assert.That(float.IsNaN(img.Pixels[i]), Is.True, "i=" + i);
                else Assert.That(img.Pixels[i], Is.EqualTo(expect), "i=" + i);
            }
        }

        [Test]
        public void Zip_CompressesDepthLikeContent()
        {
            const int W = 128, H = 64;
            var px = new float[W * H];
            for (int y = 0; y < H; y++)
                for (int x = 0; x < W; x++)
                    px[y * W + x] = (float)Math.Truncate((3.0 + 60.0 / (y / (double)H + 0.05)) * 100.0) / 100f;

            int raw = new ExrWriter(W, H, "Y", ExrCompression.None).Encode(px).Length;
            int zip = new ExrWriter(W, H, "Y", ExrCompression.Zip).Encode(px).Length;
            Assert.That(zip, Is.LessThan(raw), "ZIP must shrink smooth depth content");
        }

        [Test]
        public void Writer_IsReusableAcrossFrames()
        {
            // the scratch buffers are reused; a second Encode must not be corrupted by the first
            const int W = 32, H = 20;
            var a = new float[W * H];
            var b = new float[W * H];
            for (int i = 0; i < a.Length; i++) { a[i] = i * 0.01f; b[i] = 100f - i * 0.02f; }

            var w = new ExrWriter(W, H, "Y", ExrCompression.Zip);
            w.Encode(a);
            var second = MiniExrReader.Read(w.Encode(b));
            for (int i = 0; i < b.Length; i++)
                Assert.That(second.Pixels[i], Is.EqualTo(HalfFloat.ToFloat(HalfFloat.FromFloat(b[i]))), "i=" + i);
        }
    }

    /// <summary>
    /// Deliberately independent minimal reader used only by these tests — writing the
    /// round-trip against the writer's own internals would prove nothing.
    /// </summary>
    static class MiniExrReader
    {
        public sealed class Image
        {
            public int Width, Height, PixelType;
            public string Channel;
            public ExrCompression Compression;
            public float[] Pixels;
        }

        public static Image Read(byte[] data)
        {
            int p = 0;
            Assert.That(ReadInt32(data, ref p), Is.EqualTo(0x01312F76), "magic");
            Assert.That(ReadInt32(data, ref p) & 0xFF, Is.EqualTo(2), "version");

            var attrs = new Dictionary<string, KeyValuePair<string, byte[]>>();
            while (true)
            {
                string name = ReadNullString(data, ref p);
                if (name.Length == 0) break;
                string type = ReadNullString(data, ref p);
                int size = ReadInt32(data, ref p);
                var payload = new byte[size];
                Buffer.BlockCopy(data, p, payload, 0, size);
                p += size;
                attrs[name] = new KeyValuePair<string, byte[]>(type, payload);
            }

            byte[] dw = attrs["dataWindow"].Value;
            int q = 0;
            int xMin = ReadInt32(dw, ref q), yMin = ReadInt32(dw, ref q);
            int xMax = ReadInt32(dw, ref q), yMax = ReadInt32(dw, ref q);
            int w = xMax - xMin + 1, h = yMax - yMin + 1;

            var comp = (ExrCompression)attrs["compression"].Value[0];

            byte[] ch = attrs["channels"].Value;
            int c = 0;
            string channel = ReadNullString(ch, ref c);
            int pixelType = ReadInt32(ch, ref c);

            int lpb = ExrWriter.LinesPerBlock(comp);
            int blocks = (h + lpb - 1) / lpb;
            var offsets = new long[blocks];
            for (int i = 0; i < blocks; i++) offsets[i] = ReadInt64(data, ref p);

            var pixels = new float[w * h];
            foreach (long off in offsets)
            {
                int o = (int)off;
                int y0 = ReadInt32(data, ref o);
                int size = ReadInt32(data, ref o);
                int lines = Math.Min(lpb, h - y0);
                int expect = lines * w * 2;

                byte[] raw;
                if (size == expect)                       // stored uncompressed
                {
                    raw = new byte[expect];
                    Buffer.BlockCopy(data, o, raw, 0, expect);
                }
                else
                {
                    var z = new byte[size];
                    Buffer.BlockCopy(data, o, z, 0, size);
                    byte[] inflated = ZlibInflate(z, expect);
                    raw = new byte[expect];
                    ExrWriter.UnpredictAndUnreorder(inflated, expect, raw);
                }

                for (int i = 0; i < lines; i++)
                    for (int x = 0; x < w; x++)
                    {
                        int idx = (i * w + x) * 2;
                        ushort bits = (ushort)(raw[idx] | (raw[idx + 1] << 8));
                        pixels[(y0 + i) * w + x] = HalfFloat.ToFloat(bits);
                    }
            }

            return new Image
            {
                Width = w, Height = h, Channel = channel, PixelType = pixelType,
                Compression = comp, Pixels = pixels
            };
        }

        static byte[] ZlibInflate(byte[] z, int expected)
        {
            // skip the 2-byte zlib header, ignore the 4-byte Adler trailer
            using (var ms = new MemoryStream(z, 2, z.Length - 6))
            using (var ds = new System.IO.Compression.DeflateStream(ms, System.IO.Compression.CompressionMode.Decompress))
            {
                var outBuf = new byte[expected];
                int read = 0;
                while (read < expected)
                {
                    int n = ds.Read(outBuf, read, expected - read);
                    if (n <= 0) break;
                    read += n;
                }
                Assert.That(read, Is.EqualTo(expected), "inflated size");
                return outBuf;
            }
        }

        static int ReadInt32(byte[] b, ref int p)
        {
            int v = b[p] | (b[p + 1] << 8) | (b[p + 2] << 16) | (b[p + 3] << 24);
            p += 4;
            return v;
        }

        static long ReadInt64(byte[] b, ref int p)
        {
            long v = 0;
            for (int i = 0; i < 8; i++) v |= (long)b[p + i] << (8 * i);
            p += 8;
            return v;
        }

        static string ReadNullString(byte[] b, ref int p)
        {
            int s = p;
            while (b[p] != 0) p++;
            string v = Encoding.ASCII.GetString(b, s, p - s);
            p++;
            return v;
        }
    }
}
