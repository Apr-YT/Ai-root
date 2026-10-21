# AI-Root 安全重构审计报告

> **历史记录（非当前行为依据）**
>
> 本文记录 2026-10 的安全改造过程，用于追溯「为什么这样设计」。
> 文中「已确认漏洞」「修复方案」等描述是对**当时**代码状态的记录，不代表当前版本仍存在这些问题。
> 当前行为请以 [授权策略](../授权策略.md)、[配置说明](../配置说明.md)、[安全模型](../安全模型.md) 为准。

- 审计基线：`Apr-YT/Ai-root` 默认分支，审计时工作副本 `HEAD`（2026-10-21 拉取）
- 代码规模：`src/` 5 044 行（common 867 / exec 523 / main 729 / protocol 764 / server 1 247 / service 563 / winapi_raw 257）
- 构建与测试基线：`cargo test` → **57 passed, 0 failed**（Windows 25H2，host `x86_64-pc-windows-gnu`）
- 审计方式：逐文件通读 + 调用链追踪 + 真机执行测试。**本文所有结论均来自当前源码实际实现**，不沿用历史结论；已被修复的历史缺陷（ACL 缺陷 A/B/C）在 §5 单独列为「已正确实现」，不重复计为问题。

> 路径与行号以审计时的工作副本为准。若后续重构导致偏移，以函数名为准（本文每条都给了函数名）。

---

## 1. 当前架构与真实权限边界

### 1.1 组件与数据流

```
AI Agent / 编程助手（安装用户身份，非提权）
        │  AI-Root run "..."   （客户端进程）
        │  ① 读 %ProgramData%\AI-Root\token.txt
        │  ② 连 \\.\pipe\AI-Root
        ▼
LocalSystem 服务进程（AI-Root.exe --service）
        │  ③ 管道 SDDL 校验连接方
        │  ④ 读帧 → validate_request → ct_eq 令牌比对
        │  ⑤ enabled.flag 检查
        │  ⑥ exec::run_command → Command::new("cmd").raw_arg("/C <任意字符串>")
        ▼
cmd.exe（SYSTEM 令牌）→ 任意子进程（SYSTEM）
```

### 1.2 真实边界（关键结论）

| 边界 | 实际形态 | 说明 |
|---|---|---|
| 谁可以连管道 | `SYSTEM` / `Administrators` / **`Interactive`(IU)** / 安装用户 SID，均 `FA`（完全访问） | `common.rs::pipe_sddl()` |
| 谁可以读令牌 | SYSTEM、Administrators、安装用户 | 数据目录 ACL `SYSTEM(F)+BA(F)+安装用户(R)` |
| 谁可以**写**令牌/策略/日志 | SYSTEM、Administrators | 同上，`/inheritance:r` 已断继承 |
| 谁可以停/启服务 | SYSTEM、Administrators、**安装用户**（`GRGX`） | `service.rs::grant_acl()`，install 时主动授予 |
| 谁能替换服务二进制 | **拥有部署目录写权限者 = SYSTEM/Administrators**；但 `AI-Root update` 让安装用户**间接**做到 | 见 V-01 |
| 令牌等价于 | **完整 SYSTEM 代码执行**（无操作分级、无路径约束、无参数约束） | 见 V-02 |

**一句话**：当前模型是「一张令牌 = 一个 SYSTEM shell」。令牌的保密性与数据目录 ACL 是唯一防线；一旦令牌被读走（安装用户下任意进程都能读），防线即失效，且没有任何操作级授权。

### 1.3 普通用户（安装用户身份）能调用的全部高权限能力

