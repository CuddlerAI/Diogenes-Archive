# scripts/remote：从 Mac 远程操作采集机

用 Mac 通过 SSH 在 Windows 采集机上开 GTA、进故事模式、接电话、挂导出、进编辑器导出、收取、转视频、核对后删图、出包、打包。
beach-demo-102 到 113 组（2026-10-17、10-18）的导出和出包全程用的就是这套脚本，12 路无人值守连续导完。

操作步骤和规矩以 `../../RUNBOOK-MC4-FOUR-WORLDLINES.md` 为准，背后的坑见 `../../LESSONS-MC4-DEMO.md` 第六、七节。

## 准备

**采集机（一次）：**
1. 把本仓库检出到采集机，默认路径 `D:\workspace\mc4\Diogenes`。
2. 用登录桌面的那个用户注册三个计划任务：`powershell -File engine-data\gta-capture\scripts\gta-desktop-tasks.ps1 -Install`。
   这三个任务分别是开游戏、只截游戏窗口、只在游戏在最前面时按键。SSH 会话里开的程序不在桌面上，所以必须走计划任务。
   截图和按键文件放在 `D:\workspace\mc4\screens`。
3. 装好 Python（带 numpy）和 ffmpeg，Windows 的 OpenSSH 服务端默认 shell 保持 cmd。

**Mac：**
1. `~/.ssh/config` 里给采集机起个别名（默认 `win-pc`）。
2. 装 zsh、`python3`（带 numpy 和 pillow，供 `checks/` 判画面用），以及 macOS 自带的 sips。
3. 路径不同就在环境里覆盖 `env.sh` 的变量：

| 变量 | 默认 | 是什么 |
|---|---|---|
| `MC4_HOST` | `win-pc` | 采集机的 ssh 别名 |
| `MC4_ROOT_WIN` | `D:\workspace\mc4` | 下面有 `groups\`、`renders\`、`delivery\`、`screens\`、`tmp\` |
| `MC4_REPO_WIN` | `D:\workspace\mc4\Diogenes` | 采集机上本仓库的检出 |
| `MC4_GAME_WIN` | `D:\SteamLibrary\steamapps\common\Grand Theft Auto V` | 游戏目录（RE+ 的 Render.ini 和日志在下面） |
| `MC4_WORK` | `~/.cache/mc4-remote` | Mac 上放截图、日志、上次关游戏的时间 |
| `COOLDOWN_S` | `180` | 每关一次游戏，至少隔多久再开（散热） |
| `GROUP` | 无 | 组目录名，例如 `beach-demo-113` |

**开游戏前先打招呼**：几台机器共用一个 Steam 账号，会互相踢下线。

## Mac 端脚本

| 脚本 | 用法 |
|---|---|
| `keys.sh KEY ...` | 在游戏窗口里按键：`UP DOWN LEFT RIGHT ENTER ESC BACK SPACE TAB Q E V`、`"WAIT 3000"`、`"MOUSE dx dy"`、`CENTER`（指针放到窗口正中挪 2 像素，不点击）。游戏不在最前面时什么都不按 |
| `shot.sh <名字>` | 只截游戏窗口，存到 `$MC4_WORK/<名字>.png` 和 1280 宽的 `-small.png`；没截到就返回非 0 |
| `simeon.sh [静默秒数]` | 进故事模式后接起主角的每个来电再挂断，直到连续 240 秒没有新来电。**每遍录制前都要跑**：没接的电话会打进录制里 |
| `lane.sh <步骤>` | 一路导出的单个步骤：`boot`、`right n`、`arm 遍 路 行 [--redo]`、`export 路`、`wait`、`leave`、`collect 遍 路`、`encode 遍 路` |
| `cycle.sh <遍> <路>` | 一整路：等冷却 → 开游戏 → 进故事模式 → 挂导出 → 编辑器导出 → 退出 → 收取 → 关游戏 → 转视频 → 核对 → 删图，任何一步不对就停 |

导一遍的四路（`CLIP_C` 是这遍的片段号，挂导出时会报 `source_clip_name`；`ROW_C` 是它在编辑器片段列表里的行，最新的在第 1 行）：

```bash
export GROUP=beach-demo-113 CLIP_C=0011 ROW_C=2
for N in 1 2 3 4; do zsh cycle.sh C $N || break; done > ~/.cache/mc4-remote/chain.log 2>&1
```

重导某一路（例如第一人称算法改了，只换第 4 路的轨迹）：

1. 先 `stage-take-tape` 重出轨迹。
2. **把这遍 `capture\video_4.mp4` 改名挪开**：包里的视频是硬链接，ffmpeg 会原地覆盖写。
3. 再用 `REDO=1 zsh cycle.sh C 4`。

`checks/` 是判画面用的小脚本：选择页上哪一项亮着、片段列表选中了第几行、是不是已经进了故事模式、有没有来电、有没有 Script Hook V 的更新询问。界面布局按 1920×1080 窗口和 150% 屏幕缩放量的，换机器要核对一遍。

## 采集机端辅助脚本（`win/`，由 Mac 端经 ssh 调用，也可以在采集机上直接跑）

| 脚本 | 用法 |
|---|---|
| `verify_clean.py <root> <组> <遍> <路> [--delete]` | 核对转好的视频：帧数 = 图片数 − 30、1080p、有声、时长对得上。加 `--delete` 时，核对通过才删这一路的 render 文件夹 |
| `deliver_qc.py <组目录> <交付目录> <质检目录> [--qc-only]` | 出包（`stage-group-deliver`）并跑 `worldline_qc` |
| `supersede.ps1 -Delivery <目录> -Pattern <通配> -Tag <.superseded-原因> [-QcDir <目录>]` | 重出包前把旧包整组改名留档（文件夹、zip、sha256、README、manifest、质检），不删任何东西 |
| `dedupe.ps1 -Groups <目录> -Group <组> -Delivery <目录> -Pattern <通配>` | 包里的视频改成硬链接，链到录制目录里同 hash 的那份，一组省约 1.5 GB |
| `zip_package.ps1 -Delivery <目录> -Pattern <通配>` | 打 zip，写逐文件 sha256 清单 |
| `regen_msync.py <组目录> <交付目录> <输出目录>` | 不拷视频，只用出包同一段代码重出 12 个 multicam_sync，并和包里的旧版逐帧比，报告哪些字段变了。很轻，GTA 开着时也能跑；核对无误后自己把 12 个文件换进包里 |
| `rename_group_id.py <交付目录> <通配> <旧后缀> <新后缀>` | 包换组名：12 个 multicam_sync 只改 `capture_group_id`，另外 36 个文件硬链接过去。之后把 group.json 的 `delivery_name` 也改成新名字 |

通配写成 `2026_10_18_*_113` 这种：带中文的包名经过 ssh 命令行会乱码。ps1 只用 ASCII：Windows PowerShell 5 读不带 BOM 的脚本时按本地代码页解析。

## 出包一轮的顺序

```text
supersede.ps1（旧包改名）→ deliver_qc.py（出包 + 质检）→ tools/compare_packages.py（和旧包逐文件比）
→ 质检和上一版逐条比 → tools/delivery_spec_check.py（按交付说明自查）→ dedupe.ps1 → zip_package.ps1
```
