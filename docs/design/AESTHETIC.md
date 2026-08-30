# YeuX Harness 美学系统

**方向名：纸上信号 / Paper Signal**  
**状态：v1 设计基线**  
**适用：CLI、TUI、文档、网站与未来桌面端**

> 纸的温度，仪器的纪律。

YeuX Harness 不应长成常见的“黑底霓虹 AI 控制台”，也不应把个人主页的纸纹直接贴到终端上。它的核心形象是一件安静、触觉化、但读数精确的本地仪器：会留下墨迹，也会保留每一次事件的序号、来源和边界。

## 1. 两个来源，三个保留原则

| 来源 | 保留 | 不直接复制 |
| --- | --- | --- |
| yeuxark.com | 暖灰纸面、石墨墨色、朱红小印、鱼仔、手绘轻微重描、低饱和、大片留白 | 毛笔字体不能进入代码正文；拼贴旋转不能影响数据对齐；纸纹不能降低终端可读性 |
| music.yeuxark.com / KAZAM | 方正硬边、错位阴影、双栏仪器布局、强活动焦点、密集但直接的控制 | 不采用高饱和彩色按钮阵列；不把 emoji 当作核心图标；不让所有组件都有重边框 |
| YeuX Harness 自身 | ledger、seq、causation、replay、approval、sandbox、workspace identity | 安全语义不能被装饰隐藏；动画不能改变事件顺序；品牌不能伪装成模型输出 |

最终融合不是“纸张 + 新粗野主义”的拼贴，而是：

- **纸面负责人格**：安静、亲近、会留下痕迹。
- **仪器负责结构**：方正、清楚、精确、可展开检查。
- **乐谱负责节奏**：Turn 是小节，Item 是声部，事件 rail 是时间轴，replay 是只读播放头。

音乐感只通过节奏、层次和时间轴表达，不靠大量音符、播放器比喻或默认声音。

## 2. 品牌核心

### 2.1 四个性格词

1. **Quiet / 安静**：不抢夺注意力；空白是结构，不是未完成。
2. **Exact / 精确**：所有动作都有来源、状态和明确结束。
3. **Tactile / 有触感**：纸、墨、压痕与错位阴影让本地工具不像云端仪表盘。
4. **Alive / 有生命**：仅活动焦点有轻微呼吸，完成后立即静止。

### 2.2 标志

- 主字标写作 **YeuX**；`YeuX` 可以带少量手写不规则，`HARNESS` 使用窄体等宽大写。
- 双鱼相对是品牌记忆点，但只出现在启动页、空状态、About、发行物和应用图标。
- TUI 中使用最简化标记 `><`、`⋈` 或单色鱼形 glyph；不得在每个事件前重复鱼图标。
- `X` 是视觉铰链：既像两条鱼相遇，也像工具与模型、记录与重放的交叉点。

推荐字标结构：

```text
YeuX  /  HARNESS
       PAPER SIGNAL
```

### 2.3 文案语气

- 短句、具体、克制；先说发生了什么，再给技术细节。
- 不拟人化夸张，不使用“魔法”“革命性”“超级智能”等产品套话。
- 人类层使用自然语言；技术层保留 `thread`、`turn`、`seq`、`effect digest` 等准确术语。
- 允许少量诗性短句出现在启动页和空状态，运行中只使用清楚的状态文案。

## 3. 色彩系统

### 3.1 Paper / 纸面主题

Paper 继承 yeuxark.com 的暖灰纸色，但把功能色压低饱和度。正文与关键状态均满足普通文本可读性要求。

| Token | 色值 | 角色 |
| --- | --- | --- |
| `paper.bg` | `#D8D3CC` | 主背景 |
| `paper.surface` | `#E7E3DB` | 浮层、输入区、选中行 |
| `paper.low` | `#C6C0B6` | 次级面、轨道 |
| `paper.edge` | `#B4ADA1` | 装饰边、纸面暗部 |
| `ink.primary` | `#1B1815` | 正文、主线条 |
| `ink.soft` | `#423C36` | 次级正文 |
| `ink.muted` | `#625B54` | 元数据；相对主背景对比度约 4.5:1 |
| `signal.tide` | `#31566B` | 当前焦点、模型流、链接 |
| `signal.seal` | `#8C3A2C` | 审批、外部写、危险边界、印章 |
| `signal.moss` | `#355F43` | 成功、已验证、sandbox ready |
| `signal.ochre` | `#755719` | 等待、预算与非破坏性警告 |

