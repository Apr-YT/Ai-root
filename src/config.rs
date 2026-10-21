//! 配置加载、分层合并、迁移与校验
//!
//! ## 为什么需要这一层（而不是继续用单个 policy.json）
//!
//! 改造前的配置是「12 个编译期常量 + 3 个环境变量 + 一个 JSON 文件」三处散落，
//! 且存在三个实质缺陷（详见 [docs/CONFIG_AUDIT.md](../docs/CONFIG_AUDIT.md)）：
//!
//! - **C-01** 策略结构体没有 `deny_unknown_fields` → 字段拼错静默忽略，配置无法自证；
//! - **C-02** 策略文件被删除时**自动重建宽松默认策略** → 「删配置 = 权限放宽」；
//! - **C-03** 没有版本迁移链 → 结构演进时旧文件被"半正确"解析。
//!
//! ## 三层优先级（后者覆盖前者，但用户层只能收紧）
//!
//! ```text
//! ① 代码默认值      PolicyFile::default_autonomy()
//! ② 用户偏好        %LOCALAPPDATA%\AI-Root\user.toml      ← 当前用户可写
//! ③ 管理员授权策略  %ProgramData%\AI-Root\policy.toml     ← 仅管理员可写（权威）
//!    + 规则目录     %ProgramData%\AI-Root\rules.d\*.toml
//! ```
//!
//! ### 为什么用户层「只能收紧」
//!
//! 用户配置位于 `%LOCALAPPDATA%`，**以安装用户身份运行的 Agent 可以写它**。
//! 如果它能把 `mode` 从 `strict` 改成 `autonomy`、或追加 `rules`，
//! 那就等于「Agent 给自己发授权」，整套策略形同虚设（任务书要求 7）。
//!
//! 因此 `UserConfig` 的结构体里**根本没有** `rules` / `approved_services` /
//! `trusted_publishers` / `allow_legacy_shell` 这些字段，且开了 `deny_unknown_fields`：
//! 写了就报错，不是"忽略"（忽略会让人误以为生效）。
//!
//! 允许用户调整的只有：
//! - `mode`：只能往更严的方向（`strictness` 取 `max`）
//! - `confirm_timeout_secs`：只能调小（`min`）
//! - `default_timeout_ms`：非授权项，可任意（钳制到硬上限）
//! - `color`：终端偏好
//!
//! ## 失败即收紧（fail closed）
//!
//! | 情况 | 处理 |
//! |---|---|
//! | 管理员策略解析失败 / 校验失败 | `locked_down`（只读查询以外全拒） |
//! | 管理员策略**曾初始化**但文件不存在 | `locked_down`（不是重建默认！） |
//! | 任一 `rules.d/*.toml` 解析失败 | `locked_down`（跳过会丢 deny 规则 = 放宽） |
//! | 规则 ID 跨文件重复 | `locked_down` |
//! | 版本高于当前 | `locked_down` |
//! | 版本低于当前 | 显式迁移（先备份） |
//! | 用户配置损坏 / 含禁用字段 | **忽略用户层**（结果只会更严）+ 记录错误 |

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::policy::{LoadState, Mode, Policy, PolicyFile, Rule, LEGACY_POLICY_FILE, POLICY_VERSION};

// ============================================================
// 路径布局
// ============================================================

/// 管理员授权策略（TOML，受 ACL 保护）
pub const POLICY_FILE: &str = "policy.toml";
/// 用户偏好（LOCALAPPDATA，当前用户可写）
pub const USER_CONFIG_FILE: &str = "user.toml";
/// 自动化规则目录（管理员维护，按主题拆分）
pub const RULES_DIR: &str = "rules.d";
/// 秘密目录（令牌 / 安装用户 SID）
pub const SECRETS_DIR: &str = "secrets";
/// 日志目录
pub const LOGS_DIR: &str = "logs";
/// 运行状态目录（启用标志 / 确认请求 / 临时脚本）
pub const STATE_DIR: &str = "state";
/// 「策略已初始化」标记（区分首次安装与事后被删）
pub const POLICY_MARKER: &str = "policy.initialized";
/// 内存布局版本标记
pub const LAYOUT_MARKER: &str = "layout.version";
/// 当前磁盘布局版本
pub const LAYOUT_VERSION: u32 = 2;

/// 路径集合
#[derive(Debug, Clone)]
pub struct Paths {
    /// 机器级目录：`%ProgramData%\AI-Root`（可用 `AIROOT_DATA_DIR` 覆盖，仅测试/隔离用）
    pub machine: PathBuf,
    /// 用户级目录：`%LOCALAPPDATA%\AI-Root`（可用 `AIROOT_USER_DIR` 覆盖，仅测试用）
    pub user: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        Paths {
            machine: machine_dir(),
            user: user_dir(),
        }
    }

    pub fn policy(&self) -> PathBuf {
        self.machine.join(POLICY_FILE)
    }
    pub fn legacy_policy(&self) -> PathBuf {
        self.machine.join(LEGACY_POLICY_FILE)
    }
    pub fn rules_dir(&self) -> PathBuf {
        self.machine.join(RULES_DIR)
    }
    pub fn user_file(&self) -> PathBuf {
        self.user.join(USER_CONFIG_FILE)
    }
    pub fn secrets(&self) -> PathBuf {
        self.machine.join(SECRETS_DIR)
    }
    pub fn logs(&self) -> PathBuf {
        self.machine.join(LOGS_DIR)
    }
    pub fn state(&self) -> PathBuf {
        self.machine.join(STATE_DIR)
    }
    pub fn policy_marker(&self) -> PathBuf {
        self.state().join(POLICY_MARKER)
    }
    pub fn layout_marker(&self) -> PathBuf {
        self.state().join(LAYOUT_MARKER)
    }
}

/// 机器级数据目录（`%ProgramData%\AI-Root`）
pub fn machine_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIROOT_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    Path::new(&base).join("AI-Root")
}

