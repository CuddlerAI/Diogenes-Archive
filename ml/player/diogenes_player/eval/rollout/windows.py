"""Windows 真实后端: dxcam 采集 + SendInput 注入（语义→物理经 mapping 逆映射）。

⚠ 状态: 代码就绪、**未在实机验证**（本仓库开发机为 macOS）。W3/W4-real 检查单:
1. dxcam region 对准游戏窗口, output_color="RGB", 缩放到模型输入分辨率
2. timeBeginPeriod(1) 生效（系统计时器粒度 15.6ms → 1ms）
3. SendInput 相对鼠标 delta 被 raw input 消费（勿用 SetCursorPos）；键盘走 scan code
4. 延迟预算（research/07 §4）: 采集 4-8ms + 推理 1-3ms + 注入 <1ms，门 = 30min p99 <25ms

★ 硬红线（research/07 §4.1）: **永不在 COD 上做闭环注入**。SendInputInjector 每次 apply 前
检查前台进程；命中 FORBIDDEN_PROCESS_SUBSTRINGS 立即抛错拒绝注入。COD 有内核级 Ricochet +
TOS 明文禁自动化，闭环评估只在 CS2 -insecure / 无反作弊探针环境进行。

★ 授权通道（research/27 §1）: Halo Infinite 自 2026-08-01 起从硬拒绝改为 **C-1 授权通道**
——微软合作 C-1 确认含自动化 agent 采集扩展，但仅限白名单账号。注入必须携带
SelfplayAuthorization（consent_id + 登录账号 + 法务备案白名单）且三要素齐备，缺任一
即 fail-closed 拒绝。授权解决的是政策面；技术共存（内核 EAC 是否拦截）仍以 H-1 实测为准。
"""
from __future__ import annotations

import ctypes
import json
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np

# 按游戏的注入安全表（research/halo/s0-primary-game-decision.md §5.3、research/10 §6.2、
# research/27 §1）。三档：
#
#   FORBIDDEN —— 无可证明安全的配置、也无合作授权（COD：Ricochet + TOS 禁自动化）→ 硬拒绝。
#   AUTHORIZED —— 有合作方书面授权覆盖自动化（Halo Infinite，C-1 扩展 2026-08-01）
#                 → 仅在 SelfplayAuthorization 三要素齐备时放行，否则 fail-closed 拒绝。
#   其余 —— 存在可证明反作弊未加载的配置（MCC EAC-off、CS2 -insecure、无反作弊单机、
#           自控探针环境）→ 放行。
FORBIDDEN_PROCESS_SUBSTRINGS = (
    # COD：Ricochet 内核反作弊 + TOS 明文禁全模式自动化
    "cod.exe", "cod22", "cod23", "cod24", "sp22", "modernwarfare",
    "blackops", "warzone", "callofduty",
)

# C-1 授权游戏：政策面已由合作方授权自动化，但内核反作弊常驻（无关闭开关），
# 注入仅限白名单账号——守卫要求显式 SelfplayAuthorization，缺失即拒。
AUTHORIZED_PROCESS_SUBSTRINGS = (
    # Halo Infinite：内核 EAC 无关闭开关；C-1 授权扩展（research/27 §1，2026-08-01）
    "haloinfinite",
)

# 允许闭环注入的环境（仅文档用途，不参与判定；判定见 injection_block_reason）。
INJECTION_SAFE_NOTES = {
    "MCC-Win64-Shipping.exe": "需用 Steam「Anti-Cheat Disabled」启动项；战役/自定义局/Firefight/Forge/Theater 均可",
    "cs2.exe": "需 -insecure 本地服；Valve 官方许可路径",
    "HaloInfinite.exe": "C-1 授权通道：白名单账号 + consent_id，经 SelfplayAuthorization 显式携带",
}


@dataclass(frozen=True)
class SelfplayAuthorization:
    """C-1 自动化采集授权上下文（research/27 §1，2026-08-01 落地）。

    三要素齐备才放行：合作协议 consent_id（rights=licensed 链的同一 ID）、当前登录
    账号、法务备案的账号白名单。编排器在每个采集会话启动时装配并传给注入器；任何
    要素缺失/不匹配都视为无授权（fail-closed）。
    """

    consent_id: str
    account_id: str
    allowlist: frozenset[str] = frozenset()

    def problem(self) -> str | None:
        """授权上下文的缺陷描述；None = 三要素齐备。"""
        if not self.consent_id:
            return "consent_id 为空（合作协议编号未回填）"
        if not self.account_id:
            return "account_id 为空（无法确认登录账号）"
        if self.account_id not in self.allowlist:
            return f"账号 {self.account_id!r} 不在法务备案白名单"
        return None

_WIN = sys.platform == "win32"


def _require_windows() -> None:
    if not _WIN:
        raise RuntimeError("windows.py 后端仅在 Windows 上可用; macOS/Linux 用 Fake 后端")


