# Changelog

本文件记录 AI-Root 的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [1.4.0] - 2026-10-21

### Removed

- `install-service.ps1`。破坏性变更，改用 `scripts\install.ps1`。
  旧脚本缺少幂等判断、签名检查、`-PreflightOnly` 和安装后自检。
- 死代码：`CLIENT_READ_TIMEOUT_MS`、`log_path()`、`read_frame()`、`ACCESS_DENIED_ACE_TYPE`。
  `is_valid_pe` 改为 `#[cfg(test)]`。

### Changed

- 审计报告移入 `docs/audit/`，文件头标注为历史记录。
- 重写 `docs/项目结构说明.md`。原文声称 `server.rs` 1,247 行（实际 2,027），
  漏掉 `config`、`policy`、`consent`、`signature` 四个模块，也没有 `scripts/`。
  新版只描述职责，不记录行数。
- README 部署章节只保留 `scripts\install.ps1`，删掉旧脚本的分步说明。
- 源码注释去掉改造过程叙述和 bug 编号，保留安全相关的设计理由。

### Fixed

- clippy 警告从 20 条降到 0。
- 帧上限的两条断言改为编译期 `const _: () = assert!(...)`。

### Tests

- 176 passed，clippy 0 warning，CI 的 msvc 与 gnu 两个 job 均通过。

## [1.4.1] - 2026-10-21

### Fixed

- `read_state_file` 的白名单写死为 `policy.json`，而策略文件在 v1.2 已改名
  `policy.toml`，导致该 op 对唯一允许的目标也返回「读取失败」。
  改为引用 `config::POLICY_FILE`，并补一条端到端测试，同时断言 `token.txt`、
  `sid.txt` 和路径穿越写法都被拒绝。

### Changed