/// 用户级配置目录（`%LOCALAPPDATA%\AI-Root`）
///
/// 为什么用 LOCALAPPDATA 而不是 APPDATA：偏好配置不需要漫游，
/// 且漫游目录在域环境下可能被策略重定向/同步，不适合放"只影响本机行为"的偏好。
pub fn user_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIROOT_USER_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        if !base.trim().is_empty() {
            return Path::new(&base).join("AI-Root");
        }
    }
    // 兜底：%USERPROFILE%\AppData\Local
    if let Ok(home) = std::env::var("USERPROFILE") {
        return Path::new(&home).join("AppData").join("Local").join("AI-Root");
    }
    PathBuf::from("C:\\ProgramData\\AI-Root\\user-fallback")
}

// ============================================================
// 用户配置
// ============================================================

/// 用户偏好（`%LOCALAPPDATA%\AI-Root\user.toml`）。
///
/// ⚠ 这里**故意**不包含任何能放宽授权的字段。见模块文档。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// 配置结构版本（缺省视为当前）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// 运行模式：只能比管理员策略更严
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// 命令默认超时（毫秒），钳制到 [1000, MAX_COMMAND_TIMEOUT_MS]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
    /// 确认请求有效期（秒）：只能比管理员策略更短
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_timeout_secs: Option<u64>,
}

impl UserConfig {
    /// 生效的默认超时（钳制到硬上限）
    pub fn effective_timeout_ms(&self) -> Option<u64> {
        self.default_timeout_ms
            .map(|t| t.clamp(1_000, crate::common::MAX_COMMAND_TIMEOUT_MS))
    }
}

/// 单个 `rules.d/*.toml` 文件的结构
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesFile {
    #[serde(default, rename = "rule")]
    pub rule: Vec<Rule>,
}

// ============================================================
// 加载报告
// ============================================================

/// 单层加载状态
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LayerState {
    /// 文件不存在（首次或未配置）
    #[default]
    Missing,
    /// 正常加载
    Loaded,
    /// 本次加载时创建了默认文件
    Created,
    /// 加载失败（已按 fail-closed 规则处理）
    Error(String),
    /// 由旧格式迁移而来
    Migrated(String),
}

impl LayerState {
    pub fn ok(&self) -> bool {
        matches!(
            self,
            LayerState::Loaded | LayerState::Created | LayerState::Migrated(_)
        )
    }

    pub fn label(&self) -> String {
        match self {
            LayerState::Missing => "不存在".into(),
            LayerState::Loaded => "已加载".into(),
            LayerState::Created => "已创建默认".into(),
            LayerState::Migrated(s) => format!("已迁移（{s}）"),
            LayerState::Error(e) => format!("错误：{e}"),
        }
    }
}

/// 各层状态（供 `AI-Root config check` 展示）
#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub admin: LayerState,
    pub rules: Vec<(String, LayerState)>,
    pub user: LayerState,
}

/// 一次完整加载的结果
#[derive(Debug, Clone)]
pub struct Effective {
    /// 合并后的最终策略（含状态）
    pub policy: Policy,
    pub layers: Layers,
    pub user: Option<UserConfig>,
    /// 合并过程中的说明（被忽略的宽松项、迁移动作等）
    pub notes: Vec<String>,
}

// ============================================================
// 加载
// ============================================================

/// 加载并合并三层配置（服务端唯一入口）
pub fn load(paths: &Paths) -> Effective {
    let mut layers = Layers::default();
    let mut notes: Vec<String> = Vec::new();
    let mut state = LoadState::Loaded;
    let mut file = PolicyFile::default_autonomy();

    // ── 管理员策略（权威层）──
    match load_admin(paths, &mut layers, &mut notes) {
        Ok(Some(f)) => file = f,
        Ok(None) => {
            // 文件不存在：区分「首次」与「被删除」
            if paths.policy_marker().exists() {
                let msg = format!(
                    "策略文件缺失但存在初始化标记（{}）。\
                     这通常意味着策略被误删或被清理工具删除 —— 拒绝回落到默认策略（那会放宽权限）。\
                     恢复方式：从备份恢复该文件，或删除标记后执行 `AI-Root config init` 重新生成。",
                    paths.policy_marker().display()
                );
                layers.admin = LayerState::Error(msg.clone());
                file = PolicyFile::locked_down();
                state = LoadState::Degraded(msg);
            } else {
                match write_default_policy(paths) {
                    Ok(()) => {
                        layers.admin = LayerState::Created;
                        state = LoadState::CreatedDefault;
                        notes.push(format!("已生成默认策略：{}", paths.policy().display()));
                    }
                    Err(e) => {
                        // 写不出默认策略：内存里仍用默认（服务可启动），但明确记录
                        layers.admin = LayerState::Error(format!("写入默认策略失败: {e}"));
                        notes.push("无法写入默认策略文件，本次使用内存默认值（重启后可能不同）".into());
                        state = LoadState::CreatedDefault;
                    }
                }
            }
        }
        Err(e) => {
            layers.admin = LayerState::Error(e.clone());
            file = PolicyFile::locked_down();
            state = LoadState::Degraded(e);
        }
    }

    // ── 用户偏好（只能收紧）──
    match load_user(paths) {
        Ok(Some(u)) => {
            let before = (file.mode, file.confirm_timeout_secs);
            apply_user(&mut file, &u, &mut notes);
            layers.user = LayerState::Loaded;
            notes.push(format!(
                "用户配置已应用（mode {}→{}，confirm {}s→{}s）",
                before.0.as_str(),
                file.mode.as_str(),
                before.1,
                file.confirm_timeout_secs
            ));
            Effective {
                policy: Policy { file, state },
                layers,
                user: Some(u),
                notes,
            }
        }
        Ok(None) => {
            layers.user = LayerState::Missing;
            Effective {
                policy: Policy { file, state },
                layers,
                user: None,
                notes,
            }
        }
        Err(e) => {
            // 用户配置错误 → 忽略用户层（只会更严，不会放宽），但必须可见
            layers.user = LayerState::Error(e.clone());
            notes.push(format!("用户配置已忽略（解析失败，保持更严的管理员策略）：{e}"));
            Effective {
                policy: Policy { file, state },
                layers,
                user: None,
                notes,
            }
        }
    }
}

