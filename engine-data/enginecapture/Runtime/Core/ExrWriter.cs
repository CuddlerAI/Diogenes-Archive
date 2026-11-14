// Pure C# (no UnityEngine dependency) so the byte-level container contract can be
// unit-tested outside Unity. Spec source: docs/68 §6.4 (SFM-Depth delivery) —
// "EXR (OpenEXR, single channel)", channel named Y or Z, float16, NaN for invalid.
//
// Why hand-written instead of Texture2D.EncodeToEXR: EncodeToEXR emits RGBA half
// (4x the bytes, wrong channel layout). Post-processing every frame with an external
// CLI would add ~15 GB of pointless I/O per 180 s clip plus a second binary dependency
// (docs/83 debate 15).
//
// Compression is ZIP, matching the encoder answer fixed in docs/75 question 4.
// Only System.IO.Compression is used: DeflateStream emits a RAW deflate stream, so
// the zlib framing (2-byte header + big-endian Adler-32 trailer) is added here —
// .NET's ZLibStream is .NET 6+ and not available on Unity's .NET Standard 2.1 profile.

using System;
using System.IO;
using System.IO.Compression;
using System.Text;

namespace Diogenes.EngineCapture
{
    /// <summary>OpenEXR compression codes (only the ones we emit).</summary>
    public enum ExrCompression
    {
        None = 0,
        /// <summary>ZIP, one scanline per block.</summary>
        Zips = 2,
        /// <summary>ZIP, 16 scanlines per block. Delivery default (docs/75 Q4).</summary>
        Zip = 3
    }

    /// <summary>
    /// Minimal single-channel half scanline OpenEXR writer.
    ///
    /// Layout written (OpenEXR 2.x scanline, single part, not tiled):
    ///   magic(4) version(4) | attributes... 0x00 | offset table (int64 per block)
    ///   | per block: int32 yStart, int32 dataSize, data
    ///
    /// The instance form reuses its scratch buffers across frames — at 15 fps and
    /// 1280x720 a fresh allocation per frame is 1.8 MB of garbage per frame
    /// (docs/68 §6.5 performance note explicitly asks for pooling).
    /// </summary>
    public sealed class ExrWriter
    {
        const int Magic = 0x01312F76;
        const int Version = 2;                 // scanline, single part, short names
        const int PixelTypeHalf = 1;

        readonly int _width;
        readonly int _height;
        readonly string _channel;
        readonly ExrCompression _compression;
        readonly int _linesPerBlock;
        readonly int _blockCount;

        readonly byte[] _raw;                  // uncompressed block bytes
        readonly byte[] _tmp;                  // reorder + predictor scratch
        readonly MemoryStream _deflate = new MemoryStream();

        /// <summary>Scanlines per compressed block for a codec (OpenEXR fixed values).</summary>
        public static int LinesPerBlock(ExrCompression c) => c == ExrCompression.Zip ? 16 : 1;

        public ExrWriter(int width, int height,
                         string channel = "Y",
                         ExrCompression compression = ExrCompression.Zip)
        {
            if (width <= 0 || height <= 0)
                throw new ArgumentOutOfRangeException(nameof(width), "EXR dimensions must be positive");
            if (string.IsNullOrEmpty(channel))
                throw new ArgumentException("channel name required", nameof(channel));

            _width = width;
            _height = height;
            _channel = channel;
            _compression = compression;
            _linesPerBlock = LinesPerBlock(compression);
            _blockCount = (height + _linesPerBlock - 1) / _linesPerBlock;

            int maxBlockBytes = _linesPerBlock * width * 2;
            _raw = new byte[maxBlockBytes];
            _tmp = new byte[maxBlockBytes];
        }

        public int Width => _width;
        public int Height => _height;
        public string Channel => _channel;
        public ExrCompression Compression => _compression;