- README 纠正若干与实现不符的描述：
  - 事件流路径与事件名。原文写 `events.jsonl` 在数据目录根部、事件名为 `exec_start`，
    实际在 `logs\` 下，且没有 `exec_start`，对应的是 `policy_decision`、`exec_done`、
    `command_audit` 三条，以 `run_id` 关联。
  - 令牌路径补上 `secrets\` 子目录。
  - 环境变量表移除 `AIROOT_DEPLOY_DIR`（已无代码读取），补上 `AIROOT_USER_DIR`。
  - 测试数量与模块分布更新为实测值。
  - 免责声明原写「持有令牌即等同于拥有 SYSTEM 命令执行权」，与授权策略层矛盾，已更正。

### Tests

- 177 passed。

## [1.3.1] - 2026-10-21

### Fixed — 安装会直接失败（严重）

在本机实测 `install` 失败：

```text
[错误] secrets 目录权限不安全（未写入令牌）：残留 3 条继承 ACE
（/inheritance:r 未生效，父目录权限会继续传播）
```

根因：`icacls /inheritance:r` **只作用于指定目录，不递归子目录**。

`ensure_token` 的顺序是 `create_dir_all(secrets)` → `set_dir_acl(数据目录)`。
创建 `secrets` 时，父目录 `%ProgramData%\AI-Root` 还是从 `ProgramData` 继承来的弱
DACL，于是 `secrets` 把 3 条 ACE（含 `Users` 可写）**固化**为自己的继承 ACE。
之后对父目录执行 `/inheritance:r` 并不会清除子目录里已固化的 ACE，
`verify_dir_acl(secrets)` 随即拒绝写入令牌 —— 安装失败。

修复：`set_dir_acl` 改为**递归硬化**（深度 2，覆盖
`secrets` / `logs` / `rules.d` / `state\{tmp,pending}`），每个目录独立断继承并显式授权。

**校验逻辑没有放宽**：`verify_dir_acl` 仍然拒绝任何继承 ACE。放宽校验会让秘密目录
真的保留可写 ACE；正确做法是让硬化覆盖到子目录。

### Fixed — `-Force` 重装只卸载、不安装（严重）

`install.ps1 -Force` 在卸载之后**没有执行安装**：`$svc` 是卸载**之前**取的快照，
`if ($svc.Exists)` 仍为真，外层 `else`（全新安装）分支因此永远不可达。

实测结果：服务被删掉后只剩一个 delete-pending 的僵尸（SCM 里 `DISABLED` 却仍显示
`RUNNING`）、没有令牌文件，`exec` 直接报「未找到令牌，请先以管理员运行 install」。

修复：把安装动作抽成 `Install-Service` 函数，`-Force` 分支在卸载后**显式调用**；
并对「同名服务仍处于 delete-pending」增加 2s 后重试一次。

### Fixed — `uninstall` 对运行中的服务只做标记删除

`delete()` 不会终止正在运行的进程，只是把服务标记为删除。于是紧接着删 exe 会因
文件仍被占用而失败（os error 5），部署目录残留上一版二进制，服务继续跑旧代码，
而调用方以为已换版。

修复：`delete` 之前先 `stop` 并等待 `STOPPED`（最多 5 秒）；删文件改为
重试 6 次 × 200ms。

### Tests

- 新增 `set_dir_acl_hardens_preexisting_subdirs`：先在父目录硬化前创建两级子目录，
  使其继承弱 ACE，再断言三者都被硬化。已做**变异验证**：把递归深度改成 0 时该用例
  立即失败（报「残留 4 条继承 ACE」），恢复后通过 —— 确认它能抓住旧缺陷而不是摆设。
- 测试数 **175 → 176**。
- 真机补齐此前一直标记为「未验证」的链路：
  `install` 成功 → 服务 `Running` → `exec whoami` 返回 `nt authority\system` →
  重复 `install` 幂等（输出「已安装，未做变更」，exit 0）→
  `secrets` 目录 ACL 全显式（`SYSTEM:F` / `Administrators:F` / 用户 `:R`，无 `(I)` 标记）。

> v1.3.0 的 Release 产物含此缺陷（安装会失败），请使用 v1.3.1。

## [1.3.0] - 2026-10-21

### Added — scripts\

- **`scripts/doctor.ps1`**：检查 PowerShell 版本、进程架构、Rust 工具链、
  已装 target、MSVC `link.exe` / MinGW `as.exe`、Windows SDK、仓库路径编码、
  配置层校验与服务状态；每项缺失都给出**可复制执行**的修复建议。只读，不装任何东西。
- **`scripts/build.ps1`**：Release 编译，输出 EXE 绝对路径、版本、大小、SHA256。
  仓库路径含非 ASCII 字符时自动把 `CARGO_TARGET_DIR` 切到
  `%LOCALAPPDATA%\AI-Root\build-target`（MinGW `ld` 无法处理中文路径）。
- **`scripts/test.ps1`**：运行测试并区分「通过 / 失败 / 环境跳过」，同时明确列出
  当前环境**根本无法覆盖**的项（真实 SCM 生命周期、SYSTEM 上下文 ACL、update 端到端、域环境）。
- **`scripts/install.ps1`**：非管理员时通过 UAC 自提权；幂等（已安装只报状态、不重装）；
  `-PreflightOnly` 只检查不落盘；签名状态展示 + `-StrictSignature` 强制；
  安装后自检服务状态、`config check`、数据目录 ACL 与 `exec whoami` 冒烟。
- **`scripts/uninstall.ps1`**：清单预览 + 确认 + 默认备份 + `-DryRun`；
  `-Purge` 才彻底删除，且默认先备份到 `%ProgramData%\AI-Root-backup-<时间戳>`。
- **`scripts/dev.ps1`**：统一入口 `-Action doctor|build|test|install|uninstall|all`，
  子脚本以独立进程执行并汇总退出码。

### Fixed — 卸载会删掉用户数据（严重）

- `service.rs::uninstall` 原先 `remove_dir_all(deploy_dir)`，而 `deploy_dir` 就是
  `%ProgramData%\AI-Root` —— `policy.toml`、`rules.d\`、`logs\` 与凭据同在此目录。
  于是「卸载服务」实际等于「静默重置全部配置并删除审计日志」，紧接着却打印
  「执行日志/事件流保留于 …」，注释也写着「数据目录保留日志供审计查阅」，
  实现与声明自相矛盾。
- 现在只删除：程序文件（`AI-Root.exe` / `.old` / `.new`）、运行期残留
  （`state\tmp`、`state\pending`、`state\enabled.flag`）、凭据（`secrets\token.txt`、
  `state\sid.txt`）与全局包装器；**保留** `policy.toml`、`rules.d\`、`logs\`、
  `state\policy.initialized` 与 `%LOCALAPPDATA%\AI-Root\user.toml`，并在结束时逐项列出保留内容。

### Changed

- `scripts/uninstall.ps1` 只把**部署目录内**的 exe 计入删除项；构建产物
  （`target\` 或 `build-target\`）不属于安装内容，卸载不会碰。
- README 增补 [脚本](#脚本scripts) 章节：首次安装三条命令、日常更新两条命令、行为边界表。

### Tests

- 全部脚本已在 Windows 11 25H2 / PowerShell 5.1 真机执行：
  `doctor` 通过（1 警告：非 ASCII 仓库路径，已自动处理）；
  `build` 成功（31.5s，EXE 2.0 MB）；
  `test` **175 passed / 0 failed**（含从非仓库目录调用的路径验证）；
  `install -PreflightOnly` 正确识别未签名并保持零变更；
  `uninstall -DryRun` 在未安装时输出「无需卸载」；
  `dev.ps1 -Action doctor` 汇总正常。
- 未真实执行：服务安装/卸载（会改变主力机的 SYSTEM 服务状态）、UAC 提权路径、
  `-Update` 成功路径（需签名包 + 信任根）。这些需在可恢复虚拟机验证。

## [1.2.0] - 2026-10-21

### Added — 配置系统

- **`src/config.rs`**：三层加载与合并（默认值 → 用户偏好 → 管理员策略），
  提供层状态报告、合并说明（`notes`）与带指纹的缓存（保留"改配置即生效"）。
- **TOML 配置**（替代 JSON）：`policy.toml` 支持注释；规则可拆到 `rules.d\*.toml`，
  按文件名字典序合并，按主题维护。
- **用户偏好层** `%LOCALAPPDATA%\AI-Root\user.toml`：可调 `mode`（只能收紧）、
  `confirm_timeout_secs`（只能缩短）、`default_timeout_ms`（钳制到 1s..600s）。
- **新命令**：`AI-Root config check|show|path|init|migrate`。
  `check` 逐层校验并给出可定位的 TOML 行号错误，有错退出码 1。
- **示例配置** `examples/`：`policy.toml` + `rules.d/10-dev-tools.toml` +
  `rules.d/20-services.toml` + `user.toml`，已用 `config check` 实测通过，不含真实秘密。

### Changed — 磁盘布局（v2）

```text
%ProgramData%\AI-Root\
├── policy.toml          管理员策略（TOML）
├── rules.d\            自动化规则
├── secrets\            令牌（更严的独立目录）
├── logs\               events.jsonl / exec.log
└── state\              enabled.flag / sid.txt / policy.initialized / pending\ / tmp\
```

- 秘密、日志、状态、临时脚本**分目录**存放（原实现全在根目录，ACL 只能一刀切）。
- **自动迁移**旧扁平布局：`install` / 服务启动 / `config migrate` 时执行，幂等；
  不覆盖已存在的目标文件，迁移前备份旧策略到 `state\legacy\`。
- `token_path` / `log_path` / `events_path` / `enabled_flag_path` / `user_sid_path` /
  `tmp_dir` 全部指向新位置。

### Security — 配置错误不再放宽权限

- **修复 C-01**：`PolicyFile` / `Rule` / `UserConfig` 全部加 `#[serde(deny_unknown_fields)]`。
  拼错字段（如 `trusted_publisher`）现在**解析即报错并指出期望字段名**，不再静默忽略。
