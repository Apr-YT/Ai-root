# AI-Root 配置架构审计（v1.1 现状）

> **历史记录（非当前行为依据）**
>
> 本文记录 2026-10 的安全改造过程，用于追溯「为什么这样设计」。
> 文中「已确认漏洞」「修复方案」等描述是对**当时**代码状态的记录，不代表当前版本仍存在这些问题。
> 当前行为请以 [授权策略](../授权策略.md)、[配置说明](../配置说明.md)、[安全模型](../安全模型.md) 为准。

- 审计对象：当前工作副本 `HEAD`（v1.1.0 安全改造完成后）
- 审计方式：逐函数核对 + 真机运行验证，不沿用任何历史结论
- 结论摘要：**配置不是"系统"，而是散落在常量、环境变量、单个 JSON 文件里的三处来源**；
  其中存在 **3 个高危缺陷**，其中 1 个会导致「删掉配置文件 = 权限自动放宽」。

---

## 1. 当前配置来源（全量清单）

### 1.1 编译期常量（不可运行时修改）

`src/common.rs` 集中定义了 12 个阈值：

| 常量 | 值 | 作用 |
|---|---|---|
| `PIPE_READ_TIMEOUT_MS` | 10 000 | 服务端读帧空闲超时 |
| `CLIENT_READ_TIMEOUT_MS` | 60 000 | （已被动态预算取代，保留） |
| `DEFAULT_COMMAND_TIMEOUT_MS` | 60 000 | 命令默认超时 |
| `MAX_COMMAND_TIMEOUT_MS` | 600 000 | 超时硬上限 |
| `MAX_CONNS` | 8 | 并发连接上限 |
| `FRAME_MAX_SIZE` | 16 MB | 协议帧上限 |
| `OUTPUT_LIMIT` | 1 MB | 单流输出截断 |
| `SCRIPT_INLINE_MAX_LEN` | 6 000 | 脚本拼接上限 |
| 其余 4 个 | — | 轮询/连接等待间隔 |

注释里写着「可调常量」，但**实际不可运行时调整** —— 想改超时上限必须重新编译。

### 1.2 环境变量（3 个，无格式校验）

| 变量 | 读取点 | 影响 | 校验 |
|---|---|---|---|
| `AIROOT_DATA_DIR` | `common.rs:100` | 数据目录（令牌/日志/策略全部跟着走） | 无 |
| `AIROOT_PIPE_NAME` | `common.rs:87` | 管道名 | 仅 trim 非空 |
| `AIROOT_DEPLOY_DIR` | `service.rs:29` | `install` 的部署位置 | 仅 trim 非空 |

三者都没有在 README 里声明为「仅测试用」，但它们实际能改变生产行为。

### 1.3 管理员授权策略（单文件 JSON）

- 路径：`%ProgramData%\AI-Root\policy.json`（`policy.rs:53`）
- 格式：JSON（`serde_json`）
- 加载点：
  - `server.rs:573` —— **每个请求**都调用 `Policy::load(&data_dir)`
  - `service.rs:466` —— `update` 前加载信任根
  - `main.rs:610/685` —— `policy` / `signature` 展示（`load_existing`）
- 内容：`version` / `mode` / `default_decision` / `workspace_roots` / `approved_services` /
  `trusted_publishers` / `allow_legacy_shell` / `confirm_timeout_secs` / `rules[]`
- 默认策略在 `PolicyFile::default_autonomy()` 内硬编码 **60+ 条规则**

### 1.4 用户偏好

**不存在**。想改默认超时、确认有效期、终端着色，只能改管理员策略（需 UAC）或重新编译。

### 1.5 磁盘布局（当前）

```text
%ProgramData%\AI-Root\
├── AI-Root.exe
├── policy.json        ← 授权策略
├── token.txt          ← 秘密
├── sid.txt            ← 秘密（安装用户 SID，用于管道 SDDL）
├── enabled.flag       ← 运行状态
├── exec.log           ← 审计日志
├── events.jsonl       ← 事件流
├── pending\           ← 确认请求
└── tmp\               ← SYSTEM 待执行脚本（临时）
```

一个 `icacls` 只能对整棵目录生效，**无法给 secrets 单独更严的权限**。

---

## 2. 已确认的问题

严重度：**高** = 可能放宽权限或使安全机制失效；**中** = 可维护性/可用性实质缺陷；**低** = 一致性。

### C-01 · 高 · 策略文件缺 `deny_unknown_fields`，拼错字段静默忽略

- **位置**：`policy.rs:362` `pub struct PolicyFile`（无 `#[serde(deny_unknown_fields)]`），
  `policy.rs:317` `pub struct Rule`（同）
- **触发**：手写配置时把 `trusted_publishers` 写成 `trusted_publisher`、`approved_services`
  写成 `approved_service`、`rules` 写成 `rule`。
