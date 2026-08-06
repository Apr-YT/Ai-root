# ai-root UI 提示词工具包（开源方案）

> 配套项目：ai-root —— LocalSystem 提权命令中继（Rust + egui/eframe + tray-icon，单 exe 分发）
> 用途：把「项目真实视觉规范 + 界面结构 + 交互红线 + 技术约束」打包成可直接复用的提示词，
> 供任意 AI 助手生成 / 重构 / 走查本项目的 GUI 代码或设计稿。
> 当前 GUI 基线：egui 0.30 / eframe 0.30 / tray-icon 0.19，深色「极夜青」v3 配色，三 Tab 运维控制台。

---

## 0. 选型对比（为什么是 egui，什么情况才值得换）

| 方案 | 类型 | 优点 | 缺点 / 对本项目的风险 | 结论 |
|---|---|---|---|---|
| **egui/eframe**（现状）<br>[github.com/emilk/egui](https://github.com/emilk/egui) | 即时模式 GUI | 依赖少、单 exe 小（5.2MB）、无 WebView、全 Rust、热重载方便；SYSTEM 常驻服务无 GUI 依赖（feature-gate 双构建） | 自绘渲染（无系统原生控件）、中文字体需自备、复杂表格性能需注意 | **保持**：与本项目「服务端 headless + 客户端 GUI」双构建变体天然契合 |
| **Slint**<br>[github.com/slint-ui/slint](https://github.com/slint-ui/slint) | 声明式（保留模式） | 声明式 DSL、原生渲染、组件库更完整、支持多语言绑定 | 引入新 DSL 学习成本；二进制 + 构建链更重；与现有 gui.rs 全部重写 | 仅当 UI 复杂度显著上升（图表/复杂列表）再评估 |
| **iced**<br>[github.com/iced-rs/iced](https://github.com/iced-rs/iced) | Elm 架构（保留模式） | 纯 Rust、架构清晰、易测试 | 生态仍在 0.x；窗口/托盘集成不如 eframe 成熟；开发迭代速度慢于即时模式 | 不推荐：本项目交互重（异步任务/事件流），重写成本高 |
| **Tauri 2 + Web 前端**<br>[github.com/tauri-apps/tauri](https://github.com/tauri-apps/tauri) | WebView 混合 | 前端生态全（React/Vue）、动效/排版天花板高、CSS 可控 | **引入 WebView 运行时**：体积 +100MB 级、需 WebView2、前端构建链；对本项目「最小攻击面 + 干净静态指纹」的目标是负资产 | 不推荐：提权工具 GUI 应保持原生、依赖最小 |

**决策红线**：ai-root 的 GUI 是「SYSTEM 服务的运维面板」，不是重交互产品。egui 即时模式带来的「每帧重建 + 单 exe + 零系统依赖」与本项目「最小化攻击面、双构建变体、静态指纹干净」完全一致，**不建议迁移**。以下提示词均面向 egui/eframe。

---

## 提示词 A：从零生成 / 整体重构 UI（全量 Brief）

> 复制下面整段发给任意 AI 助手，要求其输出可直接编译的 `gui.rs`（egui/eframe）代码。

```text
你是资深 Rust GUI 工程师，正在为开源项目 ai-root 重构其 Windows 原生控制面板。
项目定位：ai-root 是一个以 LocalSystem 权限运行的命令中继服务（命名管道 IPC + 共享令牌鉴权），
本 GUI 是其配套运维控制台，供安装用户（非提权）可视化地：查看服务状态、启停服务、轮换令牌、
下发命令、实时查看 AI 操作事件流与审计日志。技术栈固定为 Rust + egui 0.30 / eframe 0.30 +
tray-icon 0.19，单 exe 分发，禁止引入 WebView 与外部运行时。

【视觉规范：深色「极夜青」主题】
- 背景中性阶（从深到浅）：#0B0F14 → #12161D → #1A202A → #232B37 → #2E3A4A（描边）
- 文本：#E6EDF3（主）/ #9AA7B4（次要）
- 语义色（GitHub-Dark 同源，禁止替换含义）：成功 #3FB950 / 警告 #D29922 / 危险 #F85149 / 信息 #58A6FF
- 品牌色 brand #3D8BFF（主操作、链接、选中态）；活动指示 accent #2DD4BF（运行中/焦点）
- 输入/卡片底色 #1A202A，窗口底与面板底可用 rgba 半透明实现玻璃拟态（from_rgba_unmultiplied），
  但必须保证对比度 ≥ 7:1（WCAG AAA），正文不得低于 #9AA7B4
- Tab 选中态 = brand 描边 + accent 2px 底部指示条；hover = 描边变 brand 色
- 全部文本为简体中文；必须加载 CJK 字体（系统字体兜底 + 嵌入 fallback），中文不得乱码

【界面结构：四区 + 托盘】
1. 顶部固定条：产品名 + 状态徽章（运行/停止/未安装，色块+文字）+ 启用开关（Switch）+ 操作提示
2. 左栏（可折叠，默认收起）：服务控制（start/stop/status）、令牌区（指纹展示 + 轮换）、
   管理员操作（install/uninstall，折叠收起，带二次确认）
3. 主区三 Tab（快捷键 Ctrl+1/2/3）：
   - 命令台：快捷命令分组（组标签彩色小字 + 组间分隔）、命令输入框（↑↓历史）、
     执行（异步，运行中可"取消等待"）、输出区（环形缓冲 20 条历史、可翻看、自动滚动开关）
   - 实时信息流：JSONL 事件（exec_start/exec_done/token_rotate/service_start）增量渲染，
     带级别筛选（全部/开始/成功/失败/服务）、PID 精确过滤、暂停滚动开关；成功✔/失败✘/警告⚠符号
   - 审计日志：增量读取 exec.log（禁止整文件重读）、文本过滤 + PID 过滤、着色
4. 底部状态栏：最近退出码 / 耗时 / 事件数 / 日志行数

【交互红线】
- 管道命令执行必须异步（线程 + channel），严禁同步 IO 阻塞 egui 事件循环
- start/stop 为"已授权"操作：install/grant 后用户 SID 已持 GRGX 权限，直接非提权调用服务控制接口，
  不得制造新的 UAC 摩擦点；仅 install/uninstall/rotate-token 走管理员（UAC 提权 + 后台轮询确认）
- 复制输出时若含 64-hex（疑似令牌），必须拦截并警告，绝不静默放行
- 关闭窗口 = 最小化到托盘（不退出）；托盘菜单「退出」才结束 GUI 进程（不影响服务）
- 托盘角标状态：绿=运行且启用 / 红=运行但禁用 / 灰=停止或未安装；右键菜单含：显示/隐藏、启停、退出

【性能与约束】
- egui 为即时模式：每帧重建 UI，无 DOM 概念；大数据区（事件流/日志）必须增量读 + 解析结果缓存，
  禁止每帧全量重读重解析文件
- 保持 `#[cfg(feature = "gui")]` 特性门控：main.rs 的 mod gui 与调用点均按此门控，
  headless（--no-default-features）构建不得残留任何 egui/tray 依赖（无 opengl32/gdi32/dwmapi）
- 无外部网络资源：字体、图标全部内嵌或走系统资源
- 输出必须是完整可编译的 gui.rs，并自检：`cargo check`（默认 features）0 警告 0 错误，
  且 `--no-default-features` 构建不受影响
```

---

## 提示词 B：局部组件 / 单 Tab 改造

> 只想改一个区域时使用。把「提示词 A」中的【视觉规范】【交互红线】【性能与约束】三节保留为硬约束，
> 任务段替换为下面这段（示例：改造实时信息流 Tab）。

```text
基于提示词 A 的视觉规范与红线，仅改造主区「实时信息流」Tab，其余部分一行都不要动：
- 当前问题：事件条目在事件密集时渲染卡顿；级别筛选与 PID 过滤需与渲染逻辑解耦
- 目标：
  1. 事件解析与筛选纯函数化（输入原始 JSONL 行 + 筛选条件，输出渲染模型），单测可覆盖
  2. 渲染时只遍历"筛选后"的缓存结果，绝不在 ui 闭包内重复 serde 解析
  3. 事件条目样式：级别色条（左侧 3px）+ 时间戳（等宽/灰）+ 符号 + 命令（截断）+ meta（次要色）
  4. 保持现有快捷键、暂停滚动开关、增量读 offset 语义不变
- 验收：仅 1 个 commit；cargo check 0 警告；不得改动命令台/审计日志/托盘/服务控制任何代码
```

---

## 提示词 C：视觉一致性走查（评审模式）

```text
你是 UI/UX 评审专家，对 ai-root 的 egui 控制面板做一次视觉与交互走查，只输出问题清单，不改代码：
- 检查项：主题色 token 是否被硬编码色值旁路（应统一走 C_* 常量）；对比度是否达标（正文 ≥ #9AA7B4，
  语义色用途是否正确——绿=成功/红=危险/蓝=操作/青=活动，禁止混用）；状态徽章/开关/按钮的
  可用态、悬停态、选中态是否齐全；中文是否全部走 CJK 字体加载路径；布局在 1280×720 与 1920×1080
  下是否有截断/溢出；关闭窗口→托盘的路径是否唯一且不会误退进程
- 输出格式：按「P0 功能性缺陷 / P1 视觉不一致 / P2 体验优化」三级列出，每条给出行号与修复建议，
  并标注与「极夜青」规范（见提示词 A）的偏差
```

---

## 提示词 D：技术栈迁移评估（仅作对比，不推荐执行）

```text
评估把 ai-root GUI 从 egui/eframe 迁移到 <Slint|iced|Tauri+Web> 的完整方案，输出商业级对比报告：
1. 迁移成本：gui.rs（约 1800 行）全部重写的工时与风险；托盘/最小化/异步任务/增量读取的等价实现
2. 产物变化：exe 体积、导入表（是否有 opengl32/gdi32/dwmapi/WebView2 等新增）、静态指纹与
   杀软误报风险、是否新增运行时依赖
3. 双构建变体（gui feature / headless）在该技术栈下是否仍能保持"SYSTEM 服务零 GUI 依赖"
4. 结论必须给出明确建议（保留 egui / 值得迁移），并列出 3 个最关键的决策依据
```

---

## 使用说明

1. **生成/重构**：用提示词 A（全量）或 B（局部），配合本仓库 `src/gui.rs` 现状（v3 极夜青 + 三 Tab + 托盘）作为参考实现。
2. **验收标准**（每次改动必须满足）：
   - `cargo check`（默认 features）0 警告 0 错误；
   - `cargo check --no-default-features --target x86_64-pc-windows-msvc` 不回归（headless 干净）；
   - GUI 中文无乱码；托盘/最小化/退出行为符合红线；无新增网络依赖。
   - 注意本机 host=gnu 时 embed-resource 走 windres 会失败，check 前需临时跳过 build.rs 资源嵌入（审查后还原）。
3. **风格基线**：当前视觉 token 常量集中在 `gui.rs` 顶部（C_* 系列），新增 UI 一律引用常量，禁止硬编码色值。

---

*开源方案链接：egui https://github.com/emilk/egui · Slint https://github.com/slint-ui/slint · iced https://github.com/iced-rs/iced · Tauri https://github.com/tauri-apps/tauri*