| 能力 | 入口 | 是否需要令牌 | 实际效果 |
|---|---|---|---|
| 任意 SYSTEM 命令 | `AI-Root run "<任意>"` | 是（可读） | **SYSTEM 代码执行** |
| 任意 SYSTEM 多行脚本 | `AI-Root run --file/--stdin` | 是（可读） | 同上，且 `&`/`|`/`()` 全语义 |
| 替换 SYSTEM 服务二进制 | `AI-Root update <任意PE>` | **否** | **SYSTEM 代码执行**（V-01） |
| 停止服务 | `AI-Root stop` | 否 | DoS（V-14） |
| 启动服务 | `AI-Root start` | 否 | 重建 `enabled.flag`，绕过禁用开关 |
| 查询状态 | `AI-Root status` | 否 | 信息泄露（低） |
| 读取令牌 | 直接读文件 | — | 得到上述全部能力 |
| 轮换令牌 | `AI-Root rotate-token` | 需写权限 | 普通用户被 ACL 拒绝（正确） |
| 修改策略/ACL/服务配置 | — | 需写权限/管理员 | 当前被 ACL 拒绝（正确），但**代码层面无显式保护** |

---

## 2. 关键文件权限实况

| 对象 | 路径 | 期望 ACL | 实际保障 |
|---|---|---|---|
| 服务二进制 | `%ProgramData%\AI-Root\AI-Root.exe` | SYSTEM:F + BA:F + 用户:R | 依赖 `set_dir_acl` 成功；**失败被静默忽略**（V-04） |
| 令牌 | `%ProgramData%\AI-Root\token.txt` | 同上（用户可读） | 同上；已存在时不重新加固（V-16） |
| 安装用户 SID | `%ProgramData%\AI-Root\sid.txt` | 同上 | 同上 |
| 启用标志 | `%ProgramData%\AI-Root\enabled.flag` | 同上 | 同上 |
| 审计日志 | `%ProgramData%\AI-Root\{exec.log,events.jsonl}` | SYSTEM:F + BA:F + 用户:R | 同上；写失败静默（V-11） |
| 全局包装器 | `C:\Windows\AI-Root.cmd` | 仅管理员可写 | 正确（`C:\Windows` 默认 ACL） |
| 服务对象 DACL | SCM 对象 | SY:F + BA:F + 用户:GRGX | 由 `grant_acl` 设置，成功与否**未被验证** |
| 临时脚本（脚本模式回退路径） | `%TEMP%\AI-Root-<pid>-<nanos>.cmd` | 仅 SYSTEM | **实际是 `C:\Windows\Temp`，非独占创建**（V-08） |

---

## 3. 已确认漏洞

严重度定义：**Critical** = 非提权用户直接获得 SYSTEM；**High** = 安全机制可被静默绕过或失效；**Medium** = 需要前置条件或有明确缓解；**Low** = 健壮性/可用性。

### V-01 · Critical · 非提权用户可替换 SYSTEM 服务二进制

- **位置**：`src/service.rs::update_self()`（约 392–468）、`src/service.rs::grant_acl()`（约 241–322）、`src/main.rs` 的 `"update"` 分支（约 125–133）
- **触发条件**：
  1. `install` 时 `grant_acl()` 主动把 `GRGX`（含 `SERVICE_START`/`SERVICE_STOP`）授予安装用户 SID；
  2. 该用户执行 `AI-Root update <任意PE>`；
  3. 代码停止服务 → `std::fs::copy` 覆盖 `%ProgramData%\AI-Root\AI-Root.exe` → `service.start()`。
- **影响**：**任意本地代码以 LocalSystem 执行**。完全绕过令牌鉴权、管道 SDDL 与策略（当前无策略）。这是本项目最严重的缺陷 —— 它使「令牌保密」这条防线变得无关紧要。
- **现有防护的实际强度**：`is_valid_pe()` 只校验 MZ + `PE\0\0` + Machine ∈ {0x8664, 0x014C}；`file_sha256()` 把指纹**打印到 stdout**，不做任何比对。任务书 4.2 明确指出「PE 格式检查和单独打印 SHA-256 不能代替签名验证」。函数自己的文档注释也承认为「等效于 SYSTEM 代码执行」。
- **附带问题**：目标路径来自 `deploy_dir()`，可被环境变量 `AIROOT_DEPLOY_DIR` 覆盖；不查询服务实际注册的 `ImagePath`；不校验版本、架构一致性；`.old` 回滚失败路径只在 `copy` 报错时触发，`rename` 失败会直接 `copy` 覆盖正在运行的映像（V-15）。

### V-02 · Critical · 任意 SYSTEM Shell，无操作分级

