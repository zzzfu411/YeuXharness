# YeuX Harness 品牌资产

**视觉母版：** `/Users/zfu/Documents/develop/win/yeuxpage/yuzai.png`
**当前批准版本：** fish doodle v2 · 2026-08-31

## 当前主资产

### `yeux-fish-doodle-paper-v2.png`

- 1200 × 1200 RGBA，透明底，石墨墨色 `#1B1815`。
- 用于 Paper 主题、浅色文档和纸面发行物。
- SHA-256：

```text
ae4d998e50b26069ff2518410b3a0d4da18f9e2f98541e081e01c4b742b5c3ef  yeux-fish-doodle-paper-v2.png
```

### `yeux-fish-doodle-nocturne-v2.png`

- 1200 × 1200 RGBA，透明底，暖骨白 `#D9D4C9`。
- 用于 Nocturne 网页 Welcome、富终端启动板和深色发行物。
- SHA-256：

```text
d45531309a9ca3257c32ab47e68614a5fa56f83e26f16fb1ed4dd75553b0ce91  yeux-fish-doodle-nocturne-v2.png
```

### `yeux-fish-doodle-fallback.svg`

- 确定性、可缩放的简化回退；通过 `prefers-color-scheme` 切换墨色。
- 位图或终端图像协议不可用时使用；再失败则回退到纯文本 `><`。

## 造型合同

实物母版优先于过去文档中的文字描述：

- 两只鱼人**背对背**；头朝画布外侧，尾巴朝内近乎相接。
- 叶形/歪圆鱼身、单点眼、两条长短不一的火柴腿。
- 左右必须分别手绘，不能镜像；右鱼保留一小段歪背鳍涂笔。
- 主体偏小、空域充足；断墨、飞白、重描与不稳定线宽是人格，不是缺陷。
- 不使用中心火花、信号波、事件 rail、无限符号、同心圆、锦鲤尾鳍、鳞片、发光、金属、3D 或企业徽章构图。

## 来源与再生成

批准的 v2 不是生成模型自由设计的 Logo，而是从用户自有 `yuzai.png` 纸面母版中确定性提取
墨线，再分别着色。再生成命令：

```text
python3 scripts/design/extract_yuzai_asset.py \
  /Users/zfu/Documents/develop/win/yeuxpage/yuzai.png \
  assets/brand/yeux-fish-doodle-paper-v2.png \
  assets/brand/yeux-fish-doodle-nocturne-v2.png
```

`scripts/design/recolor_rgba_png.py` 可对项目自有的透明单色 PNG 做确定性主题换色；两支脚本均只
支持已明确验证的 8-bit、非交错 RGB/RGBA PNG，格式不符时失败关闭。

## 被否决的探索

`yeux-signal-fish-v1.png` 是 2026-08-30 的内置 `image_gen` 探索稿。它有透明底，但被否决为
产品资产：过度对称、鱼体华丽、纹章/奇幻感过强，并擅自加入仪表几何、信号火花和多色层。
保留它只是为了记录一次失败方向，任何运行界面、欢迎页、图标和发行物都不得引用它。

2026-08-31 又用 `image_gen` 尝试了更贴近母版的私人手稿版本；造型已明显改善，但生成结果未
稳定提供真实 alpha，因此仍不作为可发布资产。结论是：**生成模型用于探索，母版提取负责生产。**

## 使用边界

位图是可选增强，不是运行时依赖。普通 TTY 不加载图片；支持 Kitty、iTerm2 或 Sixel 的欢迎页
可以显式启用，但加载失败必须静默回退，且不得触发网络、模型或工具调用。鱼仔不能成为状态
glyph，也不能出现在 approval、`unknown`、error 或任何安全证明中。图片或 SVG 永远不能改变
事件顺序、审批结论、权限上限或 replay 语义。
