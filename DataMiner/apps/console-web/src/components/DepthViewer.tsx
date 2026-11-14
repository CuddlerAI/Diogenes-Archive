import { useEffect, useMemo, useRef, useState } from "react";

import { api, describeApiError } from "../api.js";
import { EmptyState, ErrorNote, Loading, StateBadge } from "../components.js";
import { decodeExrHalfZip } from "./exr.js";

/**
 * 深度查看器 (docs/77 §5.5): render one `raw/<present_seq>.f32` frame from a
 * dm-depth-probe capture, beside the `depth-index.jsonl` that indexes it.
 *
 * What this is NOT: a viewer for the delivered EXR. The probe writes the raw
 * depth buffer — non-linear, unitless, sky left at the clear value — and
 * `depth-lab linearize` is what turns it into the fp16 metric EXR docs/68 §6.4
 * asks for. Both forms are useful to look at, and confusing them is the whole
 * reason the spec table at the bottom exists.
 */

// --- half precision (mirrors depth_lab::half) -------------------------------

const scratchF32 = new Float32Array(1);
const scratchU32 = new Uint32Array(scratchF32.buffer);

function f32ToF16Bits(value: number): number {
  scratchF32[0] = value;
  const bits = scratchU32[0]!;
  const sign = (bits >>> 16) & 0x8000;
  const exponent = (bits >>> 23) & 0xff;
  let mantissa = bits & 0x7fffff;
  if (exponent === 0xff) return sign | 0x7c00 | (mantissa ? 0x0200 : 0);
  const scaled = exponent - 127 + 15;
  if (scaled >= 0x1f) return sign | 0x7c00;
  if (scaled <= 0) {
    if (scaled < -10) return sign;
    mantissa |= 0x800000;
    const shift = 14 - scaled;
    let half = mantissa >>> shift;
    const remainder = mantissa & ((1 << shift) - 1);
    const midpoint = 1 << (shift - 1);
    if (remainder > midpoint || (remainder === midpoint && (half & 1))) half++;
    return sign | half;
  }
  let half = (scaled << 10) | (mantissa >>> 13);
  const remainder = mantissa & 0x1fff;
  // Ties to even, and a mantissa carry is allowed to bump the exponent — that
  // is the correct rounding, not an overflow.
  if (remainder > 0x1000 || (remainder === 0x1000 && (half & 1))) half++;
  return sign | half;
}

function f16BitsToF32(half: number): number {
  const sign = half & 0x8000 ? -1 : 1;
  const exponent = (half >>> 10) & 0x1f;
  const mantissa = half & 0x3ff;
  if (exponent === 0) return sign * 2 ** -14 * (mantissa / 1024);
  if (exponent === 0x1f) return mantissa ? Number.NaN : sign * Infinity;
  return sign * 2 ** (exponent - 15) * (1 + mantissa / 1024);
}

const quantizeF16 = (value: number) => f16BitsToF32(f32ToF16Bits(value));
const truncateTwoDecimals = (value: number) => Math.trunc(value * 100) / 100;
/** 2^ceil(log2(0.01·1024)) = 16 m — beyond it fp16 steps exceed 1 cm (§5.5.6). */
const F16_ONE_CM_LIMIT = 16;

// --- rendering --------------------------------------------------------------

const TURBO: Array<[number, number, number]> = [
  [48, 18, 59],
  [70, 107, 227],
  [41, 175, 214],
  [78, 222, 146],
  [176, 241, 64],
  [249, 199, 45],
  [249, 120, 32],
  [212, 47, 8],
  [122, 4, 3],
];
/** Invalid pixels get a colour no colormap produces, so "no depth here" can
 *  never be mistaken for "some depth here". */
const INVALID_RGB: [number, number, number] = [255, 0, 200];

function colormapRGB(t: number, mode: string): [number, number, number] {
  const clamped = t < 0 ? 0 : t > 1 ? 1 : t;
  if (mode === "gray") {
    const value = Math.round(clamped * 255);
    return [value, value, value];
  }
  const scaled = clamped * (TURBO.length - 1);
  const index = Math.min(TURBO.length - 2, Math.floor(scaled));
  const fraction = scaled - index;
  const a = TURBO[index]!;
  const b = TURBO[index + 1]!;
  return [
    Math.round(a[0] + (b[0] - a[0]) * fraction),
    Math.round(a[1] + (b[1] - a[1]) * fraction),
    Math.round(a[2] + (b[2] - a[2]) * fraction),
  ];
}

