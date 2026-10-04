# AI-Root

> **面向 AI 智能体自调用的 LocalSystem 提权命令中继。**
> 非提权（普通用户）的智能体进程，仅凭一个共享令牌即可通过命名管道以 `SYSTEM` 权限自治执行命令，全程无需 UAC 弹窗。本工具的主要消费方是 AI 智能体本身，文档聚焦于「如何可靠对接」而非「人怎么敲命令」。

## 最近更新（2026-08-28）

### 安全增强
- **令牌轮转**：新增 `rotate_token()` 函数支持安全的令牌轮转
- **命令安全**：阻止危险命令模式（format/diskpart等）执行
- **输入验证**：添加请求数据验证，防止无效或恶意输入
- **详细审计**：增强日志记录，包含完整的命令执行信息

### 性能优化
- **管道缓冲区**：从8KB增加到64KB，提高大输出命令吞吐量
- **内存管理**：减少不必要的克隆，直接使用所有权转移
- **连接控制**：添加5分钟最大连接时间，防止资源占用
- **资源释放**：优化并发连接处理和资源管理

### 稳定性改进
- **错误处理**：添加更详细的错误日志和结构化事件记录
- **超时保护**：改进命令执行超时处理机制
- **进程管理**：增强进程状态检查和异常处理

### 文档更新
- **项目结构说明**：详细的项目结构和文件功能描述
- **使用示例**：添加多种语言的集成示例
- **优化总结**：完整的优化工作记录

详见 [优化总结.md](docs/优化总结.md)

---

## 1. 架构

```
┌─────────────┐   命名管道 \\.\pipe\AI-Root    ┌──────────────────────┐
│  AI 智能体   │ ──── 请求(令牌+命令) ───────> │  AI-Root 服务进程     │
│ (非提权用户) │ <── 响应(退出码+stdout/stderr)─│  (LocalSystem 运行)   │
└─────────────┘                               └──────────┬───────────┘
                                                        │ cmd /C <command>
                                                        ▼
                                                  SYSTEM 最高权限执行
```

- **服务**：以 `LocalSystem` 注册为自启 Windows 服务（`SERVICE_NAME = "AI-Root"`）。
- **管道**：`\\.\pipe\AI-Root`，SDDL 限制连接方为 `SYSTEM` / `Administrators` / `Interactive` + 安装用户 SID（确保智能体进程可无人值守连接）。
- **令牌**：安装时生成 32 字节随机十六进制令牌，落盘于 `C:\ProgramData\AI-Root\token.txt`，目录 ACL 仅 `SYSTEM` 完全控制 + 当前用户只读。

---

## 2. 部署（仅需一次，管理员）

```powershell
# 以管理员运行 deploy.ps1（注册服务、生成令牌、设置 SDDL、写入 C:\Windows\AI-Root.cmd）
powershell -ExecutionPolicy Bypass -File deploy.ps1
```

> **一次性管理员，之后零介入**：`install` 除注册服务外，还会给**当前用户 SID** 追加服务的
> `GRGX` 权限（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP，Windows 服务安全描述符
> 标准机制；实测 SDDL 零散位 `RP/WP/LO` 组合不含 QUERY_STATUS，勿用）。此后智能体（用户身份、非提权）
> 可自主执行 `AI-Root status/stop/start/update`，**更新服务二进制不再需要管理员**。

部署产物：

