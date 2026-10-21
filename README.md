# AI-Root

[![CI](https://github.com/Apr-YT/Ai-root/actions/workflows/ci.yml/badge.svg)](https://github.com/Apr-YT/Ai-root/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20%7C%20edition%202021-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-0078D6.svg)](#平台支持)
[![Tests](https://img.shields.io/badge/tests-177%20passed-brightgreen.svg)](#测试)

用命名管道把命令转交给 LocalSystem 服务执行，让非提权的智能体进程拿到 SYSTEM 权限，不需要 UAC 弹窗。装一次服务，之后靠一个共享令牌和一份服务端授权策略运行。

```text
AI 智能体（非提权进程）
  │
  ├─ 读 %ProgramData%\AI-Root\secrets\token.txt
  ├─ 连命名管道 \\.\pipe\AI-Root，发「令牌 + 请求」
  │
  ▼
AI-Root 服务（LocalSystem）
  │
  ├─ 校验令牌
  ├─ 策略判定：Allow / Confirm / Deny
  └─ 以 SYSTEM 执行程序或命令
  │
  ▼
退出码 + stdout / stderr 返回调用方
```

---

## 目录

- [为什么需要](#为什么需要)
- [快速开始](#快速开始)
- [平台支持](#平台支持)
- [部署](#部署仅需一次管理员)
- [AI 调用契约](#ai-调用契约)
  - [鉴权](#鉴权) · [调用形态](#调用形态) · [命令语义](#命令语义) · [帧协议](#帧协议直接对接管道时) · [输出编码](#输出编码) · [退出码](#退出码) · [实时事件流](#实时事件流)
- [配置](#配置)
- [安全模型](#安全模型)
- [限制](#限制)
- [脚本](#脚本scripts)
- [构建](#构建)
- [测试](#测试)
- [环境变量](#环境变量)
- [项目结构](#项目结构)
- [文档](#文档)

---

## 为什么需要

智能体要装驱动、改注册表、写系统目录时，UAC 弹窗会打断无人值守流程。AI-Root 把提权一次性解决掉：

- 管理员执行一次 `install`，注册 LocalSystem 自启服务。
- 之后智能体以普通用户身份连命名管道，由服务代为执行，不再提权。
- 令牌文件受 ACL 保护，仅 SYSTEM/Administrators 可写、安装用户可读，客户端与服务端读同一份。
- 每条命令写入 `exec.log`（长期保留）和 `events.jsonl`（实时事件流）。
- 删掉 `enabled.flag` 即整体禁用中继。

---

## 快速开始

只想用现成的：下载 Release 里的 `AI-Root.exe`，不用构建，直接跳到[部署](#部署仅需一次管理员)。  
要自己编译，走下面三条命令。

从源码到可用：

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
| ------- | ---------------------------------------------------- |
| 操作系统 | 仅 Windows x64（依赖命名管道、`LocalSystem` 服务、`cmd.exe`） |
| 目标三元组 | `x86_64-pc-windows-msvc`（推荐）、`x86_64-pc-windows-gnu` |
| Rust 版本 | stable，edition 2021 |
| 构建 CI | GitHub Actions `windows-latest`，msvc 与 gnu 两个 job |
| 产物 | 单一 `AI-Root.exe`，console 子系统，约 1.8 MB，静态链接 CRT |
| 运行时依赖 | 无。不分发 DLL，不需要 .NET 或 VC++ 可再发行包 |

纯终端程序，没有 GUI。`AI-Root gui` 会报错退出。查看运行情况用 `AI-Root tail` 或直接读 `exec.log`。

---

## 部署（仅需一次，管理员）

```powershell
# 自动请求 UAC；幂等；安装后自检（服务状态 / 配置 / 目录 ACL / 中继冒烟）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1
```

只想先看它做什么、不产生任何变更：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1 -PreflightOnly
```

也可以手动分两步：`AI-Root.exe install`，然后 `AI-Root.exe start`（同样需要管理员）。

安装后自检如果报「数据目录 ACL 存在普通用户写权限」，先停用再排查，判定依据见  
[docs/安全模型.md](docs/安全模型.md)。

### 部署产物

| 项 | 路径 |
| ------- | --------------------------------------------------------------------------- |
| 服务二进制 | `%ProgramData%\AI-Root\AI-Root.exe` |
| 全局包装器 | `C:\Windows\AI-Root.cmd` |
| 管理员授权策略 | `%ProgramData%\AI-Root\policy.toml` |
| 自动化规则目录 | `%ProgramData%\AI-Root\rules.d\*.toml` |
| 秘密（令牌） | `%ProgramData%\AI-Root\secrets\token.txt` |
| 审计日志 | `%ProgramData%\AI-Root\logs\exec.log` |
| 实时事件流 | `%ProgramData%\AI-Root\logs\events.jsonl`（`AI-Root tail` 跟随） |
| 运行状态 | `%ProgramData%\AI-Root\state\`（启用标志 / SID / 待确认请求 / 临时脚本） |
| 用户偏好 | `%LOCALAPPDATA%\AI-Root\user.toml`（**只能收紧**，见 [docs/配置说明.md](docs/配置说明.md)） |

秘密、日志、状态从 v1.2 起分目录存放。旧版扁平布局会在 `install` 或服务启动时自动迁移，  
也可以手动执行 `AI-Root config migrate`。迁移不能自动回滚，升级前先备份整个目录。

### 命令一览

```text
AI-Root install            注册为 LocalSystem 自启服务（管理员，仅首次）
AI-Root uninstall          卸载服务（管理员；删程序与凭据，保留策略/规则/日志）
AI-Root grant              对已存在的服务补发用户 STOP/START 授权（管理员；install 失败降级时用）
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

只有 `install` 这一次需要管理员。它除了注册服务，还会给当前用户 SID 追加服务的 `GRGX` 权限  
（`GENERIC_READ|GENERIC_EXECUTE`，包含 `QUERY_STATUS`、`START`、`STOP`），此后以该用户身份运行的  
智能体就能自己执行 `status`、`stop`、`start`、`update`，不再提权。

`update` 不走这条旁路：它校验 Authenticode 嵌入签名和发布者证书指纹白名单，目标路径取自 SCM  
实际的 `ImagePath`。没配置信任根时一律拒绝，细节见 [docs/授权策略.md](docs/授权策略.md)。

`install` 同时配置了 SCM Recovery Actions（异常终止后 60s、300s、600s 重试），进程被意外杀死  
（事件 7034）也会在一分钟内自愈。

---

## AI 调用契约

### 鉴权

令牌位于 `%ProgramData%\AI-Root\secrets\token.txt`，当前用户可读，AI 进程以安装用户身份运行就能读到。  
每次请求在 `Request.token` 字段带上它，不匹配则拒绝执行。

令牌只决定能不能发起请求。请求能否执行由[授权策略](docs/授权策略.md)判定：`Allow` 直接执行、  
`Confirm` 等管理员批准、`Deny` 拒绝。内置保护项（修改 AI-Root 自身、间接执行宿主、不可逆磁盘操作、  
关闭安全控制、创建管理员账户等）不受令牌影响，任何令牌都放不开。

令牌本身仍是高权限凭据，不要写进日志、提交到仓库或外传。

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
AI-Root op read_state_file --target policy.toml

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
| ----- | --------------------------------------- |
| `0..` | 被执命令自身的退出码 |
| `3` | 需要人工确认，响应含 `confirmation_id`，批准后重放同一条命令 |
| `1` | 客户端或中继层错误（未安装、策略拒绝、连接失败等） |

用 `subprocess` 把含换行符的命令字符串传给 `AI-Root run` 时，服务端按「命令含 `\n`」自动判定为  
脚本模式，不需要显式加 `--file` 或 `--stdin`。

### 命令语义

| 形态 | 转义行为 | 适用场景 |
| --------------------- | ----------------------------------- | --------------------------- |
| 单行 | `^` 与 `&` 转义为字面量 | 单个命令；含 `&` 的字面量参数（如设备实例 ID） |
| 多行、`--file`、`--stdin` | 不转义，`&`/`&&`/`\|`/`()` 按 `cmd` 正常语义 | 批量执行、串联、管道 |

单行模式的代价是 `&` 和 `&&` 串联失效（退化成字面 `&`/`&&`），只有 `|`/`<`/`>`/`()` 保留。  
这是为了避免 `cmd /C` 二次解析把含 `&` 的字面量拆成多段，执行单行设备命令时正好需要这个保护。

脚本模式下，服务端把多行命令用 `&` 连接后经 `cmd /C` 直接传参（UTF-16 命令行，不落盘），  
所以中文命令不会出现代码页问题。

脚本模式的几个限制：

- 同一行内 `set x=1 & echo %x%` 的 `%x%` 在解析期展开为空，要写成 `& call echo %x%`；
- 含 `(`/`)` 的括号块（`if (...)`、`for ... do (...)`）自动回退到「包装脚本 + call」文件方案；
- 拼接后超过 6000 字符同样回退文件方案（`cmd` 命令行上限 8191）。

需要串联或管道时用多行脚本，不要用单行。

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
| ---------- | ------- | -------- | ------------ |
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

### 实时事件流

服务端在关键节点向 `%ProgramData%\AI-Root\logs\events.jsonl` 追加 JSONL，每行一个对象。  
`AI-Root tail` 按字节偏移增量读取，文件超过 2MB 时改名为 `.1` 再重写。

每次执行的完整轨迹是三条事件，用 `run_id` 串起来：

| 事件 | 触发点 | 主要字段 |
| ----------------- | ---------------------- | ------------------------------------------------------------------- |
| `policy_decision` | 鉴权通过、策略判定完成 | `run_id` `caller` `op` `params` `decision` `rule_id` |
| `exec_done` | 命令结束 | `run_id` `exit` `duration_ms` `out_preview` `timed_out` `truncated` |
| `command_audit` | 审计落定（与 `exec.log` 同时写） | `run_id` `op` `params` `decision` `rule_id` `confirmed` `exit_code` |

`params` 是脱敏后的请求摘要，`--password=`、`-p`、`API_KEY=` 这类参数值会被替换为 `***`。  
令牌不会出现在任何事件里。

其余事件用于排障：`exec_error` `exec_timeout` `exec_nonzero_exit` `taskkill_failed`  
`temp_cleanup_failed` `connection_timeout` `acl_setup_failed` `token_rotate` `service_start`  
`confirm_consumed` `update_rejected` `update_rollback` `update_done`。

`exec.log` 是另一份记录，按行追加、长期保留，格式为：

```text
[2026-10-21 16:38:00] caller=MACHINE\user pid=32836 exit=0 cmd=<allow-dev-whoami run_argv confirmed=false>
```

---

## 配置

配置分三层，后一层覆盖前一层，但用户层只能收紧：

| 层 | 位置 | 谁能写 | 能做什么 |
|---|---|---|---|
| 代码默认 | 编译进 exe | 不可改 | 67 条规则：只读查询、开发工具链 argv |
| 用户偏好 | `%LOCALAPPDATA%\AI-Root\user.toml` | 当前用户（含 Agent） | 收紧 `mode`、缩短确认有效期、设默认超时 |
| 管理员策略 | `%ProgramData%\AI-Root\policy.toml` 和 `rules.d\*.toml` | 仅管理员 | 规则、服务白名单、update 信任根、模式 |

用户层不能放宽是个硬约束。Agent 以安装用户身份运行，可以写 `user.toml`；如果它能加规则，
或者把 `strict` 改回 `autonomy`，这套授权就形同虚设。所以 `UserConfig` 里没有 `rules`、
`approved_services`、`trusted_publishers` 这些字段，写进去会报错而不是被静默忽略，
并且整层不生效。

失败时一律往紧的方向走。策略或规则解析失败、规则 ID 跨文件重复，进入 `locked_down`，
只放行只读查询。策略文件曾经初始化过又被删除，同样进入 `locked_down` 且不重建默认策略，
否则删掉配置就等于放宽权限。用户配置损坏则忽略用户层，结果只会更严。

```powershell
AI-Root config check      # 逐层校验；有错退出码 1
AI-Root config show       # 合并后的生效值 + 每层来源说明
AI-Root config path       # 各文件位置
```

格式是 TOML。策略在 `policy.toml`，规则拆在 `rules.d\*.toml` 里按文件名字典序合并。
示例见 [`examples/`](examples/)，完整字段和迁移回滚见 [docs/配置说明.md](docs/配置说明.md)。


## 安全模型

两层边界：文件 ACL 决定谁能读到令牌、谁能改策略和二进制；授权策略决定每个请求能做什么。
完整机制见 [docs/授权策略.md](docs/授权策略.md)，改造前的审计记录见
[docs/audit/SECURITY_REFACTOR_AUDIT.md](docs/audit/SECURITY_REFACTOR_AUDIT.md)。

| 层 | 作用 | 失效后果 |
|---|---|---|
| 文件 ACL（`%ProgramData%\AI-Root\`） | 令牌保密，策略与二进制完整性 | 令牌泄漏后可发起任意请求 |
| 授权策略（`policy.toml` 与内置保护） | 每个请求能做什么 | 只可能放行策略允许范围内的事 |

只坏一层不会丢权限：ACL 破了还要有令牌才能进门，令牌到手了还要策略放行才能执行。

### 令牌与 ACL

- 令牌只在安装时生成一次。目录 ACL 为 `SYSTEM(F) + Administrators(F) + 安装用户(R)`。
- `AI-Root rotate-token` 写盘后服务端按 mtime 自动热加载，旧令牌立即失效。
- 目录 ACL 校验失败则拒绝启动。服务启动时用 `GetNamedSecurityInfoW` 解析二进制 DACL，
  要求危险主体（Everyone、Users、Authenticated Users、INTERACTIVE 等）没有写权限，
  SYSTEM 有写权限，且没有残留的继承 ACE。任一条不满足就不进入主循环，并写
  `acl_setup_failed` 事件。这套判断不依赖 `icacls` 的文本输出，因为账户名的本地化
  会让字符串匹配失效。
- 令牌写入顺序是「先硬化目录 ACL，再写 `token.txt`」，避免令牌在弱权限目录里短暂存在。

### 授权判定

```text
内置保护(硬编码) → deny 规则 → approved_services → allow 规则 → confirm 规则 → 默认(confirm)
```

- 内置保护不可覆盖，包括修改 AI-Root 自身、间接执行宿主（`cmd`、`powershell`、`mshta` 等）、
  下载器、不可逆磁盘与引导操作、关闭安全控制、创建管理员账户、服务与计划任务持久化。
- deny 优先于 allow。同时写了 allow 规则也拦得住。
- 未知操作和未知字段一律拒绝。协议层用 `deny_unknown_fields`，op 取值是白名单枚举。
- 策略文件损坏即降级，除只读查询外全部拒绝，避免「破坏策略文件」变成「放开所有工具」。

### 执行形态

`exec` 走结构化 argv：程序解析成绝对路径（只在 PATH 里找，不查当前目录），参数是数组，
不经 shell，策略可以按「程序 + 参数前缀 + 目录」精确授权。这是推荐主入口。

`run` 走 `cmd /C <字符串>`，默认需要人工确认。字符串无法结构化校验，所以 `exec.rs` 里的
危险模式黑名单只是纵深防御，不能当作边界。

### 人工确认

服务在 session 0 没有桌面，确认走文件加管理员命令：

```text
请求 → decision=confirm → 返回 request_id（退出码 3，不执行）
管理员 → AI-Root approve <id>  （写 pending\<id>.approved，内容 = 参数哈希）
请求重放 → 哈希匹配 → 一次性消费 → 执行
```

批准绑定完整参数，改一个字符即失效；一次性使用，带 TTL。普通用户无法写 `pending\`
（ACL 限制），所以自己批准自己不成立。协议中没有 `confirmed` 字段，发它会导致解析失败。

### 更新

三道门控：`WinVerifyTrust` 校验 Authenticode 嵌入签名，发布者证书 SHA-256 命中
`trusted_publishers`，目标路径取自 SCM 实际的 `ImagePath` 且位于受保护目录。
另外校验架构一致、拒绝版本降级、复制后二次比对 SHA-256、启动 20 秒未就绪则自动回滚。
没有配置信任根时一律拒绝。

### 运行时加固

| 机制 | 参数 |
|---|---|
| 全局熔断 | 30s 窗口内累计失败 20 次，全局锁定 120s |
| 按调用方定向封禁 | 同一 SID 在 60s 内失败 5 次，单独拒绝该 SID 300s |
| 令牌比较 | 逐字节 XOR 累加判 0，恒定时间，防时序侧信道 |
| 超时钳制 | 请求 `timeout_ms` 上限 10 分钟 |
| 并发上限 | 同时处理 8 个连接，超出立即返回「服务繁忙」 |
| 读超时 | 帧头帧体分离计时，并按 payload 放宽绝对上限，防 slow-loris |
| 输出截断 | `stdout`、`stderr` 各 1MB，超出置 `truncated=true` |
| UTF-8 安全截断 | 审计预览用 `truncate_utf8`，不会在字符中间切断 |
| 私有临时目录 | 脚本模式写 `%ProgramData%\AI-Root\tmp\`，ACL 保护，加密随机名，`create_new` 独占创建 |
| 审计脱敏 | `--password=`、`-p`、`API_KEY=` 等参数在日志里替换为 `***` |

---

## 限制

- 仅支持 Windows x64，依赖命名管道、LocalSystem 服务和 `cmd.exe`。
- 需要先用管理员权限执行一次 `install`，此后智能体侧不再提权。
- 令牌泄漏后可以发起任意请求，但请求仍受策略约束，内置保护项不受令牌影响。
- 允许 `python`、`cargo`、`node` 在 SYSTEM 下运行，等于允许任意代码执行（`build.rs`、
  `npm postinstall` 都能跑）。这是高自主性模式的既定取舍，边界放在不可逆破坏、改自身、
  间接执行这三类上，详见 [docs/授权策略.md §2](docs/授权策略.md)。
- 传统 shell 只能整条确认，因为字符串没法结构化匹配。能用 `exec` 就别用 `run`。
- 确认需要管理员在场，`pending\` 只有 SYSTEM 和 Administrators 可写。
- 用户配置不能放宽权限，这是设计如此：Agent 能写那个文件。要放宽只能改管理员策略。
- 布局迁移不能自动回滚。升级后旧版本读不到新路径，回滚需要手工移动文件，
  见 [docs/配置说明.md §6.3](docs/配置说明.md)。
- `update` 只接受嵌入签名，System32 常见的 catalog 签名不被接受；也不检查证书吊销，
  因为服务可能在无网环境启动。
- 安装用户能停止服务（`GRGX` 含 STOP/START，为零 UAC 更新保留），因此存在 DoS 面。
- 单行命令内的 `&` 和 `&&` 串联失效。批量场景用多行脚本或 `exec`。
- 输出超过 1MB 截断，默认超时 60 秒（上限 10 分钟），并发上限 8。
- 构建产物不保证逐字节可复现，上游 `dlltool` 会写入时间戳，
  见 [docs/构建说明.md](docs/构建说明.md)。

---

## 脚本（scripts\）

`scripts\` 把常用操作收敛成**可重复执行**的入口。重复运行不会破坏已有安装。

| 脚本 | 作用 | 需要管理员 | 改系统状态 |
| --------------- | -------------------------------------- | ------- | ----- |
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

本地自编译的 exe 没有签名，`-Update` 必然被拒绝。这不是故障，是签名门控在起作用。
本地迭代用 `-Force`，它会先卸载再安装，策略与日志保留：

```powershell
scripts\dev.ps1 -Action install -Force
```

### 行为边界

| 场景 | 行为 |
| ------------------ | --------------------------------------------------------------------------------------------- |
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
| ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| 服务注册、`AI-Root.exe`(.old/.new)、`state\tmp`、`state\pending`、`state\enabled.flag`、`secrets\token.txt`、`state\sid.txt`、`C:\Windows\AI-Root.cmd` | `policy.toml`、`rules.d\`、`logs\`、`state\policy.initialized`、`%LOCALAPPDATA%\AI-Root\user.toml` |

脚本只负责编排，不绕过任何安全机制：不关闭 Defender，不改安全策略，不跳过 `update`
的签名校验，也不替用户确认 UAC。

---

## 构建

**推荐路线：msvc**（Windows 上 Rust 标准做法，无需额外配置 binutils）

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc
# 并安装 VS Build Tools（提供 link.exe）：
#   https://visualstudio.microsoft.com/visual-cpp-build-tools/
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1
```

`scripts\build.ps1` 只是薄封装，负责环境适配和结果报告；实际构建仍走仓库根的
`build.ps1`，CI 与本地是同一条路径。它会处理中文路径（切到 ASCII 产物目录），
并输出 EXE 绝对路径与 SHA256。

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
  | -------- | ----------------------------------------------------------- | ------------------- |
  | rust 根 | `rustup show home` 反推（**并校验 toolchain 真实存在**） | 高——只要 rustup 在 PATH |
  | cargo | `rustup which cargo` 直接定位（绕开 rustup shim） | 高 |
  | binutils | `MINGW_BINUTILS_BIN` → PATH 中的 `as.exe` → 常见 MSYS2/MinGW 位置 | 中——gnu 需用户提供 |
- **依赖自检**：缺 rustup / toolchain / binutils 时直接打印**可复制执行**的修复命令，  
  而不是让用户对着链接期报错猜。
- **仓库须置于纯 ASCII 路径**（MinGW `ld` 无法正确处理含中文的产物路径）。
- **不硬编码任何本机路径**；非标准安装位置由环境变量告知（见 [环境变量](#环境变量)）。

构建相关的根因分析（host≠target、rustup 布局、资源嵌入、可复现性）见
[docs/构建说明.md](docs/构建说明.md)。

### CI

`.github/workflows/ci.yml` 在 `windows-latest` 上跑**双 job**（push 到 `main` / PR / 手动 `workflow_dispatch`）：

| job | toolchain | 内容 |
| ------------ | ------------------------ | ---------------------------------------------------------------------------- |
| `build-msvc` | `x86_64-pc-windows-msvc` | `build.ps1` 构建 → `cargo test` → 打包上传 `AI-Root-windows-x64` 工件（打印 exe SHA256） |
| `build-gnu` | `x86_64-pc-windows-gnu` | 经 `msys2/setup-msys2` 装完整 binutils → `bash build.sh` → `cargo test` |

两个 job 都需要。开发者本机是 gnu-only，msvc job 覆盖不到 gnu 路径，反过来 gnu job 也
覆盖不到 msvc 的链接行为。`build-gnu` 调用的是开发者在用的 `build.sh`，不是另写一套
等价逻辑，这样 CI 验的就是本机跑的东西。

---

## 测试

```bash
cargo test --release --target x86_64-pc-windows-gnu
```

共 177 个测试：

| 模块 | 数量 | 覆盖内容 |
|---|---|---|
| `policy` | 29 | 三级判定、内置保护不可被策略覆盖、deny 优先、路径根约束、降级模式 |
| `exec` | 20 | cmd 元字符转义、argv 直调不经 shell、UTF-8 安全截断、超时终止进程树 |
| `config` | 23 | 三层合并优先级、用户层只能收紧、坏配置不放宽、布局迁移、TOML 严格字段 |
| `common` | 13 | ACL 硬化与幂等、不把 SYSTEM 降级为只读、子目录递归硬化、二进制 DACL 校验、日志脱敏 |
| `consent` | 11 | 批准内容绑定参数哈希、一次性消费、TTL 过期 |
| `protocol` | 39 | `validate_request` 全字段边界、帧编解码、真实命名管道读写、未知字段拒绝 |
| `server` | 31 | 恒定时间令牌比较、失败熔断与 SID 封禁、`handle_client` 端到端、确认重放 |
| `signature` | 6 | WinVerifyTrust 调用、发布者指纹白名单、空信任根不接受任何签名 |
| `service` | 4 | PE 架构识别、版本号解析与降级判断 |

集成测试使用真实命名管道（`e2e_pipe_pair()` 在进程内创建管道对）和真实 `icacls`，
不 mock IO。

写这些测试时遵循三条约定，违反会造成假通过：

1. 断言必须调用真实被测代码。帧测试真的调 `write_frame` 再校验落盘字节，
   而不是在测试里重抄一遍编码逻辑，否则实现改错了测试照样绿。
2. 环境无法构造的前提，要么抽成纯函数，要么承认没被测到。「进程身份是 SYSTEM」
   在测试里造不出来，所以把选 SID 的逻辑抽成 `select_grant_sid()` 再注入组合断言。
3. 阈值类断言写死期望值而不是引用常量，否则改常量时测试会跟着一起变。

几条路径尚未被测试覆盖：`connect_client` 遇到 `ERROR_PIPE_BUSY` 的重试、
命令黑名单的匹配规则、1MB 输出截断、GBK 回退解码。变异验证的逐条记录见
[docs/测试说明.md](docs/测试说明.md)。

---

## 环境变量

| 变量 | 默认 | 作用 | 阶段 |
|---|---|---|---|
| `AIROOT_DATA_DIR` | `%ProgramData%\AI-Root` | 覆盖数据目录 | 运行期、安装期 |
| `AIROOT_USER_DIR` | `%LOCALAPPDATA%\AI-Root` | 覆盖用户配置目录 | 运行期 |
| `AIROOT_PIPE_NAME` | `\\.\pipe\AI-Root` | 覆盖管道名 | 运行期 |
| `AIROOT_RUST_ROOT` | 自动探测 | rust 根目录（含 `cargo/`、`rustup/`） | 构建期 |
| `AIROOT_TARGET` | `x86_64-pc-windows-gnu` | 目标三元组 | 构建期 |
| `MINGW_BINUTILS_BIN` | 自动探测 | MinGW binutils 的 `bin` 目录，仅 gnu 目标需要 | 构建期 |
| `CARGO_TARGET_DIR` | `<仓库>/target` | 产物目录 | 构建期 |

`AIROOT_DATA_DIR` 和 `AIROOT_PIPE_NAME` 用来起一个与生产完全隔离的实例，做演练或排障。
`install` 注册的服务固定使用默认数据目录和管道名，这两个变量只对前台 `server` 模式和
多实例有效。

```powershell
# 终端 A：起隔离实例（前台，Ctrl+C 退出）
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe server

# 终端 B：连这个实例
$env:AIROOT_DATA_DIR = "C:\temp\airoot-demo"
$env:AIROOT_PIPE_NAME = "\\.\pipe\AI-Root-demo"
AI-Root.exe exec whoami
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
├── scripts/             # 一键脚本（doctor / build / test / install / uninstall）
└── .github/workflows/ci.yml
```

| 文件 | 职责 |
| ------------------- | ---------------------------------------------- |
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

运行时依赖：`windows-service`、`serde`、`serde_json`、`rand`、`chrono`、`sha2`、`toml`。
没有 GUI 相关依赖。

---

## 文档

| 文档 | 内容 |
| ---------------------------------------------- | -------------------------------------- |
| [docs/配置说明.md](docs/配置说明.md) | **三层配置优先级、TOML 字段、rules.d、版本/布局迁移与回滚** |
| [docs/授权策略.md](docs/授权策略.md) | 授权模型、判定顺序、默认白名单、确认流程、更新与回滚 |
| [docs/安全模型.md](docs/安全模型.md) | ACL 边界的设计依据、Windows 权限行为易误判点 |
| [docs/构建说明.md](docs/构建说明.md) | 工具链自省、host≠target、资源嵌入、可复现性结论 |
| [docs/测试说明.md](docs/测试说明.md) | 测试矩阵、变异验证记录、已知缺口 |
| [docs/项目结构说明.md](docs/项目结构说明.md) | 目录结构与文件职责 |
| [docs/examples/使用示例.md](docs/examples/使用示例.md) | 多语言集成示例 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录 |

历史归档（记录改造过程，**不作为当前行为依据**）：

| 文档 | 内容 |
| -------------------------------------------------------------------------------- | ---------------------------- |
| [docs/audit/SECURITY\_REFACTOR\_AUDIT.md](docs/audit/SECURITY_REFACTOR_AUDIT.md) | v1.1 安全审计：16 项确认问题、修复方案与验证状态 |
| [docs/audit/CONFIG\_AUDIT.md](docs/audit/CONFIG_AUDIT.md) | 配置架构审计：10 项问题与目标架构 |

---

## 许可

本项目采用 [MIT 许可证](LICENSE)。

## 免责声明

AI-Root 以 `LocalSystem` 身份执行被策略允许的命令。令牌是进入这道门的凭据，拿到令牌可以发起
任意请求，但能否执行仍由服务端策略判定；内置保护项不受令牌影响。

请只在自有且受控的主机上使用，并保证：

- 令牌不提交到仓库、不写入日志、不经不安全通道传输；
- 数据目录 ACL 维持 `SYSTEM(F) + Administrators(F) + 安装用户(R)`，安装脚本会自检；
- 定期执行 `AI-Root rotate-token` 轮换令牌。
