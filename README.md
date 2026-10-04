# AI-Root

> **面向 AI 智能体自调用的 LocalSystem 提权命令中继。**
> 非提权（普通用户）的智能体进程，仅凭一个共享令牌即可通过命名管道以 `SYSTEM` 权限自治执行命令，全程无需 UAC 弹窗。本工具的主要消费方是 AI 智能体本身，文档聚焦于「如何可靠对接」而非「人怎么敲命令」。

## 最近更新（2026-10-04）

### 安全修复（本轮）
- **§4.1 目录 ACL 隔离缺陷 A/B**：`set_dir_acl` 原先既未清除继承来的 `Users` 写权限（任意本地用户可覆写 `token.txt` 自授权拿 SYSTEM），又因 SID 缺 `*` 前缀导致授权静默全失败。已修复为 `/inheritance:r` + 逐条独立授权 + 收尾自检。
- **§4.1.1 服务自降权限缺陷 C**：服务以 LocalSystem 启动时，`set_dir_acl` 把 SYSTEM 自己当成"待授权用户"并 replace 成只读，导致服务无法写数据目录。已通过 `granted_user_sid()` + 纯函数 `select_grant_sid()` 从结构上消除。
- 数据目录 ACL 最终形态：`SYSTEM(F) + Administrators(F) + 安装用户(R)`，且服务反复重启后保持稳定。

### 测试
- 测试数 6 → **57**：新增 5 个 `handle_client` 端到端测试（真实管道驱动全链路），
  ACL 安全回归 3 个，全部经**变异验证**（注入缺陷必须精确 FAILED）。
- 新增 `select_grant_sid()` 纯逻辑内核，使"进程身份 = SYSTEM"这一测试环境无法复现的前提可被单元测试覆盖。

### 构建
- `build.sh` / `build.ps1` 的 binutils 路径改为**自动探测**（`MINGW_BINUTILS_BIN` → `D:\Kiln\mingw64\bin` → `D:\Kiln\mingw-binutils\bin`），
  不再硬编码单一目录（本机曾把 `mingw-binutils` 改名为 `mingw64`，硬编码会直接导致脚本失效）。

### 更早（2026-08-28）

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
- **令牌**：安装时生成 32 字节随机十六进制令牌，落盘于 `C:\ProgramData\AI-Root\token.txt`，目录 ACL 为 `SYSTEM(F) + Administrators(F) + 安装用户(R)`（隔离缺陷史见 §4.1 / §4.1.1）。

---

## 2. 部署（仅需一次，管理员）

```powershell
# 以管理员身份打开 PowerShell，然后执行：
powershell -ExecutionPolicy Bypass -File install-service.ps1
```

`install-service.ps1` 分 6 步并逐步打印结果，**不含隐藏动作**：

| 步骤 | 内容 |
|---|---|
| 0 | 提权自检（非管理员直接 `exit 1`） |
| 1 | 二进制 SHA256 指纹核对（不匹配即 `exit 1`，避免装错版本） |
| 2 | 安装前快照（服务状态 / 目录 ACL / 部署产物存在性） |
| 3 | `AI-Root install`（注册 LocalSystem 自启服务） |
| 4 | 安装后核对（服务注册与启动类型、`SERVICE_START_NAME`、部署副本与包装器、**Users 无写权限 + SYSTEM 保留 (F)**、`sid.txt`） |
| 5-6 | 启动服务 + `sc qfailure` 失败恢复核对（应 60s/300s/600s） |

> 若第 4 步 ACL 检查报 `[X]`（Users 仍持有写权限），**请立即停止使用并反馈** —— 说明 §4.1 的隔离修复未生效。
> 同时应确认 `NT AUTHORITY\SYSTEM` 显示为 `(F)`；若显示 `(R)` 则说明命中 §4.1.1 的自降权限缺陷。

> 也可手动逐步执行：`AI-Root.exe install` → `AI-Root.exe start`（同样需管理员）。

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

