# ai-root GUI：GitHub 开源 UI 资源调研与适配清单（egui 0.30）

> 调研时间：2026-08-06 ｜ 方法：crates.io API 实测依赖约束 + GitHub API 核验 star/许可/活跃度
> 项目基线：egui 0.30 / eframe 0.30 / tray-icon 0.19，双构建变体（`gui` feature / headless）
> **结论先行**：下表所有「兼容版本」均已通过 crates.io 依赖 API 实测确认依赖 `egui ^0.30`，
> 且彼此共用同一 egui 版本树 → **可同时引入，无版本冲突**。

---

## 一、兼容矩阵（核心交付）

| 库 | 兼容 egui 0.30 的版本 | 最新版（对应 egui） | GitHub（star / 许可 / 活跃） |
|---|---|---|---|
| egui_extras（官方扩展） | **0.30.0** | 0.36.0 | [emilk/egui](https://github.com/emilk/egui) 29.9k / Apache-2.0 / ✅ |
| egui_plot（官方图表） | **0.30.0** | 0.37.0 | [emilk/egui_plot](https://github.com/emilk/egui_plot) / Apache-2.0 / ✅ |
| egui_tiles（停靠布局） | **0.11.0** | 0.17.0 | [rerun-io/egui_tiles](https://github.com/rerun-io/egui_tiles) 567 / Apache-2.0 / ✅ |
| egui_commonmark（MD 渲染） | **0.19.0** | 0.25.0 | [lampsitter/egui_commonmark](https://github.com/lampsitter/egui_commonmark) 176 / Apache-2.0 / ✅ |
| egui-phosphor（Phosphor 图标） | **0.8.0** | 0.13.0 | [amPerl/egui-phosphor](https://github.com/amPerl/egui-phosphor) 162 / Apache-2.0 / ✅ |
| egui-notify（Toast 通知） | **0.18.0** | 0.22.0 | [ItsEthra/egui-notify](https://github.com/ItsEthra/egui-notify) 184 / MIT / ✅ |
| egui-file-dialog（文件选择） | **0.8.0** | 0.14.1 | [jannistpl/egui-file-dialog](https://github.com/jannistpl/egui-file-dialog) 170 / MIT / ✅ |
| egui_logger（日志面板） | **0.6.2** ⚠ | 0.11.0 | [RegenJacob/egui_logger](https://github.com/RegenJacob/egui_logger) 68 / MIT / ✅ |
| egui_infinite_scroll（无限滚动） | **0.6.0** | 0.12.0 | [lucasmerlin/hello_egui](https://github.com/lucasmerlin/hello_egui) / MIT / ✅ |
| egui_autocomplete（输入补全） | **9.0.0** | 12.0.0 | [JakeHandsome/egui_autocomplete](https://github.com/JakeHandsome/egui_autocomplete) 27 / MIT / ✅ |
| egui_json_tree（JSON 树） | **0.10.0** ⚠ | 0.16.0 | [dmackdev/egui_json_tree](https://github.com/dmackdev/egui_json_tree) 102 / Apache-2.0 / ✅ |
| egui_modal（模态框） | **0.6.0**（停更） | 同左 | [n00kii/egui-modal](https://github.com/n00kii/egui-modal) 81 / MIT / ⚠ 停更 |
| egui_dropdown（增强下拉） | **0.12.0** | 0.15.0 | [ItsEthra/egui-dropdown](https://github.com/ItsEthra/egui-dropdown) 57 / MIT / ✅ |
| egui_dnd（拖拽） | **0.11.0** | 0.17.0 | [lucasmerlin/hello_egui](https://github.com/lucasmerlin/hello_egui) / MIT / ✅ |
| **egui_code_editor（代码编辑器）** | **✗ 无 0.30 版本** | 0.3.7 | [p4ymak/egui_code_editor](https://github.com/p4ymak/egui_code_editor) 137 / MIT / ✅ |

⚠ 标注的三处版本陷阱：
1. **egui_logger 0.6.2 → 0.6.3 直接跳到 egui 0.31**，锁 0.6.2 才能用 0.30；
2. **egui_json_tree 0.11.0 起升 egui 0.31**，锁 0.10.0；
3. **egui_code_editor 最低要求 egui 0.32**（0.2.17 起），egui 0.30 下完全不可用。

---

## 二、推荐接入清单（按优先级）

### P0 推荐接入（直接改善现有 GUI 短板）

| 库（版本） | 用途与接入点 | 理由 |
|---|---|---|
| **egui-file-dialog 0.8.0** | `ai-root update <exe>` 与 `run --file` 的 GUI 化：文件选择器选新 exe / 脚本文件 | 当前 GUI 无文件选择能力，是「零授权摩擦」红线下最值得补的交互 |
| **egui_extras 0.30.0**（feature `syntect`） | 审计日志 Tab：语法高亮着色；信息流/日志长列表的 `TableBuilder` 虚拟滚动 | 官方维护、与 egui 同版本树；高亮提升日志可读性 |
| **egui-notify 0.18.0** | 全局操作反馈 Toast：复制令牌被拦截、服务启停结果、命令超时 | 当前 `hint` 单行提示不够醒目 |
| **egui-phosphor 0.8.0** | 按钮/徽章图标（启停、轮换、托盘菜单图标） | 1000+ 矢量图标、MIT 兼容、纯静态无运行时开销 |

### P1 可选（按需）

| 库（版本） | 用途 | 说明 |
|---|---|---|
| **egui_logger 0.6.2** | 审计日志独立面板（过滤/级别/滚动内置） | 与现增量读方案二选一，接入需适配现有 exec.log 行格式 |
| **egui_infinite_scroll 0.6.0** | 信息流/日志超长列表（上万条事件不卡） | 现方案 `VecDeque` 上限约数百条，事件密集时可用 |
| **egui_commonmark 0.19.0** | 命令输出 Markdown 渲染（命令台输出区） | 依赖 `egui_extras 0.30`，两者需同版本引入 |
| **egui_autocomplete 9.0.0** | 命令输入框：快捷命令/历史命令自动补全 | 与现有 ↑↓ 历史可叠加 |
| **egui_json_tree 0.10.0** | 实时信息流：事件原始 JSON 结构化查看（搜索/折叠） | 运维洞察强，适合展示 events.jsonl 细节 |

### P2 低优先 / 不推荐

| 库 | 结论 |
|---|---|
| egui_plot 0.30.0 | 项目无图表需求，不引入 |
| egui_tiles 0.11.0 | 现「左栏+三 Tab」布局已满足；拖拽停靠属过度设计 |
| egui_modal 0.6.0 | 已停更；现有 `confirm_rotate/confirm_uninstall` 自实现够用 |
| egui_dnd / egui_dropdown | 无对应交互需求 |
| **egui_code_editor** | **egui 0.30 不兼容**；若要（多行命令编辑器）须先升 egui（见第五节） |

---

## 三、中文字体方案（现状 + 开源升级路径）

**现状**（gui.rs `install_cjk_fonts`）：运行时读取 `C:\Windows\Fonts\` 下微软雅黑/黑体/宋体加载——零依赖零内嵌，但**非中文系统字体缺失时中文会变方块**，且微软雅黑有版权、不能内嵌分发。

| 方案 | 仓库（star / 许可） | 适配建议 |
|---|---|---|
| **更纱黑体 Sarasa Gothic** | [be5invis/Sarasa-Gothic](https://github.com/be5invis/Sarasa-Gothic) 12.6k / OFL | **首选**：中英混排美观，`Sarasa Term SC` 等宽变体天然适配命令/日志输出；OFL 可内嵌分发 |
| **Noto Sans SC** | [notofonts/noto-cjk](https://github.com/notofonts/noto-cjk) 4k / OFL | 备选：字形规范，但单文件较大 |
| **JetBrains Mono** | [JetBrains/JetBrainsMono](https://github.com/JetBrains/JetBrainsMono) 12.9k / OFL | 命令输出/日志等宽字体（Monospace 族），可子集化后仅用于终端区域 |

**推荐升级路径（保持「单 exe、无运行时依赖」红线）**：
1. 内嵌 Sarasa Term SC 子集（`include_bytes!` 进 exe）+ 现有系统字体读取作 fallback（顺序：内嵌 → 系统）；
2. **体积控制**：用 `fonttools pyftsubset`（Python 开源）只保留 GB2312 常用字（约 3500 字）+ ASCII + 界面用字，Sarasa 全量 ~20MB → 子集 ~2-4MB；
3. 等宽区（命令台/日志）叠加 JetBrains Mono 子集（约 150KB）。

---

## 四、接入注意事项（风险清单）

1. **版本锁定**：以上 P0/P1 库必须用表中「兼容版本」并 `=x.y.z` 精确锁版；直接 `cargo add` 默认会拉到最新版（0.34+），届时与 egui 0.30 **编译冲突**。
2. **双构建变体**：所有新库必须走 `gui = ["dep:..."]` 特性门控（与现有 egui/eframe/tray-icon 一致），保证 `--no-default-features` 的 headless 服务端二进制**零新增依赖**（不破坏 593KB / 11 DLL 的静态指纹）。
3. **体积/导入表影响**：egui_extras（syntect 高亮）与 egui_commonmark 会显著增大 exe 并新增依赖（syntect 树较大）；接入前先评估 GUI 成品体积预算（当前 5.2MB）。
4. **许可证**：全部 Apache-2.0 / MIT / OFL，与项目兼容（无 GPL 传染）；引入后应在 README/法务处登记。

---

## 五、升级 egui 的路径与代价（仅当需要 egui_code_editor 等新生态时）

- 现状锁定 egui 0.30 的原因：双构建变体 + 已稳定运行的 v3 GUI，升级是**全量重编译 + API 迁移**。
- 若要解锁 egui_code_editor 等（要求 0.32+）：升级目标建议直接 **0.34 或 0.35 LTS 线**（跳过中间版本），迁移面主要为 egui API 变更（0.31 起 `ViewportBuilder`、0.33+ 若干视觉 API 调整）。
- 代价评估：`gui.rs`（约 1800 行）逐项适配 + 托盘/字体/事件流回归 + 双变体验证；**当前无强需求，建议维持 0.30**，表中兼容版本已覆盖本项目全部真实 UI 诉求。
