# YeuX Harness 终端资产与深色 TUI 规范

**状态：v1 设计需求（终端优先）**  
**适用：`yeux` CLI、OpenTUI/TUI、`--plain` 行模式、无头 JSONL 的人类可读诊断**  
**视觉方向：Paper / 纸本仪器**

> 在黑色终端里保留纸上信号的温度，但让每一个边界、状态和因果关系都像仪器读数一样准确。

本文件补充 [AESTHETIC.md](./AESTHETIC.md) 与 [tokens.json](./tokens.json)，只规定终端呈现层。它不能修改 policy、approval、ledger、replay 或协议语义。所有安全含义必须由状态文字和结构提供，glyph、颜色、动画只能作为辅助。

## 1. 终端的美学目标

YeuX 的终端不是“黑底霓虹 AI 聊天框”，而是一台本地的黑墨工作仪器：

- **夜墨为底，纸白为字**：夜墨使用 `#2A2733` 背景和略带暖色的 `#E2DED5` 主文字，不使用纯白大面积发光。
- **一条时间轨，一份事实源**：Turn、Item、工具和 replay 共享垂直因果轨；`seq`、状态和时间保持稳定列位。
- **朱红只表示需要判断的边界**：approval、外部写、危险、`unknown` 使用朱红或对应 glyph，不能把朱红当普通品牌按钮色。
- **焦点像压痕，不像霓虹灯**：当前行用窄边框、反差或轻微错位阴影突出；不对每一行加发光、渐变或持续动画。
- **可复制、可审阅、可回放**：终端输出在窄屏、复制到纯文本、禁色和 screen reader 模式下仍保留同一语义。

### 1.1 不做什么

- 不把 emoji、彩色方块或私有区字符作为核心图标。
- 不使用光标移动、清屏、OSC 52、未验证超链接等装饰来显示不可信内容。
- 不用闪烁、彩虹渐变、逐字打字模拟表现“智能”。模型流本身就是动态内容。
- 不把图片嵌入普通 TTY。位图只允许用于网页、启动页或明确支持 Kitty/iTerm2/Sixel 的可选欢迎画面。

## 2. 深色主题表面与颜色阶梯

### 2.1 Nocturne token

| 层级 | Token | 建议值 | 用途 |
| --- | --- | --- | --- |
| 0 | `night.bg` | `#2A2733` | 主背景、空白区 |
| 1 | `night.surface` | `#101214` | Session、timeline 面板 |
| 2 | `night.raised` | `#171B1E` | 当前行、抽屉、输入区 |
| 3 | `night.line` | `#3B4145` | 分隔线、轨道、边框 |
| text | `night.ink` | `#E2DED5` | 正文、审批目标、错误摘要 |
| text-2 | `night.ink.soft` | `#C0BBB1` | 工具参数、说明 |
| text-3 | `night.muted` | `#99958D` | `seq`、耗时、来源、次级标签 |
| focus | `night.tide` | `#6C9AB3` | 当前焦点、模型流、链接 |
| focus-deep | `night.tide.deep` | `#173A52` | 选中底、错位阴影 |
| approval | `night.seal` | `#D17968` | 审批、外部写、危险边界 |
| success | `night.moss` | `#79A988` | 已完成、已验证 |
| warning | `night.ochre` | `#D0AB61` | 等待、预算、非破坏性警告 |

颜色只是第二层信号。每一个颜色状态都必须配合状态词、风险词或可读 glyph，例如 `! UNKNOWN`、`? WAITING FOR APPROVAL`、`✓ COMPLETED`。

### 2.2 ANSI 降级

呈现器按能力从高到低选择：

1. truecolor（24-bit）
2. ANSI 256 色
3. ANSI 16 色
4. 无色纯文本

无色模式仍保留上面的 glyph、标签、边框和顺序。不要用“颜色被关闭后换成另一个不透明符号”的方式改变语义。`NO_COLOR` 存在（无论值为何）时关闭 SGR；`TERM=dumb`、非 TTY、`CI=true` 或重定向输出默认进入 line/plain 模式。未来可提供显式 `--color=auto|always|never`，但 `never` 的优先级高于环境探测。

## 3. 资产使用总则

### 3.1 字符选择

