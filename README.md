# AI-Root

[![CI](https://github.com/Apr-YT/Ai-root/actions/workflows/ci.yml/badge.svg)](https://github.com/Apr-YT/Ai-root/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20%7C%20edition%202021-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078D6.svg)](#平台支持)
[![Tests](https://img.shields.io/badge/tests-57%20passed-brightgreen.svg)](#测试)

> **面向 AI 智能体的 LocalSystem 提权命令中继。**
> 非提权（普通用户）的智能体进程，仅凭一个共享令牌即可通过命名管道以 `SYSTEM` 权限自治执行命令，全程无需 UAC 弹窗。

主要消费方是 **AI 智能体本身**，因此文档聚焦「如何可靠对接」而非「人怎么敲命令」。

```text
  AI 智能体 (非提权)                      AI-Root 服务 (LocalSystem)
  ┌──────────────┐   \\.\pipe\AI-Root   ┌────────────────────────┐
  │  subprocess  │ ──── 令牌 + 命令 ───> │  鉴权 → 执行 cmd /C    │
  │  读取令牌文件 │ <─── 退出码 + 输出 ── │  (SYSTEM 最高权限)     │
  └──────────────┘                      └────────────────────────┘
```

---

## 目录

- [为什么需要](#为什么需要)
- [快速开始](#快速开始)
- [平台支持](#平台支持)
- [部署](#部署仅需一次管理员)
- [AI 调用契约](#ai-调用契约)
  - [鉴权](#鉴权) · [调用形态](#调用形态) · [命令语义](#命令语义关键) · [帧协议](#帧协议直接对接管道时) · [编码](#输出编码) · [退出码](#退出码) · [事件流](#实时事件流eventsjsonl)
- [安全模型](#安全模型)
- [限制](#限制)
- [构建](#构建)
- [测试](#测试)
- [环境变量](#环境变量)
- [项目结构](#项目结构)
- [文档](#文档)

---

## 为什么需要

AI 智能体常需执行需要管理员权限的操作（装驱动、改注册表、写系统目录），而 UAC 弹窗会打断无人值守流程。AI-Root 把「提权」这件事**一次性**解决：

- **一次管理员安装** → 注册 LocalSystem 自启服务。
- **之后零提权**：智能体以普通用户身份连命名管道，由 SYSTEM 服务代为执行。
- **凭据隔离**：令牌文件受 ACL 保护（仅 SYSTEM/Administrators 可写、安装用户可读），客户端与服务端共读同一文件。
- **可审计**：每条命令落 `exec.log`（终身保留）+ `events.jsonl`（实时事件流）。
- **可熔断**：`enabled.flag` 删除即整体禁用中继。

---

## 快速开始

**你是哪种情况？**

| 情况 | 怎么做 |
|---|---|
| **只想用** | 下载 Release 中的 `AI-Root.exe`，**无需构建**。跳到 [部署](#部署仅需一次管理员)。 |
| **要改代码 / 自己构建** | `scripts\dev.ps1 -Action all`（体检 + 编译 + 测试），见 [脚本](#脚本scripts)。 |

从源码到可用，最少三条命令：

```powershell
# 1) 体检：缺什么、怎么补（不安装任何东西）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action doctor

# 2) 编译 + 测试
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action all

# 3) 安装（弹一次 UAC；拒绝则不做任何变更）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action install
```

之后（任意用户，无需提权）：

```powershell
AI-Root exec whoami             # → nt authority\system
AI-Root run --json "whoami"     # 传统 shell 路径，默认需人工确认
```

---

## 平台支持

| 项 | 支持情况 |
|---|---|
| **操作系统** | 仅 **Windows x64**（依赖命名管道、`LocalSystem` 服务、`cmd.exe`） |
| **目标三元组** | `x86_64-pc-windows-msvc`（推荐） / `x86_64-pc-windows-gnu` |
| **Rust 版本** | stable，edition 2021 |
| **构建 CI** | GitHub Actions `windows-latest`，msvc + gnu **双 job 全绿** |
| **产物** | 单一 `AI-Root.exe`（console 子系统，约 1.07 MB，静态链接 CRT） |
| **运行时依赖** | 无（无 DLL 分发、无 .NET、无 VC++ 可再发行包） |

> **GUI 已于 2026-08-07 彻底移除**。当前为纯终端版；调用 `AI-Root gui` 会明确报错而非静默失败。
> 替代方案：`AI-Root tail` 实时跟随事件流，`exec.log` 供审计巡检。

---

## 部署（仅需一次，管理员）

```powershell
# 推荐：自动请求 UAC、幂等、安装后自检（配置 / ACL / 中继冒烟）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1

# 旧入口仍可用，但缺少签名检查、幂等判断与安装后验证：
# powershell -ExecutionPolicy Bypass -File install-service.ps1
```

`install-service.ps1` 分 6 步并**逐步打印结果，不含隐藏动作**：

| 步骤 | 内容 |
|---|---|
| 0 | 提权自检（非管理员直接 `exit 1`） |
| 1 | 二进制 SHA256 指纹核对（不匹配即 `exit 1`，避免装错版本） |
| 2 | 安装前快照（服务状态 / 目录 ACL / 部署产物存在性） |
| 3 | `AI-Root install`（注册 LocalSystem 自启服务） |
| 4 | 安装后核对（服务注册与启动类型、`SERVICE_START_NAME`、部署副本与包装器、**Users 无写权限 + SYSTEM 保留 (F)**、`sid.txt`） |
| 5-6 | 启动服务 + `sc qfailure` 失败恢复核对（应 60s/300s/600s） |

> ⚠ 若第 4 步 ACL 检查报 `[X]`（Users 仍持有写权限），**请立即停止使用并反馈**。
> 同时应确认 `NT AUTHORITY\SYSTEM` 为 `(F)`；若为 `(R)` 则命中自降权限缺陷。
> 两处判据的成因与验证见 [docs/安全模型.md](docs/安全模型.md)。

也可手动逐步执行：`AI-Root.exe install` → `AI-Root.exe start`（同样需管理员）。

### 部署产物

| 项 | 路径 |
|---|---|
| 服务二进制 | `%ProgramData%\AI-Root\AI-Root.exe` |
| 全局包装器 | `C:\Windows\AI-Root.cmd` |
| 管理员授权策略 | `%ProgramData%\AI-Root\policy.toml` |
| 自动化规则目录 | `%ProgramData%\AI-Root\rules.d\*.toml` |
| 秘密（令牌） | `%ProgramData%\AI-Root\secrets\token.txt` |
| 审计日志 | `%ProgramData%\AI-Root\logs\exec.log` |
| 实时事件流 | `%ProgramData%\AI-Root\logs\events.jsonl`（`AI-Root tail` 跟随） |
| 运行状态 | `%ProgramData%\AI-Root\state\`（启用标志 / SID / 待确认请求 / 临时脚本） |
| 用户偏好 | `%LOCALAPPDATA%\AI-Root\user.toml`（**只能收紧**，见 [docs/配置说明.md](docs/配置说明.md)） |

> v1.2 起秘密、日志、状态分目录存放；旧版扁平布局在 `install`/服务启动时自动迁移
> （`AI-Root config migrate` 可手动执行）。迁移不可自动回滚，升级前请备份整个目录。

### 命令一览

```text
AI-Root install            注册为 LocalSystem 自启服务（管理员，仅首次）
AI-Root uninstall          卸载服务（管理员；同时清理部署目录/包装器/令牌/身份文件）
AI-Root rotate-token       轮换共享令牌（写盘后服务端自动热加载，旧令牌立即失效）
AI-Root start              启动服务（install 授权后非提权即可）
AI-Root stop               停止服务（install 授权后非提权即可）
AI-Root status             查询服务状态
AI-Root update <新exe路径>  自主更新（需 Authenticode 签名 + 信任根，失败自动回滚）
AI-Root exec <程序> [参数]  结构化执行，不经 shell（策略可精确授权，推荐主入口）
AI-Root op <操作> [--target X]  结构化操作（query_system_info / query_service /
                           restart_service / read_state_file）
AI-Root run [选项] <命令>   传统 shell 执行（默认需人工确认）
AI-Root pending            查看待确认请求
AI-Root approve <id>       批准一条待确认请求（管理员）
AI-Root deny <id>          拒绝一条待确认请求（管理员）
AI-Root policy             查看当前授权策略与自动允许范围
AI-Root config check       校验所有配置层（只读；有错退出码 1）
AI-Root config show        显示合并后的生效配置（--json 供程序消费）
AI-Root config path        打印各配置文件路径
AI-Root config init        生成默认管理员策略（管理员，已存在则不覆盖）
AI-Root config migrate     执行旧布局/旧格式迁移（管理员）
AI-Root signature <exe>    查看 Authenticode 签名与信任状态
AI-Root trust-publisher <exe>  把该证书加入 update 信任根（管理员）
AI-Root server             前台运行管道服务（调试/隔离演练用）
AI-Root tail [选项]        实时跟随 events.jsonl
AI-Root --version          打印版本
AI-Root -h                 命令行帮助
```

> **一次性管理员，之后零介入**：`install` 除注册服务外，还会给**当前用户 SID** 追加服务的 `GRGX`
> 权限（`GENERIC_READ|GENERIC_EXECUTE`，含 `QUERY_STATUS`/`START`/`STOP`）。
> 此后智能体（用户身份、非提权）可自主执行 `status`/`stop`/`start`/`update`。
>
> ⚠ **更新不再是「任意 PE 都能装」**：v1.1 起 `update` 强制验证 Authenticode 嵌入签名 +
> 发布者证书指纹白名单，且目标路径取自 SCM 实际 `ImagePath`。未配置信任根时一律拒绝。
> 详见 [docs/授权策略.md](docs/授权策略.md)。
>
> **失败自动重启**：`install` 配置 SCM Recovery Actions（异常终止后 60s/300s/600s 依次重启），
> 进程被意外杀死（7034）也会在 1 分钟内自愈，无人值守不受影响。

---

## AI 调用契约

### 鉴权

- 读取令牌：`%ProgramData%\AI-Root\token.txt`（**当前用户只读**，AI 进程以安装用户身份运行即可读取）。
- 每次请求须在 `Request.token` 字段携带正确令牌；不符则服务端拒绝执行。
- ⚠ **令牌不等于「任意 SYSTEM 命令」**（v1.1 起）：令牌只决定「能否发起请求」，
  每个请求还要过一遍[授权策略](docs/授权策略.md)：`Allow` 执行 / `Confirm` 等管理员批准 / `Deny` 拒绝。
  内置保护（改 AI-Root 自身、间接执行宿主、不可逆磁盘操作、关闭安全控制、建管理员账户…）
  **任何令牌都放不开**。
- 令牌仍是高权限凭据，须妥善保管，**不得写入日志、提交仓库或泄露**。

### 调用形态

**主入口是 `AI-Root exec`（结构化 argv，不经 shell）**，`AI-Root run` 用于必须使用
cmd 语义的排障场景（默认需人工确认）。

```text
# 结构化执行（推荐）：程序 + 参数以数组传给 CreateProcess
AI-Root exec git status
AI-Root exec --json cargo test --release
AI-Root exec --cwd D:\project\app --timeout 300000 -- cargo build
AI-Root exec -- ruff check .

# 结构化操作（不涉及命令字符串）
AI-Root op query_system_info
AI-Root op query_service --target Spooler
AI-Root op restart_service --target Spooler     # 需在 approved_services 内，否则确认
AI-Root op read_state_file --target policy.json

# 传统 shell（`&`/`&&`/`|`/`()` 可用；默认需人工确认）
AI-Root run "dir C:\Windows"
AI-Root run --file C:\tmp\batch.cmd        # 多行脚本
AI-Root run --json "whoami"

# 确认流程
AI-Root pending                    # 查看待确认
AI-Root approve <id>               # 管理员批准（在提权终端）
AI-Root deny <id>
```

`exec` / `op` / `run` 的退出码：

| 退出码 | 含义 |
|---|---|
| `0..` | 被执命令自身的退出码 |
| `3` | **需要人工确认**（响应含 `confirmation_id`，批准后重放同一条命令） |
| `1` | 客户端/中继层错误（未安装、策略拒绝、连接失败…） |

> **直接拼参数的约定**：通过 `subprocess` 把含换行符的命令字符串传给 `AI-Root run` 时，
> 服务端按「命令含 `\n`」**自动判定为脚本模式**，无需显式 `--file`/`--stdin`。

> **直接拼参数的约定**：通过 `subprocess` 等机制把含换行符的命令字符串传给 `AI-Root run` 时，
> 服务端按「命令含 `\n`」**自动判定为脚本模式**，无需显式 `--file`/`--stdin`。

### 命令语义（关键）

| 形态 | 转义行为 | 适用场景 |
|---|---|---|
| **单行** | `^` 与 `&` 转义为字面量 | 单个命令；含 `&` 的字面量参数（如设备实例 ID） |
| **多行 / `--file` / `--stdin`** | **不转义**，`&`/`&&`/`\|`/`()` 恢复 `cmd` 正常语义 | 批量执行、串联、管道 |

单行的转义代价：**`&` 与 `&&` 串联在单行内失效**（退化为字面 `&`/`&&`），仅 `|`/`<`/`>`/`()` 保留。
这是为了防 `cmd /C` 二次解析把含 `&` 的字面量拆段 —— 跑单行设备命令时正是想要的保护行为。

**脚本模式实现要点**：服务端把多行命令用 ` & ` 连接后经 `cmd /C` **直接传参**（UTF-16 命令行、**不落盘**），
因此**中文命令 100% 可靠**（cmd 读 `.cmd` 文件的代码页继承不稳定）。

脚本模式注意事项：

- 同一行内 `set x=1 & echo %x%` 的 `%x%` 在解析期展开为空 → 需写 `& call echo %x%`；
- 含 `(`/`)` 的括号块（`if (...)`、`for ... do (...)`）自动回退「包装脚本 + call」文件方案；
- 单脚本拼接后超过 **6000 字符**同样回退文件方案（`cmd` 命令行上限 8191）。

> → **AI 批量执行、需要串联或管道时，一律走多行脚本。**

### 帧协议（直接对接管道时）

绕过 CLI 直接连管道时，帧格式为 `[u32 LE 长度][payload bytes]`：

```jsonc
// 客户端 -> 服务端：Request
{
  "token": "<32字节十六进制令牌>",
  "command": "<命令或脚本文本，含 \\n 即脚本模式>",
  "cwd": "<可选工作目录>",
  "timeout_ms": 60000          // 可选，默认 60000；超时后 taskkill /T 杀整棵进程树
}

// 服务端 -> 客户端：Response
{
  "ok": true,                  // 中继层是否成功（鉴权通过且命令已执行/已超时处理）
  "exit_code": 0,              // 被执命令退出码；中继层错误时为 -1
  "stdout": "...",
  "stderr": "...",
  "error": null,               // 中继层错误（鉴权失败/超时/spawn 失败）时非空
  "truncated": false           // stdout/stderr 是否因 1MB 上限被截断
}
```

| 情形 | `ok` | `error` | `exit_code` |
|---|---|---|---|
| 正常 | `true` | `null` | 被执命令的退出码 |
| 鉴权失败 / 已禁用 | `false` | 非空 | `-1` |
| 超时 | `false` | `"执行超时"` | `-1`（进程树已终止） |

### 输出编码

同一 `cmd /C` 进程内输出编码**不统一**（`dir`/`ver` 用 GBK，`echo 中文` 用 UTF-8）。服务端采用**双解码**：

1. 优先严格按 **UTF-8** 解码；
2. 失败回退 **GBK**（CP936 / `CP_OEMCP`）。

**AI 侧解析 `stdout`/`stderr` 应沿用同一策略。**

### 退出码

- `exit_code` **透传**被执命令进程的退出码（命令自身的成功/失败语义）。
- 中继层自身错误（令牌不符、超时、spawn 失败）→ `exit_code = -1` 且 `error` 非空。

### 实时事件流（events.jsonl）

服务端在关键节点追加 JSONL 到 `%ProgramData%\AI-Root\events.jsonl`（每行一个 JSON 对象），
供 `AI-Root tail` 与外部监控**增量读取**（按字节偏移读新增行，`size <= offset` 安全返空）：

| 事件类型 | 触发点 | 关键字段 |
|---|---|---|
| `exec_start` | 鉴权通过、开始执行 | `caller` / `pid` / `cmd` / `timeout_ms` |
| `exec_done` | 命令结束 | `exit` / `duration_ms` / `out_preview`（stdout 前 200 字符） |
| `token_rotate` | 令牌轮换 | 无（**绝不记录令牌明文**） |
| `service_start` | 服务启动 | `pipe` |

```jsonc
{"ts":"2026-08-06 19:30:00","type":"exec_start","caller":"MACHINE\\user","pid":32836,"cmd":"whoami","timeout_ms":60000}
{"ts":"2026-08-06 19:30:00","type":"exec_done","caller":"MACHINE\\user","pid":32836,"cmd":"whoami","exit":0,"duration_ms":52,"out_preview":"nt authority\\system\r\n"}
```

> 与 `exec.log`（审计：命令+退出码，终身保留）互补：事件流含执行中/完成状态、时长与输出摘要；
> 文件超 2MB 自动轮转（改名 `.1` 再写）。

---

## 配置

配置分三层，**后者覆盖前者，但用户层只能收紧**：

| 层 | 位置 | 谁能写 | 能做什么 |
|---|---|---|---|
| 代码默认 | 编译进 exe | —— | 67 条规则：只读查询 + 开发工具链 argv |
| 用户偏好 | `%LOCALAPPDATA%\AI-Root\user.toml` | 当前用户（含 Agent） | 只能收紧 `mode`、缩短确认有效期、设默认超时 |
| 管理员策略 | `%ProgramData%\AI-Root\policy.toml` + `rules.d\*.toml` | 仅管理员 | 规则、服务白名单、update 信任根、模式 |

为什么用户层不能放宽：Agent 以安装用户身份运行、能写 `user.toml`，
一旦它能加规则或把 `strict` 改回 `autonomy`，整套授权就是想绕就绕。
所以 `UserConfig` **结构体里没有** `rules` / `approved_services` / `trusted_publishers`
这些字段 —— 写了会报错（不是被忽略），且整层不生效。

**失败即收紧**：

- 策略/规则文件解析失败、规则 ID 重复 → `locked_down`（只读查询以外全拒）
- 策略文件**曾被初始化但被删除** → `locked_down`，不重建默认（否则删配置=放宽）
- 用户配置损坏 → 忽略用户层（只会更严）

```powershell
AI-Root config check      # 逐层校验；有错退出码 1
AI-Root config show       # 合并后的生效值 + 每层来源说明
AI-Root config path       # 各文件位置
```

格式为 **TOML**（可注释、可拆文件）：策略 `policy.toml`，规则 `rules.d\*.toml`（按文件名字典序合并）。
示例见 [`examples/`](examples/)，完整字段与迁移/回滚见 **[docs/配置说明.md](docs/配置说明.md)**。

---

## 安全模型

> v1.1 起，安全模型从「令牌 = 一个 SYSTEM shell」改为「**令牌 + 服务端授权策略**」。
> 完整机制见 [docs/授权策略.md](docs/授权策略.md)；原始审计见
> [docs/SECURITY_REFACTOR_AUDIT.md](docs/SECURITY_REFACTOR_AUDIT.md)。

### 两层边界

| 层 | 作用 | 失效后果 |
|---|---|---|
| **文件 ACL**（`%ProgramData%\AI-Root\`） | 谁能读令牌、谁能改策略/二进制 | 令牌泄漏 → 可发起任意请求 |
| **授权策略**（`policy.json` + 内置保护） | 每个请求能做什么 | 只能放行策略允许范围内的事 |

ACL 是「谁能进门」，策略是「进门后能做什么」。
历史缺陷 A/B/C（ACL 未真正生效、服务自降只读）的成因与回归测试见
[docs/安全模型.md](docs/安全模型.md)。

### 令牌与 ACL

- **令牌仅在安装时生成一次**；目录 ACL 为 `SYSTEM(F) + Administrators(F) + 安装用户(R)`。
- **令牌轮换**：`AI-Root rotate-token` 写盘后服务端按 mtime 自动热加载，旧令牌立即失效。
- **目录 ACL 失败即拒绝启动**（fail closed）：服务启动时用 `GetNamedSecurityInfoW` 解析
  **二进制 DACL**，校验「危险主体（Everyone/Users/Authenticated Users/INTERACTIVE…）
  无写权限 + SYSTEM 有写权限 + 无残留继承 ACE」。任一条不满足 → 服务不进入主循环，
  并写 `acl_setup_failed` 事件。不再依赖 `icacls` 文本匹配本地化账户名。
- **令牌写入顺序**：先硬化目录 ACL，再写 `token.txt`（避免令牌在弱权限目录中短暂存在）。

### 授权判定

```text
内置保护(硬编码) → deny 规则 → approved_services → allow 规则 → confirm 规则 → 默认(confirm)
```

- **内置保护不可覆盖**：改 AI-Root 自身、间接执行宿主（`cmd`/`powershell`/`mshta`…）、
  下载器、不可逆磁盘/引导操作、关闭安全控制、建管理员账户、服务/计划任务持久化。
- **deny 优先于 allow**：即使同时写了 allow 规则，deny 也先命中。
- **未知操作/未知字段一律拒绝**：协议 `deny_unknown_fields`；op 取值是白名单枚举。
- **策略文件损坏即降级**：除只读查询外全部拒绝（不让「破坏策略文件」变成「放开所有工具」）。

### 执行形态

- **`exec`（结构化 argv）**：程序解析成绝对路径（只在 PATH 查找，**不查当前目录**），
  参数是数组，不经 shell。策略可按「程序 + 参数前缀 + 目录」精确授权。**推荐主入口**。
- **`run`（传统 shell）**：`cmd /C <字符串>`，默认**需人工确认**。字符串无法结构化校验，
  所以 `exec.rs` 里的危险模式黑名单只是纵深防御，**不是边界**。

### 人工确认

服务在 session 0 没有桌面，因此确认走「文件 + 管理员命令」：

```text
请求 → decision=confirm → 返回 request_id（退出码 3，不执行）
管理员 → AI-Root approve <id>  （写 pending\<id>.approved，内容 = 参数哈希）
请求重放 → 哈希匹配 → 一次性消费 → 执行
```

批准与**完整参数**绑定（改一个字符即失效）、一次性、带 TTL；
普通用户无法写 `pending\`（ACL），所以「自己批准自己」不成立。
协议里没有 `confirmed` 字段 —— 发它会导致解析失败。

### 更新

`update` 三道门控：Authenticode **嵌入**签名（`WinVerifyTrust`）→ 发布者证书 SHA-256
命中 `trusted_publishers` → 目标路径取自 SCM 实际 `ImagePath` 且位于受保护目录。
另校验架构一致、拒绝降级、复制后二次比对 SHA-256、启动 20s 不健康则自动回滚。
未配置信任根时**一律拒绝**。

### 运行时加固

| 机制 | 参数 |
|---|---|
| **全局熔断** | 30s 窗口内累计失败 **20** 次 → 全局锁定 **120s** |
| **按调用方定向封禁** | 同一 SID 在 60s 内失败 **5** 次 → 单独拒绝该 SID **300s** |
| **恒定时间令牌比较** | 逐字节 XOR 累加判 0，防时序侧信道 |
| **超时钳制** | 请求 `timeout_ms` 上限 10 分钟 |
| **并发信号量** | 同时处理连接上限 **8**，超出立即返回「服务繁忙」 |
| **连接读超时** | 帧头/帧体分离计时 + 按 payload 放宽的绝对上限（防 slow-loris） |
| **输出截断** | `stdout`/`stderr` 各 **1MB**，超出置 `truncated=true` |
| **UTF-8 安全截断** | 审计预览用 `truncate_utf8`，绝不在字符中间切字节（旧实现会 panic） |
| **私有临时目录** | 脚本模式写 `%ProgramData%\AI-Root\tmp\`（ACL 保护）+ 加密随机名 + `create_new` |
| **审计脱敏** | `--password=` / `-p` / `API_KEY=` 等参数在日志中替换为 `***` |

---

## 限制

- **仅 Windows**（命名管道 + LocalSystem 服务 + `cmd.exe`）。
- 服务必须预先以管理员 `install` **一次**；之后智能体侧无需提权。
- **令牌仍是高权限凭据**：泄漏后可发起任意请求，但仍受授权策略约束（内置保护不可放开）。
- **允许解释器即允许代码执行**：`python`/`cargo`/`node` 在 SYSTEM 下等价于任意代码 ——
  这是高自主性模式的既定取舍，边界是「不可逆破坏 / 改自身 / 间接执行」，
  详见 [docs/授权策略.md §2](docs/授权策略.md)。
- **传统 shell 只能整条确认**（字符串无法结构化匹配），能用 `exec` 就别用 `run`。
- **确认需要管理员在场**：`pending\` 只有 SYSTEM/Administrators 可写。
- **用户配置不能放宽权限**：设计如此（Agent 可写该文件）。想放宽只能改管理员策略。
- **布局迁移不可自动回滚**：升级到 v1.2 后旧版本读不到新路径，回滚需手工移动文件
  （见 [docs/配置说明.md §6.3](docs/配置说明.md)）。
- **update 只接受嵌入签名**：catalog 签名（System32 常见）不被接受；且不检查证书吊销
  （服务可能在无网环境启动）。
- **安装用户可停服务**（`GRGX` 含 STOP/START，为零 UAC 更新保留）→ DoS 面（V-14）。
- 单行命令内 `&`/`&&` 串联失效（设计使然）；批量场景用多行脚本或 `exec`。
- 单命令输出 **1MB 截断**；默认超时 **60s**（上限 10 分钟）；并发上限 **8**。
- 构建产物**不保证逐字节可复现**（上游 `dlltool` 时间戳，见 [docs/构建说明.md](docs/构建说明.md)）。

---

## 脚本（scripts\）

`scripts\` 把常用操作收敛成**可重复执行**的入口。重复运行不会破坏已有安装。

| 脚本 | 作用 | 需要管理员 | 改系统状态 |
|---|---|---|---|
| `doctor.ps1` | 检查 Rust / 目标平台 / 链接器 / SDK / 配置 / 服务状态 | 否 | 否 |
| `build.ps1` | Release 编译，输出 EXE 路径与 SHA256 | 否 | 否 |
| `test.ps1` | 运行测试，区分「通过 / 失败 / 环境跳过」 | 否 | 否 |
| `install.ps1` | 安装或更新，自动请求 UAC，安装后自检 | 是（自动提权） | 是 |
| `uninstall.ps1` | 卸载，默认先备份配置与日志 | 是（自动提权） | 是 |
| `dev.ps1` | 统一入口，透传到上面五个脚本 | 取决于动作 | 取决于动作 |

统一入口：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action doctor
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action build  [-Target msvc|gnu] [-Clean]
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action test   [-Filter policy]
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action install [-PreflightOnly|-Update|-Force]
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action uninstall [-DryRun|-Purge]
```

### 首次安装

```powershell
scripts\dev.ps1 -Action doctor     # 先确认环境
scripts\dev.ps1 -Action all        # 体检 + 编译 + 测试
scripts\dev.ps1 -Action install    # 安装（UAC 一次），装完自动验证
```

安装后自检包括：服务状态、`config check`、数据目录 ACL 提示，以及一次
`AI-Root exec whoami` 冒烟（应返回 `nt authority\system`）。

### 日常更新

```powershell
scripts\dev.ps1 -Action build
scripts\dev.ps1 -Action install -Update
```

`-Update` 走服务端签名更新通道（Authenticode + 发布者指纹，失败自动回滚）。

> **本地自编译的 exe 没有签名，`-Update` 一定会被拒绝** —— 这是设计，不是故障。
> 本地迭代请用 `-Force`（先卸载再安装，策略与日志保留）：
>
> ```powershell
> scripts\dev.ps1 -Action install -Force
> ```

### 行为边界

| 场景 | 行为 |
|---|---|
| 服务已安装，再跑 `install` | 只打印状态并退出 0，**不重装** |
| 服务已安装，要换版本 | 必须显式 `-Update` 或 `-Force` |
| 本地未签名二进制 | `install` 警告后继续（本地开发必须允许）；`-StrictSignature` 则拒绝 |
| 未签名包 + `-Update` | 预检阶段就拒绝，并提示改用 `-Force` |
| 卸载 | 默认把 `policy.toml` / `rules.d` / `logs` / `user.toml` 备份到 `%ProgramData%\AI-Root-backup-<时间戳>` |
| 重复卸载 | 未安装时提示「无需卸载」，退出 0 |
| `-Purge` | 才删除配置、规则、日志与用户偏好，且默认先备份 |
| 仓库路径含中文 | 自动把 `CARGO_TARGET_DIR` 切到 `%LOCALAPPDATA%\AI-Root\build-target` |

`uninstall` 的删除边界（与 `src/service.rs::uninstall` 一致）：

| 删除 | 保留 |
|---|---|
| 服务注册、`AI-Root.exe`(.old/.new)、`state\tmp`、`state\pending`、`state\enabled.flag`、`secrets\token.txt`、`state\sid.txt`、`C:\Windows\AI-Root.cmd` | `policy.toml`、`rules.d\`、`logs\`、`state\policy.initialized`、`%LOCALAPPDATA%\AI-Root\user.toml` |

> 脚本只做编排，**不绕过任何安全机制**：不关闭 Defender、不改安全策略、
> 不跳过 `update` 的签名校验、不替用户确认 UAC。

---

## 构建

**推荐路线：msvc**（Windows 上 Rust 标准做法，无需额外配置 binutils）

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc
# 并安装 VS Build Tools（提供 link.exe）：
#   https://visualstudio.microsoft.com/visual-cpp-build-tools/
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1
```

> `scripts\build.ps1` 只是「环境适配 + 结果报告」的薄封装，真正构建仍走仓库根的
> `build.ps1`（CI 与开发者同一条路径）。它会自动处理中文路径（切 ASCII 产物目录）、
> 输出 EXE 绝对路径与 SHA256。

**备选路线：gnu**（不依赖 VS，但**需额外装 MinGW binutils**）

```bash
# Git Bash 下执行
# gnu 链接期需要完整 binutils（含 as.exe）；rustup 自带的那份缺 as.exe。
export MINGW_BINUTILS_BIN=/c/msys64/mingw64/bin   # 指向含 as.exe 的目录
bash build.sh
```

产物：`<仓库>\target\<target>\release\AI-Root.exe`。

### 设计取舍

- **配置与脚本职责分离**：`.cargo/config.toml` 承载所有与机器无关的稳定配置（`target-dir` 等），
  是**单一事实来源**；`build.sh`/`build.ps1` 只做**与机器相关**的事（定位工具链、注入 binutils、调 cargo）。
- **优先向系统自省，而非猜测路径**：

  | 信息 | 获取方式 | 可移植性 |
  |---|---|---|
  | rust 根 | `rustup show home` 反推（**并校验 toolchain 真实存在**） | 高——只要 rustup 在 PATH |
  | cargo | `rustup which cargo` 直接定位（绕开 rustup shim） | 高 |
  | binutils | `MINGW_BINUTILS_BIN` → PATH 中的 `as.exe` → 常见 MSYS2/MinGW 位置 | 中——gnu 需用户提供 |

- **依赖自检**：缺 rustup / toolchain / binutils 时直接打印**可复制执行**的修复命令，
  而不是让用户对着链接期报错猜。
- **仓库须置于纯 ASCII 路径**（MinGW `ld` 无法正确处理含中文的产物路径）。
- **不硬编码任何本机路径**；非标准安装位置由环境变量告知（见 [环境变量](#环境变量)）。

> 构建踩坑的完整根因分析（host≠target、rustup 布局、资源嵌入、可复现性）见
> [docs/构建说明.md](docs/构建说明.md)。

### CI

`.github/workflows/ci.yml` 在 `windows-latest` 上跑**双 job**（push 到 `main` / PR / 手动 `workflow_dispatch`）：

| job | toolchain | 内容 |
|---|---|---|
| `build-msvc` | `x86_64-pc-windows-msvc` | `build.ps1` 构建 → `cargo test` → 打包上传 `AI-Root-windows-x64` 工件（打印 exe SHA256） |
| `build-gnu` | `x86_64-pc-windows-gnu` | 经 `msys2/setup-msys2` 装完整 binutils → `bash build.sh` → `cargo test` |

> **为什么必须有两个 job**：本机开发环境是 **gnu-only**，msvc job 不可能覆盖 gnu 路径。
> `build-gnu` 走的是**开发者实际使用的 `build.sh`**，而非另写一套等价逻辑 —— 确保 CI 验的就是本机跑的。

---

## 测试

```bash
cargo test --release --target x86_64-pc-windows-gnu
```

当前 **57 个测试**（[6 → 57](#测试策略)）：

| 模块 | 数量 | 覆盖内容 |
|---|---|---|
| `exec` | 6 | cmd 元字符转义（引号内/外、`^`/`&`、未闭合引号） |
| `common` | 3 | `set_dir_acl` 安全回归：剥离继承的 Users 写权限 + 幂等性 + **不得把 SYSTEM 降级为只读** |
| `protocol` | 27 | `validate_request` 全字段边界；序列化向后兼容；`write_frame` 帧编码与 flush；**`read_frame_timeout` 真实命名管道集成测试** |
| `server` | 21 | `ct_eq` 恒定时间比较；`AuthThrottle` 熔断/窗口重置/锁定过期；`record_sid_fail` 定向封禁；**`handle_client` 端到端** |

### 测试策略

三条硬性原则，均经**变异测试**验证（改坏实现必须精确 FAILED）：

1. **测试必须调用真实被测代码** —— 帧测试真实调用 `write_frame` 并校验落盘字节，
   而非在测试内重抄编码逻辑（后者是「伪测试」，实现改错也照样通过）。
2. **无法在测试环境复现的前提，要么抽纯函数，要么就不算被测到** ——
   「进程身份 = SYSTEM」无法在测试中构造，故把选 SID 的逻辑抽成纯函数 `select_grant_sid()`
   后可注入 `(SYSTEM, SYSTEM)` 组合断言。
3. **阈值类断言硬编码期望值**，而非引用常量 —— 否则「改常量」的变异不会被检出（测试跟着一起变）。
   这些数值是对外承诺的**行为契约**。

集成测试用**真实命名管道**（`make_pipe_pair()` 进程内自建 `CreateNamedPipeW` + `CreateFileW`
管道对，**不 mock 任何 IO**）与**真实 `icacls`**。

**已知缺口**（优先级从高到低）：`connect_client` 的 `ERROR_PIPE_BUSY` 重试、
命令黑名单匹配规则、输出截断（1MB）、GBK 回退解码。

> 各缺陷的变异验证记录见 [docs/安全模型.md](docs/安全模型.md) 与
> [docs/测试说明.md](docs/测试说明.md)。

---

## 环境变量

| 变量 | 默认 | 作用 | 适用 |
|---|---|---|---|
| `AIROOT_DATA_DIR` | `%ProgramData%\AI-Root` | 覆盖数据目录（token / exec.log / enabled.flag / sid.txt） | 运行期 |
| `AIROOT_PIPE_NAME` | `\\.\pipe\AI-Root` | 覆盖命名管道名 | 运行期 |
| `AIROOT_DEPLOY_DIR` | `%ProgramData%\AI-Root` | 覆盖部署目录（服务副本落地位置） | 安装期 |
| `AIROOT_RUST_ROOT` | 自动探测 | rust 根目录（含 `cargo/` 与 `rustup/`） | 构建期 |
| `AIROOT_TARGET` | `x86_64-pc-windows-gnu` | 目标三元组 | 构建期 |
| `MINGW_BINUTILS_BIN` | 自动探测 | MinGW binutils 的 `bin` 目录（仅 gnu 目标需要） | 构建期 |
| `CARGO_TARGET_DIR` | `<仓库>/target` | 产物目录 | 构建期 |

**为什么需要 `AIROOT_DATA_DIR` / `AIROOT_PIPE_NAME`**：鉴权凭据与监听管道名都是进程内唯一的。
「临时演练中继」需要与生产服务完全隔离，否则会串到生产实例。
注意：`install` 注册的**服务模式固定使用默认数据目录与管道名**；
环境变量覆盖主要服务于前台 `server` 模式与多实例演练。

```powershell
# 终端 A：起隔离服务（前台模式，Ctrl+C 退出）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe server

# 终端 B：连接隔离实例
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe run "whoami"
```

---

## 项目结构

```text
AI-Root/
├── src/
│   ├── main.rs          # CLI 入口与子命令分发
│   ├── config.rs        # 配置三层加载/合并/迁移/校验
│   ├── server.rs        # 管道服务端（鉴权/策略判定/确认/并发/审计）
│   ├── policy.rs        # 授权策略引擎（Allow/Confirm/Deny + 内置保护）
│   ├── consent.rs       # 文件型确认通道（pending/ + approve/deny）
│   ├── signature.rs     # Authenticode 验证（WinVerifyTrust + 证书指纹）
│   ├── protocol.rs      # 帧协议 v2、严格字段校验、管道连接
│   ├── exec.rs          # 执行引擎（argv 直调 + shell + 安全截断）
│   ├── common.rs        # 令牌管理、路径、DACL 校验、脱敏、日志
│   ├── service.rs       # Windows 服务注册、更新（签名门控）、生命周期
│   └── winapi_raw.rs    # Win32 API 原始绑定
├── docs/                # 设计与参考文档
├── assets/              # 图标与资源脚本
├── .cargo/config.toml   # 构建配置（单一事实来源）
├── build.rs / build.sh / build.ps1
├── install-service.ps1  # 管理员安装脚本（6 步自检）
└── .github/workflows/ci.yml
```

| 文件 | 职责 |
|---|---|
| `src/server.rs` | 管道监听、鉴权、策略判定、确认、并发、审计 |
| `src/policy.rs` | 授权策略引擎：三级判定、内置保护、路径/参数约束 |
| `src/consent.rs` | 确认请求落盘、批准消费（参数哈希绑定、一次性、TTL） |
| `src/signature.rs` | Authenticode 嵌入签名验证 + 发布者指纹 |
| `src/common.rs` | 令牌、路径、二进制 DACL 校验、审计脱敏、日志 |
| `src/protocol.rs` | 协议 v2（op/argv/target/版本、`deny_unknown_fields`） |
| `src/config.rs` | 三层配置加载、合并（只能收紧）、版本/布局迁移 |
| `src/main.rs` | CLI 与子命令 |
| `src/service.rs` | 服务生命周期、更新门控与回滚 |
| `src/exec.rs` | argv 直调 / shell 执行、UTF-8 安全截断、超时清理 |
| `src/winapi_raw.rs` | Win32 绑定（含 crypt32/wintrust/advapi32） |

**运行时依赖**：`windows-service` / `serde` + `serde_json` / `rand` / `chrono` / `sha2`
（纯终端实现，**无 GUI / egui / tray-icon 依赖**）。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/配置说明.md](docs/配置说明.md) | **三层配置优先级、TOML 字段、rules.d、版本/布局迁移与回滚** |
| [docs/CONFIG_AUDIT.md](docs/CONFIG_AUDIT.md) | 配置架构审计：10 项问题与目标架构 |
| [docs/授权策略.md](docs/授权策略.md) | 授权模型、判定顺序、默认白名单、确认流程、更新与回滚 |
| [docs/SECURITY_REFACTOR_AUDIT.md](docs/SECURITY_REFACTOR_AUDIT.md) | v1.1 安全审计：16 项确认问题、修复方案、验证状态 |
| [docs/安全模型.md](docs/安全模型.md) | ACL 隔离缺陷 A/B/C 的成因、验证与回归测试 |
| [docs/构建说明.md](docs/构建说明.md) | 工具链自省、host≠target、资源嵌入、可复现性结论 |
| [docs/测试说明.md](docs/测试说明.md) | 测试矩阵、变异验证记录、已知缺口 |
| [docs/项目结构说明.md](docs/项目结构说明.md) | 目录结构与文件职责 |
| [docs/examples/使用示例.md](docs/examples/使用示例.md) | 多语言集成示例 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |

---

## 许可

本项目采用 [MIT 许可证](LICENSE)。

## 免责声明

AI-Root 以 `LocalSystem` 身份执行命令，**持有令牌即等同于拥有 SYSTEM 命令执行权**。
请仅在自有、受控的主机上使用，并确保：

- 令牌不被提交到仓库、写入日志或通过不安全通道传输；
- 数据目录 ACL 保持 `SYSTEM(F) + Administrators(F) + 安装用户(R)`（安装脚本会自检）；
- 定期用 `AI-Root rotate-token` 轮换令牌。

使用者须自行确保其使用方式符合所在地区的法律法规与目标系统的服务条款。
