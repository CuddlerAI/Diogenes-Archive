"""JSON-lines protocol used between Python orchestration and the in-game ASI."""

from __future__ import annotations

import json
import platform
import uuid
from pathlib import Path

from .errors import BackendError


CONTROL_SCHEMA = "dm-gta-control/1"
FRAME_SCHEMA = "dm-gta-frame/1"
DEFAULT_PIPE = r"\\.\pipe\diogenes-gta-capture-v1"


class JsonLinePipeClient:
    """A single-request-at-a-time named-pipe client.

    The ASI owns the pipe server and polls it from the ScriptHook tick. Payloads only
    reference versioned JSON files; large camera tapes and frame manifests never cross
    the pipe.
    """

    def __init__(self, pipe_name: str = DEFAULT_PIPE):
        self.pipe_name = pipe_name
        self._pipe = None

    def close(self) -> None:
        if self._pipe is not None:
            try:
                self._pipe.close()
            finally:
                self._pipe = None

    def _connect(self):
        if self._pipe is None:
            self._pipe = Path(self.pipe_name).open("r+b", buffering=0)
        return self._pipe

    def request(self, command: str, payload: dict | None = None) -> dict:
        if platform.system() != "Windows":
            raise BackendError("the GTA named-pipe backend requires Windows")
        request_id = str(uuid.uuid4())
        message = {
            "schema": CONTROL_SCHEMA,
            "id": request_id,
            "command": command,
            "payload": payload or {},
        }
        raw = (json.dumps(message, ensure_ascii=True, separators=(",", ":")) + "\n").encode("utf-8")
        try:
            pipe = self._connect()
            pipe.write(raw)
            response_raw = pipe.readline()
        except OSError as exc:
            self.close()
            raise BackendError(f"cannot communicate with GTA companion at {self.pipe_name}: {exc}") from exc
        if not response_raw:
            raise BackendError("GTA companion closed the pipe without a response")
        try:
            response = json.loads(response_raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise BackendError(f"GTA companion returned invalid JSON: {exc}") from exc
        if not isinstance(response, dict) or response.get("schema") != CONTROL_SCHEMA:
            raise BackendError("GTA companion response schema mismatch")
        if response.get("id") != request_id:
            raise BackendError("GTA companion response id mismatch")
        if response.get("ok") is not True:
            error = response.get("error") or "unspecified companion error"
            raise BackendError(str(error))
        payload_out = response.get("payload", {})
        if not isinstance(payload_out, dict):
            raise BackendError("GTA companion response payload is not an object")
        return payload_out