def is_forbidden_process(name: str) -> bool:
    """进程名（大小写不敏感）是否命中硬禁注入名单。跨平台可测。"""
    low = name.lower()
    return any(sub in low for sub in FORBIDDEN_PROCESS_SUBSTRINGS)


def requires_authorization(name: str) -> bool:
    """进程名是否属于 C-1 授权游戏（注入需 SelfplayAuthorization）。跨平台可测。

    MCC 与 Infinite 的非对称仍在，但含义变了：MCC 走「可证明反作弊未加载」通道
    （EAC-off 启动项）无需授权上下文；Infinite 内核 EAC 无关闭开关，走授权通道
    （research/27 §1）——政策面由 C-1 覆盖，账号必须在白名单。
    """
    low = name.lower()
    return any(sub in low for sub in AUTHORIZED_PROCESS_SUBSTRINGS)


def injection_block_reason(name: str,
                           auth: SelfplayAuthorization | None = None) -> str | None:
    """注入前判定（纯函数，跨平台可测）。返回 None = 放行，否则为拒绝理由。

    fail-closed 次序：进程不可识别 → 拒；硬红线（COD）→ 拒；授权游戏（Infinite）
    无有效授权上下文 → 拒；其余放行。
    """
    if not name:
        return ("无法识别前台进程（OpenProcess/GetModuleFileNameEx 失败）——"
                "注入前守卫 fail-closed，拒绝注入。")
    if is_forbidden_process(name):
        return (f"前台进程 {name!r} 命中硬禁注入名单——COD 闭环注入被硬禁止"
                "（Ricochet 内核反作弊 + TOS 禁自动化, research/07 §4.1）。")
    if requires_authorization(name):
        if auth is None:
            return (f"前台进程 {name!r} 是 C-1 授权游戏，但未提供 SelfplayAuthorization"
                    "——授权注入 fail-closed，拒绝（research/27 §1）。")
        problem = auth.problem()
        if problem:
            return f"前台进程 {name!r} 的授权上下文无效：{problem}（research/27 §1）。"
    return None


def foreground_process_name() -> str:  # pragma: no cover — 需 Windows
    """当前前台窗口所属进程可执行名。"""
    _require_windows()
    import ctypes.wintypes as wt

    user32, kernel32, psapi = ctypes.windll.user32, ctypes.windll.kernel32, ctypes.windll.psapi
    hwnd = user32.GetForegroundWindow()
    pid = wt.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
    h = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not h:
        return ""
    try:
        buf = ctypes.create_unicode_buffer(260)
        n = psapi.GetModuleFileNameExW(h, None, buf, 260)
        return Path(buf.value).name if n else ""
    finally:
        kernel32.CloseHandle(h)


def assert_safe_foreground(auth: SelfplayAuthorization | None = None) -> None:  # pragma: no cover — 需 Windows
    """注入前守卫：硬红线（COD）、不可识别进程、无授权的 C-1 游戏均抛错拒绝。
    判定逻辑在 injection_block_reason（纯函数，跨平台已测）。"""
    reason = injection_block_reason(foreground_process_name(), auth)
    if reason:
        raise RuntimeError(f"红线: {reason}")


class DxcamCapture:
    """DXGI Desktop Duplication 采集 (dxcam), 缩放到模型输入分辨率。"""

    def __init__(self, *, region: tuple[int, int, int, int],
                 out_height: int, out_width: int):
        _require_windows()
        import cv2  # noqa: F401 — 延迟导入, 仅 Windows 部署环境需要
        import dxcam

        self._cv2 = cv2
        self.cam = dxcam.create(output_color="RGB")
        self.region = region
        self.hw = (out_height, out_width)

    def grab(self) -> np.ndarray:
        frame = self.cam.grab(region=self.region)
        while frame is None:  # 显示器无新帧时 dxcam 返回 None
            frame = self.cam.grab(region=self.region)
        return self._cv2.resize(frame, (self.hw[1], self.hw[0]),
                                interpolation=self._cv2.INTER_AREA)


class MappingResolver:
    """从 DataMiner mapping.json 建 语义 → 物理输出 的逆映射（部署注入用）。

    keyboard 绑定 → (scan_code, extended)；mouse button 绑定 → button 序号。
    同一语义多物理键取首个（mapping 声明序）。camera.* 由 policy 的 dx/dy 直接驱动，不在此。
    """

    def __init__(self, mapping_path: str | Path):
        m = json.loads(Path(mapping_path).read_text(encoding="utf-8"))
        self.mapping_id = m.get("mapping_id", "")
        self.version = m.get("version", "")
        self.kbd: dict[str, tuple[int, bool]] = {}
        self.mouse_btn: dict[str, int] = {}
        for b in m.get("bindings", []):
            sem = b.get("semantic")
            if sem is None:
                continue
            if b.get("device") == "keyboard" and "scan_code" in b:
                self.kbd.setdefault(sem, (int(b["scan_code"]), bool(b.get("extended", False))))
            elif b.get("device") == "mouse" and "button" in b:
                self.mouse_btn.setdefault(sem, int(b["button"]))

    def resolve(self, actions: set[str]) -> tuple[set[tuple[int, bool]], set[int]]:
        """→ (键盘 scan codes 集合, 鼠标 button 集合)。未知语义（如 camera）忽略。"""
        scans = {self.kbd[a] for a in actions if a in self.kbd}
        btns = {self.mouse_btn[a] for a in actions if a in self.mouse_btn}
        return scans, btns