- 令牌仅安装时生成一次；目录 ACL `SYSTEM(F) + Administrators(F) + 安装用户(R)`，其余账户不可访问。
  ⚠ **该隔离曾长期未真正生效**，见 §4.1；另曾出现"服务自降为只读"故障，见 §4.1.1。
- **令牌轮换**：`AI-Root rotate-token`（管理员/SYSTEM）生成新令牌写盘，服务端按文件 mtime 变化**自动热加载**（无需重启），旧令牌立即失效。泄露后的标准处置流程：rotate → 更新 AI 侧令牌引用。
- **审计日志**：`C:\ProgramData\AI-Root\exec.log` 逐行记录 `[时间] caller=DOMAIN\user pid=xxx exit=code cmd=...`。`caller` 来自服务端 `GetNamedPipeClientProcessId` 解析的**管道对端进程身份**（非服务自身环境变量），可精确追溯"谁在什么进程里执行了什么"。
- 管道 SDDL 追加安装用户 SID，使以该用户身份运行的智能体（可能非交互登录）也能稳定连接。
- 服务 DACL 在 install 时追加安装用户 SID 的 `GRGX`（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP）——仅授权 status/stop/start/update 自身服务，不授予 CHANGE_CONFIG/WRITE_DAC 等管理权限、不涉及其他服务或系统级权限。
- 受「启用开关」约束（`C:\ProgramData\AI-Root\enabled.flag` 存在即启用），删除即禁用中继（服务端拒绝一切命令，`ok=false, error="中继已禁用"`）。
  **开关是运行期语义**：禁用后无法通过中继自身恢复（拒绝执行），需本地/管理员重建 flag，或直接非提权 `AI-Root stop` + `AI-Root start`（服务启动时自动重建 flag，已实测）。
- 服务运行时（SYSTEM 身份）会重固化目录 ACL，但**绝不会**把 SYSTEM 自身当"待授权用户"
  ——`granted_user_sid()` 从 `sid.txt` 取安装用户 SID 并硬性排除 `S-1-5-18`（见 §4.1.1）。

### 4.1 目录 ACL 隔离缺陷（2026-10-04 发现并修复）

`common::set_dir_acl` 声称实现「仅 SYSTEM 完全控制 + 当前用户只读」，但原实现**未能达成**。
两个缺陷互相遮蔽，须一并理解：

**缺陷 A：`/grant:r` 不清除其它 SID 的 ACE**

原实现用 `/inheritance:e` + `/grant:r`。而 `icacls /grant:r` 的 `:r`（replace）**只替换指定 SID 的那一条 ACE**，
不会移除其它 SID 的 ACE。于是 `C:\ProgramData` 继承来的
`BUILTIN\Users:(I)(CI)(WD,AD,WEA,WA)` 被**原样保留** —— 任意本地用户对该目录仍有**写权限**。

后果链：`token.txt` 是授权执行 SYSTEM 命令的共享密钥，而设计上「目录 ACL 才是真正的权限边界」。
用户可写该目录 ⇒ 可覆写 `token.txt` ⇒ **非提权用户自授权拿到 SYSTEM 命令执行**。

实测对照（父目录含可继承的 Users 写 ACE）：

| 实现 | 结果 |
|---|---|
| `/inheritance:e`（原） | `BUILTIN\Users:(I)(OI)(CI)(WD,AD)` **残留** ❌ |
| `/inheritance:r`（修复） | 仅 `SYSTEM:(OI)(CI)(F)` + 用户只读 ✅ |

**缺陷 B：`icacls` 的 SID 参数缺 `*` 前缀**

`get_user_sid()` 返回裸 SID（`S-1-5-21-...`），原实现直接拼成 `{sid}:(OI)(CI)R`。
`icacls` 会把裸 SID 当**账户名**去解析，失败即报「账户名与安全标识间无任何映射完成」，
并**整条命令非零退出、一个 ACE 都不写**（grant 参数是全有或全无）。