/// 只读加载：不创建/不迁移任何文件（供 `config show` / `signature` 等展示命令）
pub fn load_readonly(paths: &Paths) -> Effective {
    let mut layers = Layers::default();
    let mut notes: Vec<String> = Vec::new();
    let mut state = LoadState::Loaded;
    let mut file = PolicyFile::default_autonomy();

    if paths.policy().exists() {
        match parse_policy_file(&paths.policy()) {
            Ok(f) => {
                layers.admin = LayerState::Loaded;
                file = f;
            }
            Err(e) => {
                layers.admin = LayerState::Error(e.clone());
                file = PolicyFile::locked_down();
                state = LoadState::Degraded(e);
            }
        }
        if let Err(e) = merge_rules(paths, &mut file, &mut layers) {
            layers.admin = LayerState::Error(e.clone());
            file = PolicyFile::locked_down();
            state = LoadState::Degraded(e);
        }
    } else if paths.policy_marker().exists() {
        let msg = "策略文件缺失但存在初始化标记（拒绝回落默认）".to_string();
        layers.admin = LayerState::Error(msg.clone());
        file = PolicyFile::locked_down();
        state = LoadState::Degraded(msg);
    } else {
        layers.admin = LayerState::Missing;
        notes.push("策略尚未初始化（首次 install 或 `AI-Root config init` 后生成）".into());
    }

    match load_user(paths) {
        Ok(Some(u)) => {
            apply_user(&mut file, &u, &mut notes);
            layers.user = LayerState::Loaded;
            notes.push(fmt_user_applied(&u));
            Effective {
                policy: Policy { file, state },
                layers,
                user: Some(u),
                notes,
            }
        }
        Ok(None) => {
            layers.user = LayerState::Missing;
            Effective {
                policy: Policy { file, state },
                layers,
                user: None,
                notes,
            }
        }
        Err(e) => {
            layers.user = LayerState::Error(e.clone());
            notes.push(format!("用户配置已忽略：{e}"));
            Effective {
                policy: Policy { file, state },
                layers,
                user: None,
                notes,
            }
        }
    }
}

fn fmt_user_applied(u: &UserConfig) -> String {
    let mut parts = vec!["用户配置已应用".to_string()];
    if let Some(m) = u.mode {
        parts.push(format!("mode={}", m.as_str()));
    }
    if let Some(t) = u.confirm_timeout_secs {
        parts.push(format!("confirm_timeout_secs={t}"));
    }
    if let Some(t) = u.effective_timeout_ms() {
        parts.push(format!("default_timeout_ms={t}"));
    }
    parts.join("，")
}

/// 加载管理员策略 + 规则目录。返回 `Ok(None)` 表示文件不存在。
fn load_admin(
    paths: &Paths,
    layers: &mut Layers,
    notes: &mut Vec<String>,
) -> Result<Option<PolicyFile>, String> {
    let policy_path = paths.policy();

    let mut file = if policy_path.exists() {
        let f = parse_policy_file(&policy_path)?;
        layers.admin = LayerState::Loaded;
        f
    } else if paths.legacy_policy().exists() {
        // 旧 JSON → 迁移到 TOML
        let f = migrate_legacy_json(paths)?;
        layers.admin = LayerState::Migrated(format!(
            "{} → {}",
            paths.legacy_policy().display(),
            policy_path.display()
        ));
        notes.push(format!(
            "已把旧策略 {} 迁移为 {}（原文件备份在 {}）",
            paths.legacy_policy().display(),
            policy_path.display(),
            paths.state().join("legacy").display()
        ));
        f
    } else {
        return Ok(None);
    };

    merge_rules(paths, &mut file, layers)?;

    // 初始化标记：即使本次是迁移来的，也标记为「已初始化」
    if let Err(e) = ensure_marker(&paths.policy_marker()) {
        notes.push(format!("写初始化标记失败（下次删除策略将无法识别）: {e}"));
    }

    Ok(Some(file))
}

/// 解析 TOML 策略文件并校验
fn parse_policy_file(path: &Path) -> Result<PolicyFile, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let mut f: PolicyFile = toml::from_str(&raw)
        .map_err(|e| format!("解析 {} 失败（TOML/字段校验）: {e}", path.display()))?;

    if f.version > POLICY_VERSION {
        return Err(format!(
            "策略版本 {} 高于本程序支持的 {POLICY_VERSION}，拒绝猜测语义（请升级 AI-Root）",
            f.version
        ));
    }
    if f.version < POLICY_VERSION {
        // v1 → v2 是**字段超集**关系（v2 只增加「规则可拆到 rules.d」与严格校验），
        // 因此读到 v1 的 TOML 可以按 v2 语义安全解释，不需要改写用户文件。
        // 这与 JSON→TOML 的文件级迁移是两件事：后者涉及目录布局，必须显式执行。
        f.version = POLICY_VERSION;
    }
    f.validate()?;
    Ok(f)
}

/// 合并 `rules.d/*.toml`（按文件名字典序，确定性）
fn merge_rules(
    paths: &Paths,
    file: &mut PolicyFile,
    layers: &mut Layers,
) -> Result<(), String> {
    let dir = paths.rules_dir();
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("读取规则目录 {} 失败: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| x.eq_ignore_ascii_case("toml"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();

    for path in entries {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("读取规则文件 {name} 失败: {e}"))?;
        // ⚠ 任一规则文件解析失败 → 整体失败。
        // 不能「跳过这个文件继续」：被跳过的可能是 deny 规则，等于把禁止项放开。
        let parsed: RulesFile = toml::from_str(&raw)
            .map_err(|e| format!("解析规则文件 {name} 失败（跳过会导致规则丢失，故整体拒绝）: {e}"))?;

        for r in parsed.rule {
            if r.id.trim().is_empty() {
                return Err(format!("规则文件 {name} 中存在空 id"));
            }
            if file.rules.iter().any(|x| x.id == r.id) {
                return Err(format!(
                    "规则 id 重复：`{}`（出现于 {name}，或与该文件内的另一条规则冲突）",
                    r.id
                ));
            }
            file.rules.push(r);
        }
        layers.rules.push((name, LayerState::Loaded));
    }

    // 合并后重新校验（跨文件规则也可能违反「allow 规则过于宽泛」等约束）
    file.validate().map_err(|e| format!("合并规则后校验失败: {e}"))?;
    Ok(())
}