- **位置**：`src/exec.rs::run_command()`（47–89）、`src/exec.rs::run_script()`（141–168）、`src/server.rs::handle_client()`（575）
- **触发条件**：持有令牌的任意客户端发送 `{"token":"...","command":"<任意字符串>"}`。
- **影响**：令牌 = 完整 SYSTEM。`command` 直接进入 `cmd /C`，无操作类型、无目标、无参数约束、无路径约束、无确认。多行模式（`run_script`）连 `escape_cmd_meta` 都不做，`&`/`|`/`()` 全语义执行。
- **为何这是架构问题而非配置问题**：任务书 5.2 指出「危险命令黑名单不能作为主要安全边界」。当前 `dangerous_patterns`（61–68）只有 5 个字面量（`format `、`diskpart`、`del /s`、`rd /s`、`rmdir /s`），以下均可直接绕过且不被记录：
  - 大小写/空白：`FORMAT`、`format\tC:`、`f^ormat`、`"del" /s`
  - 间接执行：`cmd /c del /s`、`for /f %i in ('...') do`、`powershell -enc`、`mshta`、`rundll32`、`wscript`、`regsvr32`
  - 别名/环境：`doskey`、`%ComSpec%`、`set x=del&call %x%`
  - 脚本文件：`run --file` 路径完全不经黑名单
- **反向伤害**：黑名单还会**误伤**正常命令（如 `echo format ` 被拒），且给出「已保护」的错觉。

### V-03 · High · 部署失败回退到不可信源路径

