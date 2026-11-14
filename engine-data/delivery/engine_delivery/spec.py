"""交付**形态**的硬事实：文件清单、路径文法、组命名。

和 `engine_qc` 的分工：`engine_qc` 看**一条数据内部对不对**（帧、位姿、物理、共视）；
这里只看**它被摆成什么样子**——12 个文件是不是不多不少、目录名合不合文法、
路径里的游戏名跟 systeminfo 里的对不对得上。

两件事分开是因为它们的失效方式不一样：内容对但摆错位置，需求方那边是「入库路径不对」
整批返工；内容错但摆对了，是逐条打回。混在一个工具里，前一类会被后一类的噪声盖掉。

依据：[69 号 §1](../../../DataMiner/docs/69-multi-camera-p2-data-requirements/README.md)
（12 必要文件 + 1 份在线元信息）、`engine-data/delivery/README.md`
（上传路径按《【multi-camera2期】引擎数据上传文件步骤及要求》原件）。
"""

import re
from typing import List, Sequence

#: MC 一条数据的 12 个必要文件（69 号 §1 的表，N=1..4）
MC_REQUIRED_FILES: List[str] = [
    f"{stem}_{n}.{ext}"
    for n in (1, 2, 3, 4)
    for stem, ext in (("video", "mp4"), ("systeminfo", "json"), ("action_camera", "json"))
]

#: 我方内部产物。它们对 QC 有用，但**不是交付物**——出门前必须留在采集目录里。
#: 这份名单存在的意义是：`plan.json` 泄漏的是我们的轨迹生成参数，
#: `provenance.json` 里带着本机绝对路径和场景授权信息，都不该进对方的桶。
MC_INTERNAL_FILES: List[str] = [
    "plan.json", "covis.json", "dyn_metrics.json", "provenance.json", "audio_mix.wav",
]

#: MC 交付顶层目录（正式）。demo 阶段换 `2MultiCamera-demo`。
MC_TOP = "2MultiCamera"

#: 组名文法：`YYYY_MM_DD_<场景姓名拼音>_<两位标注序号>`，如 `2026_03_11_xingqiuchongqi_01`。
#: 拼音段只允许小写字母与数字——带下划线会把日期段和序号段的边界搅乱。
MC_GROUP_RE = re.compile(r"^(\d{4})_(\d{2})_(\d{2})_([a-z][a-z0-9]*)_(\d{2})$")

#: 日期目录：`MMDD`，如 `0411`
MC_DATE_RE = re.compile(r"^(\d{2})(\d{2})$")


def mc_group_name(year: int, month: int, day: int, scene: str, index: int) -> str:
    if not re.fullmatch(r"[a-z][a-z0-9]*", scene):
        raise ValueError(f"场景拼音段只能是小写字母数字、且字母开头：{scene!r}")
    if not 0 <= index <= 99:
        raise ValueError(f"标注序号须为两位：{index!r}")
    return f"{year:04d}_{month:02d}_{day:02d}_{scene}_{index:02d}"


def mc_relpath(game: str, group: str, prefix: str = "") -> str:
    """`[<prefix>/]2MultiCamera/<gameProcessName>/<MMDD>/<group>/`

    `MMDD` 不是另给的参数——它由组名里的日期推出来。两处日期分别填写是个
    现成的错误来源（组名 0804、目录 0803 的数据入库后按目录归档，就找不回来了）。
    """
    m = MC_GROUP_RE.match(group)
    if m is None:
        raise ValueError(f"组名不合文法：{group!r}")
    mmdd = f"{m.group(2)}{m.group(3)}"
    head = f"{prefix.strip('/')}/" if prefix.strip("/") else ""
    return f"{head}{MC_TOP}/{game}/{mmdd}/{group}"


def missing_and_extra(names: Sequence[str]) -> tuple:
    """返回 (缺的, 多的)。`names` 是次级文件夹内的文件名（不含子目录）。"""
    got = set(names)
    need = set(MC_REQUIRED_FILES)
    return sorted(need - got), sorted(got - need)
