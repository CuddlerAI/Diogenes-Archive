# annotation-review — dm-1 标注逐帧审查器

单文件网页工具，用于肉眼核验 dm-1 数据集（data_demo 布局）中**输入标注与视频画面的时间对齐**。这是机器校验（`data-demo-converter verify`）覆盖不了的最后一环：verify 能证明 jsonl 内部自洽与 sha256 溯源，但"视频第 N 帧的画面是否对应第 N 帧的输入"需要人眼判定。

## 功能

- 视频上实时叠加：键盘面板（按住键高亮）、鼠标左右键状态（含跨 chunk 按住重建）、位移矢量盘（`pointerDx/Dy` + `pointerBins`）、瞬时事件横幅（key/mouse down/up）
- 右侧同步显示当前帧的 `p0.jsonl` 原始行
- 逐帧步进（`←`/`→`）、事件间跳转（`E`/`Q`）、0.1×–1× 慢放、时间轴点击跳帧

## 用法

方式一（免服务器）：双击打开 `review.html`，用文件选择器加载任意 chunk 的 `p0.mp4` + `p0.jsonl`。

方式二（批量审查）：在数据集 session 目录的上层起静态服务，用 URL 参数切 chunk：

```bash
cd <dataset>/<game_slug>/  # 目录下有 <session_id>/cNNNN/
python3 -m http.server 8734
# 浏览器打开 http://localhost:8734/review.html?dir=<session_id>/c0007
# （review.html 需拷贝或软链到该目录）
```

## 判定标准

输入在采集时刻记录，画面反馈存在游戏渲染延迟：**合格 = 输入先于画面反馈 1–3 帧且延迟稳定**。画面反馈先于标注出现 = 错位，该 session 淘汰。

chunk 首帧若先出现 `mouse_up`，表示按下发生在上一 chunk（跨界持续按住，工具会自动还原此状态），属正常。
