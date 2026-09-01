# YeuX Harness 深色优先终端与网页美学需求

**版本：v1.1 · 2026-08-31**
**方向：Paper Signal / Nocturne default**
**适用：`yeux` CLI、OpenTUI/TUI、网页运行台、欢迎页和发行物**

> 黑墨是底，纸白是信号；每一次副作用都必须看起来像一个需要确认的事实。

这份需求把 YeuX 的个人主页（暖灰纸面、石墨墨色、双鱼线稿、留白）与
`music.yeuxark.com` / KAZAM（纯黑夜间主题、硬边仪器面板、错位阴影、密集控制）
合成一套可执行的产品语言。它只规定呈现层，不改变 Rust authority、policy、approval、
ledger、replay 或 JSON-RPC 的语义。

## 1. 设计判断

### 1.1 产品记忆点

用户应该记住三件事：

1. **黑墨仪器**：接近黑色的底、暖白的字、少量藏青和朱红；不是紫色渐变的 AI 控制台。
2. **事件时间轨**：Thread/Turn/Item 像一条有序的乐谱，`seq` 与 causation 关系始终可追溯。
3. **鱼仔手稿**：两只鱼人背对背、头朝外、尾巴朝内；它只在品牌锁定、欢迎页和空状态出现，运行中不把鱼图标重复到每一行。

### 1.2 深色默认，纸面可切换

- `nocturne` 是 TUI、网页和首次启动的默认主题：`#080909` 墨黑背景，`#E2DED5` 暖白正文。
- `paper` 是同一系统的浅色主题，保留 `#D8D3CC` 暖灰纸面，不是另一个产品。
- `mono` 和 `high-contrast` 是终端能力/无障碍降级，不参与品牌竞赛。
- 主题只能改变表面、普通焦点和装饰密度，不能重新映射 `approval`、`unknown`、`danger`、
  `trust` 或 `reconciliation`。

## 2. 共享视觉语法

| 语法 | TUI / CLI | 网页运行台 |
| --- | --- | --- |
| 结构 | 单字符 rail、固定列、窄边框 | 1–2px 硬边框、双栏仪器面板 |
| 节奏 | `seq`、状态词、时间和 token 对齐 | 时间线、局部刷新、事件抽屉 |
| 重点 | 当前焦点使用反差/轻错位阴影 | 当前焦点使用深蓝底和 3–5px 错位阴影 |
| 风险 | glyph + 状态词 + effect 文本 | 朱红边界 + 明文 effect + 默认操作 |
| 品牌 | `><` 常驻；完整鱼仔只在欢迎/空状态 | 鱼仔资产可出现，仍不得伪装成安全徽章 |
| 动效 | 只允许一个活动焦点，最多 4fps | 事件入场可错峰；审批、失败、unknown 静止 |

共享规则：先显示“发生了什么”，再显示“为什么/由谁/影响什么”。模型文本是主阅读层，
诊断、digest、耗时和 transport 是可展开的技术层。

## 3. 信息架构与视图合同

### 3.1 Session Bar

120 列或宽屏网页的首选结构：

```text
><  YeuX / HARNESS   workshop/7f2a   BUILD   local/qwen   ↔ SOCKET CONNECTED
```

必须可见：`workspace identity`、trust、mode、provider/model、transport。窄屏可折叠 provider
和 transport，但不能隐藏 mode、trust、workspace 或 approval 默认值。

### 3.2 Timeline

```text
0184 │ ◌ CONTEXT                         0.2s
0185 ├─↗ MODEL REQUESTED                 local/qwen
0186 │ ≈ STREAMING  Replay reads events only…
0189 └─✓ COMPLETED                       1.8k tok · 6.4s
```

每一行都遵守 `<seq> <rail> <glyph> <状态词> <摘要> <耗时/预算>`。状态词不可省略；模型流不
插入伪造光标，不模拟逐字打字。

### 3.3 Tool / Effect

工具卡只在存在结构或风险时展开。写入必须显示目标、base hash 和 effect：

```text
┌ WRITE · workspace.apply_patch@1
│ target  crates/yeux-core/src/approval.rs
│ effect  filesystem.write · process none · network none
│ base    6f3a…c912
└ ? WAITING FOR APPROVAL · default DENY
```

`▶ EXECUTING` 是生命周期状态，`[proc] PROCESS argv` 才是副作用类型，两者不能混用。

### 3.4 Approval

审批面板是视觉上最明确的边界，默认焦点在 deny：

```text
┏ APPROVAL REQUIRED · workspace.apply_patch@1
┃ ! WRITE 1 FILE · PROCESS none · NETWORK none
┃ target  crates/yeux-core/src/approval.rs
┃ binding d42f…91c8 · expires 45s · default DENY
┗ [a] ALLOW ONCE   [d] DENY   [i] INSPECT
```