- **影响**：serde 静默丢弃未知字段 →
  - `trusted_publishers` 拼错：`update` 永久拒绝，报错只说「未配置可信发布者」，**不指出是拼写问题**；
  - `rules` 拼错：所有规则消失，操作从 allow 退化为 confirm/deny（收紧方向，但用户以为配好了）；
  - `allow_legacy_shell` 拼错：回落 false（收紧）。
- **为什么算高危**：不是因为它放宽了权限，而是因为**配置是否正确无法自证**。
  一旦某个字段的默认值恰好是宽松的，拼错就变成放宽。当前 `mode` 字段若拼错 →
  回落 `Mode` 的必填（无 `default`）→ 解析失败 → 降级，尚属安全；但这依赖字段是否可选，不可持续。

### C-02 · 高 · 删除策略文件会自动恢复「宽松默认策略」

- **位置**：`policy.rs:425-443`
```rust
Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
    let def = PolicyFile::default_autonomy();   // ← 60+ 条 allow 规则
    let _ = std::fs::write(&path, json);        // ← 自动写回
    return Policy { file: def, state: CreatedDefault };
}
```
- **触发**：`policy.json` 被误删、被"系统清理"工具删掉、被备份还原操作暂时移走。
- **影响**：下一次请求（`server.rs:573` 每请求加载）就把**默认 autonomy 策略**写回磁盘。
  默认策略自动允许 40+ 个开发工具、winget/choco/scoop 安装、传统 shell 走 confirm。
  也就是说：**删配置 = 权限回到出厂宽松状态**，且没有任何告警。
- **违反**：本次配置改造要求 6「配置错误时不能意外放宽权限」。
- **根因**：没有「策略已初始化」标记，无法区分「首次安装」与「事后被删除」。

### C-03 · 高 · 没有版本迁移机制

- **位置**：`policy.rs:56` `POLICY_VERSION = 1`；`policy.rs:450-460` 只处理 `version > 当前`
- **事实**：`version < 当前` 时会**当作当前版本继续解析**，字段语义变化后自然错位；
  `version == 当前` 但结构变更时完全无从判断。
- **影响**：任何一次策略结构演进（例如把 `rules` 改成 `rules.d`、把 `mode` 拆成两个字段）
  都会让旧文件被"半正确"解析，可能出现「规则没读到但文件看起来正常」。
- **要求**：需要显式迁移链 + 迁移前备份 + 迁移失败不落盘。

### C-04 · 中 · 没有用户偏好层

- **事实**：`common.rs` 常量 + 管理员策略是唯一可调入口。
- **影响**：调一个默认超时、改确认有效期、关掉终端颜色，都要动 `%ProgramData%`（需 UAC）。
  与「个人主力机高自主性、减少打断」直接冲突。

### C-05 · 中 · 规则与策略同文件，无法按主题维护

- **事实**：默认策略的 60+ 条规则硬编码在 `default_autonomy()` 里；用户新增规则要重写整个
  JSON 数组。JSON 无注释，无法说明"这条为什么允许"。
- **影响**：新增一条规则 = 全文件重写；多人/多主题维护必然冲突；审查时无法只看 diff。

### C-06 · 中 · 秘密、日志、状态、临时文件混在同一目录

- **位置**：`common.rs:109 token_path` / `117 log_path` / `124 events_path` /
  `331 enabled_flag_path` / `431 user_sid_path` / `135 tmp_dir` —— 全部 `data_dir()/xxx`
