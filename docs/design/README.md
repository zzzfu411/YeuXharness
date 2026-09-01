# YeuX Harness 设计系统

YeuX Harness 的产品美学是 **纸上信号 / Paper Signal**：

> 纸的温度，仪器的纪律。

它把 `yeuxark.com` 的暖灰纸面、石墨墨色、朱红小印和鱼仔人格，与 `music.yeuxark.com` / KAZAM 的硬边框、错位阴影、双栏仪器感和高密度控制融合为一个本地优先的工作台。**Nocturne（深色）是 TUI、CLI 和网页的默认主题；Paper 是同一系统的浅色切换。**

## 文件

- [AESTHETIC.md](AESTHETIC.md)：完整品牌、美学、信息层级、TUI 状态、动效和验收规范。
- [TERMINAL-WEB-AESTHETIC.md](TERMINAL-WEB-AESTHETIC.md)：深色优先的 TUI/CLI/网页共享需求、断点、状态和发布门槛。
- [terminal-assets.md](terminal-assets.md)：Unicode/ASCII 资产、ANSI 降级、无色/窄屏/无障碍规则。
- [unicode-assets.txt](unicode-assets.txt)：可直接复制到终端快照和黄金 trace 的字符资产表。
- [tokens.json](tokens.json)：Paper / Nocturne 主题 token、glyph、终端断点与动效边界。
- [yeux-harness-concept.html](yeux-harness-concept.html)：无依赖、可直接打开的交互概念稿。

品牌位图（仅用于网页 Welcome、富终端启动板和发行物）：

- [assets/brand/README.md](../../assets/brand/README.md)：用途边界、生成 brief、回退策略与校验和。
- [assets/brand/yeux-fish-doodle-paper-v2.png](../../assets/brand/yeux-fish-doodle-paper-v2.png)：从用户自有鱼仔母版确定性提取的石墨透明墨稿。
- [assets/brand/yeux-fish-doodle-nocturne-v2.png](../../assets/brand/yeux-fish-doodle-nocturne-v2.png)：同一造型的暖骨白深色版本。
- [assets/brand/yeux-fish-doodle-fallback.svg](../../assets/brand/yeux-fish-doodle-fallback.svg)：无位图时的确定性线稿回退。

`yeux-signal-fish-v1.png` 是已否决的 `image_gen` 纹章探索，不属于产品资产；原因和再生成方式见品牌资产说明。

## 预览

```text
open docs/design/yeux-harness-concept.html
```

概念稿支持：

- Paper / Nocturne 主题切换；
- Session Bar、Turn 时间轨、工具结果、Approval Gate 与 Inspector；
- Event / Policy / Replay 三个可键盘操作的检查面板；
- Allow once / Deny（默认）状态反馈；
- 窄屏布局、键盘焦点、`prefers-reduced-motion` 和无外部资源运行。

## 实施约束

设计系统只负责呈现，不改变 Rust authority、policy、ledger、replay 或 JSONL 的语义。主题可以改变表面色与密度，但不能重新定义 trust、approval、danger、unknown 的含义；动画也不能改变事件排序或触发任何外部调用。终端位图始终是可选增强，不能成为核心 TTY 或安全决策的依赖。

当前 line renderer 已建立 `Theme`、`TerminalCapabilities`、Unicode/ASCII 资产和结构化 approval。下一步是把它迁移为完整的 Session Bar / Timeline / Approval / Inspector presenter；OpenTUI screen mode 继续保留当前纯文本路径作为 `--plain`、非 TTY 和故障恢复后备。
