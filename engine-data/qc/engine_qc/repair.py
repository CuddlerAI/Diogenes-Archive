"""存量修复：EXR 行序上下翻转重写（docs/88）。

DepthSink 修复前产出的 depth EXR 相对 video 上下镜像（sfm-d1-20260805 全部
900 帧）。像素本身没坏，只是行序反了——离线把行序翻回来即可交付，不必重录。

口径：
- **位精确**：HALF/FLOAT 通道按原始位模式整行搬运，绝不过 float 往返——
  修复不允许引入哪怕 1 ulp 的数值变化，修完 `depth.quantize` 必须原样通过；
- 容器重编码与 `enginecapture/Runtime/Core/ExrWriter.cs` 同构（属性顺序、
  ZIP 的字节重排 + 一阶预测器 + zlib、压不小回退原始块）；zlib 压缩级别与
  C# 侧不必逐字节一致——容器字节数允许不同，像素位模式不允许；
- 只支持我们交付形态覆盖的子集（scanline、NONE/ZIPS/ZIP、HALF/FLOAT）。
  其余形态明确抛错——修复工具静默改口径比不修更糟。

用法见 cli.py 的 `flip-depth` 子命令。修完必须重跑 `check-clip`，让
`depth.orientation` 与全部内容组规则重新裁决。
"""

import struct
import zipfile
import zlib
from array import array
from pathlib import Path

from . import exr

#: 压缩码 → 名称（报错用）
_ZIP_CODES = {0, 2, 3}


class RepairError(Exception):
    """修复工具拒绝处理的形态：宁可报错，不可静默改交付口径。"""


def _zip_encode_block(raw: bytes) -> bytes:
    """OpenEXR ImfZipCompressor 正变换：字节重排 → 一阶预测器 → zlib。

    与 ExrWriter.ReorderAndPredict（C#）/ exr._unzip_block（读取侧逆变换）同构。
    重排用切片赋值（C 速度）；预测器是逐字节差分，纯 Python 循环不可避免，
    720p HALF 单块 40 KB，900 帧量级下总耗时以分钟计，可接受。
    """
    n = len(raw)
    tmp = bytearray(n)
    half = (n + 1) // 2
    tmp[:half] = raw[0::2]
    tmp[half:] = raw[1::2]
    prev = tmp[0] if n else 0
    out = bytearray(tmp)
    for i in range(1, n):
        cur = tmp[i]
        out[i] = (cur - prev + 384) & 0xFF
        prev = cur
    return zlib.compress(bytes(out), 6)


def _attr(name: str, typ: str, payload: bytes) -> bytes:
    return (name.encode("ascii") + b"\0" + typ.encode("ascii") + b"\0"
            + struct.pack("<i", len(payload)) + payload)


def _encode(img: "exr.ExrImage") -> bytes:
    """把 ExrImage 重新序列化成与 ExrWriter.cs 相同布局的字节流。"""
    chlist = b""
    for cname, ptype in img.channels:
        chlist += (cname.encode("ascii") + b"\0" + struct.pack("<i", ptype)
                   + b"\0" + b"\0\0\0" + struct.pack("<ii", 1, 1))
    chlist += b"\0"
    box = struct.pack("<iiii", 0, 0, img.width - 1, img.height - 1)

    h = struct.pack("<ii", exr.MAGIC, 2)
    h += _attr("channels", "chlist", chlist)
    h += _attr("compression", "compression", bytes([img.compression]))
    h += _attr("dataWindow", "box2i", box)
    h += _attr("displayWindow", "box2i", box)
    h += _attr("lineOrder", "lineOrder", bytes([0]))          # INCREASING_Y
    h += _attr("pixelAspectRatio", "float", struct.pack("<f", 1.0))
    h += _attr("screenWindowCenter", "v2f", struct.pack("<ff", 0.0, 0.0))
    h += _attr("screenWindowWidth", "float", struct.pack("<f", 1.0))
    h += b"\0"

    lpb = exr.LINES_PER_BLOCK[img.compression]
    nblocks = (img.height + lpb - 1) // lpb
    w = img.width
    chunks = []
    for b in range(nblocks):
        y0 = b * lpb
        lines = min(lpb, img.height - y0)
        raw = bytearray()
        for y in range(y0, y0 + lines):
            for cname, _ in img.channels:
                raw += img.bits[cname][y * w:(y + 1) * w].tobytes()
        raw = bytes(raw)
        if img.compression == 0:
            payload = raw
        else:
            enc = _zip_encode_block(raw)
            payload = enc if len(enc) < len(raw) else raw     # 压不小回退原始块
        chunks.append((y0, payload))

    pos = len(h) + 8 * nblocks
    out = bytearray(h)
    for _, payload in chunks:
        out += struct.pack("<Q", pos)
        pos += 8 + len(payload)
    for y0, payload in chunks:
        out += struct.pack("<ii", y0, len(payload))
        out += payload
    return bytes(out)


def flip_exr_rows(data: bytes) -> bytes:
    """行序上下翻转一个 EXR 字节流，像素位模式逐一保留。"""
    img = exr.read(data)
    if img.compression not in _ZIP_CODES:
        raise RepairError(
            f"压缩 {exr.COMPRESSION_NAMES.get(img.compression, img.compression)} "
            "不在翻转重写支持范围（交付口径是 ZIP，见 docs/75 问题 4）")
    for cname, ptype in img.channels:
        if ptype not in (exr.PIXELTYPE_HALF, exr.PIXELTYPE_FLOAT):
            raise RepairError(f"通道 {cname} 像素类型 UINT 不在翻转重写支持范围")

    w, hgt = img.width, img.height
    for cname, ptype in img.channels:
        src = img.bits[cname]
        dst = array(src.typecode)
        for y in range(hgt - 1, -1, -1):
            dst.extend(src[y * w:(y + 1) * w])
        img.bits[cname] = dst
    return _encode(img)


def flip_depth_zip(src: Path, dst: Path) -> int:
    """翻转 depth.zip 内全部 .exr 成员写到 dst，保留成员名/顺序/压缩方式。

    返回翻转的成员数。任一成员失败即抛错——半修复的 zip 比没修更危险。
    """
    src, dst = Path(src), Path(dst)
    n = 0
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(dst, "w") as zout:
        for zi in zin.infolist():
            if zi.is_dir():
                raise RepairError(f"zip 含目录条目 {zi.filename!r}——先过 depth.flat 再修")
            payload = zin.read(zi.filename)
            if zi.filename.lower().endswith(".exr"):
                payload = flip_exr_rows(payload)
                n += 1
            info = zipfile.ZipInfo(zi.filename, date_time=zi.date_time)
            info.compress_type = zi.compress_type
            info.external_attr = zi.external_attr
            zout.writestr(info, payload)
    return n