- 核心 glyph 必须是单个 Unicode 标量、预期终端列宽为 1，并有 ASCII 备选。组合字符、零宽连接符、emoji、私有区字符和依赖字体的图示不可进入核心状态。
- 采用 Unicode 时优先使用 ASCII/Latin-1 语义明确的符号与 Box Drawing；避免 East Asian Ambiguous 宽度字符（例如 `※`、`☆`、`∞`）出现在必须对齐的列。
- 渲染器按 grapheme cluster 与 `wcwidth`/`wcswidth` 截断；永远不要按 UTF-16 code unit 直接切断 glyph 或模型文本。
- 终端列宽小于 80 时禁用装饰性双栏和长框体；小于 40 时只输出短标签和正文，不强行画 rail。
- `YEUX_ASCII=1` 或 `--ascii`（规划项）强制 ASCII；非 UTF-8 locale、`TERM=dumb` 和检测到宽度不一致时也使用 ASCII。

### 3.2 语义分层

每一条状态至少包含：

```text
<seq> <glyph> <状态词> <可选摘要> <可选耗时/预算>
```

例如：

```text
0189 ✓ COMPLETED                         1.8k tok · 6.4s
```

`✓` 被字体替换、颜色关闭或屏幕阅读器忽略时，`COMPLETED` 仍足以传达状态。高风险状态必须再包含对象和 effect，例如 `! UNKNOWN · process · reconcile required`。

### 3.3 不可信文本

模型输出、工具输出、项目文件、MCP/plugin stderr 和诊断消息都属于不可信文本：先经过现有 `sanitizeTerminalText`，再嵌入任何资产模板；不能让输入覆盖 rail、伪造审批标题或改变当前行。JSONL 保留原始协议值，终端 sink 只显示清理后的值。

## 4. Unicode / ASCII 资产表

以下“主 glyph”用于 UTF-8、普通宽度终端；“ASCII fallback”是稳定的协议外观，必须可独立阅读。状态词不可省略。

### 4.1 品牌、会话与传输

| 语义 | 主 glyph / 组合 | ASCII fallback | 推荐标签与用法 |
| --- | --- | --- | --- |
| YeuX compact mark | `><` | `><` | 唯一的常驻品牌标记；不要在每个 Item 前重复鱼形 |
| YeuX full doodle | 图片/SVG（不进对齐列） | `><` | 只用于启动、About、空状态；不是安全状态，不使用 `⋈` 无限符号 |
| 本地 workspace | `⌂` | `[local]` | 与 workspace 短码同一行，不能代替 trust |
| 已连接 daemon/socket | `↔` | `<->` | `socket CONNECTED`；连接失败显示文字而非仅断线 glyph |
| provider/model | `◇` | `o` | 仅作来源标记，不能表示 tool proposed |
| replay 播放头 | `⟲` | `[R]` | 与 `REPLAY READ-ONLY` 同现；不显示执行 spinner |
| 只读/观察模式 | `○` | `[RO]` | 必须同时显示 `OBSERVE` 或 `READ-ONLY` |

品牌标记不使用彩色方块、Unicode 鱼 emoji 或字体图标。`><` 在任何字体、复制和 ASCII 环境中都稳定，适合作为 Session Bar 的第一列。

### 4.2 Timeline rail 与结构线

