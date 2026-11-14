# Cyberpunk 2077 Steam · 正式 Depth 目录

2026-09-18 起本目录带正式 Depth 两件套，所有客户端构建都从这里加载，不再有只在诊断构建里
替换的本地清单：

- `depth-profile.json`：`sha256:fa557a62a79c449ed138df7f2bbc56674c13482eea5db6e2608a7b0e9a42d151`
- `camera-binding.json`（`cp2077-steam-holstered-local-v3`）：
  `sha256:edc26919102745259372c2089e6f48b72bfb97d239e8f3cf53a185ed43374b6c`

两份文件与 2026-09-18 四档性能认证时逐字节相同，哈希是认证 key 的一部分：**改任何一个字节
（包括 id 里的 local、证据路径）都会让该认证失效，必须重跑四档。** profile 引用的米制与
来源证据仍在 `config/cp2077-local-depth/local-depth-evidence.json`，路径与哈希都钉在
profile 里，不能挪动或改写；审核结论另写文档，不改证据文件。

晋级依据：玩家位姿按渲染相机内容归帧（`depth_lab::cp2077_pose_join`），真机步行短测与四档
认证中在线结果和离线重算逐行一致、零归帧失败；四档认证在诊断构建上通过（平均 FPS 降幅
0.02%、1% low 降幅 1.22%，RTX 5070 Ti Laptop）。仍未完成的事项见 `TODO.md`。

## 采集操作规范（正式录制）

正式路径对每个 Present 零容忍：任一帧的相机或玩家对不上，整段录制立即停止，**故障前已录好
的分段（包括视频）也不能上传**。以下规范就是为了不触发它：

1. 只用 Steam build `20383525`。上线期间关闭该游戏的自动更新；游戏一更新，整条链会拒绝录制。
2. 游戏设置：1920×1080、全屏或无边框、FOV 75、游戏内帧率上限 30，其余画质保持认证时的设置。
3. 客户端深度设置保持默认（`unbind_from_end = 4`）；改动会让性能认证对不上。
4. 只录**空手步行**的第一人称画面。拔枪、瞄准、扫描器、上车/驾驶、过场与剧情演出、拍照模式、
   超梦、菜单/地图/背包、加载/快速旅行、死亡与读档都不支持。进入这些状态前先暂停录制，
   回到空手步行后再继续；暂停会结束当前一段，继续时重新预热。
5. 分段录，每段不超过约 10 分钟，出一次故障最多损失一段。
6. 暂停（Ctrl+Alt+F8）和结束（Ctrl+Alt+F10）时先站定，三个键快速按一下就松开，不要按住。游戏会先
   收到 Ctrl 和 Alt：Ctrl 是闪避，Alt 是武器轮盘，按住会让角色闪避或掏出武器；武器在录制真正停下前
   出现，那一段会被相机检查判失败。继续录制前确认手里没有武器，否则预热过不去。
7. 显卡必须在性能认证覆盖名单里（`config/depth-probe/performance-certifications-v3.json`），
   名单外的机器会被拒绝开启深度采集。

## RGB 预检历史记录

2026-08-14 已从本机安装事实钉住：

- Steam app `1091500` / build `20383525`；
- `Cyberpunk2077.exe` product version `2.31`、Authenticode 有效、精确 SHA-256 已写入
  `game-profile.json`；
- `UserSettings.json` 在 2026-08-14 的预检快照中为 2560×1600、BorderlessWindowed、
  HDR=None、FOV=80；录制 profile 目标为 1920×1080、FOV=75，Fullscreen 与 Borderless
  均可，因此 D7 前只须调整分辨率和 FOV。DataMiner 不缩放，2560×1600 源画面会被几何门拒绝；
- 音频按 Fallout 4 同款 process loopback 契约采集：PCM s16le / 48 kHz / stereo + zstd；
- on-foot 键鼠子集来自同 build 自带的 `r6/config/inputUserMappings.xml`。

2026-08-14 当时尚未完成：

1. 未启动真实游戏观察顶层 HWND class，因此 `window_class` 保持 `null`；
2. 未跑 D7 WGC / Raw Input / NVIDIA hardware-only encoder 冒烟；
3. 当前 mapping 只覆盖 on-foot first-person 子集。驾驶状态下 WASD 等按键语义不同，完成
   context-aware mapping 或独立 driving profile 前不得把车辆片段解释成该 mapping 的动作标签；
4. 生产 `games` 准入未核对/未注册，正式客户端仍按 onboarding 展示；带
   `local-catalog-only` 的 All Games DEV 包只为本机 non-trainable 录制将本 profile 标为可选，
   不得借此绕过服务端上传门禁。

该历史记录不作为重录既有证据的要求。
