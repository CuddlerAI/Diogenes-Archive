"""engine-data-qc：引擎产线交付数据本地自动质检 CLI。

规则依据 DataMiner/docs/70 §4.4 与 docs/71 §9，执行口径以 engine-data/README.md
快照为准；键名逐字以 engine-data/fixtures 需求方样例为准。

用法（在 engine-data/qc 目录下，或将其加入 PYTHONPATH）：

    python -m engine_qc check-clip <次级文件夹> --mode {mc,sfm}
    python -m engine_qc check-batch <批目录> --mode {mc,sfm}
"""

__version__ = "0.1.0"