**为何两缺陷必须一起修**：缺陷 B 原本被缺陷 A 掩盖 ——
`/inheritance:e` 保留了继承来的 `SYSTEM:F`，所以即使显式授权失败，服务照样有权限。
一旦单独把 A 修成 `/inheritance:r` 而不管 B，继承 ACE 已清除、显式 ACE 又写入失败
⇒ **目录 DACL 变空，SYSTEM 自己都进不去，服务彻底锁死**。

**修复**（`common.rs::set_dir_acl`）：
1. 先 `/inheritance:r` 断开继承并清除全部继承 ACE；
2. 显式授予 `SYSTEM:(OI)(CI)F`、`Administrators:(OI)(CI)F`、`当前用户:(OI)(CI)R`
   —— 均带 `(OI)(CI)` 传播标志，由目录自身向下继承，故「SYSTEM 写的事件文件用户可读」不受影响；
3. SID 一律加 `*` 前缀标记为 SID 字面量；
4. **逐条独立** `grant_one()` 调用（不再把三条 grant 塞进一条命令），单条失败不牵连其它；
5. 收尾自检 `acl_has_system_full()`，若 SYSTEM 完全控制 ACE 缺失则立即补写
   —— 从结构上消除「空 DACL 锁死」这一失败模式。

回归测试 `common::tests::set_dir_acl_strips_inherited_users_write` 真实构造危险前提
（父目录授予可继承的 Users 写权限）后调用**真实** `set_dir_acl`，断言写权限被剥离、
SYSTEM 保留完全控制、当前用户保留只读。变异验证：还原缺陷 A / 缺陷 B 各一次，均 FAILED。

### 4.1.1 缺陷 C：服务启动把自己降级为只读（2026-10-04 线上发现并修复）

**症状**：服务安装并启动后，`C:\ProgramData\AI-Root` 的 ACL 变成 `NT AUTHORITY\SYSTEM:(OI)(CI)(R)` ——
SYSTEM 只剩**只读**，于是以 LocalSystem 运行的服务**无法再写** `token.txt` / `events.jsonl` / `exec.log`，
整个 SYSTEM 中继不可用（看似"服务在跑"实则已废）。

**根因链**：
1. `run_pipe_server()` 在服务启动时调用 `common::set_dir_acl(&data_dir())` 固化目录 ACL；
2. 此时进程身份是 **LocalSystem**，而旧实现内部用 `get_user_sid()` 取"当前用户"；
3. `get_user_sid()` 跑 `whoami /user` → 以 SYSTEM 身份返回 **`S-1-5-18`**（SYSTEM 自己），
   而**不是**安装用户；
4. 于是"授予当前用户只读"那步实际执行成 `grant_one("*S-1-5-18:(OI)(CI)R")`，
   而 `:r` 是 **replace** 语义 → **覆盖掉刚授予的 `SYSTEM:(F)`**；
5. 结果服务每次启动都自降为只读。

**为何兜底没拦住**：`acl_has_system_full()` 自检确实检测到了缺失并补写，但补写用的 ACE 是
`*S-1-5-18:(OI)(CI)F`，而**同一次调用里后面紧跟的用户授权又把它 replace 回 `(R)`**
（`grant_one` 是逐条独立调用，顺序即"先 F 后 R"，后者覆盖前者）。属顺序性缺陷，
非"兜底失效"。修复时通过 `granted_user_sid()` 从源头eliminate该 ACE 才真正解决。

**实测证据**（真机复现）：停服务 → 手工 `icacls ... /grant:r "*S-1-5-18:(OI)(CI)F"`
（SYSTEM 恢复 `(F)`）→ 启动服务 → ACL **立即退回** `SYSTEM:(R)`。反复稳定复现。

**修复**（`common.rs`）：把"选谁作为待授权用户"抽成**纯函数** `select_grant_sid(persisted, current)`：
1. 优先读持久化的 `sid.txt`（install 阶段以管理员身份写入，是**权威**来源）；
2. 回退到当前进程身份，但**硬性拒绝** `S-1-5-18`；
3. 两者皆 SYSTEM 或皆不可用 → `None`，走 Interactive 只读退化分支（而非降级 SYSTEM）。