- **修复 C-02**：新增 `state\policy.initialized` 标记。策略文件**曾初始化后被删除**时
  降级为 `locked_down`（只读查询以外全拒），**不再自动重建宽松默认策略**。
  旧行为等于"删掉配置文件 = 权限回到出厂状态"。
- **修复 C-03**：版本迁移链。v1 JSON 自动迁移为 v2 TOML（备份原文件）；
  v1 TOML 按 v2 语义解释（字段超集）；高于当前版本一律拒绝。
- **规则文件 fail-closed**：任一 `rules.d\*.toml` 解析失败或规则 ID 跨文件重复 →
  **整体降级**。跳过坏文件会丢失 deny 规则，等于放宽。
- **用户层只能收紧**：`UserConfig` 里没有 `rules` / `approved_services` /
  `trusted_publishers` / `allow_legacy_shell` 字段；写了报错且整层忽略。
  `mode` 按 `strictness` 取 max，`confirm_timeout_secs` 取 min。
- **用户配置损坏不影响管理员策略**：忽略用户层（结果只会更严），并在 `config check` 提示。
- **原子写**：策略写入改为 `.tmp` + `rename`，避免保存过程中被读到半截文件。

### Fixed — 一致性与可用性

- **C-06** `secrets\` 在 `ensure_token` 里单独复验 DACL（继承之外再确认一次）。
- **C-07** 每请求加载改为带 mtime/大小指纹的缓存：配置不变时只做几次 `metadata`，
  变化时立即重载（热生效保留）。
- **C-09** `AIROOT_DEPLOY_DIR` 不再影响部署位置（统一 `config::machine_dir()`），
  避免"服务二进制受保护"与"配置受保护"分裂成两套边界。
- `AI-Root policy` / `signature` 为只读命令，不创建任何文件。

### Tests

- 测试数 **152 → 175**，全部通过（Windows 25H2 真机，`cargo test --release`，
  耗时 0.76s）。新增 23 项配置测试 + 3 项端到端：
  - 三层合并：mode 只能收紧（4 种组合）、confirm 只能缩短、超时钳制；
  - fail-closed：策略删除不重建、规则文件损坏整体降级、跨文件重复 ID 降级；
  - 严格字段：策略/规则里拼错字段必须报错；
  - 迁移：v1 JSON → TOML（含备份）、旧布局 → 分层布局（不覆盖已存在目标）；
  - 缓存：文件改动后重新加载；
  - 端到端：用户配置收紧为 `disabled` 后 argv 白名单失效（`mode:disabled`）；
    用户配置注入规则无效；策略缺失时真实请求被拒（`degraded:deny`）。
- **实机验证**：`config check` 对拼错字段给出 `unknown field ... expected one of ...`
  并返回退出码 1；`examples/` 三个示例文件通过校验（mode=strict、规则 14 条）。

### 未验证（明确记录）

- 布局迁移在**真实 SYSTEM 服务**上下文中的执行（本机未安装服务）；
- 迁移后旧版本回滚的完整流程（文档给了手工步骤，未实测）；
- `%ProgramData%` 被组策略重定向 / 非系统盘 ProgramData 的场景。

## [1.1.0] - 2026-10-21

### Security — 更新与部署（Critical）

- **修复：非提权用户可替换 SYSTEM 服务二进制**（审计 V-01）。旧 `update` 只需 PE 头校验，
  任何能执行它的用户都能把任意代码变成 LocalSystem 服务。
  现改为三道门控：`WinVerifyTrust` 验证 **Authenticode 嵌入签名** →
  发布者证书 SHA-256 命中策略 `trusted_publishers` → 目标路径取自 SCM 实际 `ImagePath`
  且必须位于受保护数据目录。
- **新增**：架构必须与当前进程一致（x86 二进制不再能替换 x64 服务）；
  拒绝版本降级；复制后二次比对 SHA-256；启动 20s 未 Running **自动回滚**到 `.old`。
- **修复**：部署失败回退到源路径（V-03）。`deploy_global` 失败现在直接返回 `Err`，
  不再把用户可写目录里的 exe 注册为 SYSTEM 服务。
- **移除**：`update` 依赖 `AIROOT_DEPLOY_DIR` 推算目标路径（既是功能缺陷，也是任意目录写入原语）。

### Security — ACL（High）

- **修复**：`set_dir_acl` 返回 `Result` 并传播每条 `icacls` 的错误（V-04，原先全部 `let _ =` 丢弃）。
- **新增**：`verify_dir_acl` 用 `GetNamedSecurityInfoW` 解析**二进制 DACL** 校验三条不变量
  （危险 SID 无写权限 / SYSTEM 有写 / 无残留继承 ACE），不再依赖 `icacls` 文本匹配本地化账户名。
- **变更**：服务启动时 ACL 设置或校验失败 → **拒绝启动**（fail closed），写 `acl_setup_failed` 事件。
- **变更**：`ensure_token` 先硬化目录 ACL 再写令牌；令牌已存在时也复验目录权限（V-16）。
- **移除**：`acl_has_system_full`（只查 `contains("SYSTEM") && contains("(F)")` 的弱判据）。

### Security — 执行与授权（Critical）

- **新增 `src/policy.rs`**：Allow / Confirm / Deny 三级判定；内置保护硬编码不可覆盖
  （间接执行宿主、下载器、不可逆磁盘/引导、关闭安全控制、账户/持久化、改 AI-Root 自身）；
  deny 规则优先于 allow；未知操作/未知字段一律拒绝；策略损坏自动降级为「只读」。
- **新增 `AI-Root exec`**：结构化 argv 执行，**不经 shell**；程序解析成绝对路径（不查当前目录）；
  参数是数组。策略可按「程序 + 参数前缀 + 目录」精确授权。
- **变更**：`AI-Root run`（传统 shell）默认进入**人工确认**流程。
- **新增 `src/consent.rs`**：文件型确认通道。批准内容 = `sha256(op|完整参数摘要)`，
  与参数绑定、一次性、带 TTL；普通用户无法写 `pending\`（ACL）。
- **新增**：`AI-Root pending` / `approve` / `deny` / `policy` / `signature` / `trust-publisher`。

### Security — 协议与审计（Medium）

- **协议 v2**：`Request` 增加 `op` / `argv` / `target` / `v`；`#[serde(deny_unknown_fields)]`
  —— 发 `"confirmed": true` 会**解析失败**而不是被静默忽略（V-09）。
