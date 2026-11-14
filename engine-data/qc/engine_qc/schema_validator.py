"""JSON Schema draft-07 子集校验器（纯标准库）。

覆盖本仓 engine-data/schema/*.schema.json 用到的关键字：
type / required / properties / items / minItems / maxItems /
minimum / maximum / enum / pattern / additionalProperties。

若环境装有 jsonschema，则作为可选加速路径使用（ENGINE_QC_NO_JSONSCHEMA=1 可禁用）；
纯标准库回退始终可用，两条路径输出同为「错误字符串列表」。
"""

import json
import os
import re
from pathlib import Path
from typing import Any, List

try:  # 可选加速路径，不强依赖
    import jsonschema as _jsonschema  # type: ignore
except ImportError:  # pragma: no cover
    _jsonschema = None

#: schema 目录：engine-data/schema/（相对本包）
SCHEMA_DIR = Path(__file__).resolve().parents[2] / "schema"

SCHEMA_FILES = {
    "mc_cam1": "action_camera_mc_cam1.schema.json",
    "mc_camN": "action_camera_mc_camN.schema.json",
    "sfm_30fps": "action_camera_sfm_30fps.schema.json",
    "sfm_15fps": "action_camera_sfm_15fps.schema.json",
    "systeminfo": "systeminfo.schema.json",
    "plan": "plan.schema.json",
}

_schema_cache: dict = {}

ERROR_CAP = 20  # 错误上限，避免 1800 帧的报文刷屏


def load_schema(name: str) -> dict:
    if name not in _schema_cache:
        path = SCHEMA_DIR / SCHEMA_FILES[name]
        _schema_cache[name] = json.loads(path.read_text(encoding="utf-8"))
    return _schema_cache[name]


def _type_ok(value: Any, typ: str) -> bool:
    if typ == "object":
        return isinstance(value, dict)
    if typ == "array":
        return isinstance(value, list)
    if typ == "string":
        return isinstance(value, str)
    if typ == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if typ == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if typ == "boolean":
        return isinstance(value, bool)
    if typ == "null":
        return value is None
    return True  # 未知类型不判


def _validate(value: Any, schema: dict, path: str, errors: List[str]) -> None:
    if len(errors) >= ERROR_CAP:
        return
    loc = path or "$"

    typ = schema.get("type")
    if typ is not None:
        types = typ if isinstance(typ, list) else [typ]
        if not any(_type_ok(value, t) for t in types):
            errors.append(f"{loc}: 类型应为 {'/'.join(types)}，实际 {type(value).__name__}")
            return  # 类型不对时后续关键字无意义

    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{loc}: 取值 {value!r} 不在枚举 {schema['enum']} 内")

    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            errors.append(f"{loc}: {value} < minimum {schema['minimum']}")
        if "maximum" in schema and value > schema["maximum"]:
            errors.append(f"{loc}: {value} > maximum {schema['maximum']}")

    if isinstance(value, str) and "pattern" in schema:
        if re.search(schema["pattern"], value) is None:
            errors.append(f"{loc}: 字符串 {value!r} 不匹配 pattern {schema['pattern']!r}")

    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            errors.append(f"{loc}: 长度 {len(value)} < minItems {schema['minItems']}")
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            errors.append(f"{loc}: 长度 {len(value)} > maxItems {schema['maxItems']}")
        items = schema.get("items")
        if isinstance(items, dict):
            for i, v in enumerate(value):
                if len(errors) >= ERROR_CAP:
                    return
                _validate(v, items, f"{loc}[{i}]", errors)

    if isinstance(value, dict):
        for req in schema.get("required", []):
            if req not in value:
                errors.append(f"{loc}: 缺少必含键 {req!r}")
        props = schema.get("properties", {})
        addl = schema.get("additionalProperties", True)
        for k, v in value.items():
            if len(errors) >= ERROR_CAP:
                return
            if k in props:
                _validate(v, props[k], f"{loc}.{k}", errors)
            elif addl is False:
                errors.append(f"{loc}: 不允许的额外键 {k!r}")


def validate_instance(value: Any, schema: dict) -> List[str]:
    """返回错误字符串列表（空 = 通过）；最多 ERROR_CAP 条。"""
    if _jsonschema is not None and not os.environ.get("ENGINE_QC_NO_JSONSCHEMA"):
        try:
            validator = _jsonschema.Draft7Validator(schema)
            errors = []
            for e in validator.iter_errors(value):
                loc = "$" + "".join(
                    f"[{p}]" if isinstance(p, int) else f".{p}" for p in e.absolute_path
                )
                errors.append(f"{loc}: {e.message}")
                if len(errors) >= ERROR_CAP:
                    break
            return errors
        except Exception:  # pragma: no cover - 加速路径异常时回退
            pass
    errors: List[str] = []
    _validate(value, schema, "", errors)
    return errors