| 结构 | 主 glyph | ASCII fallback | 说明 |
| --- | --- | --- | --- |
| 主轨 | `│` | `|` | 同一 Turn 的连续因果线 |
| 分支 | `├─` | `+-` | 从 parent turn 分出的 Item/子智能体 |
| 末项 | `└─` | `` `- `` | Turn 的最后一项或 coda |
| 继续分支 | `│ ` | `| ` | 轨道下方保留两个空格，避免文本粘连 |
| 轻分隔 | `┄` | `- -` | 非安全性、可折叠的次级区块 |
| 强分隔 | `━` | `=` | Approval、replay header、模式切换 |
| 细点节拍 | `·` | `.` | 空白/accepted/静止节拍；不表示成功 |
| 缺失/断轨 | `╎` | `:` | 诊断 `SEQ GAP`，必须伴随明文 |

主 rail 不承载安全判断；它只表达结构。若宽度计算不可靠，整个行可退化为单列前缀（如 `0189 OK COMPLETED`），而不是拼接错位的 Box Drawing。

### 4.3 Turn / Item 状态

| 内部状态 | 主 glyph | ASCII fallback | 必须显示的文字 | 动态规则 |
| --- | --- | --- | --- | --- |
| accepted/queued | `·` | `.` | `QUEUED` 或 `ACCEPTED` | 静止 |
| building context | `◌` | `o` | `CONTEXT` | 可慢速变化，最多 4 fps；plain 模式静止 |
| requesting model | `↗` | `->` | `MODEL REQUESTED` | 单次出现，不旋转 |
| streaming | `≈` | `~` | `STREAMING` | 只允许一个活动焦点；不逐字打字 |
| tool proposed | `◇` | `o` | `TOOL PROPOSED` | 静止，配 tool id/effects |
| waiting approval | `?` | `?` | `WAITING FOR APPROVAL` | 静止并聚焦；默认 deny |
| authorized | `◆` | `*` | `AUTHORIZED` | 一次短脉冲后静止 |
| executing | `▶` | `>` | `EXECUTING` | 屏幕中唯一持续 spinner/执行提示 |
| integrating | `∿` | `~` | `INTEGRATING` | 慢速变化；不得暗示已提交 |
| completed | `✓` | `OK` | `COMPLETED` | 立即静止 |
| failed | `×` | `ERR` | `FAILED` | 立即静止，附原因 |
| cancelled | `—` | `--` | `CANCELLED` | 静止；附取消来源 |
| unknown | `!` | `!!` | `UNKNOWN · RECONCILIATION REQUIRED` | 持续可见；绝不自动重试 |
| paused | `Ⅱ` | `||` | `PAUSED` | 静止；附恢复条件 |
| expired | `⌛`（可选） | `EXP` | `EXPIRED` | 不依赖沙漏 glyph；审批默认拒绝 |

`unknown`、`failed`、`waiting approval` 即使在无色、低对比度或色盲环境也必须显著：使用不同 glyph、全大写状态词和明确动作文案。

### 4.4 Effect / 风险与审批

| effect 或动作 | 主 glyph | ASCII fallback | 文字模板 |
| --- | --- | --- | --- |
| filesystem read | `○` | `[r]` | `READ path` |
| filesystem write/patch | `✎` | `[w]` | `WRITE path · base <hash>` |
| delete | `⌫`（可选） | `[del]` | `DELETE path · approval required` |
| process | `▶` | `[proc]` | `PROCESS argv · sandbox <name>` |
| network | `↗` | `[net]` | `NETWORK host:port · proxy <name>` |
| secret/credential | `#` | `[secret]` | `SECRET HANDLE · never shown` |
| external write | `⇥` | `[ext]` | `EXTERNAL WRITE target` |
| sandbox boundary | `□` | `[sbx]` | `SANDBOX <profile> · ceiling <profile>` |
| approval gate | `?` | `[ask]` | `APPROVAL REQUIRED` |
| allow once | `✓` | `[allow]` | `ALLOW ONCE` |
| deny | `×` | `[deny]` | `DENY` |
| reconciliation | `↻` | `[reconcile]` | `RECONCILIATION REQUIRED` |

Effect glyph 与状态 glyph 不能互相替代。例如 `▶ EXECUTING` 只表达生命周期，`[proc] PROCESS` 才表达副作用类别。审批行必须列出 effect、目标、binding digest、有效期和默认操作。

推荐的深色审批面板：

```text
┏ APPROVAL REQUIRED · workspace.apply_patch@1
┃ ! WRITE 1 FILE · PROCESS none · NETWORK none
┃ target  crates/yeux-core/src/approval.rs
┃ binding d42f…91c8 · expires 45s · default DENY
┗ [a] ALLOW ONCE   [d] DENY   [i] INSPECT
```

ASCII 版本：

