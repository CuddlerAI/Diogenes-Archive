"""AB-P0 · ActionBridge 伪标注管线 (research/35 §3.3).

三步, 三个 CLI, 全部先在本机用 tiny trunk 验证机制, GPU 只按 finetune.py
注释里的 runbook 走:

1. `finetune`      phase0 IDM 头在 COD 具结语料(857 chunk / 19 会话)上域内微调;
2. `pseudo_label`  微调后的头对全部 chunk 输出逐 (帧,键) 标签 + 置信度 c_t;
3. `bridge`        伪标签在 30 fps chunk 侧经 wm01/dataset 的同一重采样纪律
                   进训练平面, 供 A2(均匀) / A3(c_t 加权) 消融臂消费。

两个已知口径坑在设计上显式处理(不是绕过):

- **词表**: IDM 的 12 键(`SCANNED_VKEYS`) ↔ wm01 六通道的投影只存在一份, 在
  `wm01/action_space.py`(`AB_P0_CHANNEL_VKS` / `scanned_to_wm01`), 不在本包
  重造。COD 的 `keys` 列没有鼠标键 —— combat 两通道从语义 actions 补位, 见
  finetune.py。
- **fps**: 伪标注永远在 30 fps chunk 侧做; 进平面走 `wm01.dataset.clip_grid`
  同一栅格(键盘存覆盖比例、位移求和、拒绝上采样)。在 16/24 fps 平面侧打标签
  意味着给插值帧发明动作 —— bridge 结构上做不出这件事。

治理: 全链只接受过 `is_trainable` 四项与的 chunk(owner 具结车道), 伪标签产物
原样继承源数据的 `no_publish` / `no_product_training` 标签 —— 约束跟着数据走。
"""