- **响应**增加 `decision` / `rule_id` / `reason` / `confirmation_id`，调用方能看到「为什么被拒绝」。
- **新增 `common::redact`**：审计日志遮蔽 `--password=` / `-p` / `API_KEY=` 等参数（V-11）。
- **新增**：`policy_decision` / `confirm_consumed` / `update_rejected` / `update_rollback` 事件类型。

### Fixed — 稳定性

- **修复 panic**：`exec.rs` 用 `&stdout[..200]` 做预览，中文输出时第 200 字节常落在字符中间
  → thread panic，客户端收不到响应（V-07）。改为 `truncate_utf8` 向前回退到字符边界。
- **修复 TOCTOU**：脚本模式临时文件从 `%TEMP%`（SYSTEM 下为 `C:\Windows\Temp`，用户可写）
  迁到 `%ProgramData%\AI-Root\tmp\`（ACL 保护）+ 16 字节加密随机名 + `create_new`（V-08）。
- **修复**：超时 `taskkill` 失败现在记录 `taskkill_failed` 事件并降级为 `child.kill()`（V-13）。
- **修复**：临时文件清理失败记录 `temp_cleanup_failed`（含路径），不再静默。
- **修复**：客户端读响应超时从固定 60s 改为「执行超时 + 30s」，避免长命令被客户端提前放弃。
- **修复**：测试日志污染真实 `%ProgramData%\AI-Root`（测试构建下固定写 `%TEMP%\airoot-test-logs`）。

### Changed — 行为与兼容

- `Request.command` 变为 `Option<String>`（v1 客户端仍可用，语义为 legacy shell）。
- `AI-Root update` 在未配置 `trusted_publishers` 时**一律拒绝**，并提示如何建立信任根。
- `AI-Root policy` / `signature` 为只读命令，**不会**创建 `%ProgramData%\AI-Root`。

### Tests

- 测试数 **57 → 152**（新增策略引擎、确认通道、ACL 二进制校验、签名验证、UTF-8 截断、
  协议严格字段、授权端到端等用例）。全部在 Windows 25H2 真机通过。
- 未验证项（明确记录）：服务安装/启动/真正的 SYSTEM 上下文 ACL 行为、
  `update` 端到端签名替换、域环境行为 —— 需在可恢复的 Windows 虚拟机验证。

## [Unreleased]

## [1.0.0] - 2026-10-04

### Fixed — 构建与 CI

- **CI 从未成功执行**：workflow 顶层 `env:` 使用了 `${{ runner.temp }}`，而该 context 在
  **解析期**即被展开（此时尚无 runner 实例），导致 GitHub 拒绝**整份文件**。
  症状具有强误导性：工作流名显示为文件路径、`check_runs: 0`、日志 404、push 后数秒即 failure。
  影响：此前「msvc 路径靠 CI 验证」的假设**长期为空**（18/18 次运行全部失败）。
- **gnu job 工具链自省失败**：原按目录名硬匹配 `stable-x86_64-pc-windows-gnu`，但
  `rustup toolchain install stable --target ...-gnu` 实际装出的是 **msvc host + 附加 gnu target**，
  `toolchains/` 下并无该目录。改为按「是否已装目标 target」挑选工具链。
- **rust 根目录结构假设错误**：原写死 `<root>/{cargo,rustup}`，而 CI 使用默认布局
  `%USERPROFILE%\{.cargo,.rustup}`，`dirname` 下并无 `cargo\` 子目录 → 全部候选落空。
  改为直接向 rustup 自省（`rustup which cargo`），适配任意布局与任意 host 名。
- **gnu 链接期 `file format not recognized`**：`embed-resource` 2.x 按
  `#[cfg(target_env = "msvc")]` 选择模块，而该 cfg 指**编译 build script 的 host**，非 cargo
  `--target`。在「msvc host + gnu target」下会调用 `rc.exe` 产出 MSVC 格式 `.lib`。
  已**移除该依赖**，`build.rs` 改为 gnu 分支直接调用 `windres -O coff`。

