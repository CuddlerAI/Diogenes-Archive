"""depth 测试夹具：一个**独立实现**的最小 EXR 写出器 + 解析可算的合成场景。

写出器刻意不复用 `engine_qc.exr` 的任何代码——用被测模块自己的逆运算去造测试输入，
等于用同一个 bug 验自己。场景用球 + 地面这类解析可求交的几何，
使「深度与位姿内参自洽」成为可构造的真值，反投影自检因此能被真正验到。
"""

import math
import struct
import zlib

MAGIC = 0x01312F76
COMP_NONE, COMP_ZIPS, COMP_ZIP = 0, 2, 3
LPB = {COMP_NONE: 1, COMP_ZIPS: 1, COMP_ZIP: 16}


def f2h(f: float) -> int:
    """float → half 位模式，round-to-nearest-even；NaN 归一 0x7E00（与 C# 侧同口径）。"""
    (b,) = struct.unpack("<I", struct.pack("<f", f))
    sign = (b >> 16) & 0x8000
    e = (b >> 23) & 0xFF
    m = b & 0x7FFFFF
    if e == 0xFF:
        return 0x7E00 if m else (sign | 0x7C00)
    if e == 0:
        return sign
    ee = e - 127 + 15
    if ee >= 0x1F:
        return sign | 0x7C00
    if ee <= 0:
        if ee < -10:
            return sign
        m |= 0x800000
        sh = 14 - ee
        hm = m >> sh
        rem = m & ((1 << sh) - 1)
        half = 1 << (sh - 1)
        if rem > half or (rem == half and (hm & 1)):
            hm += 1
        return sign | hm
    hm = m >> 13
    rem = m & 0x1FFF
    if rem > 0x1000 or (rem == 0x1000 and (hm & 1)):
        hm += 1
        if hm == 0x400:
            hm = 0
            ee += 1
            if ee >= 0x1F:
                return sign | 0x7C00
    return sign | (ee << 10) | hm


def _zip_encode(raw: bytes) -> bytes:
    n = len(raw)
    tmp = bytearray(n)
    half = (n + 1) // 2
    t1, t2, s = 0, half, 0
    while True:
        if s < n:
            tmp[t1] = raw[s]; t1 += 1; s += 1
        else:
            break
        if s < n:
            tmp[t2] = raw[s]; t2 += 1; s += 1
        else:
            break
    p = tmp[0] if n else 0
    for i in range(1, n):
        d = (tmp[i] - p + 384) & 0xFF
        p = tmp[i]
        tmp[i] = d
    return zlib.compress(bytes(tmp), 6)


def _attr(name, typ, payload):
    return name.encode() + b"\0" + typ.encode() + b"\0" + struct.pack("<i", len(payload)) + payload


def write_exr_bytes(width, height, pixels, channel="Y", compression=COMP_ZIP,
                    pixel_type=1):
    """pixels: 行优先 float 序列。pixel_type 1=HALF, 2=FLOAT（用于负例测试）。"""
    psize = 2 if pixel_type == 1 else 4
    chlist = (channel.encode() + b"\0" + struct.pack("<i", pixel_type)
              + b"\0" + b"\0\0\0" + struct.pack("<ii", 1, 1) + b"\0")
    box = struct.pack("<iiii", 0, 0, width - 1, height - 1)
    h = struct.pack("<ii", MAGIC, 2)
    h += _attr("channels", "chlist", chlist)
    h += _attr("compression", "compression", bytes([compression]))
    h += _attr("dataWindow", "box2i", box)
    h += _attr("displayWindow", "box2i", box)
    h += _attr("lineOrder", "lineOrder", bytes([0]))
    h += _attr("pixelAspectRatio", "float", struct.pack("<f", 1.0))
    h += _attr("screenWindowCenter", "v2f", struct.pack("<ff", 0.0, 0.0))
    h += _attr("screenWindowWidth", "float", struct.pack("<f", 1.0))
    h += b"\0"

    lpb = LPB[compression]
    nblocks = (height + lpb - 1) // lpb
    chunks = []
    for b in range(nblocks):
        y0 = b * lpb
        lines = min(lpb, height - y0)
        raw = bytearray()
        for y in range(y0, y0 + lines):
            base = y * width
            for x in range(width):
                v = pixels[base + x]
                raw += (struct.pack("<H", f2h(v)) if pixel_type == 1
                        else struct.pack("<f", v))
        raw = bytes(raw)
        if compression == COMP_NONE:
            payload = raw
        else:
            enc = _zip_encode(raw)
            payload = enc if len(enc) < len(raw) else raw
        chunks.append((y0, payload))

    pos = len(h) + 8 * nblocks
    offs = []
    for _, payload in chunks:
        offs.append(pos)
        pos += 8 + len(payload)

    out = bytearray(h)
    for o in offs:
        out += struct.pack("<Q", o)
    for y0, payload in chunks:
        out += struct.pack("<ii", y0, len(payload))
        out += payload
    return bytes(out)