        /// <summary>
        /// Encode <paramref name="pixels"/> (row-major, y=0 is the top scanline,
        /// length >= width*height) into a complete EXR byte stream.
        /// NaN values pass through as half NaN — the docs/68 invalid marker.
        /// </summary>
        public byte[] Encode(float[] pixels)
        {
            if (pixels == null) throw new ArgumentNullException(nameof(pixels));
            if (pixels.Length < _width * _height)
                throw new ArgumentException(
                    "pixel buffer holds " + pixels.Length + " floats, need " + (_width * _height),
                    nameof(pixels));

            byte[] header = BuildHeader();
            var blocks = new byte[_blockCount][];
            var blockY = new int[_blockCount];

            for (int b = 0; b < _blockCount; b++)
            {
                int y0 = b * _linesPerBlock;
                int lines = Math.Min(_linesPerBlock, _height - y0);
                int rawLen = lines * _width * 2;

                for (int i = 0; i < lines; i++)
                {
                    int src = (y0 + i) * _width;
                    int dst = i * _width * 2;
                    for (int x = 0; x < _width; x++)
                    {
                        ushort h = HalfFloat.FromFloat(pixels[src + x]);
                        _raw[dst++] = (byte)(h & 0xFF);          // little-endian
                        _raw[dst++] = (byte)(h >> 8);
                    }
                }

                blockY[b] = y0;
                blocks[b] = _compression == ExrCompression.None
                    ? Slice(_raw, rawLen)
                    : CompressBlock(_raw, rawLen);
            }

            long offset = header.Length + 8L * _blockCount;
            using (var ms = new MemoryStream(header.Length + 8 * _blockCount + RawTotal(blocks)))
            {
                ms.Write(header, 0, header.Length);
                for (int b = 0; b < _blockCount; b++)
                {
                    WriteInt64(ms, offset);
                    offset += 8 + blocks[b].Length;               // int32 y + int32 size + data
                }
                for (int b = 0; b < _blockCount; b++)
                {
                    WriteInt32(ms, blockY[b]);
                    WriteInt32(ms, blocks[b].Length);
                    ms.Write(blocks[b], 0, blocks[b].Length);
                }
                return ms.ToArray();
            }
        }

        /// <summary>Encode and write to disk (directory must exist).</summary>
        public void WriteFile(string path, float[] pixels) =>
            File.WriteAllBytes(path, Encode(pixels));

        static int RawTotal(byte[][] blocks)
        {
            int n = 0;
            for (int i = 0; i < blocks.Length; i++) n += blocks[i].Length + 8;
            return n;
        }

        // ------------------------------------------------------------------ header

        byte[] BuildHeader()
        {
            using (var ms = new MemoryStream(320))
            {
                WriteInt32(ms, Magic);
                WriteInt32(ms, Version);

                // channels: chlist = [name\0 pixelType(i32) pLinear(u8) reserved(3) xSampling(i32) ySampling(i32)] ... \0
                using (var ch = new MemoryStream(32))
                {
                    WriteNullString(ch, _channel);
                    WriteInt32(ch, PixelTypeHalf);
                    ch.WriteByte(0);                       // pLinear
                    ch.WriteByte(0); ch.WriteByte(0); ch.WriteByte(0);   // reserved
                    WriteInt32(ch, 1);                     // xSampling
                    WriteInt32(ch, 1);                     // ySampling
                    ch.WriteByte(0);                       // chlist terminator
                    WriteAttribute(ms, "channels", "chlist", ch.ToArray());
                }

                WriteAttribute(ms, "compression", "compression", new[] { (byte)_compression });

                byte[] box = Box2i(0, 0, _width - 1, _height - 1);
                WriteAttribute(ms, "dataWindow", "box2i", box);
                WriteAttribute(ms, "displayWindow", "box2i", box);
                WriteAttribute(ms, "lineOrder", "lineOrder", new byte[] { 0 });   // INCREASING_Y
                WriteAttribute(ms, "pixelAspectRatio", "float", FloatBytes(1.0f));
                WriteAttribute(ms, "screenWindowCenter", "v2f", V2f(0.0f, 0.0f));
                WriteAttribute(ms, "screenWindowWidth", "float", FloatBytes(1.0f));

                ms.WriteByte(0);                           // end of header
                return ms.ToArray();
            }
        }

        static void WriteAttribute(Stream s, string name, string type, byte[] payload)
        {
            WriteNullString(s, name);
            WriteNullString(s, type);
            WriteInt32(s, payload.Length);
            s.Write(payload, 0, payload.Length);
        }

        // ------------------------------------------------------------- ZIP codec

        /// <summary>
        /// OpenEXR ImfZipCompressor: byte reorder, then first-order predictor, then
        /// zlib deflate. If the compressed form is not smaller the raw bytes are
        /// stored instead — readers detect that by comparing the chunk's data size
        /// against the block's uncompressed size.
        /// </summary>
        byte[] CompressBlock(byte[] raw, int len)
        {
            ReorderAndPredict(raw, len, _tmp);
            byte[] z = ZlibCompress(_tmp, len);
            return z.Length < len ? z : Slice(raw, len);
        }