/// 加载用户配置
fn load_user(paths: &Paths) -> Result<Option<UserConfig>, String> {
    let p = paths.user_file();
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p)
        .map_err(|e| format!("读取 {} 失败: {e}", p.display()))?;
    let u: UserConfig = toml::from_str(&raw)
        .map_err(|e| format!("解析 {} 失败（含未授权字段也会在此报错）: {e}", p.display()))?;
    if let Some(v) = u.version {
        if v > POLICY_VERSION {
            return Err(format!(
                "用户配置版本 {v} 高于支持的 {POLICY_VERSION}"
            ));
        }
    }
    Ok(Some(u))
}

// ============================================================
// 合并规则（纯函数，便于测试）
// ============================================================

/// 模式严格度：数值越大越严
pub fn strictness(m: Mode) -> u8 {
    match m {
        Mode::Autonomy => 0,
        Mode::Balanced => 1,
        Mode::Strict => 2,
        Mode::Disabled => 3,
    }
}

/// 把用户偏好合并进策略，**只允许收紧**。
///
/// 这是整个分层设计的核心不变量，独立成纯函数以便完整覆盖测试。
pub fn apply_user(file: &mut PolicyFile, user: &UserConfig, notes: &mut Vec<String>) {
    if let Some(m) = user.mode {
        let admin_rank = strictness(file.mode);
        let user_rank = strictness(m);
        if user_rank > admin_rank {
            notes.push(format!(
                "用户配置收紧模式：{} → {}",
                file.mode.as_str(),
                m.as_str()
            ));
            file.mode = m;
        } else if user_rank < admin_rank {
            notes.push(format!(
                "用户配置的 mode={} 比管理员策略的 {} 更宽松，已忽略（用户层只能收紧）",
                m.as_str(),
                file.mode.as_str()
            ));
        }
    }

    if let Some(t) = user.confirm_timeout_secs {
        let t = t.clamp(30, 3600);
        if t < file.confirm_timeout_secs {
            notes.push(format!(
                "用户配置缩短确认有效期：{}s → {t}s",
                file.confirm_timeout_secs
            ));
            file.confirm_timeout_secs = t;
        } else if t > file.confirm_timeout_secs {
            notes.push(format!(
                "用户配置的 confirm_timeout_secs={t} 比管理员策略的 {}s 更宽松，已忽略",
                file.confirm_timeout_secs
            ));
        }
    }
}

// ============================================================
// 写入 / 迁移
// ============================================================

/// 写入默认策略（TOML + 文件头说明）
pub fn write_default_policy(paths: &Paths) -> Result<(), String> {
    std::fs::create_dir_all(&paths.machine)
        .map_err(|e| format!("创建目录 {} 失败: {e}", paths.machine.display()))?;
    let file = PolicyFile::default_autonomy();
    write_policy(paths, &file)?;
    ensure_marker(&paths.policy_marker())?;
    Ok(())
}

/// 序列化并写策略文件（原子写：先写 .tmp 再 rename）
pub fn write_policy(paths: &Paths, file: &PolicyFile) -> Result<(), String> {
    let header = "\
# AI-Root 授权策略（管理员维护）
#
# 位置：%ProgramData%\\AI-Root\\policy.toml
# 权限：SYSTEM:F / Administrators:F / 安装用户:R —— 普通 Agent 只能读
#
# 判定顺序：内置保护 → deny 规则 → approved_services → allow 规则 → confirm 规则 → default_decision
# 规则也可拆到 rules.d\\*.toml（推荐，便于按主题维护）
#
# 改完执行 `AI-Root config check` 校验，无需重启服务（下次请求即生效）。
";
    let body = toml::to_string_pretty(file).map_err(|e| format!("序列化策略失败: {e}"))?;
    atomic_write(&paths.policy(), format!("{header}\n{body}").as_bytes())
}

/// 把发布者指纹写入信任根（管理员操作）
pub fn add_trusted_publisher(paths: &Paths, thumbprint: &str) -> Result<String, String> {
    let t: String = thumbprint
        .chars()
        .filter(|c| !matches!(c, ' ' | ':' | '-'))
        .collect::<String>()
        .to_uppercase();
    if t.len() != 64 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "指纹格式无效：需要 64 位 hex（证书 SHA-256），实际 {} 位",
            t.len()
        ));
    }

    let mut file = if paths.policy().exists() {
        parse_policy_file(&paths.policy())?
    } else if !paths.policy_marker().exists() {
        write_default_policy(paths)?;
        parse_policy_file(&paths.policy())?
    } else {
        return Err("策略文件缺失（曾初始化）。请先恢复策略再添加信任根".into());
    };

    if file.trusted_publishers.iter().any(|p| p.eq_ignore_ascii_case(&t)) {
        return Ok(t);
    }
    file.trusted_publishers.push(t.clone());
    file.validate()?;
    write_policy(paths, &file)?;
    Ok(t)
}

/// 旧 JSON 策略 → TOML（备份原文件）
fn migrate_legacy_json(paths: &Paths) -> Result<PolicyFile, String> {
    let legacy = paths.legacy_policy();
    let raw = std::fs::read_to_string(&legacy)
        .map_err(|e| format!("读取旧策略 {} 失败: {e}", legacy.display()))?;
    let mut f: PolicyFile = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "解析旧 JSON 策略失败: {e}\n\
             提示：旧文件的字段名必须是 v1 格式（rules / mode / trusted_publishers …）"
        )
    })?;

    if f.version > POLICY_VERSION {
        return Err(format!(
            "旧策略版本 {} 高于当前 {POLICY_VERSION}",
            f.version
        ));
    }
    f.version = POLICY_VERSION;
    f.validate()?;

    // 先备份，再写新文件（顺序不可颠倒：写失败时至少原文件还在）
    let backup_dir = paths.state().join("legacy");
    std::fs::create_dir_all(&backup_dir)
        .map_err(|e| format!("创建备份目录 {} 失败: {e}", backup_dir.display()))?;
    std::fs::copy(&legacy, backup_dir.join(LEGACY_POLICY_FILE))
        .map_err(|e| format!("备份旧策略失败: {e}"))?;

    write_policy(paths, &f)?;

    // 校验新文件能被读回，再删除旧文件（避免"迁移后两边都不对"）
    if let Err(e) = parse_policy_file(&paths.policy()) {
        return Err(format!("迁移后校验新策略失败（已保留旧文件）: {e}"));
    }
    let _ = std::fs::remove_file(&legacy);
    Ok(f)
}

