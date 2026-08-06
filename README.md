# ai-root

> **面向 AI 智能体自调用的 LocalSystem 提权命令中继。**
> 非提权（普通用户）的智能体进程，仅凭一个共享令牌即可通过命名管道以 `SYSTEM` 权限自治执行命令，全程无需 UAC 弹窗。本工具的主要消费方是 AI 智能体本身，文档聚焦于「如何可靠对接」而非「人怎么敲命令」。

---

## 1. 架构

```
┌─────────────┐   命名管道 \\.\pipe\ai-root    ┌──────────────────────┐
│  AI 智能体   │ ──── 请求(令牌+命令) ───────> │  ai-root 服务进程     │
│ (非提权用户) │ <── 响应(退出码+stdout/stderr)─│  (LocalSystem 运行)   │
└─────────────┘                               └──────────┬───────────┘
                                                        │ cmd /C <command>
                                                        ▼
                                                  SYSTEM 最高权限执行
```

- **服务**：以 `LocalSystem` 注册为自启 Windows 服务（`SERVICE_NAME = "ai-root"`）。
- **管道**：`\\.\pipe\ai-root`，SDDL 限制连接方为 `SYSTEM` / `Administrators` / `Interactive` + 安装用户 SID（确保智能体进程可无人值守连接）。
- **令牌**：安装时生成 32 字节随机十六进制令牌，落盘于 `C:\ProgramData\ai-root\token.txt`，目录 ACL 仅 `SYSTEM` 完全控制 + 当前用户只读。

---

## 2. 部署（仅需一次，管理员）

```powershell
# 以管理员运行 deploy.ps1（注册服务、生成令牌、设置 SDDL、写入 C:\Windows\ai-root.cmd）
powershell -ExecutionPolicy Bypass -File deploy.ps1
```

> **一次性管理员，之后零介入**：`install` 除注册服务外，还会给**当前用户 SID** 追加服务的
> `GRGX` 权限（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP，Windows 服务安全描述符
> 标准机制；实测 SDDL 零散位 `RP/WP/LO` 组合不含 QUERY_STATUS，勿用）。此后智能体（用户身份、非提权）
> 可自主执行 `ai-root status/stop/start/update`，**更新服务二进制不再需要管理员**。

部署产物：

| 项 | 路径 |
|---|---|
| 服务二进制 | `D:\Kiln\AI-root\ai-root.exe` |
| 全局包装器 | `C:\Windows\ai-root.cmd` |
| 数据/令牌目录 | `C:\ProgramData\ai-root\` |
| 令牌文件 | `C:\ProgramData\ai-root\token.txt` |
| 审计日志 | `C:\ProgramData\ai-root\exec.log` |
| 实时事件流 | `C:\ProgramData\ai-root\events.jsonl`（GUI「实时信息流」数据源） |

安装后服务自启。常用管理命令：

```text
ai-root install     # 注册为 LocalSystem 自启服务（管理员，仅首次；已存在时给出指引）
ai-root uninstall   # 卸载服务（管理员；同时清理部署目录/全局包装器/令牌/身份文件）
ai-root rotate-token  # 轮换共享令牌（写盘后服务端自动热加载，旧令牌立即失效）
ai-root start       # 启动服务（install 授权后非提权即可）
ai-root stop        # 停止服务（install 授权后非提权即可）
ai-root update <新exe路径>  # 自主更新服务二进制：PE 校验→停服务→覆盖→启服务（无需管理员）
ai-root status      # 查询服务状态
ai-root gui         # 打开 GUI 控制面板（人工巡检用）
ai-root -h          # 命令行帮助（无参数双击 exe = 默认打开 GUI）
```

> **失败自动重启**：`install` 会配置 SCM Recovery Actions（异常终止后 60s/300s/600s 依次自动重启），
> 即使进程被意外杀死（7034）也会在 1 分钟内自愈，AI 无人值守不受影响。

---

## 3. AI 调用契约

### 3.1 鉴权

- 读取令牌：`C:\ProgramData\ai-root\token.txt`（当前用户只读，AI 进程以安装用户身份运行即可读取）。
- 每次请求须在 `Request.token` 字段携带正确令牌；令牌不符服务端拒绝执行。
- **持有令牌即等价于拥有 SYSTEM 命令执行权**——令牌等同于高权限凭据，须妥善保管，不得写入日志、提交仓库或泄露。

### 3.2 客户端调用形态

`ai-root run` 是智能体最常用的入口，有三种形态：

```text
# 单行命令（默认，非提权即可）
ai-root run "whoami"