| 项 | 路径 |
|---|---|
| 服务二进制 | `D:\Kiln\AI-Root\AI-Root.exe` |
| 全局包装器 | `C:\Windows\AI-Root.cmd` |
| 数据/令牌目录 | `C:\ProgramData\AI-Root\` |
| 令牌文件 | `C:\ProgramData\AI-Root\token.txt` |
| 审计日志 | `C:\ProgramData\AI-Root\exec.log` |
| 实时事件流 | `C:\ProgramData\AI-Root\events.jsonl`（`AI-Root tail` 实时跟随数据源） |

安装后服务自启。常用管理命令：

```text
AI-Root install     # 注册为 LocalSystem 自启服务（管理员，仅首次；已存在时给出指引）
AI-Root uninstall   # 卸载服务（管理员；同时清理部署目录/全局包装器/令牌/身份文件）
AI-Root rotate-token  # 轮换共享令牌（写盘后服务端自动热加载，旧令牌立即失效）
AI-Root start       # 启动服务（install 授权后非提权即可）
AI-Root stop        # 停止服务（install 授权后非提权即可）
AI-Root update <新exe路径>  # 自主更新服务二进制：PE 校验→停服务→覆盖→启服务（无需管理员）
AI-Root status      # 查询服务状态
AI-Root -h          # 命令行帮助（无参数双击 exe = 打印帮助并等待按键，防一闪而过）
```

> **失败自动重启**：`install` 会配置 SCM Recovery Actions（异常终止后 60s/300s/600s 依次自动重启），
> 即使进程被意外杀死（7034）也会在 1 分钟内自愈，AI 无人值守不受影响。

---

## 3. AI 调用契约

### 3.1 鉴权

- 读取令牌：`C:\ProgramData\AI-Root\token.txt`（当前用户只读，AI 进程以安装用户身份运行即可读取）。
- 每次请求须在 `Request.token` 字段携带正确令牌；令牌不符服务端拒绝执行。
- **持有令牌即等价于拥有 SYSTEM 命令执行权**——令牌等同于高权限凭据，须妥善保管，不得写入日志、提交仓库或泄露。

### 3.2 客户端调用形态

`AI-Root run` 是智能体最常用的入口，有三种形态：

```text
# 单行命令（默认，非提权即可）
AI-Root run "whoami"

# 覆盖默认 60s 超时（超时后整棵进程树被杀）
AI-Root run --timeout 5000 "ping -n 10 127.0.0.1"

# 多行脚本（文件）：&/&&/|/() 全部正常，推荐用于批量自治
AI-Root run --file C:\tmp\batch.cmd

# 从标准输入喂入脚本（多行）
type C:\tmp\batch.cmd | AI-Root run --stdin

# 结构化输出（AI 消费推荐）：{ok,exit_code,stdout,stderr,error,truncated}
AI-Root run --json "whoami"

# 指定工作目录执行
AI-Root run --cwd C:\Windows --json "cd"