/// 原子写：同目录 `.tmp` → rename。避免编辑器/进程被杀时留下半截文件。
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败: {e}", path.display())
    })
}

fn ensure_marker(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    }
    std::fs::write(path, b"1").map_err(|e| format!("写 {} 失败: {e}", path.display()))
}

// ============================================================
// 磁盘布局迁移（v1.1 扁平布局 → v1.2 分层布局）
// ============================================================

/// 执行布局迁移。返回已执行的动作列表。
///
/// 原则：
/// - **不覆盖**已存在的目标文件（宁可跳过并记录，也不丢数据）
/// - 任一移动失败 → 返回 `Err`，调用方应拒绝启动（fail closed）
/// - 旧文件在被确认迁移成功前不删除
pub fn migrate_layout(paths: &Paths) -> Result<Vec<String>, String> {
    let mut done = vec![];

    std::fs::create_dir_all(paths.secrets())
        .map_err(|e| format!("创建 secrets 目录失败: {e}"))?;
    std::fs::create_dir_all(paths.logs()).map_err(|e| format!("创建 logs 目录失败: {e}"))?;
    std::fs::create_dir_all(paths.state()).map_err(|e| format!("创建 state 目录失败: {e}"))?;

    // 秘密
    move_file(
        &paths.machine.join("token.txt"),
        &paths.secrets().join("token.txt"),
        "令牌",
        &mut done,
    )?;
    // 状态
    move_file(
        &paths.machine.join("sid.txt"),
        &paths.state().join("sid.txt"),
        "安装用户 SID",
        &mut done,
    )?;
    move_file(
        &paths.machine.join("enabled.flag"),
        &paths.state().join("enabled.flag"),
        "启用标志",
        &mut done,
    )?;
    // 日志
    move_file(
        &paths.machine.join("events.jsonl"),
        &paths.logs().join("events.jsonl"),
        "事件流",
        &mut done,
    )?;
    move_file(
        &paths.machine.join("exec.log"),
        &paths.logs().join("exec.log"),
        "审计日志",
        &mut done,
    )?;
    // 目录
    move_dir(
        &paths.machine.join("pending"),
        &paths.state().join("pending"),
        "确认请求目录",
        &mut done,
    )?;
    move_dir(
        &paths.machine.join("tmp"),
        &paths.state().join("tmp"),
        "临时脚本目录",
        &mut done,
    )?;

    // 策略：旧 JSON → 新 TOML（若新文件已存在则不迁移）
    if paths.legacy_policy().exists() && !paths.policy().exists() {
        migrate_legacy_json(paths)?;
        done.push(format!(
            "策略 {} → {}（原文件备份至 state/legacy/）",
            paths.legacy_policy().display(),
            paths.policy().display()
        ));
        ensure_marker(&paths.policy_marker())?;
    }

    std::fs::write(paths.layout_marker(), LAYOUT_VERSION.to_string())
        .map_err(|e| format!("写布局版本标记失败: {e}"))?;
    done.push(format!("布局版本标记 → {LAYOUT_VERSION}"));
    Ok(done)
}

fn move_file(src: &Path, dst: &Path, what: &str, done: &mut Vec<String>) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    if dst.exists() {
        done.push(format!("{what}：目标已存在，跳过（源文件保留在 {}）", src.display()));
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    }
    std::fs::rename(src, dst).or_else(|_| {
        // 跨卷 rename 会失败：退化为 copy + 校验 + 删除
        std::fs::copy(src, dst)?;
        let a = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let b = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        if a != b || a == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "复制后长度不一致",
            ));
        }
        std::fs::remove_file(src)
    })
    .map_err(|e| format!("迁移{what} {} → {} 失败: {e}", src.display(), dst.display()))?;
    done.push(format!("{what} → {}", dst.display()));
    Ok(())
}

fn move_dir(src: &Path, dst: &Path, what: &str, done: &mut Vec<String>) -> Result<(), String> {
    if !src.is_dir() {
        return Ok(());
    }
    if dst.exists() {
        done.push(format!(
            "{what}：目标已存在，跳过（源目录保留在 {}）",
            src.display()
        ));
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    }
    std::fs::rename(src, dst).map_err(|e| {
        format!("迁移{what} {} → {} 失败: {e}", src.display(), dst.display())
    })?;
    done.push(format!("{what} → {}", dst.display()));
    Ok(())
}

/// 是否需要布局迁移（旧扁平布局存在且未标记新版本）
pub fn needs_layout_migration(paths: &Paths) -> bool {
    let marked = std::fs::read_to_string(paths.layout_marker())
        .map(|s| s.trim().parse::<u32>().unwrap_or(0) >= LAYOUT_VERSION)
        .unwrap_or(false);
    if marked {
        return false;
    }
    // 旧布局特征
    paths.machine.join("token.txt").exists()
        || paths.legacy_policy().exists()
        || paths.machine.join("events.jsonl").exists()
        || paths.machine.join("enabled.flag").exists()
}

// ============================================================
// 带指纹的缓存加载（服务端每请求调用）
// ============================================================

use std::sync::Mutex;
use std::time::SystemTime;

/// 上次加载的指纹与结果。指纹覆盖所有可能影响结果的输入（文件 mtime + 大小）。
static CACHE: Mutex<Option<(u64, Effective)>> = Mutex::new(None);