### Fixed — 安全（数据目录 ACL 隔离）

- **缺陷 A**：`icacls /grant:r` 的 `:r` 只替换指定 SID 的 ACE，**不清除其它 SID** ——
  `C:\ProgramData` 继承来的 `BUILTIN\Users:(WD,AD,WEA,WA)` 被原样保留，
  任意本地用户可覆写 `token.txt` 从而**自授权获得 SYSTEM 命令执行权**。
  修复：改用 `/inheritance:r` 断开继承并清除全部继承 ACE。
- **缺陷 B**：`icacls` 参数中裸 SID 会被当**账户名**解析而失败，且**整条命令非零退出、
  一个 ACE 都不写**（grant 全有或全无）。修复：SID 统一加 `*` 前缀，并**逐条独立**调用。
- 上述两缺陷**互相遮蔽**，必须一并修复 —— 单独修 A 会导致空 DACL 使 SYSTEM 自身也无法访问。
- **缺陷 C（服务自降为只读）**：服务以 LocalSystem 启动时调用 `set_dir_acl`，
  旧实现用 `get_user_sid()` 取「当前用户」，而 SYSTEM 身份下 `whoami /user` 返回
  `S-1-5-18`（自己）→ 「授予用户只读」实为把 SYSTEM 覆盖为 `(R)` → 服务无法写数据目录。
  修复：新增纯函数 `select_grant_sid(persisted, current)`，优先读 `sid.txt`（install 权威写入），
  回退当前身份但**硬性拒绝 `S-1-5-18`**。