# 分隔符之后全部按字面量作为命令（不经任何解析）
AI-Root run -- <原始命令>
```

> **智能体直接拼参数的约定**：通过子进程 `subprocess` 等机制把含换行符的命令字符串传给 `AI-Root run` 时，
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

服务端在关键节点追加 JSONL 事件到 `C:\ProgramData\AI-Root\events.jsonl`（每行一个 JSON 对象），
供 `AI-Root tail` 及外部监控**增量读取**（按字节偏移读新增行，`size <= offset` 安全返空）：

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
- **令牌轮换**：`AI-Root rotate-token`（管理员/SYSTEM）生成新令牌写盘，服务端按文件 mtime 变化**自动热加载**（无需重启），旧令牌立即失效。泄露后的标准处置流程：rotate → 更新 AI 侧令牌引用。
- **审计日志**：`C:\ProgramData\AI-Root\exec.log` 逐行记录 `[时间] caller=DOMAIN\user pid=xxx exit=code cmd=...`。`caller` 来自服务端 `GetNamedPipeClientProcessId` 解析的**管道对端进程身份**（非服务自身环境变量），可精确追溯"谁在什么进程里执行了什么"。
- 管道 SDDL 追加安装用户 SID，使以该用户身份运行的智能体（可能非交互登录）也能稳定连接。
- 服务 DACL 在 install 时追加安装用户 SID 的 `GRGX`（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP）——仅授权 status/stop/start/update 自身服务，不授予 CHANGE_CONFIG/WRITE_DAC 等管理权限、不涉及其他服务或系统级权限。
- 受「启用开关」约束（`C:\ProgramData\AI-Root\enabled.flag` 存在即启用），删除即禁用中继（服务端拒绝一切命令，`ok=false, error="中继已禁用"`）。
  **开关是运行期语义**：禁用后无法通过中继自身恢复（拒绝执行），需本地/管理员重建 flag，或直接非提权 `AI-Root stop` + `AI-Root start`（服务启动时自动重建 flag，已实测）。
- 服务运行时（SYSTEM 身份）不会改写令牌/SID，避免错误写入 `S-1-5-18`。

### 4.1 运行时安全加固

- **鉴权失败节流**：同一来源 30s 窗口内 5 次令牌错误 → 锁定 60s（锁定态拒绝并记 `<locked>`）；成功鉴权重置计数。
- **恒定时间令牌比较**：逐字节 XOR 累加判 0，防时序侧信道逐位猜令牌。
- **超时钳制**：请求 `timeout_ms` 上限 10 分钟（`min(600_000)`），防客户端把服务端拖入超长占用。
- **并发信号量**：同时处理连接上限 8（`AtomicU32` + RAII 释放），超出立即返回「服务繁忙」（AI 侧重试即可）。
- **连接读超时**：帧头/帧体读取带超时（PeekNamedPipe 轮询），防「连上不发数据」的线程泄漏。
- **输出截断**：stdout/stderr 各 1MB 上限，超出置 `truncated=true`（Response 含该字段，旧客户端 serde 默认兼容）。
- **update 输入校验**：`AI-Root update` 先校验目标为有效 PE（MZ + PE 头 + x64/x86 Machine）并打印 SHA256，非 PE 拒绝，防被误导写入恶意文件。

---

## 5. 纯终端版（无 GUI）

> **GUI 已于 2026-08-07 彻底移除**：当前只有单一终端版，不再有 GUI 控制面板、系统托盘或 `AI-Root-gui.exe`。若调用 `AI-Root gui` 子命令，将直接报错「本构建为纯终端版，未包含 GUI 控制面板」。

- **双击 exe（无参数）**：打印帮助信息并等待按键，防止终端一闪而过；CLI 交互输出经 `AttachConsole(ATTACH_PARENT_PROCESS)` 可见，管道/重定向不受影响。
- **实时查看 AI 操作**：用 `AI-Root tail` 实时跟随 `events.jsonl`（支持 `--caller` / `--type` / `--since` 过滤），替代原 GUI「实时信息流」。
- **审计日志查看**：`C:\ProgramData\AI-Root\exec.log` 逐行记录；人工巡检用任意文本工具或 `AI-Root tail` 查看。
- **构建产物**：仅 `AI-Root.exe`（console 子系统，约 842 KB，MinGW 静态链接 CRT）。
  > 体积经 `profile.release` 优化：`lto="thin"` + `codegen-units=1` + `strip="symbols"`（早期未优化时约 1.9–2.0MB）。
  > ⚠ `strip="symbols"` 会同时剥掉 panic 回溯的符号信息 —— 线上崩溃只能拿到地址，无法直接定位函数。若需排查线上问题，临时改为 `strip=false` 或保留 `debug=1` 重建。

---

## 6. 限制

- **仅 Windows**（依赖命名管道、LocalSystem 服务、cmd.exe）。
- 服务必须预先以管理员 `install`/`deploy` **一次**；之后智能体侧完全无需提权（含二进制更新）。
- 令牌等同于高权限凭据，泄露即等同于获得 SYSTEM 命令执行权。
- 单行命令内 `&`/`&&` 串联失效（设计使然，见 3.3）；批量场景用多行脚本。
- **单命令输出 1MB 截断**（`truncated` 标记）；默认超时 60s（上限 10 分钟）；并发上限 8（超出返回「服务繁忙」，重试即可）；鉴权失败 30s 内 5 次锁定 60s。

---

## 7. 构建与测试

当前为**单一纯终端版**（无 `gui` feature、无双变体），构建产物仅 `AI-Root.exe`。

```bash
# 本机构建（git bash 执行 build.sh）
bash build.sh
# → $CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/AI-Root.exe（约 842 KB，MinGW 静态链接 CRT）