网页中同一信息进入右侧 drawer；Allow/Deny 必须是可聚焦按钮，effect、binding digest、
workspace identity、工具版本和有效期不能藏在 hover 或 tooltip 中。

### 3.5 Replay

Replay 必须看起来像“读取纸带”，而不是“重新运行”：

```text
⟲ REPLAY READ-ONLY · seq 0180–0189 · ZERO EXTERNAL CALLS
```

Replay 视图禁止发送按钮、执行 spinner、工具动作按钮和“再次运行”暗示；允许查看原始事件、
checkpoint 来源、projection match/drift 和 causation 链。

## 4. 终端断点与输出契约

| 宽度 | 模式 | 要求 |
| ---: | --- | --- |
| `< 40` | compact line | 仅短状态、短 effect、`[a]/[d]`；不画 box/rail，不显示长 hash |
| `40–79` | append-only | 单 rail、稳定换行；不使用 alternate screen，不做双栏 |
| `80–119` | single pane | timeline + footer；Inspector 用快捷键展开 |
| `120–159` | instrument | timeline + 窄 Inspector；approval 可双栏 |
| `>= 160` | wide instrument | 可显示完整 digest、causation 和双栏工具详情，但不铺满装饰 |

环境优先级：显式 `--color=never` > `NO_COLOR` > `TERM=dumb`/非 TTY/CI 自动 plain > 终端能力。
支持 truecolor、ANSI 256、ANSI 16、无色四级降级；`--jsonl` 永远输出协议原样，不受主题影响。

`--plain`、`YEUX_ASCII=1` 和非 UTF-8 locale 必须走稳定 ASCII 外观。所有 Unicode 资产都按
`wcwidth`/grapheme cluster 截断，不按 UTF-16 code unit 切割。

## 5. 色彩与排版

### 5.1 Nocturne token

| 角色 | 值 | 说明 |
| --- | --- | --- |
| background | `#080909` | 墨黑，不使用蓝紫发光 |
| surface | `#101214` | timeline/session 面板 |
| raised | `#171B1E` | 当前行、输入区、approval drawer |
| line | `#3B4145` | rail、分隔线、边框 |
| ink | `#E2DED5` | 主文字，带纸白温度 |
| muted | `#99958D` | seq、时间、来源、次级说明 |
| tide | `#6C9AB3` | 模型流、焦点、链接 |
| tide.deep | `#173A52` | 选中底、错位阴影 |
| seal | `#D17968` | approval、外部写、危险、unknown |
| moss | `#79A988` | completed、verified |
| ochre | `#D0AB61` | waiting、预算与非破坏性 warning |

Nocturne 有两层深色，而不是一张蓝黑渐变：

- **运行层 / instrument black**：`#080909`、`#101214`、`#171B1E`，用于 timeline、approval、Inspector 和 TUI；继承 KAZAM 的硬边与高密度控制。
- **人格层 / dusk paper**：`#2A2733` 配 `#D9D4C9`，只用于 Welcome、About、空状态与鱼仔墨稿；继承个人主页“夜墨纸面”的安静感。

天青 `#5F968F` 只能是 replay/idle 的极弱次级流动色，不能替代 tide 焦点，更不能表示安全；人格层与运行层之间使用硬切面、纸雾或留白，不使用渐变光效。

### 5.2 排版

- TUI 由用户终端字体决定；所有技术字段使用等宽列，数字启用 tabular alignment。
- 网页正文可使用 LXGW WenKai Screen；技术层使用 Iosevka、Monaspace 或系统等宽字体。
- Ma Shan Zheng/Caveat 只用于品牌画面、短 eyebrow 或印章，不用于命令、路径、digest 或审批正文。
- 全大写仅用于短状态标签（`BUILD`、`REPLAY`、`UNKNOWN`），不用于模型长文本。

## 6. 状态、颜色和安全语义

| 状态 | Glyph | 文案 | 颜色（辅助） | 动效 |
| --- | --- | --- | --- | --- |
| queued | `·` | `QUEUED` | muted | 静止 |
| context | `◌` | `CONTEXT` | tide | ≤4fps 慢循环 |
| model requested | `↗` | `MODEL REQUESTED` | tide | 单次出现 |
| streaming | `≈` | `STREAMING` | tide | 仅一个活动焦点 |
| tool proposed | `◇` | `TOOL PROPOSED` | tide | 静止 |
| approval | `?` | `WAITING FOR APPROVAL` | ochre/seal | 静止、聚焦 |
| authorized | `◆` | `AUTHORIZED` | seal | 一次短脉冲后静止 |
| executing | `▶` | `EXECUTING` | tide | 唯一持续提示 |
| completed | `✓` | `COMPLETED` | moss | 立即静止 |
| failed | `×` | `FAILED` | seal | 静止、附原因 |
| unknown | `!` | `UNKNOWN · RECONCILIATION REQUIRED` | seal | 持续可见 |

