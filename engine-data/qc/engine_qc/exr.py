"""最小 OpenEXR 读取器（纯标准库，只用 zlib）。

只支持交付形态需要的子集：**scanline、单 part、非 tiled**，压缩 NONE/RLE/ZIPS/ZIP，
像素类型 HALF/FLOAT/UINT。其余形态（tiled / deep / multipart / PIZ / PXR24 / B44 / DWA）
一律抛 `ExrError` 并说明原因——QC 宁可明确报「读不了」，也不要静默给出半通的结论。

写出侧是 `engine-data/enginecapture/Runtime/Core/ExrWriter.cs`，两侧的 ZIP 反变换
（先反预测器、再反字节重排）必须逐字一致。
"""

import struct
import zlib
from array import array

#: 压缩码 → 每块扫描线数（OpenEXR 固定值）
LINES_PER_BLOCK = {0: 1, 1: 1, 2: 1, 3: 16, 4: 32, 5: 16, 6: 32, 7: 32}

COMPRESSION_NAMES = {0: "NONE", 1: "RLE", 2: "ZIPS", 3: "ZIP", 4: "PIZ",
                     5: "PXR24", 6: "B44", 7: "B44A", 8: "DWAA", 9: "DWAB"}

SUPPORTED_COMPRESSION = {0, 1, 2, 3}

PIXELTYPE_UINT, PIXELTYPE_HALF, PIXELTYPE_FLOAT = 0, 1, 2
PIXELTYPE_SIZE = {PIXELTYPE_UINT: 4, PIXELTYPE_HALF: 2, PIXELTYPE_FLOAT: 4}
PIXELTYPE_NAMES = {0: "UINT", 1: "HALF", 2: "FLOAT"}

MAGIC = 0x01312F76


class ExrError(Exception):
    """EXR 结构性错误：文件不是我们能读的形态，或内容自相矛盾。"""


# ---------------------------------------------------------------- half 查表
def _build_half_table():
    """65536 项 half→float 查表。逐值转换在纯 Python 里太慢，建一次表复用。"""
    tbl = [0.0] * 65536
    for h in range(65536):
        sign = -1.0 if (h >> 15) else 1.0
        exp = (h >> 10) & 0x1F
        man = h & 0x3FF
        if exp == 0x1F:
            tbl[h] = float("nan") if man else sign * float("inf")
        elif exp == 0:
            tbl[h] = sign * man * 2.0 ** -24
        else:
            tbl[h] = sign * (1.0 + man / 1024.0) * 2.0 ** (exp - 15)
    return tbl


HALF_TO_FLOAT = _build_half_table()


def half_is_nan(bits: int) -> bool:
    return (bits & 0x7C00) == 0x7C00 and (bits & 0x03FF) != 0


class ExrImage:
    """解出的单 part scanline 图像。`bits` 是各通道的原始位模式数组（HALF 时为 uint16）。"""

    def __init__(self, width, height, channels, compression, bits):
        self.width = width
        self.height = height
        self.channels = channels        # [(name, pixel_type)]，按 EXR 内的字母序
        self.compression = compression
        self.bits = bits                # {channel_name: array}

    @property
    def channel_names(self):
        return [c[0] for c in self.channels]

    def floats(self, channel=None):
        """按 float 取某通道（HALF 走查表）。"""
        name = channel or self.channels[0][0]
        ptype = dict(self.channels)[name]
        raw = self.bits[name]
        if ptype == PIXELTYPE_HALF:
            t = HALF_TO_FLOAT
            return [t[b] for b in raw]
        if ptype == PIXELTYPE_FLOAT:
            return list(raw)
        return [float(v) for v in raw]

    def at(self, x, y, channel=None):
        """单点取值（float）。越界返回 None。"""
        if not (0 <= x < self.width and 0 <= y < self.height):
            return None
        name = channel or self.channels[0][0]
        ptype = dict(self.channels)[name]
        v = self.bits[name][y * self.width + x]
        return HALF_TO_FLOAT[v] if ptype == PIXELTYPE_HALF else float(v)


def _read_null_str(data, p):
    e = data.index(b"\0", p)
    return data[p:e].decode("ascii", "replace"), e + 1


def _unzip_block(payload, expected):
    """ZIP/ZIPS 解块：zlib 展开 → 反预测器 → 反字节重排。"""
    tmp = bytearray(zlib.decompress(payload))
    if len(tmp) != expected:
        raise ExrError(f"ZIP 块展开后 {len(tmp)} 字节，期望 {expected}")
    for i in range(1, len(tmp)):
        tmp[i] = (tmp[i] + tmp[i - 1] - 128) & 0xFF
    n = len(tmp)
    out = bytearray(n)
    half = (n + 1) // 2
    t1, t2, s = 0, half, 0
    while True:
        if s < n:
            out[s] = tmp[t1]; t1 += 1; s += 1
        else:
            break
        if s < n:
            out[s] = tmp[t2]; t2 += 1; s += 1
        else:
            break
    return bytes(out)