- **位置**：`src/service.rs::deploy_global()`（42–68）
- **触发条件**：`create_dir_all` 或 `std::fs::copy` 失败（磁盘满、目录被占用、权限异常、杀软拦截）。
- **影响**：函数在失败分支 **返回 `src_exe`**，随后 `install()` 把它写进 `CreateServiceW` 的 `executable_path`。若源 exe 位于用户可写目录（`D:\project\...\target\release\`、`Downloads`、`%TEMP%`），服务将以 SYSTEM 从**用户可写路径**加载 —— 该用户此后可直接替换文件获得 SYSTEM。
- **违反**：任务书 4.1「任何关键部署步骤失败时必须终止流程，不能使用未经验证的源路径或临时文件作为回退方案」。

### V-04 · High · ACL 设置全程忽略错误（fail-open）

- **位置**：`src/common.rs::set_dir_acl()`（443–488）、`grant_one()`（504–510）
- **实现事实**：返回类型是 `()`；三处 `Command::new("icacls").output()` 的结果全部丢弃（`let _ =`），`status.success()` 从不检查。
- **影响**：`/inheritance:r` 成功后任一 `grant_one` 失败，目录可能落入「SYSTEM:F 缺失」或「继承 ACE 未清除」状态，而调用方（`ensure_token`、`set_enabled`、`rotate_token`、`run_pipe_server`）**全部认为成功并继续**。
- **调用链风险点**：`run_pipe_server()`（server.rs 704–706）在服务启动时调用它；失败后服务照常监听管道 → 令牌可能处于可被非授权主体读取/写入的状态。
- **违反**：任务书 4.3「ACL 设置函数必须返回并传播错误」「对安全相关 ACL 验证失败采用 fail closed」。

### V-05 · High · ACL 自检只看「SYSTEM 是否存在 (F)」，不构成权限验证

- **位置**：`src/common.rs::acl_has_system_full()`（491–500）
- **实现事实**：唯一判据是 `l.contains("SYSTEM") && l.contains("(F)")`。
- **覆盖不到的失效模式**：
  1. `BUILTIN\Users:(OI)(CI)(F)` 残留 —— 含 `SYSTEM` 行的同时用户仍可写，函数返回 true；
  2. 继承未断（`(I)` 标记仍在）—— 不检查；
  3. `Everyone` / `Authenticated Users` / `INTERACTIVE` 的写 ACE —— 不检查；
  4. 显示名本地化（非英文 Windows）—— `contains("SYSTEM")` 直接漏检，然后**误判为失败并重复补写 ACE**。
- **违反**：任务书 4.3「检查实际 DACL 和继承规则，不能仅仅检查是否存在 SYSTEM ACE」。

### V-06 · High · 更新无代码签名验证，目标路径可被环境变量改写

- **位置**：`src/service.rs::update_self()`（392–468）、`src/service.rs::deploy_dir()`（28–37）
- **实现事实**：无 `WinVerifyTrust`、无证书/发布者校验、无版本与架构一致性校验（`is_valid_pe` 接受 x86 与 x64 任意一种，也不与当前进程架构比对）。
- **影响**：任何能通过 MZ/PE 头的文件都能被安装为 SYSTEM 服务（与 V-01 叠加）。`AIROOT_DEPLOY_DIR` 让 `update` 的写入目标脱离服务实际运行路径 —— 既是功能缺陷（更新了错误位置），也是任意目录写入原语。

### V-07 · Medium · 非 UTF-8 字符边界字节切片导致 panic

- **位置**：`src/exec.rs::run_child()` 约 300–301
```rust
"stdout_preview": &stdout[..stdout.len().min(200)],
"stderr_preview": &stderr[..stderr.len().min(200)],
```
- **触发条件**：命令输出前 200 字节内含多字节 UTF-8 字符，且第 200 字节落在字符中间（中文 Windows 下极常见，如 `echo 中文…`、`ipconfig` 本地化输出）。
- **影响**：`handle_client` 线程 panic。`dispatch_connection` 有 `catch_unwind` 兜住进程（正确），但**客户端收不到响应帧**（panic 发生在 `write_frame` 之前），表现为「命令执行了但调用方超时/读失败」；在 `run_pipe_server_forever()`（前台模式，657 行）中该线程无 `catch_unwind`，线程直接死亡。
- **对照**：同文件 591 行 `r.stdout.chars().take(200)` 是正确的 —— 说明这是遗漏而非设计。
- **违反**：任务书 5.3「安全截断 UTF-8 字符串，避免在非字符边界处进行字节切片」。

### V-08 · Medium · 临时脚本落在 `C:\Windows\Temp` 且非独占创建（TOCTOU）

- **位置**：`src/exec.rs::run_script_file()`（180–213），临时路径在 186–187
- **实现事实**：以 SYSTEM 运行时 `std::env::temp_dir()` 解析为 `C:\Windows\Temp`；文件名含 pid + `subsec_nanos()`（**不是**加密随机）；`std::fs::write` 覆盖式创建（非 `create_new`）。
- **影响**：写入与 `cmd /C <wrap>` 读取之间存在窗口；`C:\Windows\Temp` 对 `Users` 的默认 ACL 允许创建文件/目录（本机实测前提需确认，见 §6）。理论上可由本地用户预置同名文件或竞争替换，使 SYSTEM 执行攻击者内容。属经典的 Windows 临时目录 TOCTOU 提权面。
- **附带**：清理用 `let _ = remove_file(...)`，失败静默留下 SYSTEM 权限的脚本（信息泄露面）。

### V-09 · Medium · 协议无版本号、未知字段静默忽略

- **位置**：`src/protocol.rs::Request`（14–26）、`validate_request()`（56–100）
- **实现事实**：`Request` 无 `#[serde(deny_unknown_fields)]`，无协议版本字段。`token` 只校验长度为 64 字节，不校验 hex（`protocol.rs` 测试 `rejects_multibyte_token_of_byte_len_64` 已如实记录该行为并断言当前放行）。
- **影响**：客户端多传字段（如伪 `"confirmed": true`）不会报错，而是被静默丢弃。在引入确认机制后，这会退化为**安全语义缺失** —— 未来若有人把未知字段当作控制位读取，就会出现「客户端可宣称已确认」。任务书 6.1「任何未知字段 … 不能默认放行」。

### V-10 · Medium · 管道 SDDL 向 `Interactive` 授予完全访问