# --- SendInput ctypes 结构 (仅 win32 下有意义) --------------------------------
if _WIN:  # pragma: no cover
    ULONG_PTR = ctypes.POINTER(ctypes.c_ulong)

    class _KEYBDINPUT(ctypes.Structure):
        _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort),
                    ("dwFlags", ctypes.c_ulong), ("time", ctypes.c_ulong),
                    ("dwExtraInfo", ULONG_PTR)]

    class _MOUSEINPUT(ctypes.Structure):
        _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long),
                    ("mouseData", ctypes.c_ulong), ("dwFlags", ctypes.c_ulong),
                    ("time", ctypes.c_ulong), ("dwExtraInfo", ULONG_PTR)]

    class _INPUT_UNION(ctypes.Union):
        _fields_ = [("ki", _KEYBDINPUT), ("mi", _MOUSEINPUT)]

    class _INPUT(ctypes.Structure):
        _fields_ = [("type", ctypes.c_ulong), ("u", _INPUT_UNION)]

    _INPUT_KEYBOARD, _INPUT_MOUSE = 1, 0
    _KEYEVENTF_KEYUP = 0x0002
    _KEYEVENTF_SCANCODE = 0x0008
    _KEYEVENTF_EXTENDEDKEY = 0x0001
    _MOUSEEVENTF_MOVE = 0x0001
    _MOUSE_BTN_FLAGS = {  # button 序号 → (down, up)
        1: (0x0002, 0x0004),   # L
        2: (0x0008, 0x0010),   # R
        4: (0x0020, 0x0040),   # M（button 值沿用 VK 惯例 L=1/R=2/M=4）
    }


class SendInputInjector:
    """语义动作 → mapping 逆映射 → 物理 SendInput。每次 apply 前守卫前台进程红线。

    在 C-1 授权游戏（Halo Infinite）上注入必须传 authorization（编排器每会话装配），
    否则守卫 fail-closed 拒绝。
    """

    def __init__(self, resolver: MappingResolver, *, enforce_guard: bool = True,
                 authorization: SelfplayAuthorization | None = None) -> None:
        _require_windows()
        self.resolver = resolver
        self.enforce_guard = enforce_guard
        self.authorization = authorization
        ctypes.windll.winmm.timeBeginPeriod(1)
        self._scans: set[tuple[int, bool]] = set()
        self._btns: set[int] = set()

    def apply(self, actions: set[str], dx: int, dy: int) -> None:  # pragma: no cover
        if self.enforce_guard:
            assert_safe_foreground(self.authorization)
        scans, btns = self.resolver.resolve(actions)
        inputs: list[_INPUT] = []
        for sc in scans - self._scans:
            inputs.append(self._scan_event(sc, down=True))
        for sc in self._scans - scans:
            inputs.append(self._scan_event(sc, down=False))
        for b in btns - self._btns:
            inputs.append(self._btn_event(b, down=True))
        for b in self._btns - btns:
            inputs.append(self._btn_event(b, down=False))
        if dx or dy:
            mi = _MOUSEINPUT(dx, dy, 0, _MOUSEEVENTF_MOVE, 0, None)
            inputs.append(_INPUT(_INPUT_MOUSE, _INPUT_UNION(mi=mi)))
        if inputs:
            arr = (_INPUT * len(inputs))(*inputs)
            ctypes.windll.user32.SendInput(len(inputs), arr, ctypes.sizeof(_INPUT))
        self._scans, self._btns = scans, btns

    def _scan_event(self, sc: tuple[int, bool], *, down: bool):  # pragma: no cover
        code, extended = sc
        flags = _KEYEVENTF_SCANCODE | (0 if down else _KEYEVENTF_KEYUP)
        if extended:
            flags |= _KEYEVENTF_EXTENDEDKEY
        ki = _KEYBDINPUT(0, code, flags, 0, None)
        return _INPUT(_INPUT_KEYBOARD, _INPUT_UNION(ki=ki))

    def _btn_event(self, b: int, *, down: bool):  # pragma: no cover
        flag = _MOUSE_BTN_FLAGS[b][0 if down else 1]
        mi = _MOUSEINPUT(0, 0, 0, flag, 0, None)
        return _INPUT(_INPUT_MOUSE, _INPUT_UNION(mi=mi))

    def release_all(self) -> None:  # pragma: no cover — episode 边界/退出清理
        self.apply(set(), 0, 0)