def _unrle_block(payload, expected):
    """RLE 解块：先 RLE 展开，再与 ZIP 相同的反预测器/反重排。"""
    out = bytearray()
    i = 0
    n = len(payload)
    while i < n and len(out) < expected:
        c = payload[i]
        i += 1
        if c > 127:                       # 有符号负数 → 重复游程
            count = 257 - c
            if i >= n:
                break
            out.extend(bytes([payload[i]]) * count)
            i += 1
        else:
            count = c + 1
            out.extend(payload[i:i + count])
            i += count
    if len(out) != expected:
        raise ExrError(f"RLE 块展开后 {len(out)} 字节，期望 {expected}")
    tmp = out
    for i in range(1, len(tmp)):
        tmp[i] = (tmp[i] + tmp[i - 1] - 128) & 0xFF
    n = len(tmp)
    res = bytearray(n)
    half = (n + 1) // 2
    t1, t2, s = 0, half, 0
    while True:
        if s < n:
            res[s] = tmp[t1]; t1 += 1; s += 1
        else:
            break
        if s < n:
            res[s] = tmp[t2]; t2 += 1; s += 1
        else:
            break
    return bytes(res)


def read(data: bytes) -> ExrImage:
    """解析一个 EXR 字节流。结构不符即抛 ExrError。"""
    if len(data) < 8:
        raise ExrError("文件过短")
    magic, ver = struct.unpack_from("<ii", data, 0)
    if magic != MAGIC:
        raise ExrError(f"magic 不符：0x{magic:08X}")
    if (ver & 0xFF) != 2:
        raise ExrError(f"version {ver & 0xFF}，只支持 2")
    if ver & 0x200:
        raise ExrError("tiled EXR 不在支持范围（交付要求 scanline）")
    if ver & 0x800:
        raise ExrError("deep EXR 不在支持范围")
    if ver & 0x1000:
        raise ExrError("multipart EXR 不在支持范围")

    p = 8
    attrs = {}
    while True:
        name, p = _read_null_str(data, p)
        if name == "":
            break
        typ, p = _read_null_str(data, p)
        (size,) = struct.unpack_from("<i", data, p)
        p += 4
        attrs[name] = (typ, data[p:p + size])
        p += size

    for req in ("channels", "compression", "dataWindow", "lineOrder"):
        if req not in attrs:
            raise ExrError(f"缺必需属性 {req}")

    xmin, ymin, xmax, ymax = struct.unpack("<iiii", attrs["dataWindow"][1])
    width, height = xmax - xmin + 1, ymax - ymin + 1
    if width <= 0 or height <= 0:
        raise ExrError(f"dataWindow 非法：{xmin},{ymin},{xmax},{ymax}")

    comp = attrs["compression"][1][0]
    if comp not in SUPPORTED_COMPRESSION:
        raise ExrError(f"压缩 {COMPRESSION_NAMES.get(comp, comp)} 不在支持范围"
                       f"（支持 NONE/RLE/ZIPS/ZIP）")
    if attrs["lineOrder"][1][0] != 0:
        raise ExrError("lineOrder 非 INCREASING_Y")

    # chlist
    ch = attrs["channels"][1]
    channels = []
    q = 0
    while q < len(ch) and ch[q] != 0:
        cname, q = _read_null_str(ch, q)
        (ptype,) = struct.unpack_from("<i", ch, q)
        q += 4
        q += 4                                   # pLinear + 3 保留字节
        (xs, ys) = struct.unpack_from("<ii", ch, q)
        q += 8
        if xs != 1 or ys != 1:
            raise ExrError(f"通道 {cname} 有子采样 {xs},{ys}，不支持")
        if ptype not in PIXELTYPE_SIZE:
            raise ExrError(f"通道 {cname} 像素类型 {ptype} 未知")
        channels.append((cname, ptype))
    if not channels:
        raise ExrError("chlist 为空")

    lpb = LINES_PER_BLOCK[comp]
    nblocks = (height + lpb - 1) // lpb
    if p + 8 * nblocks > len(data):
        raise ExrError("偏移表越界")
    offsets = struct.unpack_from("<%dQ" % nblocks, data, p)

    row_bytes = sum(PIXELTYPE_SIZE[t] for _, t in channels) * width
    out = {}
    for cname, ptype in channels:
        out[cname] = array("H" if ptype == PIXELTYPE_HALF else "f",
                           bytes(PIXELTYPE_SIZE[ptype] * width * height))

    for off in offsets:
        off = int(off)
        if off + 8 > len(data):
            raise ExrError("块偏移越界")
        y0, size = struct.unpack_from("<ii", data, off)
        payload = data[off + 8: off + 8 + size]
        if len(payload) != size:
            raise ExrError("块数据截断")
        lines = min(lpb, height - (y0 - ymin))
        if lines <= 0:
            raise ExrError(f"块 y={y0} 超出 dataWindow")
        expected = lines * row_bytes
        if size == expected:
            raw = payload                       # 压不小时按原始存
        elif comp in (2, 3):
            raw = _unzip_block(payload, expected)
        elif comp == 1:
            raw = _unrle_block(payload, expected)
        else:
            raise ExrError(f"NONE 压缩块大小 {size} != {expected}")

        # 块内按 scanline，每行内按通道字母序连续存放
        pos = 0
        for i in range(lines):
            y = (y0 - ymin) + i
            for cname, ptype in channels:
                n = width
                size_b = PIXELTYPE_SIZE[ptype] * n
                seg = raw[pos:pos + size_b]
                pos += size_b
                a = array("H" if ptype == PIXELTYPE_HALF else "f")
                a.frombytes(seg)
                out[cname][y * width:(y + 1) * width] = a

    return ExrImage(width, height, channels, comp, out)


def read_file(path) -> ExrImage:
    with open(path, "rb") as f:
        return read(f.read())