        /// <summary>
        /// The deterministic, zlib-independent half of ZIP: byte reorder followed by a
        /// first-order predictor, writing <paramref name="len"/> bytes into
        /// <paramref name="tmp"/>. Public so the golden-vector test can pin it down
        /// without depending on any particular deflate implementation's output bytes.
        /// </summary>
        public static void ReorderAndPredict(byte[] raw, int len, byte[] tmp)
        {
            // 1. reorder: even-indexed bytes into the first half, odd into the second
            int half = (len + 1) / 2;
            int t1 = 0, t2 = half, s = 0;
            while (true)
            {
                if (s < len) tmp[t1++] = raw[s++]; else break;
                if (s < len) tmp[t2++] = raw[s++]; else break;
            }

            // 2. predictor: delta against the previous byte, biased to stay in [0,255]
            int p = len > 0 ? tmp[0] : 0;
            for (int i = 1; i < len; i++)
            {
                int d = tmp[i] - p + (128 + 256);
                p = tmp[i];
                tmp[i] = (byte)d;
            }
        }

        /// <summary>Inverse of <see cref="ReorderAndPredict"/> (test / QC side).</summary>
        public static void UnpredictAndUnreorder(byte[] tmp, int len, byte[] raw)
        {
            for (int i = 1; i < len; i++)
                tmp[i] = (byte)(tmp[i] + tmp[i - 1] - 128);

            int half = (len + 1) / 2;
            int t1 = 0, t2 = half, s = 0;
            while (true)
            {
                if (s < len) raw[s++] = tmp[t1++]; else break;
                if (s < len) raw[s++] = tmp[t2++]; else break;
            }
        }

        byte[] ZlibCompress(byte[] data, int len)
        {
            _deflate.SetLength(0);
            _deflate.WriteByte(0x78);              // CMF: deflate, 32K window
            _deflate.WriteByte(0x9C);              // FLG: default level; (0x78<<8|0x9C) % 31 == 0
            using (var ds = new DeflateStream(_deflate, CompressionLevel.Optimal, leaveOpen: true))
                ds.Write(data, 0, len);
            uint adler = Adler32(data, len);
            _deflate.WriteByte((byte)(adler >> 24));   // zlib trailer is big-endian
            _deflate.WriteByte((byte)(adler >> 16));
            _deflate.WriteByte((byte)(adler >> 8));
            _deflate.WriteByte((byte)adler);
            return _deflate.ToArray();
        }

        public static uint Adler32(byte[] data, int len)
        {
            const uint Mod = 65521;
            uint a = 1, b = 0;
            for (int i = 0; i < len; i++)
            {
                a += data[i];
                if (a >= Mod) a -= Mod;
                b += a;
                if (b >= Mod) b -= Mod;
            }
            return (b << 16) | a;
        }

        // --------------------------------------------------------------- helpers

        static byte[] Slice(byte[] src, int len)
        {
            var outBuf = new byte[len];
            Buffer.BlockCopy(src, 0, outBuf, 0, len);
            return outBuf;
        }

        static void WriteNullString(Stream s, string v)
        {
            byte[] b = Encoding.ASCII.GetBytes(v);
            s.Write(b, 0, b.Length);
            s.WriteByte(0);
        }

        static void WriteInt32(Stream s, int v)
        {
            s.WriteByte((byte)v);
            s.WriteByte((byte)(v >> 8));
            s.WriteByte((byte)(v >> 16));
            s.WriteByte((byte)(v >> 24));
        }

        static void WriteInt64(Stream s, long v)
        {
            for (int i = 0; i < 8; i++) s.WriteByte((byte)(v >> (8 * i)));
        }

        static byte[] Box2i(int xMin, int yMin, int xMax, int yMax)
        {
            var b = new byte[16];
            WriteInt32ToArray(b, 0, xMin);
            WriteInt32ToArray(b, 4, yMin);
            WriteInt32ToArray(b, 8, xMax);
            WriteInt32ToArray(b, 12, yMax);
            return b;
        }

        static byte[] V2f(float x, float y)
        {
            var b = new byte[8];
            Buffer.BlockCopy(FloatBytes(x), 0, b, 0, 4);
            Buffer.BlockCopy(FloatBytes(y), 0, b, 4, 4);
            return b;
        }

        static byte[] FloatBytes(float v)
        {
            byte[] b = BitConverter.GetBytes(v);
            if (!BitConverter.IsLittleEndian) Array.Reverse(b);
            return b;
        }

        static void WriteInt32ToArray(byte[] b, int off, int v)
        {
            b[off] = (byte)v;
            b[off + 1] = (byte)(v >> 8);
            b[off + 2] = (byte)(v >> 16);
            b[off + 3] = (byte)(v >> 24);
        }
    }
}