- **位置**：`src/common.rs::pipe_sddl()`（400–409）
- **实现事实**：基础 SDDL 为 `(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;IU)`，再追加安装用户 SID。
- **影响**：任何本地交互登录用户都能连接管道并发起请求。虽然仍需令牌，但把「谁能打这个服务」从「安装用户 + 管理员」扩大到「所有交互用户」，扩大了暴力试探面（当前只靠 20 次/30s 熔断兜底）。`IU` 的存在与「仅安装用户可连」的设计意图不一致。

### V-11 · Medium · 审计日志写入失败被完全忽略

- **位置**：`src/common.rs::LogFile::write_line()`（163–180，`let _ = self.writer.write_all(data)`）、`append_with_rotate()`（243–256）、`log_exec()`（624–640）
- **影响**：磁盘满、权限错、句柄失效时审计条目静默丢失，而操作**照常执行**。任务书 8.2 要求「日志失败不能导致未授权操作被放行；应明确区分普通日志故障与安全审计不可用时的处理策略」—— 当前没有任何区分。
- **另**：`log_event` 写入的是完整 `req.command`（`exec_start`/`exec_done`/`command_audit`），无脱敏。若命令中含 `--password=xxx`、`-p xxx`、`set API_KEY=...`，会明文长期留在 `events.jsonl`。

### V-12 · Medium · 危险命令黑名单不是边界（且误伤正常命令）

- **位置**：`src/exec.rs::run_command()`（60–68）
- **影响**：见 V-02 的绕过清单。这层检查既不能阻止攻击者，又会拒绝 `echo format ` 一类正常命令；更糟的是它给运维一种「已有防护」的错觉。**不是要放宽它，而是要用策略引擎替代它作为判定层**。

### V-13 · Low · 超时杀进程失败未处理

- **位置**：`src/exec.rs::run_child()`（253–255）
- **实现事实**：`taskkill /T /F` 的返回状态被丢弃；随后 `child.wait()` 无二次兜底。
- **影响**：`taskkill` 失败时 `wait()` 可能长期阻塞（子进程仍在写管道），该工作线程与一个 `MAX_CONNS` 名额被占用直至子进程自然退出。

### V-14 · Low · 非提权用户可停止服务（DoS）

- **位置**：`src/service.rs::grant_acl()`（241–322）、`stop_svc()`（470–479）
- **影响**：`GRGX` 含 `SERVICE_STOP`，安装用户可随时停服务。属设计取舍（为了「零 UAC 更新」），但在引入确认/策略后应重新评估：更新不再需要非提权 STOP 权限。

### V-15 · Low/Medium · update 的 `.old` 回滚与架构校验缺失

- **位置**：`src/service.rs::update_self()`（433–455）
- **实现事实**：
  - `rename(deploy → .old)` 失败时直接 `std::fs::copy(new, deploy)` —— 若旧映像正被占用会失败，此时**旧文件未被移动**，尚属安全；但若 copy 部分成功后被中断，部署目录留下半个 PE。
  - 服务启动失败后不做健康检查（只打印 `query_status`），不自动回滚到 `.old`。
  - 不比对架构与当前进程（x86 服务被 x64 二进制替换会无法启动）。
- **违反**：任务书 4.2「更新失败时保留可恢复的已知良好版本」。

### V-16 · Medium · 已存在的 token 不重新加固

- **位置**：`src/common.rs::ensure_token()`（513–524）
- **实现事实**：`if tp.exists() { return Ok(read...) }` —— 存在即返回，不重设 ACL、不校验文件权限、不校验内容格式。
- **影响**：若 `token.txt` 在 ACL 生效前（install 早期窗口）被创建为宽松权限，或事后被外部工具改写权限，`ensure_token` 不会修复。`run_pipe_server` 无条件调用 `set_dir_acl` 只覆盖了「目录」这一层，且该调用本身 fail-open（V-04）。

---

## 4. 潜在风险与设计改进建议