### 3.2 Nocturne / 夜墨主题

Nocturne 吸收 KAZAM 的纯黑与藏青，但使用暖白墨色连接个人主页的纸面世界。

| Token | 色值 | 角色 |
| --- | --- | --- |
| `night.bg` | `#080909` | 主背景，接近墨黑而非蓝黑 |
| `night.surface` | `#101214` | 主面板 |
| `night.raised` | `#171B1E` | 活动与浮层 |
| `night.line` | `#3B4145` | 结构线 |
| `night.ink` | `#E2DED5` | 主文字，带纸白温度 |
| `night.muted` | `#99958D` | 次级信息 |
| `night.tide` | `#6C9AB3` | 当前焦点、模型流 |
| `night.tide.deep` | `#173A52` | 错位阴影、选中底 |
| `night.seal` | `#D17968` | 审批与危险边界 |
| `night.moss` | `#79A988` | 成功 |
| `night.ochre` | `#D0AB61` | 等待与警告 |

### 3.3 颜色纪律

- 一个视图中最多存在一个高显著度活动色。
- 朱红不是普通品牌按钮色，只用于“需要人类判断”或“越过边界”的时刻。
- `unknown / reconciliation` 使用朱红 glyph、明文状态和持续可见的边界，绝不只靠颜色。
- 成功状态完成后降低饱和度；界面不长期铺满绿色。
- Workspace 可以由 identity digest 派生低饱和 motif，但不能覆盖 trust、approval、danger 颜色。
- 支持 truecolor、256 色、16 色、无色四级降级；无色模式保留 glyph 与文字。

## 4. 排版

### 4.1 图形界面与品牌材料

- 中文叙事：`LXGW WenKai Screen` / 霞鹜文楷。
- 中文展示：`Ma Shan Zheng` 只用于短标题、印章或品牌画面，不用于工具参数和长段正文。
- 英文手写点缀：`Caveat`，仅用于 eyebrow、版本签名与非关键标签。
- 技术信息：`Monaspace Argon`、`Iosevka`、`SFMono-Regular` 等等宽字体。

### 4.2 TUI

TUI 不控制用户终端字体，只控制层级：

- 正文正常字重；当前动作与标题用加粗。
- `seq`、耗时、token、digest 使用 tabular-number 对齐。
- 大写只用于短标签，如 `BUILD`、`REPLAY`、`WAITING`。
- 不用全角装饰字符制造“东方感”；中英文都保持自然可读。

## 5. 形状、线条与材质

- 默认圆角为 `0–3px`。这是纸片、设备面板与终端，不是柔软 SaaS 卡片。
- 基础结构使用 1px/单字符线；高风险审批可使用 2px/双线。
- KAZAM 式错位阴影仅用于当前焦点、modal、selected tool 和 approval，不施加到每一项。
- Paper 主题允许极轻的噪点、暗角和纸面亮度起伏；内容区域不得出现影响小字号的高频纹理。
- 手绘抖动只用于鱼仔、品牌分隔线、空状态或一次性入场；表格、代码、路径、digest 与审批边界必须完全笔直。
- 照片和插图仅用于文档、Welcome 与未来桌面端；核心 TUI 不依赖位图。

## 6. 信息架构

```text
AppShell
├── SessionBar
│   ├── YeuX mark
│   ├── workspace identity
│   ├── trust / mode
│   └── provider / transport
├── Timeline
│   └── TurnScore[]
│       ├── UserPrompt
│       ├── PhaseRail
│       ├── AssistantStream
│       ├── ReasoningFold
│       ├── ToolInvocation
│       ├── ToolResult
│       └── TurnCoda
├── Inspector
│   ├── seq / event / causation
│   ├── effects / digest
│   └── replay provenance
├── ApprovalDrawer
├── Composer
└── StatusFooter
```

