# AI-Root

[![CI](https://github.com/Apr-YT/ai-root/actions/workflows/ci.yml/badge.svg)](https://github.com/Apr-YT/ai-root/actions/workflows/ci.yml)
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
| **要改代码 / 自己构建** | 见 [构建](#构建)。 |

最小可用流程（管理员 PowerShell，一次）：

```powershell
powershell -ExecutionPolicy Bypass -File install-service.ps1
```

之后（任意用户，无需提权）：

```powershell
AI-Root run --json "whoami"     # → nt authority\system
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
# 以管理员身份打开 PowerShell，然后执行：
powershell -ExecutionPolicy Bypass -File install-service.ps1
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
| 服务二进制 | `%ProgramData%\AI-Root\AI-Root.exe`（可用 `AIROOT_DEPLOY_DIR` 覆盖） |
| 全局包装器 | `C:\Windows\AI-Root.cmd` |
| 数据/令牌目录 | `%ProgramData%\AI-Root\` |
| 令牌文件 | `%ProgramData%\AI-Root\token.txt` |
| 审计日志 | `%ProgramData%\AI-Root\exec.log` |
| 实时事件流 | `%ProgramData%\AI-Root\events.jsonl`（`AI-Root tail` 实时跟随） |

### 命令一览

```text
AI-Root install            注册为 LocalSystem 自启服务（管理员，仅首次）
AI-Root uninstall          卸载服务（管理员；同时清理部署目录/包装器/令牌/身份文件）
AI-Root rotate-token       轮换共享令牌（写盘后服务端自动热加载，旧令牌立即失效）
AI-Root start              启动服务（install 授权后非提权即可）
AI-Root stop               停止服务（install 授权后非提权即可）
AI-Root status             查询服务状态
AI-Root update <新exe路径>  自主更新服务二进制：PE 校验→停服务→覆盖→启服务（无需管理员）
AI-Root run [选项] <命令>   以 SYSTEM 权限执行（智能体主入口）
AI-Root server             前台运行管道服务（调试/隔离演练用）
AI-Root tail [选项]        实时跟随 events.jsonl
AI-Root -h                 命令行帮助
```

> **一次性管理员，之后零介入**：`install` 除注册服务外，还会给**当前用户 SID** 追加服务的 `GRGX`
> 权限（`GENERIC_READ|GENERIC_EXECUTE`，含 `QUERY_STATUS`/`START`/`STOP`）。
> 此后智能体（用户身份、非提权）可自主执行 `status`/`stop`/`start`/`update`，**更新服务二进制不再需要管理员**。
>
> **失败自动重启**：`install` 配置 SCM Recovery Actions（异常终止后 60s/300s/600s 依次重启），
> 进程被意外杀死（7034）也会在 1 分钟内自愈，无人值守不受影响。

---

## AI 调用契约

### 鉴权

- 读取令牌：`%ProgramData%\AI-Root\token.txt`（**当前用户只读**，AI 进程以安装用户身份运行即可读取）。
- 每次请求须在 `Request.token` 字段携带正确令牌；不符则服务端拒绝执行。
- **持有令牌即等价于拥有 SYSTEM 命令执行权** —— 令牌等同于高权限凭据，须妥善保管，
  **不得写入日志、提交仓库或泄露**。

### 调用形态

`AI-Root run` 是智能体主入口：

```text
# 单行命令（默认超时 60s，非提权即可）
AI-Root run "whoami"

# 覆盖超时（超时后整棵进程树被杀）
AI-Root run --timeout 5000 "ping -n 10 127.0.0.1"

# 多行脚本（文件）：&/&&/|/() 全部正常，推荐用于批量自治
AI-Root run --file C:\tmp\batch.cmd

# 从标准输入喂入脚本
type C:\tmp\batch.cmd | AI-Root run --stdin

# 结构化输出（AI 消费推荐）：{ok,exit_code,stdout,stderr,error,truncated}
AI-Root run --json "whoami"

# 指定工作目录
AI-Root run --cwd C:\Windows --json "cd"

# 分隔符之后全部按字面量（不经任何解析）
AI-Root run -- <原始命令>
```

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

## 安全模型

- **令牌仅在安装时生成一次**；目录 ACL 为 `SYSTEM(F) + Administrators(F) + 安装用户(R)`，
  其余账户不可访问。⚠ 该隔离**曾长期未真正生效**（缺陷 A/B）且曾出现服务自降只读（缺陷 C），
  成因、验证与回归测试见 [docs/安全模型.md](docs/安全模型.md)。
- **令牌轮换**：`AI-Root rotate-token` 生成新令牌写盘，服务端按文件 mtime 变化**自动热加载**（无需重启），
  旧令牌立即失效。泄露后的标准处置：`rotate-token` → 更新 AI 侧令牌引用。
- **审计日志**：`exec.log` 逐行记录 `[时间] caller=DOMAIN\user pid=xxx exit=code cmd=...`。
  `caller` 来自服务端 `GetNamedPipeClientProcessId` 解析的**管道对端进程身份**（非服务自身环境变量），
  可精确追溯「谁在什么进程里执行了什么」。
- **管道 SDDL** 追加安装用户 SID，使以该用户身份运行的智能体（**可能非交互登录**）也能稳定连接。
- **服务 DACL** 在 `install` 时追加安装用户 SID 的 `GRGX` —— 仅授权 `status`/`stop`/`start`/`update`
  自身服务，**不授予** `CHANGE_CONFIG`/`WRITE_DAC` 等管理权限，不涉及其他服务或系统级权限。
- **启用开关**：`enabled.flag` 存在即启用；删除即禁用（服务端拒绝一切命令，`ok=false`）。
  开关是**运行期语义** —— 禁用后无法经中继自身恢复，需本地/管理员重建 flag，
  或非提权 `AI-Root stop` + `start`（服务启动时自动重建，已实测）。
- **SYSTEM 身份下重固化 ACL 时绝不会把自己当"待授权用户"** —— `granted_user_sid()`
  从 `sid.txt` 取安装用户 SID 并硬性排除 `S-1-5-18`。

### 运行时加固

| 机制 | 参数 |
|---|---|
| **全局熔断** | 30s 窗口内累计失败 **20** 次 → 全局锁定 **120s**（期间拒绝一切请求） |
| **按调用方定向封禁** | 同一 SID 在 60s 内失败 **5** 次 → 单独拒绝该 SID **300s**，不影响其他调用方 |
| **恒定时间令牌比较** | 逐字节 XOR 累加判 0，防时序侧信道逐位猜令牌 |
| **超时钳制** | 请求 `timeout_ms` 上限 10 分钟（`min(600_000)`） |
| **并发信号量** | 同时处理连接上限 **8**（`AtomicU32` + RAII 释放），超出立即返回「服务繁忙」 |
| **连接读超时** | 帧头/帧体读取带超时（`PeekNamedPipe` 轮询），防「连上不发数据」的线程泄漏 |
| **输出截断** | `stdout`/`stderr` 各 **1MB** 上限，超出置 `truncated=true` |
| **update 输入校验** | 先校验目标为有效 PE（MZ + PE 头 + x64/x86 Machine）并打印 SHA256，非 PE 拒绝 |

> 两层节流互补：定向封禁防「同一身份反复试探」，全局熔断兜底「无差别暴力尝试」。

---

## 限制

- **仅 Windows**（命名管道 + LocalSystem 服务 + `cmd.exe`）。
- 服务必须预先以管理员 `install` **一次**；之后智能体侧完全无需提权（含二进制更新）。
- **令牌等同于高权限凭据**，泄露即等同于获得 SYSTEM 命令执行权。
- **单行命令内 `&`/`&&` 串联失效**（设计使然，见 [命令语义](#命令语义关键)）；批量场景用多行脚本。
- 单命令输出 **1MB 截断**（`truncated` 标记）。
- 默认超时 **60s**（上限 10 分钟）；并发上限 **8**（超出返回「服务繁忙」，重试即可）。
- 鉴权失败节流见 [运行时加固](#运行时加固)。
- 构建产物**不保证逐字节可复现**（上游 `dlltool` 时间戳，见 [docs/构建说明.md](docs/构建说明.md)）。

---

## 构建

**推荐路线：msvc**（Windows 上 Rust 标准做法，无需额外配置 binutils）

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc
# 并安装 VS Build Tools（提供 link.exe）：
#   https://visualstudio.microsoft.com/visual-cpp-build-tools/
powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
```

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
│   ├── server.rs        # 命名管道服务端（鉴权/节流/并发/审计）
│   ├── protocol.rs      # 帧协议、请求校验、管道连接
│   ├── exec.rs          # 命令执行引擎（转义/超时/输出解码）
│   ├── common.rs        # 令牌管理、路径、ACL、日志
│   ├── service.rs       # Windows 服务注册与生命周期
│   └── winapi_raw.rs    # Win32 API 原始绑定
├── docs/                # 设计与参考文档
├── assets/              # 图标与资源脚本
├── .cargo/config.toml   # 构建配置（单一事实来源）
├── build.rs / build.sh / build.ps1
├── install-service.ps1  # 管理员安装脚本（6 步自检）
└── .github/workflows/ci.yml
```

| 文件 | 行数 | 职责 |
|---|---|---|
| `src/server.rs` | 1247 | 管道监听、鉴权、节流、并发、审计 |
| `src/common.rs` | 867 | 令牌、路径、ACL、日志 |
| `src/protocol.rs` | 764 | 帧协议与校验 |
| `src/main.rs` | 729 | CLI 与子命令 |
| `src/service.rs` | 563 | 服务生命周期 |
| `src/exec.rs` | 523 | 命令执行 |
| `src/winapi_raw.rs` | 257 | Win32 绑定 |

**运行时依赖**：`windows-service` / `serde` + `serde_json` / `rand` / `chrono`
（纯终端实现，**无 GUI / egui / tray-icon 依赖**）。

---

## 文档

| 文档 | 内容 |
|---|---|
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