# 覆盖默认 60s 超时（超时后整棵进程树被杀）
ai-root run --timeout 5000 "ping -n 10 127.0.0.1"

# 多行脚本（文件）：&/&&/|/() 全部正常，推荐用于批量自治
ai-root run --file C:\tmp\batch.cmd

# 从标准输入喂入脚本（多行）
type C:\tmp\batch.cmd | ai-root run --stdin

# 结构化输出（AI 消费推荐）：{ok,exit_code,stdout,stderr,error,truncated}
ai-root run --json "whoami"

# 指定工作目录执行
ai-root run --cwd C:\Windows --json "cd"

# 分隔符之后全部按字面量作为命令（不经任何解析）
ai-root run -- <原始命令>
```

> **智能体直接拼参数的约定**：通过子进程 `subprocess` 等机制把含换行符的命令字符串传给 `ai-root run` 时，
> 服务端按「命令含 `\n`」自动判定为脚本模式，无需显式 `--file`/`--stdin`。

### 3.3 命令语义（关键）

- **单行命令**：`^` 与 `&` 会被转义为字面量（防止 `cmd /C` 二次解析把含 `&` 的字面量如设备实例 ID 拆段）。
  代价：**`&` 与 `&&` 串联在单行内失效**（退化为字面 `&`/`&&`）；仅 `|`/`<`/`>`/`()` 保留。
  → 跑单行设备命令（如 `pnputil /remove-device "...&..."`）时这正是想要的保护行为。
- **多行脚本 / `--file` / `--stdin`**：**不转义**，`&`/`&&`/`|`/`()` 恢复 `cmd` 正常语义。
  实现上服务端把多行命令用 ` & ` 连接后经 `cmd /C` **直接传参**（UTF-16 命令行、不读文件），
  **中文命令 100% 可靠**（cmd 读 `.cmd` 文件的代码页继承不稳定，故不落盘）。
  → AI 批量执行多条命令、需要串联/管道时，**一律走多行脚本**。
- **脚本模式注意事项**：
  - 同一行内 `set x=1 & echo %x%` 的 `%x%` 在解析期展开为空，需写 `& call echo %x%`；
  - 含 `(`/`)` 的括号块（`if (...)`、`for ... do (...)`）自动回退「包装脚本 + call」文件方案；
  - 单脚本拼接后超过 6000 字符同样回退文件方案（cmd 命令行上限 8191）。

### 3.4 帧协议（直接对接管道时）

若智能体绕过 CLI 直接连管道，帧格式为：`[u32 LE 长度][payload bytes]`。

```jsonc
// 客户端 -> 服务端：Request
{
  "token": "<32字节十六进制令牌>",
  "command": "<命令或脚本文本，含 \\n 即脚本模式>",
  "cwd": "<可选工作目录>",
  "timeout_ms": 60000          // 可选，默认 60000；超时后服务端 taskkill /T 杀整棵进程树
}