### 6.1 响应式规则

- `< 80` 列：append-only line UI；不得依赖 alternate screen。
- `80–119` 列：单列 timeline + 固定 footer；Inspector 通过快捷键展开。
- `>= 120` 列：timeline 与 Inspector 双栏；Inspector 默认收起或窄栏。
- 窄屏可以省略品牌说明，但不能省略 trust、mode、effects、approval default 或 `unknown` 状态。

## 7. 事件是一份乐谱

界面使用时间轨而不是聊天气泡。每个 Turn 是一个连续段落，工具与模型属于同一条因果轨迹。

| 状态 | Glyph | 颜色 | 行为 |
| --- | --- | --- | --- |
| accepted | `·` | muted | 静止 |
| building context | `◌` | tide | 最多 4 fps 的慢循环 |
| requesting model | `↗` | tide | 单次出现 |
| streaming | `≈` | tide | 仅活动游标呼吸 |
| tool proposed | `◇` | tide | 静止 |
| waiting approval | `?` | ochre / seal | 静止并保持焦点 |
| authorized | `◆` | seal | 一次短脉冲后静止 |
| executing | `▶` | tide | 屏幕中唯一持续 spinner |
| integrating | `∿` | tide | 慢循环 |
| completed | `✓` | moss | 立即静止 |
| failed | `×` | seal | 立即静止 |
| cancelled | `—` | muted / ochre | 静止 |
| unknown | `!` | seal + 明文 | 持续可见，不自动重试 |

所有状态必须同时包含文字，不能只依赖 glyph、颜色或动画。

## 8. 核心组件

### 8.1 Session Bar

一行完成身份确认：

```text
><  YeuX · Harness   workshop/7f2a   BUILD   local/qwen   socket ✓
```

- `workshop/7f2a` 中的短码来自 workspace identity，不是随机装饰。
- trust 与 mode 相邻显示，避免用户误把 workspace 名称当作权限状态。
- Provider 与 transport 是次级信息，但始终可见。

### 8.2 Turn Score

```text
YOU  14:32:08
› audit the replay boundary and summarize the invariants

  0184 · ◌ context                         0.2s
  0185 · ↗ model                     local/qwen
  0186 · ≈ Replay reads persisted events only…
  0189 └ ✓ complete             1.8k tok · 6.4s
```

- 不使用左右聊天气泡。
- `seq` 默认弱化，可在 technical 模式中显示完整 event ID 与 causation。
- 模型文本是主阅读层；阶段轨、计费和诊断退后一层。

### 8.3 Tool Invocation

工具卡只在存在结构或风险时出现；普通只读步骤可压缩为一行。

```text
┌ read · workspace/search
│ query   "approval digest"
│ scope   crates/yeux-core
└ ✓ 8 matches · 42 ms
```

写操作明确显示 base hash、目标与 effect，不用模糊的“正在修改”。

### 8.4 Approval Drawer

```text
┏ approval · workspace.apply_patch @1
┃ target    crates/yeux-core/src/approval.rs
┃ effects   write 1 file · process none · network none
┃ binding   d42f…91c8 · expires 45s
┃ reason    apply the reviewed patch
┗ [a] allow once    [d] deny (default)    [i] inspect
```

- 默认选择永远是 deny。
- 显示工具版本、workspace、effect digest、有效期和规范化目标。
- 删除、外部写、secret、network 各自独立成行。
- 手绘朱红印章可以出现在图形界面 approval 角落，但不是批准证明；真正授权信息仍是明文 binding。

### 8.5 Replay

- Replay 顶部显示只读播放头、事件范围与 checkpoint 来源。
- Replay 模式不得出现执行 spinner、发送按钮或工具动作按钮。
- 原始事件与投影差异可在 Inspector 中对照；视觉上使用“纸带回放”而不是重新生成动画。