`granted_user_sid()` 负责取值（读文件 + `whoami`）后委托该纯函数；
`set_dir_acl` 改用 `granted_user_sid()`。

**为何必须抽纯函数**：测试进程**永远不是 SYSTEM**，无法在测试里构造
"当前身份 = S-1-5-18"这一环境。首版回归测试直接断言 `granted_user_sid() != S-1-5-18`，
**变异验证实测漏检**（把实现改回 `get_user_sid()` 后测试仍全绿）——
因为测试环境里 `whoami` 本来就返回真实用户。抽出纯函数后可喂入
`select_grant_sid(Some("S-1-5-18"), Some("S-1-5-18"))` 断言 `None`，变异验证随即 FAILED。

**回归测试** `common::tests::set_dir_acl_never_downgrades_system`：
- ① 纯逻辑组：6 条断言覆盖 `select_grant_sid` 的 persisted/current 全组合（含两者皆 SYSTEM、
  sid.txt 被污染、非法值、空白裁剪）；
- ② 集成组：人为把 scratch 目录 SYSTEM 降为 `(R)`（构造前提并断言成立），
  调**真实** `set_dir_acl`，断言 SYSTEM 重新获得 `(F)`。

变异验证（均 FAILED，见提交说明）：
- 移除 `current` 分支的 `sid != LOCAL_SYSTEM_SID` 守卫；
- 移除 `persisted` 分支的同一守卫。

**真机修复后 ACL**（服务重启后仍保持）：

| 主体 | 权限 |
|---|---|
| `KING\64391`（安装用户） | `(OI)(CI)(R)` |
| `BUILTIN\Administrators` | `(OI)(CI)(F)` |
| `NT AUTHORITY\SYSTEM` | `(OI)(CI)(F)` ✅ |

端到端验证：非提权 `AI-Root run "whoami /user"` → `nt authority\system S-1-5-18`；
`AI-Root run "cmd /c echo ... > C:\ProgramData\AI-Root\probe.txt"` → exit=0 且文件落盘成功。

### 4.2 运行时安全加固

- **鉴权失败节流（双层）**：
  - **全局熔断**：30s 窗口内累计失败达 **20** 次 → 全局锁定 **120s**，期间拒绝一切请求（记 `<locked>`）。
  - **按调用方定向封禁**：同一 SID 在 60s 窗口内失败达 **5** 次 → 单独拒绝该 SID **300s**，不影响其他调用方。
  - 两层互补：定向封禁防「同一身份反复试探」，全局熔断兜底「无差别暴力尝试」。
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
- **构建产物**：仅 `AI-Root.exe`（console 子系统，约 1.07 MB，MinGW 静态链接 CRT）。
  > 体积经 `profile.release` 优化：`lto="thin"` + `codegen-units=1` + `strip="debuginfo"`。
  > ⚠ **`strip` 取 `debuginfo` 而非 `symbols`**（2026-10-04 实测调整）：
  >
  > | 配置 | 体积 | 符号数 | panic 回溯 |
  > |---|---|---|---|
  > | `symbols` | 842 KB | 0 | **空**，仅一行 `note: Some details are omitted` |
  > | `debuginfo` | 1.07 MB | 3333 | 完整函数名 + 调用链 |
  > | `none` | 3.41 MB | 完整 | 多出体积几乎全是无用 debug 段 |
  >
  > 本项目以 LocalSystem 执行任意命令，线上崩溃拿不到符号的排障代价远高于 256 KB 体积。
  > 另注：panic 的 `file:line:col` 位置信息编在 panic 元数据中，**两种配置下都有**，不受 `strip` 影响。

---

## 6. 限制