/// 计算配置指纹：策略 / 用户配置 / 规则目录内每个文件的 (mtime, size)。
///
/// 为什么需要：`server.rs` 每个请求都会取一次策略（保证改配置即生效）。
/// 无缓存时一次请求要读 1~N 个文件并解析；有缓存时通常只做几次 `metadata`。
/// 指纹任一变化即重新加载，因此**不牺牲热生效**。
///
/// 目录本身也进指纹：文件新增/删除会改变目录 mtime（内容修改不会，所以文件也要进）。
fn fingerprint(paths: &Paths) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    let mut feed = |p: &Path| {
        p.to_string_lossy().hash(&mut h);
        match std::fs::metadata(p) {
            Ok(m) => {
                let mt = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                mt.hash(&mut h);
                m.len().hash(&mut h);
            }
            Err(_) => 0u8.hash(&mut h),
        }
    };

    feed(&paths.policy());
    feed(&paths.legacy_policy());
    feed(&paths.user_file());
    feed(&paths.policy_marker());

    let dir = paths.rules_dir();
    feed(&dir);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map(|x| x.eq_ignore_ascii_case("toml"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        for f in files {
            feed(&f);
        }
    }
    h.finish()
}

/// 带缓存的加载（服务端入口）。配置未变时复用上次结果。
pub fn load_cached(paths: &Paths) -> Effective {
    let fp = fingerprint(paths);
    if let Ok(g) = CACHE.lock() {
        if let Some((cached_fp, eff)) = g.as_ref() {
            if *cached_fp == fp {
                return eff.clone();
            }
        }
    }
    let eff = load(paths);
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((fp, eff.clone()));
    }
    eff
}

/// 仅读取用户偏好（CLI 用来取默认超时，不必加载整棵策略/规则）
pub fn user_prefs() -> Option<UserConfig> {
    load_user(&Paths::resolve()).ok().flatten()
}