```text
+- APPROVAL REQUIRED · workspace.apply_patch@1
| !! WRITE 1 FILE · PROCESS none · NETWORK none
| target  crates/yeux-core/src/approval.rs
| binding d42f...91c8 · expires 45s · default DENY
`- [a] ALLOW ONCE   [d] DENY   [i] INSPECT
```

### 4.5 Replay、诊断与一致性

| 语义 | 主 glyph | ASCII fallback | 文字要求 |
| --- | --- | --- | --- |
| replay header | `⟲` | `[R]` | `REPLAY READ-ONLY · seq 0180–0189` |
| checkpoint | `▣` | `[C]` | `CHECKPOINT <source range>` |
| causation link | `↳` | `->` | `CAUSED BY <event id>` |
| projection match | `≡` | `==` | `PROJECTION MATCH` |
| projection drift | `≠` | `!=` | `PROJECTION DRIFT · inspect required` |
| sequence gap | `╎` | `:` | `SEQ GAP · expected … received …` |
| backpressure | `⋮` | `...` | `CLIENT SLOW · replay available` |
| diagnostic | `!` | `[diag]` | `DIAGNOSTIC <code> · <message>` |

诊断和 replay 必须是可复制的静态行，不得使用闪烁或清屏。`PROJECTION DRIFT` 与 `UNKNOWN` 均不提供“继续执行”快捷键，除非用户明确进入 reconciliation 流程。

### 4.6 键盘提示

键盘提示的规范形式固定为 ASCII 方括号，保证 macOS、Linux、SSH、tmux 和 screen reader 一致：

| 动作 | 首选提示 | 可选平台别名 | 规则 |
| --- | --- | --- | --- |
| allow once | `[a] allow once` | `[y]` | 不把 `⌘`/`⌥` 作为唯一提示 |
| deny | `[d] deny (default)` | `[n]` | 默认焦点必须落在 deny |
| inspect | `[i] inspect` | `[tab]` | 展开 effect、digest、来源 |
| submit | `[enter] submit` | — | 输入区底部常驻 |
| cancel/close | `[esc] cancel` | `Ctrl-C` | 先取消当前回合，再退出进程 |
| interrupt | `Ctrl-C interrupt` | — | 第二次才关闭客户端 |
| navigate | `[↑↓] navigate` | `j/k` | 同时支持方向键和字母键 |
| expand/collapse | `[space] toggle` | `o` | 读屏模式输出 `expanded/collapsed` |
| replay | `[r] replay` | — | 只读，不执行外部调用 |
| help | `[?] help` | `F1` | 不覆盖模型文本 |

提示可以使用 `␣`、`⌁` 等装饰性别名，但核心标签必须保留 `[space]`、`[enter]`、`Ctrl-C` 等 ASCII 文案。不要把快捷键只放在颜色或底部状态栏中。

## 5. 组件节奏与密度

### 5.1 Session Bar

120 列以上的首选形式：

```text
><  YeuX / HARNESS   workshop/7f2a   OBSERVE   local/qwen   ↔ SOCKET CONNECTED
```

80–119 列压缩为：

```text
>< YeuX  workshop/7f2a  OBSERVE  local/qwen  SOCKET OK
```

小于 80 列时只保留不会改变权限判断的字段：

```text
>< OBSERVE · workshop/7f2a · READ-ONLY
```

`workspace identity`、`trust`、`mode` 和 `approval` 不得因为窄屏被隐藏；provider、transport 和长 digest 可折叠到 Inspector。

### 5.2 Timeline 行

```text
0184 │ ◌ CONTEXT                         0.2s
0185 ├─↗ MODEL REQUESTED                 local/qwen
0186 │ ≈ STREAMING  Replay reads events only…
0189 └─✓ COMPLETED                       1.8k tok · 6.4s
```

ASCII fallback：

```text
0184 | . CONTEXT                         0.2s
0185 +- -> MODEL REQUESTED               local/qwen
0186 | ~ STREAMING  Replay reads events only...
0189 `- OK COMPLETED                    1.8k tok · 6.4s
```

模型正文占主阅读层；`seq`、耗时、token、digest 采用 tabular-number 对齐。长行先按 terminal width wrap，再在字段边界截断；绝不把路径中间截断成看似另一个路径。

### 5.3 低宽度模式

| 列数 | 呈现 | 允许 | 禁止 |
| --- | --- | --- | --- |
| `< 40` | 单行短状态 + 正文 | `OK`, `ERR`, `?`, `[a]/[d]` | Box Drawing、双栏、长 hash |
| `40–79` | append-only line UI | 单 rail、短 effect 摘要 | alternate screen、侧栏 |
| `80–119` | 单列 timeline + footer | Inspector 通过快捷键展开 | 持久双栏 |
| `>= 120` | timeline + Inspector 双栏 | 轻边框、局部错位阴影 | 每项重框、全屏噪点 |

## 6. 无色、无 Unicode 与无障碍验收

### 6.1 必须测试的组合

每次 TUI 变更至少录制以下黄金输出并逐字符比较语义：

