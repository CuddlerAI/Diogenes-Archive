/**
 * 浏览器侧的 OpenEXR 解码器，只覆盖 depth-lab 写出的那一种形状
 * （docs/85 §2）：单通道、HALF、ZIP16、scanline、行序递增。
 *
 * 为什么手写而不是引一个库：解码这一种形状只要下面这些代码，而引一个通用
 * EXR 库要为 console 的包体加上几百 KB，且它们大多依赖 Node 的 zlib。
 * 浏览器原生的 DecompressionStream 已经够用。
 *
 * **不做格式猜测**：遇到任何不是上述形状的文件就抛错，而不是尽力而为地渲染。
 * 一张解错的深度图看起来仍然像一张深度图——它不会崩，只会让人对着错的数据下判断。
 */

const MAGIC = 0x01312f76;
/** ZIP16 每块 16 行。ZIPS 是 1 行，我们不写它，因此也不接受它。 */
const ZIP16_LINES_PER_BLOCK = 16;
const COMPRESSION_ZIP16 = 3;

export type ExrFrame = {
  width: number;
  height: number;
  /** 单通道数据，行序自上而下。 */
  data: Float32Array;
  channel: string;
};

class Reader {
  offset = 0;
  private readonly view: DataView;
  private readonly bytes: Uint8Array;

  constructor(buffer: ArrayBuffer) {
    this.view = new DataView(buffer);
    this.bytes = new Uint8Array(buffer);
  }

  u32(): number {
    const v = this.view.getUint32(this.offset, true);
    this.offset += 4;
    return v;
  }

  i32(): number {
    const v = this.view.getInt32(this.offset, true);
    this.offset += 4;
    return v;
  }

  u64(): number {
    // 偏移表是 u64。EXR 文件不会大到超过 2^53，所以拼成 number 是安全的。
    const lo = this.view.getUint32(this.offset, true);
    const hi = this.view.getUint32(this.offset + 4, true);
    this.offset += 8;
    return hi * 2 ** 32 + lo;
  }

  u8(): number {
    return this.bytes[this.offset++]!;
  }

  /** 以 NUL 结尾的字符串。 */
  str(): string {
    let end = this.offset;
    while (this.bytes[end] !== 0) end++;
    const s = new TextDecoder().decode(this.bytes.subarray(this.offset, end));
    this.offset = end + 1;
    return s;
  }

  slice(length: number): Uint8Array {
    const s = this.bytes.subarray(this.offset, this.offset + length);
    this.offset += length;
    return s;
  }
}

function halfToFloat(half: number): number {
  const sign = half & 0x8000 ? -1 : 1;
  const exponent = (half >>> 10) & 0x1f;
  const mantissa = half & 0x3ff;
  if (exponent === 0) return sign * 2 ** -14 * (mantissa / 1024);
  if (exponent === 0x1f) return mantissa ? Number.NaN : sign * Infinity;
  return sign * 2 ** (exponent - 15) * (1 + mantissa / 1024);
}

async function inflate(data: Uint8Array): Promise<Uint8Array> {
  // EXR 的 ZIP 是带 zlib 包头的 deflate，对应 "deflate"（"deflate-raw" 是不带包头的）。
  const stream = new Blob([data as BlobPart])
    .stream()
    .pipeThrough(new DecompressionStream("deflate"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

/**
 * 撤销 EXR ZIP 的两道前置滤波。**顺序不能反**：压缩时是先重排后差分，
 * 解压时必须先积分后重排，反过来得到的是一张看起来合理但完全错的图。
 */
function undoZipFilters(raw: Uint8Array): Uint8Array {
  // 1. 撤 delta predictor
  for (let i = 1; i < raw.length; i++) {
    raw[i] = (raw[i - 1]! + raw[i]! - 128) & 0xff;
  }
  // 2. 撤字节重排：前半段是偶数位，后半段是奇数位
  const out = new Uint8Array(raw.length);
  const half = Math.floor((raw.length + 1) / 2);
  let t1 = 0;
  let t2 = half;
  for (let i = 0; i < raw.length; ) {
    out[i++] = raw[t1++]!;
    if (i < raw.length) out[i++] = raw[t2++]!;
  }
  return out;
}

/** 解一帧 EXR。只接受 depth-lab 写出的形状，其余一律抛错。 */
export async function decodeExrHalfZip(buffer: ArrayBuffer): Promise<ExrFrame> {
  const r = new Reader(buffer);
  if (r.u32() !== MAGIC) throw new Error("不是 EXR 文件（magic 不符）");
  r.u32(); // version + flags：我们写的是单部件 scanline，不需要读标志位

  let dataWindow: [number, number, number, number] | null = null;
  let compression: number | null = null;
  const channels: { name: string; pixelType: number }[] = [];

  for (;;) {
    const name = r.str();
    if (name === "") break; // 空名 = 头结束
    const type = r.str();
    const size = r.i32();
    const end = r.offset + size;
    if (name === "dataWindow") {
      dataWindow = [r.i32(), r.i32(), r.i32(), r.i32()];
    } else if (name === "compression") {
      compression = r.u8();
    } else if (name === "channels") {
      for (;;) {
        const channelName = r.str();
        if (channelName === "") break;
        const pixelType = r.i32();
        r.offset += 4 + 4 + 4; // pLinear + reserved + xSampling/ySampling
        channels.push({ name: channelName, pixelType });
      }
    }
    void type;
    r.offset = end;
  }

  if (!dataWindow) throw new Error("EXR 头里没有 dataWindow");
  if (compression !== COMPRESSION_ZIP16) {
    throw new Error(`只支持 ZIP16 压缩，实得编码 ${compression}`);
  }
  if (channels.length !== 1) {
    throw new Error(`只支持单通道，实得 ${channels.length} 个：${channels.map((c) => c.name).join(",")}`);
  }
  const channel = channels[0]!;
  // pixelType: 0=UINT 1=HALF 2=FLOAT
  if (channel.pixelType !== 1) {
    throw new Error(`只支持 HALF 通道，实得 pixelType ${channel.pixelType}`);
  }

  const [xMin, yMin, xMax, yMax] = dataWindow;
  const width = xMax - xMin + 1;
  const height = yMax - yMin + 1;
  const blockCount = Math.ceil(height / ZIP16_LINES_PER_BLOCK);

  const offsets: number[] = [];
  for (let i = 0; i < blockCount; i++) offsets.push(r.u64());

  const out = new Float32Array(width * height);
  for (const offset of offsets) {
    const b = new Reader(buffer);
    b.offset = offset;
    const y = b.i32() - yMin;
    const dataSize = b.i32();
    const lines = Math.min(ZIP16_LINES_PER_BLOCK, height - y);
    const uncompressedSize = lines * width * 2;

    let bytes = b.slice(dataSize);
    if (dataSize < uncompressedSize) {
      // 压缩后更大时 EXR 会原样存，此时既没压也没滤波
      bytes = undoZipFilters(await inflate(bytes));
    }

    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    for (let line = 0; line < lines; line++) {
      const rowStart = (y + line) * width;
      const src = line * width * 2;
      for (let x = 0; x < width; x++) {
        out[rowStart + x] = halfToFloat(view.getUint16(src + x * 2, true));
      }
    }
  }

  return { width, height, data: out, channel: channel.name };
}
