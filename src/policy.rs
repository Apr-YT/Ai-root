//! 授权策略引擎（服务端强制执行）
//!
//! ## 为什么需要这一层
//!
//! 改造前 AI-Root 的授权模型是「**一张令牌 = 一个 SYSTEM shell**」：只要读到
//! `token.txt`，就能让 LocalSystem 执行任意命令。这不是密码强度问题，而是
//! **完全没有操作级授权** —— 没有任何东西区分「查一下 CPU 型号」和「格式化 D 盘」。
//!
//! 本模块把「能不能执行」从「有没有令牌」中拆出来，改为按
//! **操作类型 + 目标 + 程序 + 参数 + 工作目录**逐项判定，输出三种结果：
//!
//! - [`Decision::Allow`]：严格匹配的低风险规则，直接执行
//! - [`Decision::Confirm`]：需要人工确认（见 `server.rs` 的确认通道）
//! - [`Decision::Deny`]：明确禁止，**任何确认都不能放行**
//!
//! ## 判定顺序（不可调换）
//!
//! ```text
//! 1. 内置保护（硬编码，策略文件无法覆盖）  -> Deny
//! 2. 策略文件里的 deny 规则（全部先扫）    -> Deny
//! 3. 策略文件里的 allow 规则               -> Allow
//! 4. 策略文件里的 confirm 规则             -> Confirm
//! 5. 未匹配                                -> default_decision
//! ```
//!
//! 禁止优先于允许是**结构性保证**：即使配置文件里同时写了 allow 和 deny，
//! deny 也一定先命中。这样「管理员加了一条宽松规则」不会意外放行被禁操作。
//!
//! ## 为什么不用命令黑名单
//!
//! 改造前的 `exec.rs` 有一份 5 个字符串的黑名单（`format ` / `diskpart` / …）。
//! 它既拦不住人（`f^ormat`、`cmd /c del /s`、`powershell -enc`、`for /f` 全绕过），
//! 又会误伤（`echo format ` 被拒）。黑名单只能作为**廉价的纵深防御提示**，
//! 不能当边界。真正的边界是这里的结构化判定 + 「argv 执行不经 shell」。
//!
//! ## 一个重要且必须诚实说明的取舍
//!
//! 在 LocalSystem 下运行 `python` / `cargo` / `node` 这类解释器或编译器，
//! **在效果上就等于允许任意代码执行**（`python -c`、`build.rs`、`npm postinstall`）。
//! 因此本模式的边界**不是**「禁止代码执行」，而是：
//!
//! 1. 不允许**间接**执行（`cmd /c`、`powershell -enc`、`mshta`…），保证每次执行
//!    都能被结构化审计，无法把真实意图藏进字符串里；
//! 2. 不允许**不可逆破坏**（格式化、删除卷、改引导、清审计、建管理员账户）；
//! 3. 不允许**改动 AI-Root 自身**（服务二进制、令牌、策略、服务配置）。
//!
//! 这个取舍写进了 `docs/安全模型.md`，不做隐瞒。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 旧版策略文件名（JSON）。v1.1 及以前使用；v1.2 起由 `config.rs` 迁移到 TOML。
pub const LEGACY_POLICY_FILE: &str = "policy.json";

/// 当前支持的策略文件版本。
///
/// 版本变更历史：
/// - v1：JSON 单文件（`policy.json`），规则内联在 `rules` 数组
/// - v2：TOML（`policy.toml`）+ 独立 `rules.d\*.toml` + 严格字段校验
///
/// 读到更高版本 → **拒绝**（不猜测未来语义）；读到更低版本 → 走 `config::migrate`。
pub const POLICY_VERSION: u32 = 2;

/// 授权判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// 满足严格限定的低风险规则，直接执行
    Allow,
    /// 需要人工确认；无可用确认通道时等同拒绝
    Confirm,
    /// 明确禁止；确认流程不能放行
    Deny,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Confirm => "confirm",
            Decision::Deny => "deny",
        }
    }
}

/// 运行模式（决定「未匹配请求」的默认归宿）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// 个人高自主性：开发工具链 argv 自动执行，破坏性操作仍被拦
    Autonomy,
    /// 平衡：同上，但 argv 必须落在 workspace_roots 内
    Balanced,
    /// 严格：只允许查询类，其余需确认或拒绝
    Strict,
    /// 停用：除查询类外全部拒绝（等价于「只读中继」）
    Disabled,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Autonomy => "autonomy",
            Mode::Balanced => "balanced",
            Mode::Strict => "strict",
            Mode::Disabled => "disabled",
        }
    }
}

/// 结构化操作类型。
///
/// 是**枚举而非字符串**：未知操作在反序列化阶段就被拒绝，
/// 不可能因为拼写差异漏进后面的判定逻辑（任务书 6.1「未知操作不能默认放行」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// 查询系统版本 / CPU / 内存 / 磁盘（只读）
    QuerySystemInfo,
    /// 查询服务状态（只读）
    QueryService,
    /// 重启指定服务（目标必须在 approved_services 内）
    RestartService,
    /// 读取明确允许的非敏感状态文件
    ReadStateFile,
    /// 通过可信包管理器安装软件
    InstallPackage,
    /// 更新 AI-Root 自身（签名 + 信任根双重门控）
    UpdateSelf,
    /// 结构化 argv 执行（不经 shell）
    RunArgv,
    /// 传统 shell 字符串执行（经 `cmd /C`）
    RunShell,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::QuerySystemInfo => "query_system_info",
            OpKind::QueryService => "query_service",
            OpKind::RestartService => "restart_service",
            OpKind::ReadStateFile => "read_state_file",
            OpKind::InstallPackage => "install_package",
            OpKind::UpdateSelf => "update_self",
            OpKind::RunArgv => "run_argv",
            OpKind::RunShell => "run_shell",
        }
    }

    /// 是否为只读查询类（Disabled / 降级模式下仍放行）
    pub fn is_readonly(self) -> bool {
        matches!(
            self,
            OpKind::QuerySystemInfo | OpKind::QueryService | OpKind::ReadStateFile
        )
    }
}

/// 待判定动作（借用式，避免为每次请求克隆命令字符串）
#[derive(Debug)]
pub enum Action<'a> {
    /// 结构化操作（无 argv，仅带 target）
    Op {
        op: OpKind,
        target: Option<&'a str>,
    },
    /// 结构化 argv：程序 + 参数数组 + 可选工作目录
    Argv {
        program: &'a str,
        args: &'a [String],
        cwd: Option<&'a str>,
    },
    /// 传统 shell 字符串
    Shell {
        command: &'a str,
        cwd: Option<&'a str>,
    },
}

impl Action<'_> {
    pub fn op_kind(&self) -> OpKind {
        match self {
            Action::Op { op, .. } => *op,
            Action::Argv { .. } => OpKind::RunArgv,
            Action::Shell { .. } => OpKind::RunShell,
        }
    }

    /// 用于审计与确认请求的可读描述（不含令牌；命令参数由调用方脱敏）
    pub fn describe(&self) -> String {
        match self {
            Action::Op { op, target } => match target {
                Some(t) => format!("op={} target={}", op.as_str(), t),
                None => format!("op={}", op.as_str()),
            },
            Action::Argv {
                program,
                args,
                cwd,
            } => format!(
                "argv=[{}]{}",
                std::iter::once(program.to_string())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
                cwd.map(|c| format!(" cwd={c}")).unwrap_or_default()
            ),
            Action::Shell { command, cwd } => format!(
                "shell={}{}",
                command.replace('\n', " ; "),
                cwd.map(|c| format!(" cwd={c}")).unwrap_or_default()
            ),
        }
    }
}