## 9. 动效与声音

- 模型流本身就是主要动效，不做逐字符打字模拟。
- 同屏最多一个持续动画焦点，推荐上限 4 fps。
- 完成、失败、审批、unknown 都使用静态视觉。
- 鱼仔重描只用于启动或空状态，频率远低于个人主页的 90ms 帧动画；产品中建议 500–900ms 一帧或一次 3 帧入场后静止。
- `prefers-reduced-motion`、`NO_COLOR`、`TERM=dumb`、非 TTY、screen reader/plain 模式完全关闭装饰动画。
- 默认不发声。音乐感来自时序与层次；可选的长任务完成提示音必须显式启用，并提供全局静音。

## 10. 内置主题

v1 建议只提供四套，避免无限主题破坏语义：

1. `paper`：默认图形主题；暖灰纸、墨色、藏青信号、朱红审批。
2. `nocturne`：默认夜间主题；墨黑、暖白、藏青错位阴影。
3. `mono`：只使用终端前景色、粗细与 glyph。
4. `high-contrast`：遵循终端背景，显著边框与更少层级色。

主题可以修改表面与普通强调色，不能重新映射安全状态含义。

## 11. 禁止项

- 紫蓝渐变、玻璃拟态、发光 AI 球体、网格宇宙背景。
- 所有卡片统一大圆角、统一阴影、统一 hover 上浮。
- 把 emoji 当作稳定跨终端图标。
- 在审批、失败、unknown 状态上使用抖动、闪烁或欢快弹跳。
- 用模型头像制造聊天软件感。
- 为“音乐感”加入常驻频谱、波形或默认声音。
- 在技术正文中使用毛笔字、手写字或旋转布局。
- 为了美观隐藏 seq、effects、workspace、provider 或默认拒绝状态。

## 12. 实施顺序

### A. 现有 line UI

1. 新增 `Theme` 与 `TerminalCapabilities`，移除 renderer 中的硬编码 ANSI。
2. 新增纯函数 presenter，将协议事件转换为稳定的展示语义。
3. 实现 Session Bar、Turn rail、Turn coda 和结构化 approval。
4. 补齐 `NO_COLOR`、ASCII、plain、80/120/160 列快照测试。

### B. OpenTUI screen mode

1. Timeline + Composer。
2. Inspector drawer。
3. Approval drawer。
4. 局部刷新、焦点管理、SIGINT/异常终端恢复。

### C. 品牌与发行物

1. 双鱼字标与单鱼应用图标。
2. README 截图、官网、Homebrew 与 release notes 模板。
3. Paper / Nocturne 两套一致的录屏和静态素材。

## 13. 验收标准

- 不看 Logo，也能从纸面、硬边与事件时间轨认出 YeuX。
- 5 秒内能确认 workspace、trust、mode、provider 和当前 Turn 状态。
- 审批页在无色、80 列和 reduced-motion 下仍完整可用。
- 同一事件投影在 line、screen、JSONL 中语义一致；JSONL 字节内容不因主题改变。
- replay 画面不会让用户误以为模型或工具正在重新执行。
- 恶意控制字符不能生成品牌栏、审批框或状态 glyph。
- Paper 与 Nocturne 是同一产品，而不是两个互不相关的皮肤。

## 14. 参考基线

- 个人站视觉源：`/Users/zfu/Documents/develop/win/yeuxpage/design/art-direction.md`
- 个人站设计 token：`/Users/zfu/Documents/develop/win/yeuxpage/assets/css/paper.css`
- 个人站鱼仔动效：`/Users/zfu/Documents/develop/win/yeuxpage/assets/js/ink.js`
- KAZAM 公开仓库：`zzzfu411/kazamusic-web`
- Harness 当前 TUI：`packages/tui/src/renderer.ts`、`packages/tui/src/prompter.ts`、`packages/tui/src/app.ts`

这些来源提供设计 DNA；YeuX Harness 的产品语义、安全颜色和信息层级以本文件为准。
