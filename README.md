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

安装后服务自启。常用管理命令：

```text
ai-root install     # 注册为 LocalSystem 自启服务（管理员，仅首次）
ai-root uninstall   # 卸载服务（管理员）
ai-root start       # 启动服务（install 授权后非提权即可）
ai-root stop        # 停止服务（install 授权后非提权即可）
ai-root update <新exe路径>  # 自主更新服务二进制：停服务→覆盖→启服务（无需管理员）
ai-root status      # 查询服务状态
ai-root gui         # 原生 GUI 控制面板（人工巡检用）
```

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

# 多行脚本（文件）：&/&&/|/() 全部正常，推荐用于批量自治
ai-root run --file C:\tmp\batch.cmd

# 从标准输入喂入脚本（多行）
type C:\tmp\batch.cmd | ai-root run --stdin
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
  "timeout_ms": 60000          // 可选，默认 60000
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

---

## 4. 安全模型

- 令牌仅安装时生成一次；目录 ACL `SYSTEM(F) + 当前用户(R)`，其余账户不可访问。
- 管道 SDDL 追加安装用户 SID，使以该用户身份运行的智能体（可能非交互登录）也能稳定连接。
- 服务 DACL 在 install 时追加安装用户 SID 的 `GRGX`（GENERIC_READ|GENERIC_EXECUTE，含 QUERY_STATUS/START/STOP）——仅授权 status/stop/start/update 自身服务，不授予 CHANGE_CONFIG/WRITE_DAC 等管理权限、不涉及其他服务或系统级权限。
- 受「启用开关」约束（`C:\ProgramData\ai-root\enabled.flag` 存在即启用），删除即禁用中继（服务端拒绝一切命令，`ok=false, error="中继已禁用"`）。
  **开关是运行期语义**：禁用后无法通过中继自身恢复（拒绝执行），需本地/管理员重建 flag，或直接非提权 `ai-root stop` + `ai-root start`（服务启动时自动重建 flag，已实测）。
- 服务运行时（SYSTEM 身份）不会改写令牌/SID，避免错误写入 `S-1-5-18`。

---

## 5. 限制

- **仅 Windows**（依赖命名管道、LocalSystem 服务、cmd.exe）。
- 服务必须预先以管理员 `install`/`deploy` **一次**；之后智能体侧完全无需提权（含二进制更新）。
- 令牌等同于高权限凭据，泄露即等同于获得 SYSTEM 命令执行权。
- 单行命令内 `&`/`&&` 串联失效（设计使然，见 3.3）；批量场景用多行脚本。

---

## 6. 构建

```bash
export CARGO_TARGET_DIR=D:/wbrelay_target
cargo build --release --target x86_64-pc-windows-msvc
```

依赖：`serde` / `serde_json` / `rand` / `chrono` / `eframe`+`egui`（GUI）。