/// 清空缓存（写配置后调用，避免同进程内读到旧值）
pub fn invalidate_cache() {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;
    use crate::policy::{Decision, Mode, OpKind};

    /// 构造完全隔离的路径集合（不依赖进程级环境变量，避免并行测试互相干扰）
    fn tmp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!("airoot-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("machine")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Paths {
            machine: base.join("machine"),
            user: base.join("user"),
        }
    }

    fn write_rules(paths: &Paths, name: &str, body: &str) {
        std::fs::create_dir_all(paths.rules_dir()).unwrap();
        std::fs::write(paths.rules_dir().join(name), body).unwrap();
    }

    // ---------- 默认生成 / 删除后不重建（C-02 回归）----------

    #[test]
    fn first_load_creates_default_policy_and_marker() {
        let paths = tmp_paths("first");
        let eff = load(&paths);
        assert_eq!(eff.policy.state, LoadState::CreatedDefault);
        assert!(paths.policy().exists(), "应写出 policy.toml");
        assert!(paths.policy_marker().exists(), "应写初始化标记");
        assert_eq!(eff.policy.file.mode, Mode::Autonomy);
        // 生成的文件必须能被读回（TOML 序列化/反序列化自洽）
        let again = load(&paths);
        assert_eq!(again.policy.state, LoadState::Loaded);
        assert_eq!(again.policy.file.rules.len(), eff.policy.file.rules.len());
    }

    #[test]
    fn deleted_policy_after_init_is_locked_down_not_recreated() {
        let paths = tmp_paths("deleted");
        write_default_policy(&paths).expect("初始化");
        std::fs::remove_file(paths.policy()).expect("模拟误删");

        let eff = load(&paths);
        assert!(
            eff.policy.is_degraded(),
            "策略被删除必须降级，不能回落默认（否则=删配置就放宽权限）"
        );
        assert!(
            !paths.policy().exists(),
            "不得自动重建默认策略文件，否则下次请求又会用宽松默认"
        );
        // 降级后仍允许只读查询（保证可自查）
        let act = crate::policy::Action::Op {
            op: OpKind::QuerySystemInfo,
            target: None,
        };
        assert_eq!(eff.policy.evaluate(&act, &[]).decision, Decision::Allow);
    }

    // ---------- 严格字段（C-01 回归）----------

    #[test]
    fn unknown_field_in_policy_is_rejected() {
        let paths = tmp_paths("unknown");
        std::fs::write(
            paths.policy(),
            "version = 2\nmode = \"autonomy\"\ntrusted_publisher = []\n",
        )
        .unwrap();
        let eff = load(&paths);
        assert!(eff.policy.is_degraded(), "拼错字段必须报错而不是静默忽略");
        match &eff.layers.admin {
            LayerState::Error(e) => assert!(
                e.contains("unknown field") || e.contains("trusted_publisher"),
                "错误信息应指出未知字段: {e}"
            ),
            other => panic!("应为错误状态，实际 {other:?}"),
        }
    }

    #[test]
    fn unknown_field_in_rule_is_rejected() {
        let paths = tmp_paths("unknown-rule");
        write_default_policy(&paths).unwrap();
        write_rules(
            &paths,
            "10-bad.toml",
            "[[rule]]\nid = \"x\"\ndecision = \"allow\"\nop = \"run_argv\"\nprogram = \"git\"\narg_prefixes = [\"status\"]\n",
        );
        let eff = load(&paths);
        assert!(eff.policy.is_degraded(), "规则里拼错字段也必须报错");
    }

    // ---------- 规则目录 ----------

    #[test]
    fn rules_dir_merges_in_filename_order() {
        let paths = tmp_paths("rules-merge");
        write_default_policy(&paths).unwrap();
        let before = load(&paths).policy.file.rules.len();

        write_rules(
            &paths,
            "20-second.toml",
            "[[rule]]\nid = \"zz-second\"\ndecision = \"allow\"\nop = \"run_argv\"\nprogram = \"second\"\n",
        );
        write_rules(
            &paths,
            "10-first.toml",
            "[[rule]]\nid = \"aa-first\"\ndecision = \"allow\"\nop = \"run_argv\"\nprogram = \"first\"\n",
        );

        let eff = load(&paths);
        assert_eq!(eff.policy.file.rules.len(), before + 2);
        let ids: Vec<&str> = eff.policy.file.rules.iter().map(|r| r.id.as_str()).collect();
        let pos_first = ids.iter().position(|x| *x == "aa-first").unwrap();
        let pos_second = ids.iter().position(|x| *x == "zz-second").unwrap();
        assert!(pos_first < pos_second, "规则应按文件名字典序合并");
        assert_eq!(eff.layers.rules.len(), 2);
        assert!(eff.layers.rules.iter().all(|(_, s)| s.ok()));
    }

    #[test]
    fn broken_rule_file_locks_down_entire_policy() {
        let paths = tmp_paths("rules-broken");
        write_default_policy(&paths).unwrap();
        std::fs::create_dir_all(paths.rules_dir()).unwrap();
        std::fs::write(paths.rules_dir().join("10-bad.toml"), "not = [valid toml").unwrap();

        let eff = load(&paths);
        assert!(
            eff.policy.is_degraded(),
            "单个规则文件损坏必须整体降级（跳过它会丢失 deny 规则 = 放宽）"
        );
        assert_eq!(eff.policy.file.mode, Mode::Disabled);
    }

    #[test]
    fn duplicate_rule_id_across_files_locks_down() {
        let paths = tmp_paths("rules-dup");
        write_default_policy(&paths).unwrap();
        // 默认策略里已有 allow-dev-git
        write_rules(
            &paths,
            "10-dup.toml",
            "[[rule]]\nid = \"allow-dev-git\"\ndecision = \"deny\"\nop = \"run_argv\"\nprogram = \"git\"\n",
        );
        let eff = load(&paths);
        assert!(
            eff.policy.is_degraded(),
            "跨文件重复 ID 必须拒绝（否则 deny 可能被同名 allow 覆盖）"
        );
    }

    // ---------- 用户层：只能收紧 ----------

    #[test]
    fn user_mode_can_tighten_autonomy_to_strict() {
        let paths = tmp_paths("user-tighten");
        write_default_policy(&paths).unwrap();
        std::fs::write(paths.user_file(), "version = 2\nmode = \"strict\"\n").unwrap();

        let eff = load(&paths);
        assert_eq!(eff.policy.file.mode, Mode::Strict);
        assert!(eff.layers.user.ok());
        assert!(eff.notes.iter().any(|n| n.contains("收紧")));
    }

    #[test]
    fn user_mode_cannot_relax_strict() {
        let paths = tmp_paths("user-relax");
        let mut f = PolicyFile::default_autonomy();
        f.mode = Mode::Strict;
        write_policy(&paths, &f).unwrap();
        std::fs::write(paths.user_file(), "version = 2\nmode = \"autonomy\"\n").unwrap();

        let eff = load(&paths);
        assert_eq!(
            eff.policy.file.mode,
            Mode::Strict,
            "用户层不能放宽管理员的 strict"
        );
        assert!(eff.notes.iter().any(|n| n.contains("更宽松，已忽略")));
    }

    #[test]
    fn user_confirm_timeout_can_only_shrink() {
        let paths = tmp_paths("user-timeout");
        let mut f = PolicyFile::default_autonomy();
        f.confirm_timeout_secs = 120;
        write_policy(&paths, &f).unwrap();

        std::fs::write(paths.user_file(), "version = 2\nconfirm_timeout_secs = 600\n").unwrap();
        assert_eq!(
            load(&paths).policy.confirm_timeout_secs(),
            120,
            "更长（更宽松）的用户值必须被忽略"
        );

        std::fs::write(paths.user_file(), "version = 2\nconfirm_timeout_secs = 45\n").unwrap();
        assert_eq!(
            load(&paths).policy.confirm_timeout_secs(),
            45,
            "更短的用户值应生效"
        );
    }

    #[test]
    fn user_config_forbidden_field_is_rejected_and_ignored() {
        let paths = tmp_paths("user-forbidden");
        let mut f = PolicyFile::default_autonomy();
        f.mode = Mode::Strict;
        write_policy(&paths, &f).unwrap();

        // 试图给自己加服务白名单 → 用户配置里根本没有这个字段
        std::fs::write(
            paths.user_file(),
            "version = 2\napproved_services = [\"WinDefend\"]\n",
        )
        .unwrap();

        let eff = load(&paths);
        match &eff.layers.user {
            LayerState::Error(e) => {
                assert!(e.contains("approved_services"), "错误应指出非法字段: {e}")
            }
            other => panic!("应报错，实际 {other:?}"),
        }
        // 关键：用户层被忽略后，管理员策略仍然完整生效（而不是回落默认）
        assert_eq!(eff.policy.file.mode, Mode::Strict);
        assert!(eff.policy.file.approved_services.is_empty());
    }

    #[test]
    fn user_config_error_does_not_widen_permissions() {
        let paths = tmp_paths("user-broken");
        let mut f = PolicyFile::default_autonomy();
        f.mode = Mode::Strict;
        f.approved_services = vec!["Spooler".into()];
        write_policy(&paths, &f).unwrap();
        std::fs::write(paths.user_file(), "mode = \"autonomy\"\n{{{{").unwrap();

        let eff = load(&paths);
        assert_eq!(eff.policy.file.mode, Mode::Strict, "坏用户配置不得放宽");
        assert_eq!(eff.policy.file.approved_services, vec!["Spooler".to_string()]);
    }

    #[test]
    fn user_timeout_is_clamped() {
        let u = UserConfig {
            default_timeout_ms: Some(99_999_999),
            ..Default::default()
        };
        assert_eq!(
            u.effective_timeout_ms(),
            Some(crate::common::MAX_COMMAND_TIMEOUT_MS)
        );
        let u2 = UserConfig {
            default_timeout_ms: Some(1),
            ..Default::default()
        };
        assert_eq!(u2.effective_timeout_ms(), Some(1_000));
    }

    // ---------- 版本 ----------

    #[test]
    fn future_policy_version_is_rejected() {
        let paths = tmp_paths("future");
        let mut f = PolicyFile::default_autonomy();
        f.version = POLICY_VERSION + 1;
        std::fs::write(paths.policy(), toml::to_string(&f).unwrap()).unwrap();
        assert!(load(&paths).policy.is_degraded(), "未来版本必须拒绝");
    }

    #[test]
    fn v1_toml_is_read_as_v2() {
        let paths = tmp_paths("v1toml");
        let mut f = PolicyFile::default_autonomy();
        f.version = 1;
        std::fs::write(paths.policy(), toml::to_string(&f).unwrap()).unwrap();

        let eff = load(&paths);
        assert!(
            !eff.policy.is_degraded(),
            "v1 TOML 字段是 v2 的子集，应可安全读取"
        );
        assert_eq!(eff.policy.file.version, POLICY_VERSION);
    }

    // ---------- 旧 JSON 迁移 ----------

    #[test]
    fn legacy_json_is_migrated_to_toml_with_backup() {
        let paths = tmp_paths("legacy");
        let legacy = serde_json::json!({
            "version": 1,
            "mode": "strict",
            "default_decision": "confirm",
            "allow_legacy_shell": false,
            "confirm_timeout_secs": 120,
            "approved_services": ["Spooler"],
            "trusted_publishers": [],
            "rules": [],
        });
        std::fs::write(paths.legacy_policy(), legacy.to_string()).unwrap();

        let eff = load(&paths);
        assert!(!eff.policy.is_degraded(), "迁移应成功: {:?}", eff.layers.admin);
        assert_eq!(eff.policy.file.mode, Mode::Strict);
        assert_eq!(eff.policy.file.approved_services, vec!["Spooler".to_string()]);
        assert!(paths.policy().exists(), "应生成 policy.toml");
        assert!(
            !paths.legacy_policy().exists(),
            "迁移成功后旧 JSON 应被移除（已备份）"
        );
        assert!(
            paths.state().join("legacy").join(LEGACY_POLICY_FILE).exists(),
            "迁移前必须备份原文件"
        );
        assert!(paths.policy_marker().exists());
    }

    // ---------- 布局迁移 ----------

    #[test]
    fn layout_migration_moves_secrets_and_logs() {
        let paths = tmp_paths("layout");
        std::fs::write(paths.machine.join("token.txt"), "tok").unwrap();
        std::fs::write(paths.machine.join("enabled.flag"), "1").unwrap();
        std::fs::write(paths.machine.join("events.jsonl"), "{}\n").unwrap();
        std::fs::write(paths.machine.join("sid.txt"), "S-1-5-21-1").unwrap();
        assert!(needs_layout_migration(&paths));

        let actions = migrate_layout(&paths).expect("迁移应成功");
        assert!(!actions.is_empty());
        assert!(paths.secrets().join("token.txt").exists());
        assert_eq!(
            std::fs::read_to_string(paths.secrets().join("token.txt")).unwrap(),
            "tok"
        );
        assert!(paths.state().join("enabled.flag").exists());
        assert!(paths.state().join("sid.txt").exists());
        assert!(paths.logs().join("events.jsonl").exists());
        assert!(!paths.machine.join("token.txt").exists(), "旧位置不应残留");
        assert!(!needs_layout_migration(&paths), "迁移后不应再次触发");
    }

    #[test]
    fn layout_migration_does_not_overwrite_existing_target() {
        let paths = tmp_paths("layout-noover");
        std::fs::write(paths.machine.join("token.txt"), "OLD").unwrap();
        std::fs::create_dir_all(paths.secrets()).unwrap();
        std::fs::write(paths.secrets().join("token.txt"), "NEW").unwrap();

        let actions = migrate_layout(&paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(paths.secrets().join("token.txt")).unwrap(),
            "NEW",
            "目标已存在时不得覆盖"
        );
        assert!(
            paths.machine.join("token.txt").exists(),
            "跳过时源文件应保留（不丢数据）"
        );
        assert!(actions.iter().any(|a| a.contains("跳过")));
    }

    // ---------- 缓存 ----------

    #[test]
    fn cached_load_picks_up_file_change() {
        let paths = tmp_paths("cache");
        write_default_policy(&paths).unwrap();
        invalidate_cache();

        let a = load_cached(&paths);
        assert_eq!(a.policy.file.mode, Mode::Autonomy);

        let mut f = PolicyFile::default_autonomy();
        f.mode = Mode::Strict;
        write_policy(&paths, &f).unwrap();
        // 确保 mtime 变化（NTFS 分辨率很高，但保守等待一次）
        std::thread::sleep(std::time::Duration::from_millis(20));

        let b = load_cached(&paths);
        assert_eq!(
            b.policy.file.mode,
            Mode::Strict,
            "配置文件变化后缓存必须失效（保留热生效）"
        );
    }

    // ---------- 信任根 ----------

    #[test]
    fn trusted_publisher_persists() {
        let paths = tmp_paths("pub");
        write_default_policy(&paths).unwrap();
        let thumb = "AB".repeat(32);
        let got = add_trusted_publisher(&paths, &thumb).unwrap();
        assert_eq!(got.len(), 64);

        let eff = load(&paths);
        assert!(eff.policy.publisher_trusted(&thumb));
        add_trusted_publisher(&paths, &thumb).unwrap();
        assert_eq!(load(&paths).policy.file.trusted_publishers.len(), 1);
    }

    #[test]
    fn add_trusted_publisher_rejects_bad_thumbprint() {
        let paths = tmp_paths("pub-bad");
        write_default_policy(&paths).unwrap();
        assert!(add_trusted_publisher(&paths, "xyz").is_err());
    }

    // ---------- 默认策略自洽 ----------

    #[test]
    fn default_policy_toml_roundtrip_preserves_semantics() {
        let paths = tmp_paths("roundtrip");
        write_default_policy(&paths).unwrap();
        let a = load_readonly(&paths).policy.file;
        write_policy(&paths, &a).unwrap();
        let b = load_readonly(&paths).policy.file;
        assert_eq!(a.mode, b.mode);
        assert_eq!(a.default_decision, b.default_decision);
        assert_eq!(a.rules.len(), b.rules.len());
        assert_eq!(a.approved_services, b.approved_services);
        assert_eq!(a.trusted_publishers, b.trusted_publishers);
        assert_eq!(a.confirm_timeout_secs, b.confirm_timeout_secs);
    }

    #[test]
    fn readonly_load_never_creates_files() {
        let paths = tmp_paths("readonly");
        let eff = load_readonly(&paths);
        assert_eq!(eff.layers.admin, LayerState::Missing);
        assert!(!paths.policy().exists(), "只读加载不得创建策略文件");
        assert!(!paths.policy_marker().exists());
    }
}
