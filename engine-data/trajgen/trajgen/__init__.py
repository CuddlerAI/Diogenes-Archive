"""engine-data-trajgen：Multi-Camera 二期计划生成器 + M3a 排程仿真。

规则依据 docs/71 §4–6（原语库 / 共视与解耦 / 第四路）与 §10（M3a 排程仿真），
口径快照以 `engine-data/README.md` 为准，交付字段键名以 `engine-data/fixtures/` 为准。

纯标准库实现（与 engine-data-qc 同纪律），无第三方依赖。
"""

__version__ = "0.1.0"

__all__ = ["__version__"]