- **影响**：
  - 无法给 `token.txt`（SYSTEM 凭据）更严的 ACL（例如禁止列目录、仅允许直接读取）；
  - 日志（可读）与秘密（应最小可读）同权限，任何"为了看日志"而放宽的 ACL 会连带放宽秘密；
  - `tmp\` 里是**即将被 SYSTEM 执行**的脚本，与可读日志同级，审计时容易混淆。

### C-07 · 中 · 每次请求重新读盘解析策略，且无缓存/无原子性

- **位置**：`server.rs:573` 每请求 `Policy::load`
- **影响**：
  - 高频调用（AI 连续执行几十条命令）时反复读 + 解析同一个文件；
  - 编辑器保存策略的瞬间（截断后重写）可能读到空文件/半截 JSON → `Degraded` →
    **短暂把所有请求变成 Deny**（安全方向，但会造成难以理解的"突然全部被拒"）；
  - 无 mtime 缓存，无法区分「内容没变」与「内容变了」。

### C-08 · 中 · 没有配置检查命令

- **事实**：只有 `AI-Root policy` 打印策略摘要。没有"逐层校验 + 指出第几行错"的能力。
- **影响**：配置错误只能靠"某条命令突然被拒/被放行"来发现。

### C-09 · 低 · 环境变量未声明用途、无校验

- `AIROOT_DATA_DIR` / `AIROOT_PIPE_NAME` / `AIROOT_DEPLOY_DIR` 实际能改变生产行为，
  但文档未标注「仅测试/隔离用」，也无路径合法性校验（相对路径、UNC、超长）。

### C-10 · 低 · 没有可直接校验的示例配置

- 文档里只有片段，没有完整文件；用户无法"复制 → `config check` → 看到通过"。

---

## 3. 已验证正确、本次不得回退的机制

1. **策略解析失败 → `locked_down`**（除只读查询外全拒），不是回落默认。
2. **版本高于当前 → 拒绝**，不猜测语义。
3. `PolicyFile::validate()`：规则 ID 重复、allow 规则过于宽泛（未约束 op/program）、
   `program_path`/`cwd_roots` 必须是绝对路径、指纹必须是 64 位 hex。
4. **deny 规则优先于 allow**，内置保护优先于一切。
5. 策略文件 ACL：`SYSTEM:F / Administrators:F / 安装用户:R`（普通 Agent 只读）。
6. **策略热生效**：改文件即生效，无需重启服务（这个性质要保留）。
7. 内置保护（`builtin:shell-host` / `builtin:destructive` / `builtin:self-protect`）不可由配置覆盖。

---

## 4. 目标架构（本次实施）

```text
优先级（后者覆盖前者，但"用户层只能收紧"）
  ① 代码默认值          PolicyFile::default_autonomy()
  ② 用户偏好            %LOCALAPPDATA%\AI-Root\user.toml        ← Agent 可写，但只能收紧
  ③ 管理员授权策略      %ProgramData%\AI-Root\policy.toml        ← 仅管理员可写（权威）
     + 规则目录         %ProgramData%\AI-Root\rules.d\*.toml     ← 按主题拆分
```

磁盘布局（秘密/日志/状态分离）：

```text
%ProgramData%\AI-Root\          (SYSTEM:F / BA:F / 安装用户:R)
├── AI-Root.exe
├── policy.toml                 授权策略（TOML，可注释）
├── rules.d\                    自动化规则（每个文件一组主题）
│   ├── 10-dev-tools.toml
│   └── 90-local.toml
├── secrets\                    秘密（更严：SYSTEM:F / BA:F）
│   └── token.txt
├── logs\
│   ├── events.jsonl
│   └── exec.log
└── state\
    ├── enabled.flag
    ├── sid.txt
    ├── policy.initialized      ← 区分「首次安装」与「事后被删」
    ├── layout.version
    ├── pending\
    └── tmp\

%LOCALAPPDATA%\AI-Root\         (当前用户:F)
└── user.toml                   用户偏好（只能收紧授权）
```

关键安全约束（必须由代码强制，不能只写在文档里）：

| 约束 | 实现 |
|---|---|
| 用户配置**不能**出现 `rules` / `approved_services` / `trusted_publishers` / `allow_legacy_shell` | `UserConfig` 使用 `deny_unknown_fields`，出现即报错 |
| 用户 `mode` 只能收紧 | `strictness(autonomy)=0 < balanced=1 < strict=2 < disabled=3`，取 `max` |
| 用户 `confirm_timeout_secs` 只能调小 | `min(user, admin)` |
| 用户 `default_timeout_ms` 不得超硬上限 | `clamp(1000, MAX_COMMAND_TIMEOUT_MS)` |
| 策略文件曾被初始化但丢失 | `state/policy.initialized` 存在 → `locked_down`（而不是重建默认） |
| 任一 `rules.d/*.toml` 解析失败 | **整体** `locked_down`（不能跳过，否则丢失 deny 规则 = 放宽） |
| 规则 ID 跨文件重复 | `locked_down` |
| 版本低于当前 | 走显式迁移链（备份原文件） |
| 版本高于当前 | 拒绝 |

---

## 5. 实施阶段与验证方式

| 阶段 | 内容 | 验证 |
|---|---|---|
| P1 | 目录分层 + 旧布局迁移（含备份、失败不破坏） | 单元测试构造旧布局 → 迁移 → 断言新路径与内容；旧文件保留备份 |
| P2 | TOML 策略 + `rules.d` + 严格字段 + 初始化标记 + 版本迁移（v1 JSON → v2 TOML） | 单元测试：拼错字段报错、规则文件损坏降级、跨文件重复 ID 降级、删除后不重建 |
| P3 | 用户配置 + 三层合并（只能收紧） | 纯函数测试：8 种 mode 组合、超时取 min、禁用字段报错 |
| P4 | 接入 `server` / `service` / `main`（真实加载与授权路径） | 端到端测试：用户配置收紧后 argv 白名单失效；管理员策略照旧生效 |
| P5 | `AI-Root config check` / `show` / `path` + 示例配置 | CLI 实测：正常/故意写坏两种场景的退出码与输出 |
| P6 | README + `docs/配置说明.md` + `examples/` | 文档与实际字段逐一对照 |