# 或 PowerShell（本机与 CI 通用，自动选 target）
powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
```

**构建脚本自动选 target**（`build.ps1`，2026-10-04 重写）：

1. 已装 `x86_64-pc-windows-msvc` 且能找到 `link.exe`（VS Build Tools）→ 用 msvc（CI `windows-latest` 走此路径）。
2. 否则若已装 `x86_64-pc-windows-gnu` → 用 gnu，并注入完整 MinGW binutils。
3. rustup 根目录按 `$RUSTUP_HOME` → `%USERPROFILE%\.rustup` → `D:\software\rust\rustup` 顺序探测。

> ⚠ **gnu 构建的两个硬约束**（2026-10-04 实测定位）：
>
> 1. **需要含 `as.exe` 的完整 binutils**。rustup 的 `self-contained` 目录实测只有 `dlltool.exe`/`ld.exe`/`x86_64-w64-mingw32-gcc.exe`，**缺 `as.exe`**，不能单独使用。本机位于 `D:\Kiln\mingw-binutils\bin`（可用 `MINGW_BINUTILS_BIN` 覆盖）。获取：清华 msys2 镜像 `mingw-w64-x86_64-binutils-*.pkg.tar.zst`，解压后把 `mingw64/bin` 内容放入该目录。
> 2. **Git Bash 下 PATH 只认 POSIX 形式**。把 `D:/Kiln/...` 或 `D:\Kiln\...` 写进 `PATH` 后，`command -v dlltool.exe` / `which` 一律查不到（不是 Permission denied，是彻底找不到），导致 cargo 解析回落到 `CARGO_HOME\bin` 的 rustup shim，最终报 `error calling dlltool 'dlltool.exe': program not found`。`build.sh` 用 `to_posix()` 统一转换，并直接调用 toolchain 内的真实 `cargo.exe`；`build.ps1` 侧为原生 Windows 路径语义，无此问题。

> CI（GitHub Actions `windows-latest`）自带 MSVC，`build.ps1` 走 msvc 路径；本机与 CI 的 `CARGO_TARGET_DIR` 需分离，避免互相覆盖。CI 另附「Package artifact」步骤，产出 `AI-Root-windows-x64` 工件（含 exe + `build.ps1` + README，保留 30 天，并打印 exe 的 SHA256）。

> 📌 **现状核对（2026-10-04）**：下述交付目录**当前均不存在** —— `D:\project\workbuddy\工具\AI-Root-build\` 与 `D:\Finished product\AI-Root\` 都未创建，`D:\Kiln\` 下也只有 `mingw-binutils`（无 `AI-Root` 部署目录）。构建产物目前只落在 `CARGO_TARGET_DIR`（本机 `D:\AI-Root-target\x86_64-pc-windows-gnu\release\AI-Root.exe`）。首次部署时需按下文流程创建。

**产物交付目录 `AI-Root-build\`**（与源码平级，`D:\project\workbuddy\工具\AI-Root-build\`）：

| 文件 | 说明 |
|---|---|
| `AI-Root.exe` | 纯终端服务端（部署用，`update` 热更新到 `D:\Kiln\AI-Root\`） |

**正式成品包**（2026-08-18 起，与 GG-Ghost 同规：exe + docs/）：

```
D:\Finished product\AI-Root\
├── AI-Root.exe        # 交付用二进制（与部署/构建产物一致）
└── docs\README.md     # 本文档
```

取用流程：`D:\Finished product\AI-Root\AI-Root.exe` → `AI-Root update <该exe路径>`（服务在跑时热更新，无需管理员）。

**部署流程（务必现场重建，勿复用早前构建）**：工作树存在并发编辑，`AI-Root-build` 中的旧二进制可能滞后于源码（实测同尺寸但哈希不同）。固定流程：
`bash build.sh`（本机 gnu；CI 走 `build.ps1` 自动选 msvc）→ 复制到 `AI-Root-build` → `AI-Root update <AI-Root-build\AI-Root.exe>` → 校验部署哈希一致 + `run --json whoami`=SYSTEM。

**CI**：`.github/workflows/ci.yml` 在 GitHub Actions `windows-latest` 上自动执行 `build.ps1`（提交到 main、PR 或手动 `workflow_dispatch` 触发），并打包上传 `AI-Root-windows-x64` 工件（保留 30 天）。

依赖：`serde` / `serde_json` / `rand` / `chrono`（纯终端实现，无 GUI / egui / tray-icon 依赖）。

---

## 8. 环境变量（隔离 / 多实例 / 测试）

| 变量 | 默认 | 作用 |
|---|---|---|
| `AIROOT_DATA_DIR` | `C:\ProgramData\AI-Root` | 覆盖数据目录（token / exec.log / enabled.flag / sid.txt） |
| `AIROOT_PIPE_NAME` | `\\.\pipe\AI-Root` | 覆盖命名管道名 |

**为什么需要**：AI-Root 的鉴权凭据（令牌）与监听管道名都是进程内唯一的。「临时演练中继」需要与生产服务完全隔离，否则会串到生产实例（实测教训：测试曾连到生产管道导致数据串扰）。

**手动起隔离实例**（演练 / 压测 / 临时中继，不影响生产）：

```powershell
# 终端 A：起隔离服务（前台模式，Ctrl+C 退出）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe server

# 终端 B：连接隔离实例（读写都走 demo 数据目录与管道）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe run "whoami"
```

> 注意：`install` 注册的**服务模式**固定使用默认数据目录与管道名；环境变量覆盖主要服务于前台 `server` 模式与多实例演练场景。