# ---------------------------------------------------------------- 合成场景

def quat_matrix(q):
    x, y, z, w = q
    n = math.sqrt(x * x + y * y + z * z + w * w)
    x, y, z, w = x / n, y / n, z / n, w / n
    return ((1 - 2 * (y * y + z * z), 2 * (x * y - w * z), 2 * (x * z + w * y)),
            (2 * (x * y + w * z), 1 - 2 * (x * x + z * z), 2 * (y * z - w * x)),
            (2 * (x * z - w * y), 2 * (y * z + w * x), 1 - 2 * (x * x + y * y)))


def yaw_quat(deg):
    r = math.radians(deg) / 2.0
    return [0.0, math.sin(r), 0.0, math.cos(r)]


SPHERE_C = (0.0, 0.0, 0.0)
SPHERE_R = 6.0
GROUND_Y = -3.0


def render_depth(width, height, cam_pos, cam_quat, fx, fy, cx, cy):
    """解析求交出 **view-space 沿光轴 Z**（不是欧氏距离），按 cm 截断。

    球 + 地面，都打不中 → NaN（天空）。这正是 68 号要的深度语义。
    """
    R = quat_matrix(cam_quat)
    px = []
    for y in range(height):
        for x in range(width):
            # 像素 → 相机系方向（+z 前、+y 上，图像 v 轴朝下）
            dx = (x + 0.5 - cx) / fx
            dy = -(y + 0.5 - cy) / fy
            dcam = (dx, dy, 1.0)
            n = math.sqrt(dx * dx + dy * dy + 1.0)
            dw = (R[0][0] * dcam[0] + R[0][1] * dcam[1] + R[0][2] * dcam[2],
                  R[1][0] * dcam[0] + R[1][1] * dcam[1] + R[1][2] * dcam[2],
                  R[2][0] * dcam[0] + R[2][1] * dcam[1] + R[2][2] * dcam[2])
            dw = (dw[0] / n, dw[1] / n, dw[2] / n)

            best = None
            # 球
            oc = (cam_pos[0] - SPHERE_C[0], cam_pos[1] - SPHERE_C[1], cam_pos[2] - SPHERE_C[2])
            b = 2.0 * (oc[0] * dw[0] + oc[1] * dw[1] + oc[2] * dw[2])
            c = oc[0] ** 2 + oc[1] ** 2 + oc[2] ** 2 - SPHERE_R ** 2
            disc = b * b - 4 * c
            if disc >= 0:
                t = (-b - math.sqrt(disc)) / 2.0
                if t > 1e-4:
                    best = t
            # 地面
            if abs(dw[1]) > 1e-9:
                t = (GROUND_Y - cam_pos[1]) / dw[1]
                if t > 1e-4 and (best is None or t < best):
                    hx = cam_pos[0] + dw[0] * t
                    hz = cam_pos[2] + dw[2] * t
                    if abs(hx) < 60 and abs(hz) < 60:
                        best = t

            if best is None:
                px.append(float("nan"))
                continue
            # 命中点 → view-space Z（沿光轴分量）
            hit = (cam_pos[0] + dw[0] * best, cam_pos[1] + dw[1] * best, cam_pos[2] + dw[2] * best)
            rel = (hit[0] - cam_pos[0], hit[1] - cam_pos[1], hit[2] - cam_pos[2])
            z = R[0][2] * rel[0] + R[1][2] * rel[1] + R[2][2] * rel[2]
            px.append(math.trunc(max(z, 0.0) * 100.0) / 100.0)
    return px