| ID | 类别 | 内容 | 建议 |
|---|---|---|---|
| R-01 | 设计 | 令牌是**单一全权凭据**，无作用域、无过期、无按操作分级 | 引入策略引擎（本重构 Phase 4），使「持有令牌」≠「可执行任意操作」 |
| R-02 | 设计 | 无用户确认通道 | 引入文件型确认（`pending/` + `approve`），确认与请求参数哈希绑定 |
| R-03 | 设计 | `enabled.flag` 是运行期开关，被 `start` 隐式重建 | 禁用语义应改为策略层 `mode: "disabled"`，不再依赖可被服务启动重建的文件 |
| R-04 | 加固 | 子进程继承 SYSTEM 完整环境变量（含 `COMPUTERNAME`、可能的服务凭据） | 显式清理环境，仅保留最小集合 |
| R-05 | 加固 | 无 `SeDebugPrivilege` / 特权裁剪；服务保留 LocalSystem 全部特权 | 评估用 `AdjustTokenPrivileges` 裁剪非必需特权（改动面大，列为后续） |
| R-06 | 健壮 | `ACTIVE_CONNS` 上限仅在 `handle_client` 入口检查，连接建立即可占位 | 可接受；配合读超时已无泄漏 |
| R-07 | 兼容 | `Request.command` 为必填 `String` | 改造为 `Option`，新增 `op`/`argv`，旧客户端仍可用（shell 模式降级为需确认） |
| R-08 | 文档 | README §安全模型把「update 输入校验」列为加固手段 | 必须改写：PE 校验不构成安全边界 |
| R-09 | 加固 | `C:\Windows\AI-Root.cmd` 包装器内容未校验 | install 时校验写入结果；已受 `C:\Windows` ACL 保护 |
| R-10 | 观察 | `is_valid_pe` 允许 x86 与 x64 | 改为必须与当前进程架构一致 |

---

## 5. 当前已正确实现的机制（不能只列问题）

这些机制经代码复核确认有效，**重构中不得回退**：

1. **恒定时间令牌比较** `server.rs::ct_eq()` —— XOR 累加，无提前返回；长度不等即 false。
2. **双层鉴权节流** —— 全局熔断（30s/20 次 → 锁 120s）+ 按调用方 SID 定向封禁（60s/5 次 → 300s），且有 `SID_FAILS` 表规模上限 256 与惰性清理。
3. **帧层防护** —— 4 字节长度前缀 + `FRAME_MAX_SIZE`(16MB) 上限；帧头/帧体分离计时；帧体按 payload 放宽的绝对上限（256 KB/s 折算）防 slow-loris。
4. **并发上限** —— `ACTIVE_CONNS` + RAII `ConnGuard`，超限立即回「服务繁忙」。
5. **输出排空而非中断** —— `drain_pipe` 超限后继续消费丢弃，避免子进程阻塞在内核管道缓冲（B1 修复）。
6. **日志并发安全** —— 每文件一个 `Mutex` + 64KB `BufWriter`，2MB 轮转，服务停止时 `flush_logs()`。
7. **优雅停止** —— `CancelIoEx` 中断阻塞的 `ConnectNamedPipe`，句柄所有权只属于主循环或单一工作线程（无 use-after-free）。
8. **panic 隔离** —— `dispatch_connection` 用 `catch_unwind(AssertUnwindSafe)` 包裹，单连接 panic 不拖垮服务。
9. **ACL 缺陷 A/B/C 的正确修复** —— `/inheritance:r` 断继承、SID 加 `*` 前缀、逐条独立 `grant_one`、`select_grant_sid` 硬性排除 `S-1-5-18`；且三者都有**做过变异验证**的回归测试。
10. **输出编码** —— 纯 ASCII 直通 / UTF-8 校验 / OEM 代码页回退；`decode_codepage` 用 `MultiByteToWideChar` 而非手工解码。
11. **超时杀进程树** —— `taskkill /T /F` 而非仅 `child.kill()`。
12. **协议回环测试** —— `read_frame_*` 用真实命名管道（`CreateNamedPipeW` + `CreateFileW` 同进程）测试，而非文件替身。
13. **e2e 全链路测试** —— 真实驱动 `handle_client`（鉴权→执行→回写），并用全局锁串行化以隔离进程级 static。

---

## 6. 尚未验证的假设（明确标注）