安全状态始终由 glyph、文字和对象/effect 三层表达。任何主题、`NO_COLOR`、色盲模式或 screen
reader 都必须保留同一批准结论。

## 7. 网页运行台规格

- 桌面宽度 ≥ 1180px：Session Bar + timeline（8 列）+ Inspector（4 列）。
- 768–1179px：单列 timeline，Inspector 变为可聚焦 drawer；approval drawer 保持固定底部操作。
- < 768px：事件变为 append-only cards，workspace/trust/mode 固定在顶部；不把安全字段放进横向滚动。
- 页面默认 Nocturne；Paper 切换必须保留相同文案、状态词和键盘顺序。
- 只在当前焦点、入场和 drawer 开合使用动效；`prefers-reduced-motion` 时改为静态切换。
- 远程模型/工具输出视为不可信文本，网页与终端共用清理规则；禁止将其解释为 HTML、CSS 或控制序列。
- 位图只用于 Welcome/About/空状态；加载失败静默回退到 SVG/Unicode，不影响会话功能。

## 8. 可访问性与安全的视觉验收

每次视觉改动必须检查以下组合：

1. UTF-8 + truecolor + 120 列 Nocturne；
2. `NO_COLOR=1` + 80 列；
3. `TERM=dumb` + `LC_ALL=C` + 79 列；
4. 非 TTY/管道输出；
5. screen reader/plain（无 spinner、无 alternate screen）；
6. approval、unknown、replay 和 seq gap 黄金 trace。

验收问题必须能用“是/否”回答：

- 5 秒内能否读出 workspace、trust、mode、provider 与当前 Turn 状态？
- 无色时能否判断默认是 DENY，且知道具体 effect 与目标？
- Replay 是否明确写出 `READ-ONLY` 和 `ZERO EXTERNAL CALLS`？
- `unknown` 是否持续可见，并明确下一步是 reconciliation 而非重试？
- 恶意 ANSI/OSC、双向控制字符或伪造 box 文本能否改变视觉结构？
- TUI、`--plain`、网页和 JSONL 是否表达同一事件顺序？

## 9. 资产清单与使用边界

### 9.1 必需的代码/文本资产

- `packages/tui/src/aesthetic.ts`：glyph、状态词、effect 词、快捷键、主题和 ASCII fallback 的单一来源。
- `docs/design/unicode-assets.txt`：可复制的 Session/Timeline/Approval/Replay 示例。
- `docs/design/tokens.json`：Paper/Nocturne token、终端断点、动效边界。

### 9.2 可选位图资产

- `assets/brand/yeux-fish-doodle-paper-v2.png`：从用户自有 `yuzai.png` 母版确定性提取的石墨透明墨稿。
- `assets/brand/yeux-fish-doodle-nocturne-v2.png`：同一 alpha 墨稿的暖骨白深色版本，用于网页 Welcome 与富终端启动板。
- `assets/brand/yeux-fish-doodle-fallback.svg`：无位图/无图像协议时的确定性线稿回退。
- `assets/brand/yeux-signal-fish-v1.png`：已否决的 `image_gen` 纹章探索稿，只保留审计来源，产品不得引用。

鱼仔的生产语义是“私人手稿角色”，不是“信号徽记”：不得添加中心火花、事件 rail、Wi-Fi 波纹、
无限符号、镜像对称、鳞片或华丽尾鳍。生成模型可用于探索，但批准资产必须服从实物母版。

推荐的富终端启动板：

```text
><  YEUX / HARNESS
    PAPER SIGNAL · NOCTURNE
    local-first · replayable · explicit boundaries
```

图片加载失败时只保留这三行和 `><`；不尝试重新调用模型或外部网络。

## 10. 分阶段实现目标

### A. 当前 line renderer

1. 集中 `TerminalCapabilities`、`Theme` 与 glyph 资产；移除散落 ANSI 常量。
2. 将事件渲染成稳定的 Session/Timeline/Approval/Replay 语义行。
3. 实现 Nocturne 默认、`NO_COLOR`、ASCII fallback、窄屏折叠和 sanitized sink。

### B. OpenTUI screen mode

1. Timeline + Composer + Inspector drawer；
2. approval focus、deny 默认、SIGINT/异常终端恢复；
3. screen/plain 模式与 40/80/120/160 列快照。

### C. 网页运行台

1. 复用 tokens、状态 glyph、effect 文案和事件顺序；
2. Nocturne 默认、Paper 可切换、replay 明确只读；
3. 引入可选 RGBA/SVG 欢迎资产，不把图片耦合到 authority。

### D. 发布门槛

- TUI 与网页的同一黄金 trace 语义一致；
- 无色/ASCII/plain 下安全字段完整；
- 24 小时运行没有动画、图片、终端状态或未清理输出泄漏；
- 资产、提示词、许可证和 SHA-256 记录齐全。