- **仅 Windows**（依赖命名管道、LocalSystem 服务、cmd.exe）。
- 服务必须预先以管理员 `install`/`deploy` **一次**；之后智能体侧完全无需提权（含二进制更新）。
- 令牌等同于高权限凭据，泄露即等同于获得 SYSTEM 命令执行权。
- 单行命令内 `&`/`&&` 串联失效（设计使然，见 3.3）；批量场景用多行脚本。
- **单命令输出 1MB 截断**（`truncated` 标记）；默认超时 60s（上限 10 分钟）；并发上限 8（超出返回「服务繁忙」，重试即可）；鉴权失败节流见 4.1（全局 30s/20 次锁定 120s，单 SID 60s/5 次封禁 300s）。

---

## 7. 构建与测试

当前为**单一纯终端版**（无 `gui` feature、无双变体），构建产物仅 `AI-Root.exe`。

```bash
# 本机构建（git bash 执行 build.sh）
bash build.sh
# → $CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/AI-Root.exe（约 1.07 MB，MinGW 静态链接 CRT）

# 或 PowerShell（本机与 CI 通用，自动选 target）
powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
```

**构建脚本自动选 target**（`build.ps1`，2026-10-04 重写）：

1. 已装 `x86_64-pc-windows-msvc` 且能找到 `link.exe`（VS Build Tools）→ 用 msvc（CI `windows-latest` 走此路径）。
2. 否则若已装 `x86_64-pc-windows-gnu` → 用 gnu，并注入完整 MinGW binutils。
3. rustup 根目录按 `$RUSTUP_HOME` → `%USERPROFILE%\.rustup` → `D:\software\rust\rustup` 顺序探测。

> ⚠ **gnu 构建的两个硬约束**（2026-10-04 实测定位）：
>
> 1. **需要含 `as.exe` 的完整 binutils**。rustup 的 `self-contained` 目录实测只有 `dlltool.exe`/`ld.exe`/`x86_64-w64-mingw32-gcc.exe`，**缺 `as.exe`**，不能单独使用。两个构建脚本按 `MINGW_BINUTILS_BIN`（环境变量）→ `D:\Kiln\mingw64\bin` → `D:\Kiln\mingw-binutils\bin` 顺序**自动探测**（见注释：2026-10-04 本机把目录从 `mingw-binutils` 改名为 `mingw64`，硬编码单路径会直接失效）。获取：清华 msys2 镜像 `mingw-w64-x86_64-binutils-*.pkg.tar.zst`，解压后把 `mingw64/bin` 内容放入上述任一目录。
> 2. **Git Bash 下 PATH 只认 POSIX 形式**。把 `D:/Kiln/...` 或 `D:\Kiln\...` 写进 `PATH` 后，`command -v dlltool.exe` / `which` 一律查不到（不是 Permission denied，是彻底找不到），导致 cargo 解析回落到 `CARGO_HOME\bin` 的 rustup shim，最终报 `error calling dlltool 'dlltool.exe': program not found`。`build.sh` 用 `to_posix()` 统一转换，并直接调用 toolchain 内的真实 `cargo.exe`；`build.ps1` 侧为原生 Windows 路径语义，无此问题。
> 3. **gnu 目标的 `CARGO_TARGET_DIR` 必须是纯 ASCII 路径**。MinGW binutils（`dlltool`/`ld`）按 ANSI 代码页解释路径参数，遇到非 ASCII（本项目路径含 `工具`）会把它毁成 `????`，报 `dlltool.exe: Can't create .lib file: ...\????\...\kernel32.dll_imports.lib` —— 错误信息里路径已乱码，极难定位。因此**不要**用仓库内默认的 `target/`（本项目路径非 ASCII），本机固定 `D:\AI-Root-target`。两个构建脚本均已加前置校验，命中时直接给出可执行的修复建议而非让链接期失败。

**CI 双 job**（`.github/workflows/ci.yml`，`windows-latest`）：

