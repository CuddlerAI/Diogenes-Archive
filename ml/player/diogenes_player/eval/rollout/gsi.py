"""CS2 Game State Integration 记分器 (W4, research/04 §5.2)。

CS2 客户端按 cfg (configs/gamestate_integration_diogenes.cfg) 把游戏状态 POST 到本地
HTTP 端点。本模块提供:
- GsiState: 线程安全的最新状态 + kill/death 事件流 (处理换图/重开时计数归零)
- GsiServer: 阻塞式 HTTPServer 的后台线程封装

评估纪律: 仅 -insecure 本地服务器 (Pearce 验证过的研究实践); 永不连公网匹配。
"""
from __future__ import annotations

import json
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, HTTPServer


@dataclass
class GsiSnapshot:
    kills: int = 0
    deaths: int = 0
    map_phase: str = ""
    round_phase: str = ""
    map_name: str = ""
    t: float = 0.0


@dataclass
class GsiState:
    """update() 幂等消费 GSI payload; 跨 payload 差分出 kill/death 事件。"""
    snapshot: GsiSnapshot = field(default_factory=GsiSnapshot)
    events: list[tuple[float, str]] = field(default_factory=list)  # (t, "kill"|"death")
    _lock: threading.Lock = field(default_factory=threading.Lock)

    def update(self, payload: dict, *, now: float | None = None) -> None:
        now = time.monotonic() if now is None else now
        ms = (payload.get("player") or {}).get("match_stats") or {}
        new = GsiSnapshot(
            kills=int(ms.get("kills", 0)),
            deaths=int(ms.get("deaths", 0)),
            map_phase=str((payload.get("map") or {}).get("phase", "")),
            round_phase=str((payload.get("round") or {}).get("phase", "")),
            map_name=str((payload.get("map") or {}).get("name", "")),
            t=now,
        )
        with self._lock:
            old = self.snapshot
            same_match = new.map_name == old.map_name and new.kills >= old.kills \
                and new.deaths >= old.deaths
            if same_match:
                self.events += [(now, "kill")] * (new.kills - old.kills)
                self.events += [(now, "death")] * (new.deaths - old.deaths)
            # 计数回退/换图 = 新对局: 不产生负事件, 直接换基线
            self.snapshot = new

    def counts_since(self, t0: float) -> tuple[int, int]:
        with self._lock:
            kills = sum(1 for t, e in self.events if t >= t0 and e == "kill")
            deaths = sum(1 for t, e in self.events if t >= t0 and e == "death")
        return kills, deaths


class _Handler(BaseHTTPRequestHandler):
    state: GsiState  # 由 GsiServer 注入

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", 0))
        try:
            payload = json.loads(self.rfile.read(length) or b"{}")
            self.state.update(payload)
        except (json.JSONDecodeError, ValueError):
            pass  # GSI 流里偶发脏包, 丢弃不断线
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):  # 静默访问日志
        pass


class GsiServer:
    def __init__(self, state: GsiState, host: str = "127.0.0.1", port: int = 3000):
        handler = type("BoundHandler", (_Handler,), {"state": state})
        self._httpd = HTTPServer((host, port), handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)

    @property
    def port(self) -> int:
        return self._httpd.server_address[1]

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()