// 服务端 -> 客户端：Response
{
  "ok": true,                  // 中继层是否成功（鉴权通过且命令已执行/已超时处理）
  "exit_code": 0,              // 被执命令进程退出码；中继层错误时为 -1
  "stdout": "...",
  "stderr": "...",
  "error": null                // 中继层错误（鉴权失败/超时/spawn 失败）时非空
}
```

- 鉴权失败 / 已禁用：`ok=false` 且 `error` 非空，`exit_code=-1`。
- 超时：`ok=false`，`error="执行超时"`，命令进程已被终止。

### 3.5 输出编码约定

同一 `cmd /C` 进程内输出编码不统一（`dir`/`ver` 用 GBK，`echo 中文` 用 UTF-8）。服务端采用**双解码**：

1. 优先严格按 **UTF-8** 解码；
2. 失败回退 **GBK（CP936 / `CP_OEMCP`）**。

**AI 侧解析 stdout/stderr 应沿用同一策略**：先按 UTF-8 解析，失败回退 GBK。

### 3.6 退出码

- `Response.exit_code` 透传被执命令进程的退出码（命令自身的成功/失败语义）。
- 中继层自身错误（令牌不符、超时、spawn 失败）时 `exit_code = -1` 且 `error` 非空。

### 3.7 实时事件流（events.jsonl）

服务端在关键节点追加 JSONL 事件到 `C:\ProgramData\ai-root\events.jsonl`（每行一个 JSON 对象），
供 GUI「实时信息流」及外部监控**增量读取**（按字节偏移读新增行，`size <= offset` 安全返空）：

| 事件类型 | 触发点 | 关键字段 |
|---|---|---|
| `exec_start` | 鉴权通过、开始执行 | `caller` / `pid` / `cmd` / `timeout_ms` |
| `exec_done` | 命令结束 | `exit` / `duration_ms` / `out_preview`（stdout 前 200 字符） |
| `token_rotate` | 令牌轮换 | 无（**绝不记录令牌明文**） |
| `service_start` | 服务启动 | `pipe` |

示例：

```jsonc
{"ts":"2026-08-06 19:30:00","type":"exec_start","caller":"king\\AprYT","pid":32836,"cmd":"whoami","timeout_ms":60000}
{"ts":"2026-08-06 19:30:00","type":"exec_done","caller":"king\\AprYT","pid":32836,"cmd":"whoami","exit":0,"duration_ms":52,"out_preview":"nt authority\\system\r\n"}
```

> 与 `exec.log`（审计：命令+退出码，终身保留）互补：事件流含执行中/完成状态、时长与输出摘要，文件超过 2MB 自动轮转（改名 `.1` 再写）。

---

## 4. 安全模型

- 令牌仅安装时生成一次；目录 ACL `SYSTEM(F) + 当前用户(R)`，其余账户不可访问。
- **令牌轮换**：`ai-root rotate-token`（管理员/SYSTEM）生成新令牌写盘，服务端按文件 mtime 变化**自动热加载**（无需重启），旧令牌立即失效。泄露后的标准处置流程：rotate → 更新 AI 侧令牌引用。
- **审计日志**：`C:\ProgramData\ai-root\exec.log` 逐行记录 `[时间] caller=DOMAIN\user pid=xxx exit=code cmd=...`。`caller` 来自服务端 `GetNamedPipeClientProcessId` 解析的**管道对端进程身份**（非服务自身环境变量），可精确追溯"谁在什么进程里执行了什么"。
- 管道 SDDL 追加安装用户 SID，使以该用户身份运行的智能体（可能非交互登录）也能稳定连接。
- 服务 DACL 在 install 时追加安装用户 SID 的 `GRGX`（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP）——仅授权 status/stop/start/update 自身服务，不授予 CHANGE_CONFIG/WRITE_DAC 等管理权限、不涉及其他服务或系统级权限。
- 受「启用开关」约束（`C:\ProgramData\ai-root\enabled.flag` 存在即启用），删除即禁用中继（服务端拒绝一切命令，`ok=false, error="中继已禁用"`）。
  **开关是运行期语义**：禁用后无法通过中继自身恢复（拒绝执行），需本地/管理员重建 flag，或直接非提权 `ai-root stop` + `ai-root start`（服务启动时自动重建 flag，已实测）。
- 服务运行时（SYSTEM 身份）不会改写令牌/SID，避免错误写入 `S-1-5-18`。

### 4.1 运行时安全加固

- **鉴权失败节流**：同一来源 30s 窗口内 5 次令牌错误 → 锁定 60s（锁定态拒绝并记 `<locked>`）；成功鉴权重置计数。
- **恒定时间令牌比较**：逐字节 XOR 累加判 0，防时序侧信道逐位猜令牌。
- **超时钳制**：请求 `timeout_ms` 上限 10 分钟（`min(600_000)`），防客户端把服务端拖入超长占用。
- **并发信号量**：同时处理连接上限 8（`AtomicU32` + RAII 释放），超出立即返回「服务繁忙」（AI 侧重试即可）。
- **连接读超时**：帧头/帧体读取带超时（PeekNamedPipe 轮询），防「连上不发数据」的线程泄漏。
- **输出截断**：stdout/stderr 各 1MB 上限，超出置 `truncated=true`（Response 含该字段，旧客户端 serde 默认兼容）。
- **update 输入校验**：`ai-root update` 先校验目标为有效 PE（MZ + PE 头 + x64/x86 Machine）并打印 SHA256，非 PE 拒绝，防被误导写入恶意文件。

---

## 5. GUI 控制台（人工巡检面板）

`ai-root gui`（或**双击 exe**）打开原生 GUI 控制台——**人类巡检/应急面板**，不替代管道/CLI（AI 消费方权威接口仍是 §3 的调用契约）。

- **深色「运维控制台」主题**：深蓝灰面板 + 高对比语义色（开始蓝 / 成功绿 / 失败红 / 轮换琥珀），状态一目了然。
- **顶部**：服务状态徽章（运行/停止/未安装 + 中继启用 toggle 开关）。
- **实时信息流**：1s 增量轮询 `events.jsonl`，实时显示 AI 每次执行的「开始/完成」（含 caller/pid/退出码/时长/输出摘要）。
- **命令台**：常用命令一键、输入框 Enter 执行 / ↑ 历史、执行历史回填、输出区。
- **审计日志**：`exec.log` 着色（exit≠0 红色 + 左侧色条）+ 关键字过滤。
- **令牌区**：前 8 位指纹 + 写入时间；「轮换令牌」二次确认后走系统确认弹窗。
- **管理员操作隔离**：安装/卸载折叠在红色「仅首次/应急」区，与日常操作视觉切割。
- **系统托盘常驻**：角标绿=运行且启用 / 红=运行但禁用 / 灰=停止；右键菜单（启停/中继开关/轮换/打开/退出）；**关闭窗口=最小化到托盘**，托盘「退出」才结束 GUI 进程（不影响服务）。
- **CLI 输出可见性**：exe 为 GUI 子系统，CLI 命令（status/run/update 等）启动时自动附加父进程控制台（`AttachConsole`），交互终端输出可见；管道/重定向捕获不受影响。

---

## 6. 限制

- **仅 Windows**（依赖命名管道、LocalSystem 服务、cmd.exe）。
- 服务必须预先以管理员 `install`/`deploy` **一次**；之后智能体侧完全无需提权（含二进制更新）。
- 令牌等同于高权限凭据，泄露即等同于获得 SYSTEM 命令执行权。
- 单行命令内 `&`/`&&` 串联失效（设计使然，见 3.3）；批量场景用多行脚本。
- **单命令输出 1MB 截断**（`truncated` 标记）；默认超时 60s（上限 10 分钟）；并发上限 8（超出返回「服务繁忙」，重试即可）；鉴权失败 30s 内 5 次锁定 60s。

---

## 7. 构建与测试

两种构建变体（Cargo feature `gui`，默认开启）：

```text
headless（服务端/CLI，无 GUI 依赖）   cargo build --release --no-default-features     → ~672KB / 11 导入 DLL
GUI 成品（含 egui + tray-icon）      cargo build --release                        → ~5.5MB / 25 导入 DLL
```

> **必须分离 CARGO_TARGET_DIR**（同目录构建会互相覆盖）：headless 用 `D:/wbrelay_target`，GUI 用 `D:/wbrelay_target_gui`。

```bash
# 构建（本机需先 export RUSTUP_HOME='C:\Users\AprYT\.rustup' 指向含 msvc toolchain 的 rustup home）
export RUSTUP_HOME='C:\Users\AprYT\.rustup'
export PATH="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64:$PATH"
export INCLUDE="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/include;/c/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/um;/c/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/shared;/c/Program Files (x86)/Windows Kits/10/Include/10.0.26100.0/ucrt"
export LIB="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/lib/x64;/c/Program Files (x86)/Windows Kits/10/Lib/10.0.26100.0/um/x64;/c/Program Files (x86)/Windows Kits/10/Lib/10.0.26100.0/ucrt/x64"
export CARGO_TARGET_DIR=D:/wbrelay_target          # headless
"/c/Users/AprYT/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe" build --release --target x86_64-pc-windows-msvc --no-default-features
export CARGO_TARGET_DIR=D:/wbrelay_target_gui      # GUI
"/c/Users/AprYT/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe" build --release --target x86_64-pc-windows-msvc
```

**产物交付目录 `Ai-root-build\`**（与源码平级，`D:\project\workbuddy\工具\Ai-root-build\`）：

| 文件 | 说明 |
|---|---|
| `ai-root.exe` | headless 服务端（部署用，`update` 热更新到 `D:\Kiln\AI-root\`） |
| `ai-root-gui.exe` | GUI 桌面成品（双击开控制台） |

集成测试（spawn `--test-pipe-server` + `AIROOT_DATA_DIR` 隔离，不触碰生产数据）：

```bash
export CARGO_TARGET_DIR=D:/wbrelay_target
"/c/Users/AprYT/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe" test --release --no-default-features
```

覆盖：鉴权（错误/正确令牌）、脚本模式与单行转义、退出码传播、超时杀进程树、200KB 大输出、cwd、**令牌轮换热加载**、启用开关、CLI `--timeout`/`--stdin`、10 并发、生产目录零污染。

**CI**：`.github/workflows/ci.yml` 在 GitHub Actions `windows-latest` 上自动执行 `build.ps1` + `cargo test --release`（提交到 main 或 PR 时触发）。

依赖：`serde` / `serde_json` / `rand` / `chrono` / `eframe`+`egui` / `tray-icon`（均为 GUI feature，headless 构建不含）。

---

## 8. 环境变量（隔离 / 多实例 / 测试）

| 变量 | 默认 | 作用 |
|---|---|---|
| `AIROOT_DATA_DIR` | `C:\ProgramData\ai-root` | 覆盖数据目录（token / exec.log / enabled.flag / sid.txt） |
| `AIROOT_PIPE_NAME` | `\\.\pipe\ai-root` | 覆盖命名管道名 |

**为什么需要**：ai-root 的鉴权凭据（令牌）与监听管道名都是进程内唯一的。测试套件与「临时演练中继」需要与生产服务完全隔离，否则会串到生产实例（实测教训：测试曾连到生产管道导致数据串扰）。

**集成测试自动使用**（`tests/integration.rs` 无需任何配置）：

- 每次测试 spawn `--test-pipe-server` 时自动设置随机的 `AIROOT_DATA_DIR`（临时目录）与 `AIROOT_PIPE_NAME`（`\\.\pipe\ai-root-it-<随机数>`），服务端与客户端双向隔离；
- 断言「测试令牌不得写入生产目录」（`t_09_no_production_touch`），杜绝污染；
- 测试结束后 RAII 守卫 kill 服务进程并删除临时目录。

**手动起隔离实例**（演练 / 压测 / 临时中继，不影响生产）：

```powershell
# 终端 A：起隔离服务（前台模式）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\ai-root-demo"
ai-root.exe --test-pipe-server

# 终端 B：连接隔离实例（读写都走 demo 数据目录与管道）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\ai-root-demo"
ai-root.exe run "whoami"
```

> 注意：`install` 注册的**服务模式**固定使用默认数据目录与管道名；环境变量覆盖主要服务于测试、前台 `server`/`--test-pipe-server` 模式与多实例演练场景。
