"""WM 0.1 · 方法验证包(research/33 Lane B).

底座裁决: LongLive-1.3B(Apache-2.0 代码 / **CC-BY-NC 4.0 权重**, 仅方法验证)
+ 从 Matrix-Game 3.0 移植的 ActionModule(Apache-2.0)。产物**不得进产品** ——
产品路径要把同一配方搬回 Wan2.1-T2V-1.3B(Apache-2.0), 对照臂 B6 就是量这个迁移成本的。
"""
from .action_space import (
    BASE_KEYBOARD_DIM, BASE_MOUSE_DIM, BASE_VAE_TIME_COMPRESSION,
    ActionSpace, encode_window, vocab_fingerprint,
)

__all__ = ["ActionSpace", "encode_window", "vocab_fingerprint",
           "BASE_KEYBOARD_DIM", "BASE_MOUSE_DIM", "BASE_VAE_TIME_COMPRESSION"]