### Changed — 构建系统

- 仓库迁移至**纯 ASCII 路径**，根治 MinGW `ld`/`dlltool` 按 ANSI 代码页解释含中文产物路径
  导致的「文件存在却找不到」；随之删除外部 `CARGO_TARGET_DIR` + ASCII 前置校验等整套绕行机制。
- 新增 `.cargo/config.toml` 作为**单一事实来源**，承载所有与机器无关的稳定配置；
  构建脚本只保留与机器相关的工作（定位工具链、注入 binutils、调 cargo）。
- 构建脚本改为**向系统自省**而非猜测路径；所有候选路径均经存在性校验后才采用。
- 修复 `build.ps1` 不回写 `CARGO_HOME`/`RUSTUP_HOME` 的缺陷（曾导致与 `build.sh` 使用
  两套 registry 源）。
- `build.sh` 由 212 行精简至约 116 行，`build.ps1` 由 207 行精简至约 145 行。

### Changed — 其他

- **移除全部本机私有路径与身份痕迹**，机器相关配置一律改由环境变量传入
  （`AIROOT_DEPLOY_DIR` / `AIROOT_RUST_ROOT` / `MINGW_BINUTILS_BIN`）。
  部署目录默认值由硬编码改为通用的 `%ProgramData%\AI-Root`。