type IndexRow = {
  present_seq: number;
  width: number;
  height: number;
  /** v1 用 `format`，v2 改叫 `buffer_format`（它描述来源 DSV，不是盘上的编码）。 */
  format?: string;
  buffer_format?: string;
  dsv_id: string;
  path: string;
  readback_latency_frames?: number;
  /** v2 起有；缺失即 v1（裸 f32）。 */
  schema?: string;
  source_frame?: number;
  storage?: { encoding: string; domain: string; channel: string; transform?: string };
};

/** v1 的帧是裸 f32，v2 是 fp16 EXR。索引说了算——**不靠扩展名猜**。 */
function frameEncoding(row: IndexRow): "raw-f32" | "exr-half-zip" {
  return row.storage?.encoding === "exr-half-zip" ? "exr-half-zip" : "raw-f32";
}

/**
 * 盘上的字节要怎么还原成缓冲值 `d`（94 号裁决）。
 *
 * 传统 Z 的帧存的是 `1−d`——fp16 存 `d` 会把深度层次量化没（20 m 处只剩 0.95 m
 * 分辨率）。**忘了还原不会报错**，只会让画面近处变远、远处变近，而看起来完全正常，
 * 所以这里宁可抛也不猜。
 *
 * v2/v3 缺这个键读作 identity 不是猜：那两版的 writer 只写原始 `d`，可查证。
 */
function resolveTransform(row: IndexRow): "identity" | "one_minus_d" {
  const declared = row.storage?.transform;
  if (declared === "identity" || declared === "one_minus_d") return declared;
  if (declared !== undefined) {
    throw new Error(`索引里的 storage.transform 是本查看器不认识的 \`${declared}\``);
  }
  if (row.schema === "depth-index/v4" || row.schema === "depth-index/v5") {
    throw new Error(
      `帧 ${row.present_seq} 是 ${row.schema} 却没有 storage.transform——该版本必填。` +
        "盘上是否做过 1−d 变换无法判断，按 identity 显示会把深度整体读反（94 号）",
    );
  }
  return "identity";
}

/** 盘上值 → 缓冲值。`1−d` 自反，但方向要在调用点看得出来。 */
function toBufferValues(stored: Float32Array, transform: "identity" | "one_minus_d"): Float32Array {
  if (transform === "identity") return stored;
  const out = new Float32Array(stored.length);
  // NaN 必须穿过去：68 号用它标无效像素，变成 0 会显示成贴着镜头的表面
  for (let i = 0; i < stored.length; i++) out[i] = 1 - stored[i]!;
  return out;
}

type Projection = { a: number; b: number; scale: number };

function linearize(d: number, projection: Projection): number {
  const denominator = d - projection.a;
  if (denominator === 0) return Number.NaN;
  const z = (projection.b / denominator) * projection.scale;
  return Number.isFinite(z) && z > 0 ? z : Number.NaN;
}

function formatNumber(value: number, digits = 4): string {
  if (!Number.isFinite(value)) return String(value);
  if (Math.abs(value) >= 1e5 || (value !== 0 && Math.abs(value) < 1e-4)) {
    return value.toExponential(3);
  }
  return value.toFixed(digits);
}

const percentOf = (x: number) => `${(x * 100).toFixed(2)}%`;

function baseName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** Ascending copy of the finite values — percentiles and histogram equalisation
 *  both need it, and sorting 921 600 floats twice per redraw is the difference
 *  between a responsive slider and a stuttering one. */
function sortedFinite(values: Float32Array): Float32Array {
  const buffer = new Float32Array(values.length);
  let count = 0;
  for (let i = 0; i < values.length; i++) {
    const value = values[i]!;
    if (Number.isFinite(value)) buffer[count++] = value;
  }
  return buffer.subarray(0, count).slice().sort();
}