| job | toolchain | 内容 |
|---|---|---|
| `build-msvc` | `x86_64-pc-windows-msvc` | `build.ps1` 构建 → `cargo test` → 打包上传 `AI-Root-windows-x64` 工件（30 天保留，打印 exe SHA256） |
| `build-gnu` | `x86_64-pc-windows-gnu` | 经 `msys2/setup-msys2` 装完整 binutils → `bash build.sh` 构建 → `cargo test` |

> **为什么必须有两个 job**（2026-10-04 教训）：本机开发环境是 **gnu-only**，而 msvc job 不可能覆盖 gnu 路径。
> `build.sh` 曾长期因 binutils/PATH 问题无法构建，**但 CI 一直是绿的** —— 因为只验了 msvc。`build-gnu` 专门堵住这个盲区：
> 它走的是**开发者实际使用的 `build.sh`**，而非另写一套等价逻辑，确保 CI 验的就是本机跑的。
>
> `build-gnu` 不手工下载 `.pkg.tar.zst`：该格式是 zstd 压缩，实测本机 GNU tar 1.35、Python 标准库（无 `zstandard`）均无法解压，runner 上是否可行无法在本地验证 —— 属于「靠猜」。改用 `msys2/setup-msys2` 由官方 action 保证正确性。

`build.ps1` 构建产物随 job 上传；本机与 CI 的 `CARGO_TARGET_DIR` 需分离，避免互相覆盖。

### 7.1 测试覆盖现状

```bash
cargo test --release --target x86_64-pc-windows-gnu
```

当前 **57 个测试**（2026-10-04 从 6 个扩充：44 个单元测试 + 5 个真实管道集成测试 + 3 个 ACL 安全回归测试 + 5 个 `handle_client` 端到端测试），分布：

| 模块 | 数量 | 覆盖内容 |
|---|---|---|
| `exec` | 6 | cmd 元字符转义（引号内/外、`^`/`&`、未闭合引号） |
| `common` | 3 | `set_dir_acl` 安全回归：剥离继承的 Users 写权限（§4.1 缺陷 A）+ 幂等性 + **不得把 SYSTEM 降级为只读**（§4.1.1 缺陷 C，含 `select_grant_sid` 纯函数全组合断言） |
| `protocol` | 27 | `validate_request` 全字段边界（令牌长度/命令 8191/cwd MAX_PATH/超时上下限/校验顺序）；序列化向后兼容；`write_frame` 帧编码与 flush；**`read_frame_timeout` 真实命名管道集成测试**（正常往返、300 KB 大帧、空闲超时、超长长度前缀拒绝、对端关闭） |
| `server` | 21 | `ct_eq` 恒定时间比较（含逐字节差异、首末字节）；`AuthThrottle` 熔断阈值/窗口重置/锁定过期；`record_sid_fail` 定向封禁/隔离/窗口过期；**`handle_client` 端到端**（见下） |

> **集成测试用真实命名管道**：`read_frame_timeout` 的 5 个测试通过 `make_pipe_pair()` 在进程内自建 `CreateNamedPipeW` + `CreateFileW` 管道对（管道名含 `pid` + 测试标签，保证并行安全），**不 mock 任何 IO**。因此它真实覆盖了 `PeekNamedPipe` 轮询、分段超时预算、`FRAME_MAX_SIZE` 长度前缀校验这几条最容易出安全 bug 的路径。
>
> **ACL 测试用真实 icacls**：`set_dir_acl_strips_inherited_users_write` 先构造「父目录授予可继承 Users 写权限」这一危险前提并断言前提成立，再调用真实 `set_dir_acl`，最后断言写权限被剥离 / SYSTEM 保留 / 用户保留只读。前提不成立时测试主动失败（避免"前提没了还假装通过"）。
>
> 该组测试经变异验证有效：注入「去掉 `FRAME_MAX_SIZE` 校验」→ 仅 `read_frame_rejects_oversized_length_prefix` 失败；注入「去掉头部空闲超时检查」→ `read_frame_times_out_when_no_data_sent` **直接挂死**（被 `timeout 60` 杀掉）——恰好印证它守的是线程泄漏型 DoS 的兜底。
>
> ACL 测试同样经变异验证：还原 §4.1 缺陷 A（`/inheritance:r` → `/inheritance:e`）→ 两个 ACL 测试均失败；还原缺陷 B（去掉 SID 的 `*` 前缀）→ 首次**未被捕获**，据此补了"当前用户保留只读 ACE"断言后才捕获。这正说明变异测试的价值：不跑一次就不知道断言其实没覆盖到。
>
> §4.1.1 缺陷 C 的变异验证同样有价值：首版测试直接断言 `granted_user_sid() != S-1-5-18`，注入变异（改回 `get_user_sid()`）后**测试仍全绿**——因为测试进程本来就不是 SYSTEM。把选 SID 的逻辑抽成纯函数 `select_grant_sid()` 后可注入 `(SYSTEM, SYSTEM)` 组合，两种变异（persisted 守卫、current 守卫）随即均 FAILED。**教训：无法在测试环境复现的前提（此处是"进程身份 = SYSTEM"），要么抽成纯函数，要么就不算被测到。**