- `profile.release` 的 `strip` 由 `symbols` 改为 `debuginfo` ——
  `symbols` 下 panic 回溯**完全为空**，排障代价远高于 256 KB 体积收益。

### Added

- 新增 5 个 `handle_client` **端到端测试**（真实驱动读帧→校验→鉴权→执行→回写全链路）。
- 新增 3 个 ACL 安全回归测试，其中 `select_grant_sid` 相关断言覆盖 persisted/current 全组合。
- 新增 `AI-Root tail` 实时事件流查看（支持 `--caller` / `--type` / `--since` 过滤）。
- 新增 `AI-Root update` 的 PE 输入校验（MZ + PE 头 + Machine 类型）。
- 新增 `install-service.ps1` 管理员安装脚本（6 步自检，逐步打印结果）。
- CI 新增 `build-gnu` job，覆盖开发者实际使用的 `build.sh` 路径。

### Removed

- **GUI 控制面板彻底移除**（2026-08-07），改为纯终端版；系统托盘与 `AI-Root-gui.exe` 不再提供。
- 移除 `embed-resource` 构建依赖及其 `vswhom` / `winreg` 传递依赖。

### 测试状态

- 测试数 6 → **57**，全部通过。集成测试使用**真实命名管道**与**真实 `icacls`**，不 mock IO。
- 关键缺陷均经**变异测试**验证（改坏实现必须精确 FAILED）。

## [0.9.0] - 2026-08-28

> `0.9.0` 为 1.0.0 之前的开发里程碑，**未发布过 Release**，故无 tag 链接。

### Added

- **令牌轮转**：`rotate_token()` 支持安全轮换，服务端按文件 mtime 自动热加载。
- **命令安全**：阻止危险命令模式（`format` / `diskpart` 等）执行。
- **输入验证**：`validate_request()` 校验令牌格式、命令长度、工作目录、超时范围。
- **详细审计**：日志包含完整命令执行信息与结构化事件。

### Changed

- **性能**：管道缓冲区 8 KB → 64 KB；减少不必要的克隆与分配。
- **连接控制**:新增 5 分钟最大连接时间，防止资源长期占用。
- **稳定性**：增强进程状态检查、超时处理与错误日志。

### 文档

- 新增项目结构说明、多语言集成示例与优化总结。

---

[Unreleased]: https://github.com/Apr-YT/Ai-root/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Apr-YT/Ai-root/releases/tag/v1.0.0
