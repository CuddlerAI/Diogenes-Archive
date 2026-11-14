# Issue #750 · WGC alpha / 超屏客户区实验

独立实验程序，一次关闭 V1/V2/V3/V7：

- 在 1920×1080 主屏上创建 1920×1088 borderless D3D11 窗口；
- 后缓冲左上 192×8 的 RGB 固定，三段 alpha 分别写 0 / 247 / 255；
- 最下方离屏 8 行写洋红色；
- WGC 按 HWND 捕获并读回 alpha 与离屏行；
- 同一套 DataMiner `BgraToNv12` hardware VideoProcessor 对“同 RGB、alpha=0/255”分别转换，
  比较全尺寸 NV12 原始字节，再把两帧送进项目实际 NVIDIA hardware-only H.264 编码器。

```powershell
cd DataMiner/tools/issue750-wgc-probe
cargo run --release -- --output evidence/wgc-alpha-oversize
```

输出 `captured.png` 与 `summary.json`。这是测试窗口取证，不启动游戏、不修改 probe/recorder，
产物始终是 non-trainable。