function quantile(sorted: Float32Array, q: number): number {
  if (sorted.length === 0) return Number.NaN;
  const index = Math.round(q * (sorted.length - 1));
  return sorted[Math.min(sorted.length - 1, Math.max(0, index))]!;
}

export function DepthViewer({
  sourceId,
  unitPrefix,
  onClose,
}: {
  sourceId: string;
  unitPrefix: string;
  onClose: () => void;
}) {
  const [rows, setRows] = useState<IndexRow[] | null>(null);
  const [indexError, setIndexError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number>(0);
  const [frame, setFrame] = useState<{ row: IndexRow; data: Float32Array } | null>(null);
  const [frameError, setFrameError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [domain, setDomain] = useState<"raw" | "z">("raw");
  const [scaleMode, setScaleMode] = useState<"histeq" | "pct" | "minmax">("histeq");
  const [colormap, setColormap] = useState<"gray" | "turbo">("gray");
  const [near, setNear] = useState(0.1);
  const [probe, setProbe] = useState<{ x: number; y: number } | null>(null);

  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  // The index is one object; the frames are thousands. Load the index once.
  useEffect(() => {
    let live = true;
    setRows(null);
    setIndexError(null);
    api
      .reviewDepthObject(sourceId, `${unitPrefix}depth-index.jsonl`)
      .then((buffer) => {
        if (!live) return;
        const parsed: IndexRow[] = [];
        for (const line of new TextDecoder().decode(buffer).split("\n")) {
          const trimmed = line.trim();
          if (!trimmed) continue;
          try {
            parsed.push(JSON.parse(trimmed) as IndexRow);
          } catch {
            // A truncated last line is expected while the capture is still
            // uploading; a malformed middle line would show up as a gap in the
            // seq column rather than as a crash.
          }
        }
        setRows(parsed);
      })
      .catch((thrown: unknown) => {
        if (live) setIndexError(describeApiError(thrown));
      });
    return () => {
      live = false;
    };
  }, [sourceId, unitPrefix]);

  const row = rows?.[selected] ?? null;

  useEffect(() => {
    if (!row) return;
    let live = true;
    setBusy(true);
    setFrameError(null);
    // `path` is relative to the unit directory in the probe's own index.
    const key = row.path.startsWith(unitPrefix) ? row.path : `${unitPrefix}${row.path}`;
    api
      .reviewDepthObject(sourceId, key)
      .then(async (buffer) => {
        if (!live) return;
        // 变换在解码后立刻还原，`frame.data` 从此**永远是缓冲值 `d`**——
        // 下游的线性化、直方图、取点都按这个前提写的，多一处知道变换存在
        // 就多一处会漏掉它。
        const transform = resolveTransform(row);
        if (frameEncoding(row) === "exr-half-zip") {
          const exr = await decodeExrHalfZip(buffer);
          if (!live) return;
          if (exr.width !== row.width || exr.height !== row.height) {
            setFrameError(
              `EXR 是 ${exr.width}×${exr.height}，索引说 ${row.width}×${row.height}`,
            );
            setFrame(null);
            return;
          }
          setFrame({ row, data: toBufferValues(exr.data, transform) });
          return;
        }
        const expected = row.width * row.height * 4;
        if (buffer.byteLength !== expected) {
          setFrameError(
            `字节数 ${buffer.byteLength} 与 ${row.width}×${row.height}×4 = ${expected} 不符`,
          );
          setFrame(null);
          return;
        }
        setFrame({ row, data: toBufferValues(new Float32Array(buffer), transform) });
      })
      .catch((thrown: unknown) => {
        if (live) setFrameError(describeApiError(thrown));
      })
      .finally(() => {
        if (live) setBusy(false);
      });
    return () => {
      live = false;
    };
  }, [sourceId, unitPrefix, row]);

  const projection: Projection = useMemo(() => ({ a: 0, b: near, scale: 1 }), [near]);

  const displayed = useMemo(() => {
    if (!frame) return null;
    if (domain === "raw") return frame.data;
    const out = new Float32Array(frame.data.length);
    for (let i = 0; i < frame.data.length; i++) out[i] = linearize(frame.data[i]!, projection);
    return out;
  }, [frame, domain, projection]);

  const sorted = useMemo(() => (displayed ? sortedFinite(displayed) : null), [displayed]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !frame || !displayed || !sorted) return;
    const { width, height } = frame.row;
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d");
    if (!context) return;
    const image = context.createImageData(width, height);
    const pixels = image.data;

    let low: number;
    let high: number;
    if (scaleMode === "pct") {
      low = quantile(sorted, 0.01);
      high = quantile(sorted, 0.99);
    } else {
      low = sorted[0] ?? 0;
      high = sorted[sorted.length - 1] ?? 1;
    }
    if (!(high > low)) high = low + 1e-9;

    const lut = new Uint8Array(256 * 3);
    for (let i = 0; i < 256; i++) {
      const [r, g, b] = colormapRGB(i / 255, colormap);
      lut[i * 3] = r;
      lut[i * 3 + 1] = g;
      lut[i * 3 + 2] = b;
    }

    const total = sorted.length;
    const rank = (value: number): number => {
      let lo = 0;
      let hi = total;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (sorted[mid]! < value) lo = mid + 1;
        else hi = mid;
      }
      return lo / (total || 1);
    };

    for (let i = 0, offset = 0; i < displayed.length; i++, offset += 4) {
      const value = displayed[i]!;
      if (!Number.isFinite(value)) {
        pixels[offset] = INVALID_RGB[0];
        pixels[offset + 1] = INVALID_RGB[1];
        pixels[offset + 2] = INVALID_RGB[2];
        pixels[offset + 3] = 255;
        continue;
      }
      let t = scaleMode === "histeq" ? rank(value) : (value - low) / (high - low);
      t = t < 0 ? 0 : t > 1 ? 1 : t;
      const q = (t * 255) | 0;
      pixels[offset] = lut[q * 3]!;
      pixels[offset + 1] = lut[q * 3 + 1]!;
      pixels[offset + 2] = lut[q * 3 + 2]!;
      pixels[offset + 3] = 255;
    }
    context.putImageData(image, 0, 0);
  }, [frame, displayed, sorted, scaleMode, colormap]);

  const stats = useMemo(() => {
    if (!frame || !displayed || !sorted) return null;
    let zeros = 0;
    let invalid = 0;
    for (let i = 0; i < displayed.length; i++) {
      const value = displayed[i]!;
      if (!Number.isFinite(value)) invalid++;
      else if (value === 0) zeros++;
    }
    let sum = 0;
    for (let i = 0; i < sorted.length; i++) sum += sorted[i]!;
    return {
      zeros,
      invalid,
      finite: sorted.length,
      total: displayed.length,
      min: sorted[0] ?? Number.NaN,
      max: sorted[sorted.length - 1] ?? Number.NaN,
      mean: sum / (sorted.length || 1),
      p25: quantile(sorted, 0.25),
      p50: quantile(sorted, 0.5),
      p75: quantile(sorted, 0.75),
    };
  }, [frame, displayed, sorted]);

  // fp16 feasibility is only meaningful against a metric depth, so it always
  // reads the linearised array regardless of what is on screen.
  const f16 = useMemo(() => {
    if (!frame) return null;
    let valid = 0;
    let beyond = 0;
    let maxAbsError = 0;
    for (let i = 0; i < frame.data.length; i++) {
      const z = linearize(frame.data[i]!, projection);
      if (!Number.isFinite(z)) continue;
      valid++;
      if (z >= F16_ONE_CM_LIMIT) beyond++;
      const error = Math.abs(quantizeF16(truncateTwoDecimals(z)) - Math.fround(z));
      if (error > maxAbsError) maxAbsError = error;
    }
    return { valid, beyond, maxAbsError, total: frame.data.length };
  }, [frame, projection]);

  const indexSummary = useMemo(() => {
    if (!rows || rows.length === 0) return null;
    const seqs = rows.map((entry) => entry.present_seq);
    let missing = 0;
    for (let i = 1; i < seqs.length; i++) {
      const step = seqs[i]! - seqs[i - 1]!;
      if (step > 2) missing += step / 2 - 1;
    }
    return {
      count: rows.length,
      first: seqs[0]!,
      last: seqs[seqs.length - 1]!,
      odd: seqs.filter((seq) => seq % 2 !== 0).length,
      missing,
      formats: [...new Set(rows.map((entry) => entry.buffer_format ?? entry.format ?? "?"))].join("、"),
      encodings: [...new Set(rows.map((entry) => entry.storage?.encoding ?? "raw-f32（v1）"))].join("、"),
      resolutions: [...new Set(rows.map((entry) => `${entry.width}×${entry.height}`))].join("、"),
    };
  }, [rows]);

  if (indexError) return <ErrorNote message={indexError} />;
  if (!rows) return <Loading />;
  if (rows.length === 0) {
    return (
      <EmptyState
        title="depth-index.jsonl 为空"
        explanation="该目录下的索引没有任何可解析的行——采集可能刚开始，或者上传中断在了第一行之前。"
      />
    );
  }

  const probeInfo = (() => {
    if (!probe || !frame) return null;
    const index = probe.y * frame.row.width + probe.x;
    const d = frame.data[index];
    if (d === undefined) return null;
    const z = linearize(d, projection);
    const specValue = Number.isFinite(z)
      ? quantizeF16(truncateTwoDecimals(z))
      : Number.NaN;
    return { index, d, z, specValue };
  })();

  return (
    <div className="depth-viewer">
      <div className="review-toolbar">
        <button className="link" onClick={onClose}>
          ← 返回目录
        </button>
        <StateBadge label="probe 原始缓冲，未 linearize" tone="warn" />
        <label>
          帧{" "}
          <select value={selected} onChange={(event) => setSelected(Number(event.target.value))}>
            {rows.map((entry, index) => (
              <option key={entry.present_seq} value={index}>
                #{entry.present_seq}
                {entry.readback_latency_frames !== undefined
                  ? `（lat ${entry.readback_latency_frames}）`
                  : ""}
              </option>
            ))}
          </select>
        </label>
        <button
          className="link"
          onClick={() => setSelected(Math.max(0, selected - 1))}
          disabled={selected === 0}
        >
          上一帧
        </button>
        <button
          className="link"
          onClick={() => setSelected(Math.min(rows.length - 1, selected + 1))}
          disabled={selected >= rows.length - 1}
        >
          下一帧
        </button>
      </div>

      <div className="review-toolbar">
        <label>
          值域{" "}
          <select value={domain} onChange={(event) => setDomain(event.target.value as "raw" | "z")}>
            <option value="raw">原始缓冲值 d</option>
            <option value="z">线性深度 z（米）</option>
          </select>
        </label>
        <label>
          映射{" "}
          <select
            value={scaleMode}
            onChange={(event) => setScaleMode(event.target.value as typeof scaleMode)}
          >
            <option value="histeq">直方图均衡</option>
            <option value="pct">分位裁剪 p1–p99</option>
            <option value="minmax">min–max</option>
          </select>
        </label>
        <label>
          色表{" "}
          <select
            value={colormap}
            onChange={(event) => setColormap(event.target.value as typeof colormap)}
          >
            <option value="gray">灰度</option>
            <option value="turbo">Turbo</option>
          </select>
        </label>
        <label>
          near（= B_m）{" "}
          <input
            type="number"
            step="any"
            value={near}
            onChange={(event) => setNear(Number(event.target.value) || 0)}
          />
        </label>
      </div>

      {frameError ? <ErrorNote message={frameError} /> : null}
      {busy ? <Loading /> : null}

      <canvas
        ref={canvasRef}
        className="depth-canvas"
        onMouseMove={(event) => {
          const canvas = canvasRef.current;
          if (!canvas) return;
          const rect = canvas.getBoundingClientRect();
          const x = Math.floor(((event.clientX - rect.left) / rect.width) * canvas.width);
          const y = Math.floor(((event.clientY - rect.top) / rect.height) * canvas.height);
          if (x >= 0 && y >= 0 && x < canvas.width && y < canvas.height) setProbe({ x, y });
        }}
      />

      <div className="depth-panels">
        <section>
          <h4>像素探针</h4>
          {probeInfo && frame ? (
            <dl className="depth-kv">
              <dt>像素</dt>
              <dd>
                ({probe!.x}, {probe!.y}) idx {probeInfo.index.toLocaleString()}
              </dd>
              <dt>原始 d</dt>
              <dd>{formatNumber(probeInfo.d, 8)}</dd>
              <dt>线性 z</dt>
              <dd>
                {Number.isFinite(probeInfo.z)
                  ? `${formatNumber(probeInfo.z, 5)} m`
                  : "无效（d = a）→ 按 68 号写 NaN"}
              </dd>
              <dt>Variant S</dt>
              <dd>
                {Number.isFinite(probeInfo.specValue)
                  ? `${formatNumber(probeInfo.specValue, 5)} m`
                  : "NaN"}
              </dd>
            </dl>
          ) : (
            <p className="muted">把鼠标移到图上。</p>
          )}
        </section>

        <section>
          <h4>本帧统计</h4>
          {stats ? (
            <dl className="depth-kv">
              <dt>尺寸</dt>
              <dd>
                {frame!.row.width} × {frame!.row.height}
              </dd>
              <dt>有限值</dt>
              <dd>
                {stats.finite.toLocaleString()}（{percentOf(stats.finite / stats.total)}）
              </dd>
              <dt>{domain === "raw" ? "精确 0（clear 值）" : "无效 → 应写 NaN"}</dt>
              <dd>
                {domain === "raw"
                  ? `${stats.zeros.toLocaleString()}（${percentOf(stats.zeros / stats.total)}）`
                  : `${stats.invalid.toLocaleString()}（${percentOf(stats.invalid / stats.total)}）`}
              </dd>
              <dt>min / max</dt>
              <dd>
                {formatNumber(stats.min)} / {formatNumber(stats.max)}
              </dd>
              <dt>p25 / p50 / p75</dt>
              <dd>
                {formatNumber(stats.p25)} / {formatNumber(stats.p50)} / {formatNumber(stats.p75)}
              </dd>
            </dl>
          ) : null}
        </section>

        <section>
          <h4>fp16 交付可行性（68 号 §6.4）</h4>
          {f16 ? (
            <>
              <dl className="depth-kv">
                <dt>linearize 有效像素</dt>
                <dd>
                  {f16.valid.toLocaleString()}（{percentOf(f16.valid / f16.total)}）
                </dd>
                <dt>z ≥ 16 m 的像素</dt>
                <dd>{percentOf(f16.beyond / (f16.valid || 1))}</dd>
                <dt>max |S − R|</dt>
                <dd>{formatNumber(f16.maxAbsError)} m</dd>
              </dl>
              <p className="muted">
                随左侧 near 变化。真实投影参数需 <code>depth-lab calibrate</code> 标定，
                这里的 near 只是让你把缓冲值换算成米看一眼。
              </p>
            </>
          ) : null}
        </section>

        <section>
          <h4>索引概览</h4>
          {indexSummary ? (
            <dl className="depth-kv">
              <dt>行数</dt>
              <dd>{indexSummary.count.toLocaleString()}</dd>
              <dt>present_seq</dt>
              <dd>
                {indexSummary.first} … {indexSummary.last}
              </dd>
              <dt>奇数 seq</dt>
              <dd>{indexSummary.odd === 0 ? "0（全偶数，符合 15fps 抽帧）" : indexSummary.odd}</dd>
              <dt>漏采偶数帧</dt>
              <dd>{indexSummary.missing.toLocaleString()}</dd>
              <dt>缓冲格式</dt>
              <dd>{indexSummary.formats}</dd>
              <dt>盘上编码</dt>
              <dd>{indexSummary.encodings}</dd>
              <dt>分辨率</dt>
              <dd>{indexSummary.resolutions}</dd>
            </dl>
          ) : null}
        </section>
      </div>

      <p className="muted">
        这是 dm-depth-probe 的原始缓冲读回，不是交付物：68 号 §6.4 要的是 View-space linear
        depth、float16、EXR 单通道、无效像素 NaN、<code>trunc(depth×100)/100</code>、命名{" "}
        <code>depth_&lt;偶数帧&gt;</code>。这六项由 <code>depth-lab linearize</code> 补齐，
        当前文件一项都不满足是预期的。
      </p>
      <p className="muted">
        当前单元：<code>{baseName(unitPrefix.slice(0, -1))}</code>
      </p>
    </div>
  );
}