> **`handle_client` 端到端测试（5 个）**：真实驱动「读请求帧 → 序列化校验 → 鉴权 → 执行 → 回写响应」全链路，主线程当客户端、工作线程跑真实 `handle_client`，**不 mock 任何环节**。覆盖：有效令牌正常执行、错误令牌拒绝且不执行命令、禁用开关生效、退出码透传（`cmd /c exit 7` → 7）、非法请求在执行前被拦。
>
> 环境隔离三要点：① `AIROOT_DATA_DIR` 指向临时目录（令牌/flag 才有测试值）；② `AUTH_THROTTLE`/`SID_FAILS`/`ACTIVE_CONNS` 是进程级 static，必须用全局锁 `E2E_LOCK` **串行化**本组用例；③ `LOGGER`（OnceLock）首次写日志即绑定目录，故本组共享同一 `AIROOT_DATA_DIR`。
>
> 变异验证（均精确命中，每次只有对应那一个测试 FAILED）：① 取消鉴权（`ct_eq` 恒真）→ 仅 `e2e_..._rejects_wrong_token` 失败；② 去掉 `is_enabled()` 检查 → 仅 `e2e_..._respects_disabled_flag` 失败；③ 退出码硬编码为 0 → 仅 `e2e_..._passes_through_exit_code` 失败。

> **测试用真实被测代码**：帧测试真实调用 `write_frame` 并校验落盘字节，而非在测试内重抄一遍编码逻辑（后者是「伪测试」——实现改错也照样通过）。
>
> **阈值类断言硬编码期望值**，而非引用常量。否则「改常量」的变异不会被检出（测试跟着一起变）。这些数值是对外承诺的行为契约（见 4.1），变更应是有意识的决策。
>
> **尚未覆盖**（已知缺口，优先级从高到低）：
> - `connect_client` 的 `ERROR_PIPE_BUSY` 重试路径（需构造「实例全忙」场景，较难稳定复现）
> - 命令黑名单（`format ` 等）的匹配规则
> - 输出截断（1MB）、GBK 回退解码

> 📌 **现状核对（2026-10-04）**：下述交付目录**当前均不存在** —— `D:\project\workbuddy\工具\AI-Root-build\` 与 `D:\Finished product\AI-Root\` 都未创建。`D:\Kiln\` 下有 `AI-Root`（服务部署副本，服务已安装运行）。构建产物落在 `CARGO_TARGET_DIR`（本机 `D:\AI-Root-target\x86_64-pc-windows-gnu\release\AI-Root.exe`）。首次部署时需按下文流程创建。

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

**CI**：`.github/workflows/ci.yml` 在 GitHub Actions `windows-latest` 上跑 msvc + gnu 双 job（提交到 main、PR 或手动 `workflow_dispatch` 触发），详见第 7 节。

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