| 假设 | 为何未能验证 | 处置 |
|---|---|---|
| `C:\Windows\Temp` 对 `Users` 的可写性 | 需在真机用不同用户会话实测 ACL；审计环境为单管理员会话 | **不依赖该假设**：重构把临时脚本移入 `%ProgramData%\AI-Root\tmp\`（ACL 保护）+ 独占创建 |
| `grant_acl` 的 `GRGX` 是否足以 `update` 全流程 | 需要真实服务生命周期（安装/停止/启动）实测 | 文档已记载「实测通过」；重构后 update 不再依赖非提权 STOP（改为策略+签名门控） |
| WinVerifyTrust / Crypt* 在本机构建下的结构体布局 | 需实际调用已签名 PE 验证 | 在重构中以只读方式对系统已签名 exe 实测；未通过则 update 保持 **deny**（安全失败） |
| 服务以 SYSTEM 启动时 `set_dir_acl` 的实际行为 | 需服务模式实测（审计未安装服务，避免污染主力机） | 保留现有失败自愈逻辑，并新增启动验证；**未在本机安装服务验证的部分明确标为未验证** |
| 域环境下 `LookupAccountSidW` 延迟 | 本机非域 | 不影响安全结论 |

---

## 7. 修复方案总览（与实施阶段对应）

| 漏洞 | 修复手段 | 阶段 |
|---|---|---|
| V-01 / V-06 / V-15 | update 全链路重做：签名验证（WinVerifyTrust + 发布者指纹绑定）+ 路径固定为 SCM 实际 ImagePath + 架构/版本校验 + 失败回滚 + 健康检查 | Phase 2 |
| V-03 | 部署失败即返回 Err，禁止回退到源路径；创建服务前校验目标路径可信（非用户可写） | Phase 2 |
| V-04 / V-05 / V-16 | `set_dir_acl -> Result`；新增基于 `GetNamedSecurityInfo` 的真实 DACL 校验（危险 SID 判写权限、SYSTEM 判 F、继承判 `(I)`）；服务启动验证失败即拒绝启动；`ensure_token` 强制复验 | Phase 2 |
| V-02 / V-12 | 新增 `policy.rs`：Allow/Confirm/Deny 三级判定 + 结构化 argv 执行（不经 shell）+ 内置不可覆盖保护清单；shell 模式默认需确认 | Phase 3/4 |
| V-07 / V-08 / V-13 | UTF-8 安全截断；临时脚本迁入受保护目录 + `create_new` + 加密随机名；超时清理错误检查与二次兜底 | Phase 3 |
| V-09 / V-10 | 协议版本 + `deny_unknown_fields` + 决策字段；管道 SDDL 移除 `IU`（保留安装用户 SID） | Phase 6 |
| V-11 | 审计写入结果传播；高风险操作在审计不可用时拒绝执行；敏感参数脱敏 | Phase 6 |
| V-14 | update 不再需要非提权 STOP；服务 DACL 收回 `WP`（保留 `RP`/查询），或仅在配置开启时授予 | Phase 2 |
| R-01 / R-02 | 策略引擎 + 文件型确认通道（`pending/<rid>.json` + `approve/deny`，与参数哈希绑定，超时即拒） | Phase 4/5 |

---

## 8. 审计结论

1. **当前实现的安全性完全建立在「令牌不被读走」之上**，而令牌对安装用户可读、等价于完整 SYSTEM。这不是加密强度问题，是**授权模型缺失**。
2. **`update` 是比令牌更短的攻击路径**：不需要令牌、不需要读文件，只要能以安装用户身份执行一条命令。任何「加固令牌」的努力在此之前都是次要的。
3. 大量 ACL 代码是**正确但不可信**的：逻辑（A/B/C 修复）是对的，但错误被吞掉（V-04）、自检判据太弱（V-05），所以「逻辑正确」不等于「运行时确实生效」。
4. 现有 57 个测试质量偏高（真实管道、真实 icacls、变异验证记录），**应扩展而非替换**。缺失的是：授权层、失败路径、UTF-8 边界、签名拒绝路径的测试。
5. 结论：需要按任务书阶段 2→7 依次实施，其中 **Phase 2（update/部署/ACL）与 Phase 4（策略）是收益最高的两步**；Phase 3/5/6 是必要配套。
