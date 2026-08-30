# YeuX Harness 设计系统

YeuX Harness 的产品美学是 **纸上信号 / Paper Signal**：

> 纸的温度，仪器的纪律。

它把 `yeuxark.com` 的暖灰纸面、石墨墨色、朱红小印和鱼仔人格，与 `music.yeuxark.com` / KAZAM 的硬边框、错位阴影、双栏仪器感和高密度控制融合为一个本地优先的工作台。

## 文件

- [AESTHETIC.md](AESTHETIC.md)：完整品牌、美学、信息层级、TUI 状态、动效和验收规范。
- [tokens.json](tokens.json)：Paper / Nocturne 主题 token、glyph、终端断点与动效边界。
- [yeux-harness-concept.html](yeux-harness-concept.html)：无依赖、可直接打开的交互概念稿。

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

设计系统只负责呈现，不改变 Rust authority、policy、ledger、replay 或 JSONL 的语义。主题可以改变表面色与密度，但不能重新定义 trust、approval、danger、unknown 的含义；动画也不能改变事件排序或触发任何外部调用。

下一步接入 TUI 时，先建立 `Theme`、`TerminalCapabilities` 和纯 presenter，再把 line renderer 迁移到 Session Bar / Timeline / Approval / Inspector 组件。OpenTUI screen mode 保留当前纯文本路径作为 `--plain`、非 TTY 和故障恢复后备。