/// 策略判定结果 + 命中的规则标识（全部写入审计日志）
#[derive(Debug, Clone)]
pub struct Verdict {
    pub decision: Decision,
    /// 命中规则 ID：`builtin:*` 为内置保护，`rule:<id>` 为策略文件规则，
    /// `default` 为未匹配时按模式取值
    pub rule_id: String,
    /// 人类可读原因（写入审计，便于回溯「为什么放行/拒绝」）
    pub reason: String,
}

impl Verdict {
    fn new(decision: Decision, rule_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Verdict {
            decision,
            rule_id: rule_id.into(),
            reason: reason.into(),
        }
    }
}

/// 一条策略规则。
///
/// 匹配是**全字段与语义**（所有填写了的字段都必须匹配），不是「任一命中」。
/// 未填写（`None` / 空数组）的字段表示「不约束」。
///
/// `deny_unknown_fields`：拼错字段名必须报错。
/// 反例：把 `arg_prefix` 写成 `arg_prefixes` 会被静默忽略 → 规则意外变宽。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// 稳定标识，写入审计日志；重复 ID 在加载时被拒绝
    pub id: String,
    pub decision: Decision,

    /// 约束操作类型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<OpKind>,
    /// 约束程序（basename，大小写不敏感，可省略 `.exe`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// 约束程序的**绝对路径**（规范化后精确相等；优先级高于 `program`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_path: Option<String>,
    /// 参数**前缀**必须逐项相等（大小写敏感，数组整体比较）
    #[serde(default)]
    pub arg_prefix: Vec<String>,
    /// 参数中**任意位置**出现其中任一 token 即**不匹配**（用于禁 `python -c`）
    #[serde(default)]
    pub arg_forbid: Vec<String>,
    /// 约束 `op` 的 target（规范化后精确相等，或 `prefix:*` 形式做前缀匹配）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// 约束工作目录必须落在这些根之内
    #[serde(default)]
    pub cwd_roots: Vec<String>,
    /// 备注（写给人看）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Rule {
    fn matches(&self, action: &Action<'_>) -> bool {
        if let Some(op) = self.op {
            if op != action.op_kind() {
                return false;
            }
        }

        match action {
            Action::Op { target, .. } => {
                if let Some(want) = &self.target {
                    let got = match target {
                        Some(t) => *t,
                        None => return false, // 规则要求 target，但请求没给
                    };
                    if !match_target(want, got) {
                        return false;
                    }
                }
                // 带 program 约束的规则不匹配无程序的 op
                if self.program.is_some() || self.program_path.is_some() {
                    return false;
                }
            }
            Action::Argv {
                program,
                args,
                cwd,
            } => {
                if let Some(want_path) = &self.program_path {
                    match norm_abs_path(want_path) {
                        Some(want) => {
                            if resolve_arg_program(program) != Some(want) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                } else if let Some(want) = &self.program {
                    if !program_eq(want, program) {
                        return false;
                    }
                }
                if !self.arg_prefix.is_empty() {
                    if args.len() < self.arg_prefix.len() {
                        return false;
                    }
                    for (want, got) in self.arg_prefix.iter().zip(args.iter()) {
                        if want != got {
                            return false;
                        }
                    }
                }
                for bad in &self.arg_forbid {
                    if args.iter().any(|a| a == bad) {
                        return false;
                    }
                }
                if !self.cwd_roots.is_empty() {
                    let c = match cwd {
                        Some(c) => c,
                        None => return false, // 规则要求目录约束，但请求未指定 cwd
                    };
                    if !path_under_any_ci(c, &self.cwd_roots) {
                        return false;
                    }
                }
            }
            Action::Shell { command: _, cwd } => {
                // shell 动作只按 op / target / cwd 约束；
                // 不做 command 子串匹配（子串匹配是黑名单思路，绕过太多）
                if self.program.is_some()
                    || self.program_path.is_some()
                    || !self.arg_prefix.is_empty()
                    || !self.arg_forbid.is_empty()
                {
                    return false;
                }
                if !self.cwd_roots.is_empty() {
                    let c = match cwd {
                        Some(c) => c,
                        None => return false,
                    };
                    if !path_under_any_ci(c, &self.cwd_roots) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// 策略文件的内存表示
///
/// `deny_unknown_fields` 是硬要求（审计 C-01）：策略文件里的字段拼错时，
/// serde 默认会**静默丢弃**，导致「看起来配置了但没生效」。这类问题无法从
/// 运行时行为反推（只表现为某条命令莫名被拒），必须靠解析阶段报错。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub version: u32,
    pub mode: Mode,
    /// 未匹配任何规则时的归宿（内置保护与 deny 规则仍优先）
    #[serde(default = "default_decision_default")]
    pub default_decision: Decision,
    /// 允许 argv 执行的工作目录根（Balanced 模式为硬约束）
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    /// 允许 `RestartService` 的服务名（精确、大小写不敏感）
    #[serde(default)]
    pub approved_services: Vec<String>,
    /// 可信发布者证书 SHA-256 指纹（大写 hex，无分隔符）——update 的唯一信任根
    #[serde(default)]
    pub trusted_publishers: Vec<String>,
    /// 允许的传统 shell 动作（false 时 shell 走确认流程）
    #[serde(default)]
    pub allow_legacy_shell: bool,
    /// 确认请求有效期（秒），钳制到 [30, 3600]
    #[serde(default = "default_confirm_timeout")]
    pub confirm_timeout_secs: u64,
    /// 规则表。
    ///
    /// TOML 里写作 `[[rule]]`（单数、数组表），旧 JSON 写作 `"rules": [...]`；
    /// 两者都接受，序列化时输出 `rule`（TOML 友好）。
    #[serde(default, rename = "rule", alias = "rules")]
    pub rules: Vec<Rule>,
}

fn default_decision_default() -> Decision {
    Decision::Confirm
}
fn default_confirm_timeout() -> u64 {
    300
}

/// 策略加载状态（用于区分「正常」与「读坏了降级」——降级必须可见）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// 正常读取
    Loaded,
    /// 文件不存在，已写入默认策略
    CreatedDefault,
    /// 文件无法读取 / 解析失败 / 版本不支持 → 已降级为 locked_down
    Degraded(String),
}

/// 已加载的策略 + 加载状态
#[derive(Debug, Clone)]
pub struct Policy {
    pub file: PolicyFile,
    pub state: LoadState,
}

impl Policy {
    /// 当前策略是否处于降级（只允许只读查询）
    pub fn is_degraded(&self) -> bool {
        matches!(self.state, LoadState::Degraded(_))
    }

    /// 核心判定。`protected` 是被硬保护的路径集合（数据目录 / 部署目录）。
    pub fn evaluate(&self, action: &Action<'_>, protected: &[PathBuf]) -> Verdict {
        // ── 1. 内置保护：硬编码，策略文件无法覆盖 ──
        if let Some((id, why)) = builtin_deny(action, protected) {
            return Verdict::new(Decision::Deny, id, why);
        }

        // ── 降级模式：只放行只读查询 ──
        if self.is_degraded() {
            if action.op_kind().is_readonly() {
                return Verdict::new(
                    Decision::Allow,
                    "degraded:readonly",
                    "策略降级：仅放行只读查询",
                );
            }
            let why = match &self.state {
                LoadState::Degraded(e) => format!("策略不可用（{e}），拒绝非只读操作"),
                _ => "策略降级，拒绝非只读操作".to_string(),
            };
            return Verdict::new(Decision::Deny, "degraded:deny", why);
        }

        // ── disabled 模式是硬约束：连 allow 规则也不生效（否则「停用」名不副实）──
        if self.file.mode == Mode::Disabled && !action.op_kind().is_readonly() {
            return Verdict::new(
                Decision::Deny,
                "mode:disabled",
                "策略处于 disabled 模式，仅放行只读查询",
            );
        }

        // ── 2. deny 规则：全部先扫，确保「禁止优先」是结构性保证 ──
        for r in self.file.rules.iter().filter(|r| r.decision == Decision::Deny) {
            if r.matches(action) {
                return Verdict::new(
                    Decision::Deny,
                    format!("rule:{}", r.id),
                    r.note.clone().unwrap_or_else(|| "命中禁止规则".into()),
                );
            }
        }

        // ── 3. 动态允许：approved_services 名单内的服务重启 ──
        //
        // 这是「管理员通过配置扩展自动执行范围，不必重写执行器」的落点：
        // 想放开某个服务的重启，只加一行 `approved_services` 即可，
        // 既不用写规则，也不用改代码。deny 规则仍优先（在上方已扫过）。
        if let Action::Op {
            op: OpKind::RestartService,
            target: Some(t),
        } = action
        {
            if self.service_is_approved(t) {
                return Verdict::new(
                    Decision::Allow,
                    "policy:approved-service",
                    format!("服务 `{t}` 在 approved_services 名单内"),
                );
            }
        }

        // ── 4. allow 规则 ──
        for r in self
            .file
            .rules
            .iter()
            .filter(|r| r.decision == Decision::Allow)
        {
            if r.matches(action) {
                // Balanced 模式额外硬约束：argv 必须落在 workspace_roots 内
                if self.file.mode == Mode::Balanced && action.op_kind() == OpKind::RunArgv {
                    if !self.workspace_allows(action) {
                        return Verdict::new(
                            Decision::Confirm,
                            format!("rule:{}", r.id),
                            "Balanced 模式：工作目录不在 workspace_roots 内，需确认",
                        );
                    }
                }
                return Verdict::new(
                    Decision::Allow,
                    format!("rule:{}", r.id),
                    r.note.clone().unwrap_or_else(|| "命中自动允许规则".into()),
                );
            }
        }

        // ── 5. confirm 规则 ──
        for r in self
            .file
            .rules
            .iter()
            .filter(|r| r.decision == Decision::Confirm)
        {
            if r.matches(action) {
                return Verdict::new(
                    Decision::Confirm,
                    format!("rule:{}", r.id),
                    r.note.clone().unwrap_or_else(|| "命中确认规则".into()),
                );
            }
        }

        // ── 6. 未匹配：按模式取默认 ──
        match self.file.mode {
            Mode::Disabled => Verdict::new(
                Decision::Deny,
                "default:disabled",
                "策略处于 disabled 模式，仅放行只读查询",
            ),
            Mode::Strict => Verdict::new(
                Decision::Deny,
                "default:strict",
                "严格模式：未显式允许的操作一律拒绝",
            ),
            Mode::Balanced | Mode::Autonomy => {
                // 传统 shell 在未命中规则时，若策略未启用 legacy shell → 一律确认，
                // 不受 default_decision 影响（避免有人把 default 改成 allow 就顺带放开 shell）
                if action.op_kind() == OpKind::RunShell && !self.file.allow_legacy_shell {
                    return Verdict::new(
                        Decision::Confirm,
                        "default:shell-confirm",
                        "传统 shell 未命中任何规则：需人工确认（不经结构化校验，风险较高）",
                    );
                }
                Verdict::new(
                    self.file.default_decision,
                    "default",
                    format!(
                        "未命中规则，按 default_decision={} 处理",
                        self.file.default_decision.as_str()
                    ),
                )
            }
        }
    }

    /// Balanced 模式下判断 argv 的 cwd 是否在 workspace_roots 内
    fn workspace_allows(&self, action: &Action<'_>) -> bool {
        if self.file.workspace_roots.is_empty() {
            return false;
        }
        match action {
            Action::Argv { cwd, .. } => match cwd {
                Some(c) => path_under_any_ci(c, &self.file.workspace_roots),
                None => false,
            },
            _ => false,
        }
    }

    /// 确认请求有效期
    pub fn confirm_timeout_secs(&self) -> u64 {
        self.file.confirm_timeout_secs.clamp(30, 3600)
    }

    /// 目标服务是否为已批准服务（RestartService 用）
    pub fn service_is_approved(&self, name: &str) -> bool {
        self.file
            .approved_services
            .iter()
            .any(|s| s.eq_ignore_ascii_case(name))
    }

    /// 发布者指纹是否可信（update 用）
    pub fn publisher_trusted(&self, thumbprint: &str) -> bool {
        let want = thumbprint.replace([' ', ':'], "").to_uppercase();
        self.file
            .trusted_publishers
            .iter()
            .any(|p| p.replace([' ', ':'], "").to_uppercase() == want)
    }
}

// ============================================================
// 内置保护（硬编码）：策略文件无法覆盖
// ============================================================

/// 间接执行 / 下载类程序：argv 模式下直接拒绝。
///
/// 理由：允许它们等于放弃「结构化审计」这个唯一优势 ——
/// 真实命令藏在 `-enc`/`-Command`/`/c` 后面，日志里只能看到一层壳。
const SHELL_HOSTS: &[&str] = &[
    "cmd",
    "powershell",
    "pwsh",
    "wscript",
    "cscript",
    "mshta",
    "rundll32",
    "regsvr32",
    "wsl",
    "bash",
    "sh",
];

/// 可下载远程内容的工具（默认拒绝，需管理员显式加规则）
const DOWNLOADERS: &[&str] = &["bitsadmin", "certutil", "curl", "wget", "invoke-webrequest"];

/// 内置保护判定。返回 `(rule_id, 原因)`。
fn builtin_deny(action: &Action<'_>, protected: &[PathBuf]) -> Option<(String, String)> {
    match action {
        Action::Argv {
            program, args, cwd, ..
        } => {
            let base = program_basename(program);

            // 1) 间接执行宿主
            if SHELL_HOSTS.contains(&base.as_str()) {
                return Some((
                    "builtin:shell-host".into(),
                    format!("拒绝经由 {base} 的间接执行（请改用结构化 argv 或经确认的 shell 请求）"),
                ));
            }
            // 2) 下载器（certutil 的只读子命令放行，见下）
            if DOWNLOADERS.contains(&base.as_str()) {
                let readonly_certutil = base == "certutil"
                    && args
                        .first()
                        .map(|a| {
                            matches!(
                                a.to_ascii_lowercase().as_str(),
                                "-hashfile" | "-dump" | "-verify" | "-store"
                            )
                        })
                        .unwrap_or(false);
                if !readonly_certutil {
                    return Some((
                        "builtin:downloader".into(),
                        format!("拒绝经由 {base} 下载/写入远程内容"),
                    ));
                }
            }
            // 3) 参数或 cwd 指向 AI-Root 自身受保护路径
            if let Some(hit) = touched_protected(program, args, *cwd, protected) {
                return Some((
                    "builtin:self-protect".into(),
                    format!("参数指向受保护路径（{hit}），拒绝修改 AI-Root 自身"),
                ));
            }
            // 4) 破坏性系统工具（按子命令判定，避免误伤只读查询）
            if let Some(why) = destructive_system_tool(&base, args) {
                return Some(("builtin:destructive".into(), why));
            }
            None
        }
        Action::Shell { command, cwd } => {
            if let Some(hit) = touched_protected_str(command, *cwd, protected) {
                return Some((
                    "builtin:self-protect".into(),
                    format!("命令涉及受保护路径（{hit}），拒绝修改 AI-Root 自身"),
                ));
            }
            shell_destructive(command).map(|why| ("builtin:destructive".into(), why))
        }
        Action::Op { op, target } => {
            // update 自身永远不能由客户端「声明自己是安全的」而放行；
            // 真正的门控在 service::update_self（签名 + 信任根）
            if *op == OpKind::UpdateSelf {
                return Some((
                    "builtin:update-gated".into(),
                    "更新自身必须经签名验证与信任根比对，不能由策略声明放行".into(),
                ));
            }
            if let Some(t) = target {
                if let Some(hit) = touched_protected_str(t, None, protected) {
                    return Some((
                        "builtin:self-protect".into(),
                        format!("操作目标涉及受保护路径（{hit}）"),
                    ));
                }
            }
            None
        }
    }
}

/// 破坏性系统工具的结构化判定（argv 模式）。
///
/// 只拦「不可逆 / 关闭安全控制 / 建立持久化」这三类，其余交给策略确认。
fn destructive_system_tool(base: &str, args: &[String]) -> Option<String> {
    let lower: Vec<String> = args.iter().map(|a| a.to_ascii_lowercase()).collect();
    let has = |needle: &str| lower.iter().any(|a| a == needle);
    let has_any = |needles: &[&str]| lower.iter().any(|a| needles.contains(&a.as_str()));

    match base {
        // 磁盘/卷级不可逆操作
        "format" | "diskpart" | "cleanmgr" => Some(format!("{base} 属磁盘级不可逆操作")),
        // 引导配置
        "bcdedit" => Some("bcdedit 属引导配置修改，禁止经中继执行".into()),
        // 备份/卷影副本删除 → 破坏可恢复性
        "vssadmin" | "wbadmin" if has_any(&["delete", "resize"]) => {
            Some(format!("{base} 删除备份/卷影副本会破坏可恢复性"))
        }
        // 审计日志清空
        "wevtutil" if has("cl") || has("clear-log") => Some("清空 Windows 事件日志会破坏审计".into()),
        "auditpol" if has_any(&["/clear", "/remove"]) => Some("清除审计策略会破坏审计".into()),
        // 服务注册/删除 = 持久化
        "sc" if has_any(&["create", "delete", "config"]) => {
            Some("服务注册/删除/改配置属持久化与自我修改".into())
        }
        // 计划任务 = 持久化
        "schtasks" if has_any(&["/create", "/change", "/delete"]) => {
            Some("计划任务属持久化机制".into())
        }
        // 账户与提权组
        "net" if has_any(&["user", "localgroup"]) && has("/add") => {
            Some("创建账户 / 提升为管理员组属提权操作".into())
        }
        // 注册表自启动点
        "reg"
            if has_any(&["add", "delete"])
                && lower.iter().any(|a| a.contains("currentversion\\run") || a.contains("\\runonce")) =>
        {
            Some("写入自启动注册表项属持久化机制".into())
        }
        // 关闭安全软件 / 防火墙
        "netsh" if lower.iter().any(|a| a.contains("advfirewall")) => {
            Some("修改防火墙策略属关闭安全控制".into())
        }
        "set-mppreference" | "add-mppreference" => Some("修改 Defender 配置属关闭安全控制".into()),
        // 凭据提取（明确列举常见工具名，作为纵深防御提示，不作为唯一依据）
        "procdump" | "mimikatz" | "pwdump" => Some("凭据转储工具，禁止经中继执行".into()),
        _ => None,
    }
}

/// shell 字符串的破坏性判定（argv 无法结构化时的兜底）。
///
/// ⚠ 这是**纵深防御**，不是边界（字符串可拼接、可间接调用）。真正的边界是：
///   1) shell 动作默认走人工确认；2) 确认绑定完整命令哈希。
fn shell_destructive(cmd: &str) -> Option<String> {
    let s = cmd.to_ascii_lowercase();

    // 不可逆磁盘/引导操作：无论怎么拼都拦
    for pat in [
        "format ",
        "format:",
        "diskpart",
        "bcdedit",
        "vssadmin delete",
        "wbadmin delete",
        "cipher /w",
        "clean all",
    ] {
        if s.contains(pat) {
            return Some(format!("检测到不可逆操作模式 `{pat}`，禁止经中继执行"));
        }
    }
    // 关闭审计 / 安全控制
    for pat in [
        "wevtutil cl",
        "auditpol /clear",
        "set-mppreference",
        "netsh advfirewall set",
        "sc stop windefender",
        "sc stop sense",
    ] {
        if s.contains(pat) {
            return Some(format!("检测到关闭安全控制模式 `{pat}`，禁止经中继执行"));
        }
    }
    // 账户 / 持久化（组合判断，避免误伤 `net user` 查询）
    if s.contains("net user") && s.contains("/add") {
        return Some("创建本地账户属提权操作，禁止经中继执行".into());
    }
    if s.contains("net localgroup administrators") && s.contains("/add") {
        return Some("加入管理员组属提权操作，禁止经中继执行".into());
    }
    if s.contains("sc create") || s.contains("sc delete") {
        return Some("服务注册/删除属持久化机制，禁止经中继执行".into());
    }
    if s.contains("schtasks") && s.contains("/create") {
        return Some("创建计划任务属持久化机制，禁止经中继执行".into());
    }
    if s.contains("reg add") && (s.contains("\\run") || s.contains("\\runonce")) {
        return Some("写入自启动注册表项属持久化机制，禁止经中继执行".into());
    }
    None
}

/// argv 中是否触碰受保护路径
fn touched_protected(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    protected: &[PathBuf],
) -> Option<String> {
    let mut all: Vec<&str> = Vec::with_capacity(args.len() + 2);
    all.push(program);
    all.extend(args.iter().map(|s| s.as_str()));
    if let Some(c) = cwd {
        all.push(c);
    }
    for item in all {
        for p in protected {
            if let Some(root) = norm_abs_path(&p.to_string_lossy()) {
                let norm = normalize_win_path(item);
                if norm == root || norm.starts_with(&format!("{root}/")) {
                    return Some(root);
                }
            }
        }
    }
    None
}

/// shell 字符串中是否出现受保护路径
fn touched_protected_str(cmd: &str, cwd: Option<&str>, protected: &[PathBuf]) -> Option<String> {
    let hay = normalize_win_path(&format!("{} {}", cmd, cwd.unwrap_or("")));
    for p in protected {
        if let Some(root) = norm_abs_path(&p.to_string_lossy()) {
            if hay.contains(&root) {
                return Some(root);
            }
        }
    }
    None
}

// ============================================================
// 路径与程序名工具
// ============================================================

/// Windows 路径规范化：小写、`\`→`/`、消除 `.`/`..`、去尾斜杠、去长前缀。
///
/// 为什么不用 `std::fs::canonicalize`：它要求路径存在，而策略判定需要处理
/// 「即将创建」的路径；且 canonicalize 会解析符号链接，导致「规则里的根」
/// 与「实际目标」比较基准不一致。这里做纯词法规范化，比较双方用同一套规则。
pub fn normalize_win_path(p: &str) -> String {
    let mut s = p.trim().replace('\\', "/");
    // 去 `\\?\` 与 `//?/` 前缀
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    let lower = s.to_lowercase();
    // 拆前缀：UNC（保留 `//`，否则会被拼成相对路径）/ 盘符 / 相对
    let unc = lower.starts_with("//");
    let (prefix, rest) = if unc {
        ("//".to_string(), &lower[2..])
    } else if lower.len() >= 2 && lower.as_bytes()[1] == b':' {
        (lower[..2].to_string(), &lower[2..])
    } else {
        (String::new(), lower.as_str())
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if prefix.is_empty() {
        out.join("/")
    } else if prefix == "//" {
        format!("//{}", out.join("/"))
    } else {
        format!("{prefix}/{}", out.join("/"))
    }
}

/// 规范化并要求是绝对路径（盘符或 UNC）。返回 `None` 表示不是绝对路径。
fn norm_abs_path(p: &str) -> Option<String> {
    let n = normalize_win_path(p);
    let is_abs = n.len() >= 2 && n.as_bytes()[1] == b':' || n.starts_with("//");
    if is_abs && !n.is_empty() {
        Some(n)
    } else {
        None
    }
}

/// 路径是否落在任一根本之内（大小写不敏感，按路径边界比较）。
///
/// 关键：必须按**目录边界**比较，否则 `C:\project-evil` 会被 `C:\project` 误判为子目录。
pub fn path_under_any_ci(path: &str, roots: &[String]) -> bool {
    let p = normalize_win_path(path);
    if p.is_empty() {
        return false;
    }
    roots.iter().any(|r| {
        let Some(root) = norm_abs_path(r) else {
            return false;
        };
        p == root || p.starts_with(&format!("{root}/"))
    })
}

/// 取程序 basename（小写、去 `.exe`）。非路径输入原样小写。
fn program_basename(program: &str) -> String {
    let n = program.trim().replace('\\', "/");
    let base = n.rsplit('/').next().unwrap_or(&n).to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_string()
}

/// 程序名比较：规则里的 `program` 与请求 program 的 basename 相等
fn program_eq(rule_program: &str, action_program: &str) -> bool {
    program_basename(rule_program) == program_basename(action_program)
}

/// 把请求里的 program 解析成规范化绝对路径（用于 `program_path` 精确匹配）。
///
/// 只做词法判定，不做 PATH 搜索：`program_path` 规则的语义是「必须是这个文件」，
/// 若请求给的是裸名字，无法确定它最终落到哪个文件 → 视为不匹配（保守）。
fn resolve_arg_program(program: &str) -> Option<String> {
    norm_abs_path(program)
}

/// target 匹配：`*` 表示任意；精确；或 `prefix:*` 前缀
fn match_target(rule: &str, got: &str) -> bool {
    if rule == "*" {
        return true;
    }
    if let Some(prefix) = rule.strip_suffix(":*") {
        let g = normalize_win_path(got);
        let p = normalize_win_path(prefix);
        g == p || g.starts_with(&format!("{p}/")) || g.starts_with(&format!("{p}:"))
    } else {
        rule.eq_ignore_ascii_case(got)
    }
}

// ============================================================
// 默认策略
// ============================================================

impl PolicyFile {
    /// 策略自校验（加载时执行，失败即降级）
    pub fn validate(&mut self) -> Result<(), String> {
        if self.version == 0 {
            return Err("策略 version 必须 >= 1".into());
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.rules {
            if r.id.trim().is_empty() {
                return Err("规则 id 不能为空".into());
            }
            if !seen.insert(r.id.clone()) {
                return Err(format!("规则 id 重复: {}", r.id));
            }
            if r.decision == Decision::Allow
                && r.op.is_none()
                && r.program.is_none()
                && r.program_path.is_none()
            {
                return Err(format!(
                    "允许规则 {} 过于宽泛：必须至少约束 op / program / program_path 之一",
                    r.id
                ));
            }
            if let Some(p) = &r.program_path {
                if !Path::new(p).is_absolute() {
                    return Err(format!("规则 {} 的 program_path 必须是绝对路径", r.id));
                }
            }
            for root in r.cwd_roots.iter().chain(self.workspace_roots.iter()) {
                if !Path::new(root).is_absolute() {
                    return Err(format!("目录根必须是绝对路径: {root}"));
                }
            }
        }
        for p in &self.trusted_publishers {
            let ok = p.len() == 64 && p.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(format!(
                    "trusted_publishers 项必须是 64 位 hex（证书 SHA-256）: {p}"
                ));
            }
        }
        if self.confirm_timeout_secs < 30 || self.confirm_timeout_secs > 3600 {
            self.confirm_timeout_secs = self.confirm_timeout_secs.clamp(30, 3600);
        }
        Ok(())
    }


    /// 默认的**个人高自主性**策略：开发工具链 argv 自动执行，破坏性操作仍需确认/被拒。
    ///
    /// 设计意图：日常开发（编译、测试、包管理、版本控制、系统信息查询）不打断用户；
    /// 涉及系统配置、批量删除、服务/账户/网络改动的动作落到确认；不可逆动作硬拒绝。
    pub fn default_autonomy() -> PolicyFile {
        let mut rules: Vec<Rule> = Vec::new();

        // ── 只读查询：永远自动允许 ──
        for (op, note) in [
            (OpKind::QuerySystemInfo, "查询系统版本/CPU/内存/磁盘（只读）"),
            (OpKind::QueryService, "查询服务状态（只读）"),
            (OpKind::ReadStateFile, "读取明确允许的非敏感状态文件"),
        ] {
            rules.push(Rule {
                id: format!("allow-{}", op.as_str()),
                decision: Decision::Allow,
                op: Some(op),
                program: None,
                program_path: None,
                arg_prefix: vec![],
                arg_forbid: vec![],
                target: None,
                cwd_roots: vec![],
                note: Some(note.into()),
            });
        }

        // ── 开发工具链：argv 自动允许 ──
        // (program, 禁止出现的参数 token, 备注)
        let dev: &[(&str, &[&str], &str)] = &[
            ("git", &[], "版本控制"),
            ("cargo", &[], "Rust 构建/测试"),
            ("rustc", &[], "Rust 编译器"),
            ("rustup", &[], "Rust 工具链"),
            ("rustfmt", &[], "Rust 格式化"),
            ("clippy-driver", &[], "Rust lint"),
            ("node", &["-e", "--eval", "-p", "--print"], "Node 运行时（禁 -e 内联脚本）"),
            ("npm", &[], "npm 包管理/脚本"),
            ("pnpm", &[], "pnpm 包管理"),
            ("yarn", &[], "yarn 包管理"),
            ("npx", &[], "npx 临时执行"),
            ("python", &["-c"], "Python（禁 -c 内联）"),
            ("python3", &["-c"], "Python3（禁 -c 内联）"),
            ("py", &["-c"], "py 启动器（禁 -c 内联）"),
            ("pip", &[], "pip 包管理"),
            ("go", &[], "Go 工具链"),
            ("dotnet", &[], ".NET CLI"),
            ("java", &[], "JVM"),
            ("javac", &[], "Java 编译器"),
            ("mvn", &[], "Maven"),
            ("gradle", &[], "Gradle"),
            ("cmake", &[], "CMake"),
            ("ninja", &[], "Ninja"),
            ("make", &[], "make"),
            ("msbuild", &[], "MSBuild"),
            ("cl", &[], "MSVC 编译器"),
            ("link", &[], "MSVC 链接器"),
            ("clang", &[], "Clang"),
            ("clang++", &[], "Clang++"),
            ("gcc", &[], "GCC"),
            ("g++", &[], "G++"),
            ("ld", &[], "链接器"),
            ("objdump", &[], "反汇编/目标文件分析"),
            ("nm", &[], "符号表"),
            ("strings", &[], "字符串提取"),
            ("readelf", &[], "ELF 头解析"),
            ("r2", &[], "radare2"),
            ("rabin2", &[], "radare2 信息提取"),
            ("7z", &[], "7-Zip 压缩/解压"),
            ("tar", &[], "tar 归档"),
            ("ffmpeg", &[], "音视频转换"),
            ("magick", &[], "ImageMagick"),
            // 包管理器：安装软件属任务书明确要求的高自主性场景
            ("winget", &[], "Windows 包管理器"),
            ("choco", &[], "Chocolatey"),
            ("scoop", &[], "Scoop"),
            // 只读系统信息工具
            ("systeminfo", &[], "系统信息"),
            ("ipconfig", &[], "网络配置查询"),
            ("whoami", &[], "当前身份"),
            ("hostname", &[], "主机名"),
            ("tasklist", &[], "进程列表"),
            ("netstat", &[], "网络连接"),
            ("driverquery", &[], "驱动列表"),
            ("pnputil", &[], "驱动包管理"),
            ("powercfg", &[], "电源配置查询"),
            ("where", &[], "查找程序位置"),
        ];

        for (prog, forbid, note) in dev {
            rules.push(Rule {
                id: format!("allow-dev-{prog}"),
                decision: Decision::Allow,
                op: Some(OpKind::RunArgv),
                program: Some((*prog).to_string()),
                program_path: None,
                arg_prefix: vec![],
                arg_forbid: forbid.iter().map(|s| s.to_string()).collect(),
                target: None,
                cwd_roots: vec![],
                note: Some(format!("开发工具链自动允许（{note}）")),
            });
        }

        // ── 需要确认的高影响动作（命中即不会静默执行） ──
        for (id, prog, note) in [
            ("confirm-sc", "sc", "服务控制：需确认"),
            ("confirm-reg", "reg", "注册表修改：需确认"),
            ("confirm-netsh", "netsh", "网络/防火墙配置：需确认"),
            ("confirm-schtasks", "schtasks", "计划任务：需确认"),
            ("confirm-takeown", "takeown", "取得文件所有权：需确认"),
            ("confirm-icacls", "icacls", "修改文件 ACL：需确认"),
            ("confirm-bcdedit", "bcdedit", "引导配置：需确认"),
            ("confirm-diskpart", "diskpart", "磁盘管理：需确认"),
        ] {
            rules.push(Rule {
                id: id.to_string(),
                decision: Decision::Confirm,
                op: Some(OpKind::RunArgv),
                program: Some(prog.to_string()),
                program_path: None,
                arg_prefix: vec![],
                arg_forbid: vec![],
                target: None,
                cwd_roots: vec![],
                note: Some(note.into()),
            });
        }

        // ── 服务重启：只在 approved_services 内自动允许，其余确认 ──
        rules.push(Rule {
            id: "confirm-restart-service".into(),
            decision: Decision::Confirm,
            op: Some(OpKind::RestartService),
            program: None,
            program_path: None,
            arg_prefix: vec![],
            arg_forbid: vec![],
            target: Some("*".into()),
            cwd_roots: vec![],
            note: Some("重启未列入 approved_services 的服务：需确认".into()),
        });

        PolicyFile {
            version: POLICY_VERSION,
            mode: Mode::Autonomy,
            default_decision: Decision::Confirm,
            workspace_roots: vec![],
            approved_services: vec![],
            trusted_publishers: vec![],
            allow_legacy_shell: false,
            confirm_timeout_secs: 300,
            rules,
        }
    }

    /// 降级策略：除只读查询外全部拒绝（策略文件损坏/版本不支持时使用）
    pub fn locked_down() -> PolicyFile {
        PolicyFile {
            version: POLICY_VERSION,
            mode: Mode::Disabled,
            default_decision: Decision::Deny,
            workspace_roots: vec![],
            approved_services: vec![],
            trusted_publishers: vec![],
            allow_legacy_shell: false,
            confirm_timeout_secs: 300,
            rules: vec![],
        }
    }
}

impl Default for PolicyFile {
    fn default() -> Self {
        PolicyFile::default_autonomy()
    }
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            file: PolicyFile::default_autonomy(),
            state: LoadState::Loaded,
        }
    }

    fn argv(p: &str, args: &[&str]) -> (String, Vec<String>) {
        (
            p.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn eval_argv(p: &Policy, prog: &str, args: &[&str]) -> Verdict {
        let (_, a) = argv(prog, args);
        let act = Action::Argv {
            program: prog,
            args: &a,
            cwd: None,
        };
        p.evaluate(&act, &[])
    }

    // ---------- 默认策略：低风险自动允许 ----------

    #[test]
    fn dev_tools_are_allowed_by_default() {
        let p = policy();
        for (prog, args) in [
            ("git", vec!["status"]),
            ("cargo", vec!["test"]),
            ("npm", vec!["run", "build"]),
            ("python", vec!["-m", "pytest"]),
            ("systeminfo", vec![]),
        ] {
            let v = eval_argv(&p, prog, &args);
            assert_eq!(
                v.decision,
                Decision::Allow,
                "{prog} 应自动允许，实际 {:?} ({})",
                v.decision,
                v.rule_id
            );
        }
    }

    #[test]
    fn python_inline_script_is_not_auto_allowed() {
        // `python -c` 等价于任意代码，默认规则靠 arg_forbid 排除
        let p = policy();
        let v = eval_argv(&p, "python", &["-c", "import os; os.system('calc')"]);
        assert_ne!(
            v.decision,
            Decision::Allow,
            "python -c 不得被自动允许（rule={}）",
            v.rule_id
        );
    }

    #[test]
    fn node_eval_flags_are_not_auto_allowed() {
        let p = policy();
        for flag in ["-e", "--eval", "-p", "--print"] {
            let v = eval_argv(&p, "node", &[flag, "1+1"]);
            assert_ne!(v.decision, Decision::Allow, "node {flag} 不得自动允许");
        }
        // 普通脚本执行仍允许
        assert_eq!(
            eval_argv(&p, "node", &["script.js"]).decision,
            Decision::Allow
        );
    }

    // ---------- 内置保护不可覆盖 ----------

    #[test]
    fn builtin_denies_shell_hosts_even_if_rule_allows() {
        let mut p = policy();
        // 管理员写出「允许 cmd」的规则，内置保护仍必须先命中
        p.file.rules.insert(
            0,
            Rule {
                id: "evil-allow-cmd".into(),
                decision: Decision::Allow,
                op: Some(OpKind::RunArgv),
                program: Some("cmd".into()),
                program_path: None,
                arg_prefix: vec![],
                arg_forbid: vec![],
                target: None,
                cwd_roots: vec![],
                note: None,
            },
        );
        let v = eval_argv(&p, "cmd", &["/c", "whoami"]);
        assert_eq!(v.decision, Decision::Deny, "内置保护必须优先于 allow 规则");
        assert_eq!(v.rule_id, "builtin:shell-host");
    }

    #[test]
    fn builtin_denies_destructive_disk_ops() {
        let p = policy();
        let v = eval_argv(&p, "format", &["C:"]);
        assert_eq!(v.decision, Decision::Deny);
        let v = eval_argv(&p, "diskpart", &[]);
        assert_eq!(v.decision, Decision::Deny);
        let v = eval_argv(&p, "bcdedit", &["/set", "{default}", "safeboot", "minimal"]);
        assert_eq!(v.decision, Decision::Deny);
    }

    #[test]
    fn builtin_denies_service_persistence() {
        let p = policy();
        for (prog, args) in [
            ("sc", vec!["create", "Evil", "binPath=", "C:\\evil.exe"]),
            ("sc", vec!["delete", "AI-Root"]),
            ("schtasks", vec!["/create", "/tn", "x", "/tr", "y"]),
        ] {
            let v = eval_argv(&p, prog, &args);
            assert_eq!(v.decision, Decision::Deny, "{prog} {args:?} 应被内置保护拒绝");
        }
    }

    #[test]
    fn builtin_denies_account_creation() {
        let p = policy();
        let v = eval_argv(&p, "net", &["user", "backdoor", "P@ss", "/add"]);
        assert_eq!(v.decision, Decision::Deny);
        let v = eval_argv(&p, "net", &["localgroup", "administrators", "backdoor", "/add"]);
        assert_eq!(v.decision, Decision::Deny);
    }

    #[test]
    fn builtin_allows_readonly_net_query() {
        // 只读查询不该被账户保护误伤
        let p = policy();
        let v = eval_argv(&p, "net", &["user"]);
        assert_ne!(v.decision, Decision::Deny, "net user 查询不应被拒绝");
    }

    #[test]
    fn builtin_protects_airoot_own_paths() {
        let p = policy();
        let protected = vec![PathBuf::from(r"C:\ProgramData\AI-Root")];
        let (_, args) = argv("icacls", &[r"C:\ProgramData\AI-Root\token.txt", "/grant", "Everyone:F"]);
        let act = Action::Argv {
            program: "icacls",
            args: &args,
            cwd: None,
        };
        let v = p.evaluate(&act, &protected);
        assert_eq!(v.decision, Decision::Deny, "触碰令牌文件的请求必须被拒");
        assert_eq!(v.rule_id, "builtin:self-protect");
    }

    #[test]
    fn self_protect_handles_backslash_and_case() {
        let p = policy();
        let protected = vec![PathBuf::from(r"C:\ProgramData\AI-Root")];
        let (_, args) = argv("7z", &[r"c:\programdata\ai-root\policy.json"]);
        let act = Action::Argv {
            program: "7z",
            args: &args,
            cwd: None,
        };
        assert_eq!(
            p.evaluate(&act, &protected).decision,
            Decision::Deny,
            "大小写/分隔符差异不应绕过自我保护"
        );
    }

    #[test]
    fn builtin_denies_update_self_op_from_client() {
        let p = policy();
        let act = Action::Op {
            op: OpKind::UpdateSelf,
            target: None,
        };
        let v = p.evaluate(&act, &[]);
        assert_eq!(v.decision, Decision::Deny);
        assert_eq!(v.rule_id, "builtin:update-gated");
    }

    // ---------- shell 动作 ----------

    #[test]
    fn shell_requires_confirmation_by_default() {
        let p = policy();
        let act = Action::Shell {
            command: "whoami",
            cwd: None,
        };
        let v = p.evaluate(&act, &[]);
        assert_eq!(
            v.decision,
            Decision::Confirm,
            "传统 shell 应默认需确认（rule={}）",
            v.rule_id
        );
    }

    #[test]
    fn shell_destructive_is_denied_not_confirmed() {
        let p = policy();
        for cmd in [
            "format C: /q",
            "diskpart /s x.txt",
            "bcdedit /set {default} safeboot minimal",
            "wevtutil cl System",
            "net user hacker P@ss /add",
            "sc create Evil binPath= C:\\evil.exe",
        ] {
            let act = Action::Shell { command: cmd, cwd: None };
            let v = p.evaluate(&act, &[]);
            assert_eq!(
                v.decision,
                Decision::Deny,
                "破坏性 shell `{cmd}` 必须 Deny（确认也不能放行），实际 {:?}",
                v.decision
            );
        }
    }

    #[test]
    fn shell_benign_command_is_not_denied() {
        let p = policy();
        // `net user` 查询不应因账户保护被误杀
        for cmd in ["net user", "dir C:\\", "echo hello", "format"] {
            let act = Action::Shell { command: cmd, cwd: None };
            let v = p.evaluate(&act, &[]);
            assert_ne!(v.decision, Decision::Deny, "`{cmd}` 不应被 Deny");
        }
    }

    #[test]
    fn shell_deny_cannot_be_bypassed_by_confirm_allow_flag() {
        // allow_legacy_shell 只是「免确认」，不能越过 Deny
        let mut p = policy();
        p.file.allow_legacy_shell = true;
        let act = Action::Shell {
            command: "format D:",
            cwd: None,
        };
        assert_eq!(p.evaluate(&act, &[]).decision, Decision::Deny);
    }

    // ---------- 模式 ----------

    #[test]
    fn disabled_mode_denies_everything_but_readonly() {
        let mut p = policy();
        p.file.mode = Mode::Disabled;
        assert_eq!(
            eval_argv(&p, "git", &["status"]).decision,
            Decision::Deny,
            "disabled 模式应拒绝开发命令"
        );
        let act = Action::Op {
            op: OpKind::QuerySystemInfo,
            target: None,
        };
        assert_eq!(
            p.evaluate(&act, &[]).decision,
            Decision::Allow,
            "只读查询在 disabled 模式下仍应可用"
        );
    }

    #[test]
    fn strict_mode_denies_unmatched_argv() {
        let mut p = policy();
        p.file.mode = Mode::Strict;
        p.file.rules.clear();
        assert_eq!(eval_argv(&p, "git", &["status"]).decision, Decision::Deny);
    }

    #[test]
    fn balanced_mode_requires_cwd_inside_workspace() {
        let mut p = policy();
        p.file.mode = Mode::Balanced;
        p.file.workspace_roots = vec![r"D:\project".into()];

        let (_, args) = argv("cargo", &["build"]);
        // 无 cwd → 确认
        let v = p.evaluate(
            &Action::Argv {
                program: "cargo",
                args: &args,
                cwd: None,
            },
            &[],
        );
        assert_eq!(v.decision, Decision::Confirm);

        // cwd 在根内 → 允许
        let v = p.evaluate(
            &Action::Argv {
                program: "cargo",
                args: &args,
                cwd: Some(r"D:\project\ai"),
            },
            &[],
        );
        assert_eq!(v.decision, Decision::Allow);

        // cwd 不在根内 → 确认
        let v = p.evaluate(
            &Action::Argv {
                program: "cargo",
                args: &args,
                cwd: Some(r"C:\Windows\System32"),
            },
            &[],
        );
        assert_eq!(v.decision, Decision::Confirm);
    }

    // ---------- 规则匹配精确性 ----------

    #[test]
    fn arg_prefix_is_strict_and_case_sensitive() {
        let mut p = policy();
        p.file.rules.clear();
        p.file.rules.push(Rule {
            id: "only-git-status".into(),
            decision: Decision::Allow,
            op: Some(OpKind::RunArgv),
            program: Some("git".into()),
            program_path: None,
            arg_prefix: vec!["status".into()],
            arg_forbid: vec![],
            target: None,
            cwd_roots: vec![],
            note: None,
        });
        assert_eq!(eval_argv(&p, "git", &["status"]).decision, Decision::Allow);
        // 额外参数（前缀之后）仍算匹配 —— 这是 arg_prefix 的语义
        assert_eq!(
            eval_argv(&p, "git", &["status", "--short"]).decision,
            Decision::Allow
        );
        // 大小写不同 → 不匹配
        assert_ne!(eval_argv(&p, "git", &["STATUS"]).decision, Decision::Allow);
        // 不同子命令 → 不匹配
        assert_ne!(eval_argv(&p, "git", &["push"]).decision, Decision::Allow);
    }

    #[test]
    fn rule_cannot_match_when_cwd_missing_but_required() {
        let mut p = policy();
        p.file.rules.clear();
        p.file.rules.push(Rule {
            id: "only-in-workspace".into(),
            decision: Decision::Allow,
            op: Some(OpKind::RunArgv),
            program: Some("cargo".into()),
            program_path: None,
            arg_prefix: vec![],
            arg_forbid: vec![],
            target: None,
            cwd_roots: vec![r"D:\project".into()],
            note: None,
        });
        let (_, args) = argv("cargo", &["build"]);
        assert_eq!(
            p.evaluate(
                &Action::Argv {
                    program: "cargo",
                    args: &args,
                    cwd: None
                },
                &[]
            )
            .decision,
            Decision::Confirm,
            "规则要求 cwd 但请求未给 → 不得放行"
        );
    }

    #[test]
    fn program_match_ignores_path_and_exe_suffix() {
        let p = policy();
        // 带路径、带 .exe、大小写不同，都应命中 allow-dev-git
        let v = eval_argv(&p, r"C:\Program Files\Git\bin\GIT.EXE", &["status"]);
        assert_eq!(v.decision, Decision::Allow, "rule={}", v.rule_id);
    }

    #[test]
    fn deny_rule_wins_over_allow_rule() {
        let mut p = policy();
        p.file.rules.push(Rule {
            id: "deny-git-push".into(),
            decision: Decision::Deny,
            op: Some(OpKind::RunArgv),
            program: Some("git".into()),
            program_path: None,
            arg_prefix: vec!["push".into()],
            arg_forbid: vec![],
            target: None,
            cwd_roots: vec![],
            note: Some("禁止 push".into()),
        });
        assert_eq!(
            eval_argv(&p, "git", &["push", "origin", "main"]).decision,
            Decision::Deny
        );
        assert_eq!(eval_argv(&p, "git", &["status"]).decision, Decision::Allow);
    }

    // ---------- 加载 / 校验 ----------

    #[test]
    fn validate_rejects_duplicate_rule_ids() {
        let mut f = PolicyFile::default_autonomy();
        f.rules.push(f.rules[0].clone());
        assert!(f.validate().unwrap_err().contains("重复"));
    }

    #[test]
    fn validate_rejects_too_broad_allow_rule() {
        let mut f = PolicyFile::default_autonomy();
        f.rules.push(Rule {
            id: "allow-all".into(),
            decision: Decision::Allow,
            op: None,
            program: None,
            program_path: None,
            arg_prefix: vec![],
            arg_forbid: vec![],
            target: None,
            cwd_roots: vec![],
            note: None,
        });
        assert!(f.validate().unwrap_err().contains("过于宽泛"));
    }

    #[test]
    fn validate_rejects_relative_program_path() {
        let mut f = PolicyFile::default_autonomy();
        f.rules.push(Rule {
            id: "rel".into(),
            decision: Decision::Allow,
            op: Some(OpKind::RunArgv),
            program: None,
            program_path: Some("git.exe".into()),
            arg_prefix: vec![],
            arg_forbid: vec![],
            target: None,
            cwd_roots: vec![],
            note: None,
        });
        assert!(f.validate().unwrap_err().contains("绝对路径"));
    }

    #[test]
    fn validate_rejects_bad_publisher_thumbprint() {
        let mut f = PolicyFile::default_autonomy();
        f.trusted_publishers = vec!["not-a-thumbprint".into()];
        assert!(f.validate().is_err());
    }

    #[test]
    fn normalize_handles_dots_case_and_backslashes() {
        assert_eq!(normalize_win_path(r"C:\A\B\..\C"), "c:/a/c");
        assert_eq!(normalize_win_path(r"\\?\C:\X"), "c:/x");
        assert_eq!(normalize_win_path("C:/A/"), "c:/a");
    }

    #[test]
    fn path_boundary_prevents_prefix_confusion() {
        // `C:\project-evil` 不是 `C:\project` 的子目录
        assert!(!path_under_any_ci(
            r"C:\project-evil\x",
            &[r"C:\project".into()]
        ));
        assert!(path_under_any_ci(
            r"C:\project\x",
            &[r"C:\project".into()]
        ));
        assert!(path_under_any_ci(r"C:\PROJECT\X", &[r"c:\project".into()]));
    }

    #[test]
    fn target_prefix_matching() {
        assert!(match_target("*", "anything"));
        assert!(!match_target("svc:*", "other"));
        // `svc:*` 语义为路径/名前缀，此处仅验证精确分支不误判
        assert!(match_target("Spooler", "spooler"));
    }
}