1. UTF-8 + truecolor + 120 列
2. UTF-8 + `NO_COLOR=1` + 80 列
3. `LC_ALL=C` 或非 UTF-8 + `TERM=dumb` + 79 列
4. SSH/tmux + ANSI 16 色 + 40 列
5. 非 TTY stdout（管道/文件）
6. screen reader/plain 模式（无 spinner、无 alternate screen）

### 6.2 无障碍规则

- 安全状态永远由 **glyph + 英文状态词 + 目标/effect 文本** 三重表达；颜色从输出中移除后，用户仍能作出同一批准决定。
- 选中行使用 `>` 前缀、`FOCUS` 文案或反差背景至少两种方式；不能只依靠反色。
- 不闪烁、不反复重绘同一行、不在模型文本中插入光标控制；持续动画最多一个焦点、最多 4 fps。
- screen reader 模式按逻辑顺序朗读：`seq → 状态 → 对象 → effect → 可用动作`，而不是读取装饰线。
- 路径、命令、digest 和批准绑定使用可复制的等宽文本；不使用连字、花体或图像字符替代。
- 支持 `--plain`（规划项）时，所有 box/rail 都可省略，但状态词、风险词、快捷键和因果顺序必须保留。

### 6.3 安全与终端控制

- 终端 sink 只允许固定模板产生 ANSI；不可信文本经过清理后再拼接。
- 默认禁用 OSC 8 超链接、OSC 52 剪贴板、窗口标题设置、鼠标报告和未显式请求的 alternate screen。
- 若未来启用可信路径超链接，链接目标必须由本地 renderer 生成并单独标记；模型/插件提供的 ESC 序列永不透传。
- 关闭 TUI 或发生异常时恢复 `show cursor`、输入回显和终端模式；不能留下隐藏光标或 raw mode。

## 7. 位图 / ImageGen 资产边界

普通终端不依赖位图。网页和支持图像协议的欢迎页可以使用一个 **静态、低对比、无文字** 的鱼仔墨稿，但它不是状态图标，也不能出现在 approval、unknown 或 error 行中。鱼仔保持实物母版的“背对背、头朝外、尾巴朝内”构图；时间轨属于运行界面，不画进角色资产。

建议的生成 brief（供未来 `image_gen` 或设计师使用）：

```text
Private-notebook fish doodle for YeuX Harness, explicitly not a logo or badge.
Two crude fish people stand back-to-back: leaf-shaped heads face outward, tiny
forked tails point inward and nearly touch, one dot eye and two crooked legs per
fish, with the right fish retaining one small dorsal scribble. Dry broken
graphite ink, separately drawn and non-mirrored, small subject with generous
transparent negative space. One monochrome ink layer only; no spark, signal
wave, event rail, frame, text, geometry, glow, gradient, neon, scales or elegant
fish anatomy. The application deterministically recolors the transparent ink
for Paper or Nocturne.
```

导出建议：SVG/PNG 仅用于 web/欢迎页；终端核心仍使用上表中的 `><`、rail 和状态 glyph。若终端支持 Kitty/iTerm2/Sixel，图片加载失败必须静默回退到纯 Unicode 启动画面。首选资产是从用户自有 `yuzai.png` 母版确定性提取的单色透明墨稿；`image_gen` 只用于探索，不得覆盖母版造型。

## 8. 实施顺序与验收门槛

1. 建立 `TerminalCapabilities` 与 `Theme`，集中决定 Unicode/ASCII、颜色深度、列宽、动画和 plain 模式。
2. 把 glyph、标签、快捷键和 effect 名称放入单一资产表；renderer 不得散落硬编码。
3. 将 Session Bar、Timeline、Approval、Inspector 和 replay header 迁移到纯 presenter；协议事件只提供事实。
4. 为每个状态录制 Unicode、ASCII、无色和窄屏黄金 trace；检查 `wcwidth`、换行、复制和 screen reader 顺序。
5. 在 `NO_COLOR=1 TERM=dumb`、非 TTY、SSH/tmux 下运行真实只读闭环，确认没有 ANSI 控制序列、重复 spinner 或隐藏的安全字段。

**完成定义**：任何用户在 40 列、无色、ASCII-only 或 screen reader 模式下，都能仅凭文字判断当前 mode、workspace、工具 effect、审批默认值、回合状态和 `unknown` 是否需要 reconciliation；在 120 列深色 TUI 中，再获得 YeuX 的“黑墨仪器”层次与节奏，而不是额外的安全含义。
