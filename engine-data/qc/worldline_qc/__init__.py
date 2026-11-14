"""worldline_qc：Multi-Camera 三期（四世界线 A/B/C/D、12 路）交付组本地质检。

用法（在 engine-data/qc 目录下，或把它加进 PYTHONPATH；依赖同目录的 engine_qc）：

    python -m worldline_qc check-group <采集组目录> --expect-duration-s 120 --html out.html --json out.json

规则依据：《【Multi-Camera】游戏引擎数据采集新需求 for 外部》《Multi-camera 12 路相机数据交付说明》、
09-21/09-22 两次会议纪要、demo 跟进表、用户本轮确认点①–④；条款目录见 requirements.py。
"""

__version__ = "0.1.0"
