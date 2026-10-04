# Changelog

本文件记录 AI-Root 的版本变更。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [1.0.0] — 2026-10-04

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

## [0.9.0] — 2026-08-28

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

[Unreleased]: https://github.com/Apr-YT/ai-root/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Apr-YT/ai-root/releases/tag/v1.0.0
[0.9.0]: https://github.com/Apr-YT/ai-root/releases/tag/v0.9.0
