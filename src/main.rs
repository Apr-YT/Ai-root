//! AI-Root —— 入口与子命令分发
//!
//! 子命令：
//!   install    注册为 LocalSystem 自启服务（需管理员）
//!   uninstall  卸载服务（需管理员）
//!   start      启动服务（需管理员）
//!   stop       停止服务（需管理员）
//!   status     查询服务状态
//!   server     前台管道服务模式（当前进程直接运行，演练/多实例用）
//!   run <cmd>  以客户端身份连接管道并执行命令（非提权即可）
//!   --service  SCM 派发进入服务主循环（用户勿直接调用）

mod common;
mod config;
mod consent;
mod exec;
mod policy;
mod protocol;
mod server;
mod service;
mod signature;
mod winapi_raw;

use std::io::IsTerminal;
use std::process::exit;
use std::thread;
use std::time::Duration;
use windows_service::service::ServiceState;

// ============================================================
// 终端着色：仅交互式终端生效，管道/重定向自动降级，尊重 NO_COLOR/CLICOLOR
// ============================================================

/// 是否对 stdout 启用 ANSI 颜色：
/// - NO_COLOR 存在（任意值，含空）→ 禁用（https://no-color.org）
/// - CLICOLOR=0 → 禁用
/// - 仅当 stdout 真正接终端（is_terminal）才上色；管道/文件重定向一律纯文本，
///   避免把 ESC 序列写进日志或污染 AI 消费流（AI 走 `run --json`，本保护是双保险）。
fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(v) = std::env::var("CLICOLOR") {
        if v == "0" {
            return false;
        }
    }
    std::io::stdout().is_terminal()
}

/// 用 ANSI 转义包裹文本（仅当 on=true）。每段独立 reset，互不串色。
fn c(code: &str, s: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

// 固定语义色（与终端主题无关）
const C_DIM: &str = "2"; // 暗灰：时间戳、caller、附注
const C_BOLD: &str = "1"; // 加粗
const C_CYAN: &str = "36"; // 开始 / 进行
const C_GREEN: &str = "32"; // 成功
const C_RED: &str = "31"; // 失败 / 错误
const C_YELLOW: &str = "33"; // 警告 / 待定
const C_BLUE: &str = "34"; // 服务事件

/// `AI-Root tail` 启动回放历史时最多显示的最近行数（避免打开即刷屏）
const TAIL_INITIAL_MAX: usize = 200;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        // 无参数（典型：双击 exe）：打印帮助并等待按键，防止终端一闪而过。
        // 管道/重定向场景 stdin 为 EOF，read 立即返回，不阻塞 AI 调用。
        let on = color_enabled();
        println!();
        println!("{}", c(C_BOLD, "AI-Root 终端版", on));
        println!(
            "以 SYSTEM 执行命令请用 {}；",
            c(C_CYAN, "AI-Root run <命令>", on)
        );
        println!(
            "实时查看 AI 操作可用 {}。命令行完整用法见下方帮助。",
            c(C_CYAN, "AI-Root tail", on)
        );
        println!();
        print_help(on);
        println!();
        println!("{}", c(C_DIM, "按任意键退出...", on));
        wait_key();
        return;
    }

    let cmd = args[1].as_str();
    // CLI 输出在交互式终端下可见；启动前附加父进程控制台使输出可见；
    // 管道/重定向场景标准句柄有效，AttachConsole 不破坏；
    // 父进程无控制台（如 SCM 服务、计划任务）时静默忽略。
    attach_parent_console();
    let result: Result<(), String> = match cmd {
        "-h" | "--help" | "help" => {
            print_help(color_enabled());
            Ok(())
        }
        "--service" => service::run_as_service().map_err(|e| e.to_string()),
        "install" => service::install().map_err(|e| e.to_string()),
        "grant" => {
            // 对已存在的服务补发用户 STOP/START 授权（需管理员；deploy.ps1 部署后自动调用一次）
            service::grant_acl().map_err(|e| e.to_string())
        }
        "uninstall" => service::uninstall().map_err(|e| e.to_string()),
        "rotate-token" => {
            // 轮换共享令牌：生成新令牌写盘，服务端下一请求自动热加载（无需重启）。
            // 需对 token.txt 有写权限（管理员/SYSTEM）；输出仅打印一次，勿写入日志。
            match common::rotate_token() {
                Ok(tok) => {
                    println!("[ok] 令牌已轮换（服务端自动热加载，旧令牌立即失效）");
                    println!("新令牌: {tok}");
                    println!("[安全] 令牌等同 SYSTEM 凭据，请妥善传递，勿写入日志/提交仓库");
                    Ok(())
                }
                Err(e) => Err(format!("令牌轮换失败: {e}")),
            }
        }
        "start" => service::start_svc().map_err(|e| e.to_string()),
        "stop" => service::stop_svc().map_err(|e| e.to_string()),
        "update" => {
            // 自主更新服务二进制（需 install 时已对当前用户授权 STOP/START）：
            // 停止服务 -> 覆盖 exe -> 启动服务。AI 一条命令完成，无需管理员。
            if args.len() < 3 {
                eprintln!("用法: AI-Root update <新exe路径>");
                exit(1);
            }
            service::update_self(std::path::Path::new(&args[2])).map_err(|e| e.to_string())
        }
        "status" => {
            // 概览：服务状态 + 启用标志 + 管道名 + 令牌就绪情况，一行一属性，带语义色。
            let enabled = common::is_enabled();
            let pipe = common::pipe_name();
            let token_ok = common::read_token().is_some();
            let on = color_enabled();
            match service::query_status() {
                Ok(state) => {
                    let (label, code) = match state {
                        ServiceState::Running => ("运行中", C_GREEN),
                        ServiceState::Stopped => ("已停止", C_DIM),
                        ServiceState::StartPending | ServiceState::StopPending => {
                            ("◐ 启动/停止中…", C_YELLOW)
                        }
                        ServiceState::ContinuePending
                        | ServiceState::PausePending
                        | ServiceState::Paused => ("◐ 挂起/暂停", C_YELLOW),
                    };
                    println!("{} {}", c(C_BOLD, "服务状态:", on), c(code, label, on));
                    println!(
                        "  {} {}",
                        c(C_DIM, "启用标志:", on),
                        if enabled {
                            c(C_GREEN, "开 (enabled.flag 存在)", on)
                        } else {
                            c(C_RED, "关 (enabled.flag 不存在)", on)
                        }
                    );
                    println!("  {} {}", c(C_DIM, "管道名称:", on), pipe);
                    println!(
                        "  {} {}",
                        c(C_DIM, "共享令牌:", on),
                        if token_ok {
                            c(C_GREEN, "已就绪", on)
                        } else {
                            c(C_RED, "缺失 (需 AI-Root install)", on)
                        }
                    );
                }
                Err(_) => {
                    println!(
                        "{} {}",
                        c(C_BOLD, "服务状态:", on),
                        c(C_RED, "未安装 / 不可访问", on)
                    );
                }
            }
            Ok(())
        }
        "server" => {
            server::run_pipe_server_forever();
            Ok(())
        }
        "--version" | "-V" | "version" => {
            println!("AI-Root {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "run" => match parse_and_run(&args[2..]) {
            Ok(code) => exit(code),
            Err(e) => Err(e),
        },
        "exec" => match parse_exec(&args[2..]) {
            Ok(code) => exit(code),
            Err(e) => Err(e),
        },
        "op" => match parse_op(&args[2..]) {
            Ok(code) => exit(code),
            Err(e) => Err(e),
        },
        "pending" | "approvals" => {
            show_pending();
            Ok(())
        }
        "approve" => do_approve(args.get(2).map(|s| s.as_str())),
        "deny" => do_deny(args.get(2).map(|s| s.as_str())),
        "policy" => show_policy(),
        "config" => match args.get(2).map(|s| s.as_str()) {
            Some("check") => config_check(),
            Some("show") => config_show(args.iter().any(|a| a == "--json")),
            Some("path") => {
                config_path();
                Ok(())
            }
            Some("init") => config_init(),
            Some("migrate") => config_migrate(),
            _ => Err("用法: AI-Root config <check|show|path|init|migrate>".into()),
        },
        "signature" | "sig" => show_signature(args.get(2).map(|s| s.as_str())),
        "trust-publisher" => do_trust_publisher(args.get(2).map(|s| s.as_str())),
        "gui" => Err("本构建为纯终端版，未包含 GUI 控制面板。".to_string()),
        "tail" => {
            // 实时跟随 events.jsonl（增量读），终端实时显示 AI 操作（exec_start/exec_done 等）。
            // 支持 --caller/--type/--since 过滤，AI 可快速锁定特定事件。
            // Ctrl+C 退出；Ctrl+C 无法在 Rust 中优雅捕获，直接终止即可（无副作用）。
            tail_events(&args[2..]);
            Ok(())
        }
        _ => {
            print_help(color_enabled());
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("[错误] {e}");
        exit(1);
    }
}

/// 一次请求的完整描述（客户端侧）
struct CallSpec {
    op: Option<String>,
    argv: Option<Vec<String>>,
    command: Option<String>,
    target: Option<String>,
    cwd: Option<String>,
    timeout_ms: u64,
}

/// 统一的请求发送与结果打印。返回进程退出码。
///
/// 退出码约定（与 README 一致）：
/// - `0..`: 被执命令自身的退出码
/// - `3`  : **需要人工确认**（响应 decision=confirm，附 request id）
/// - `1`  : 客户端/中继层错误（未安装、令牌缺失、策略拒绝、连接失败…）
fn client_call(spec: CallSpec, json_mode: bool) -> i32 {
    let token = match common::read_token() {
        Some(t) => t,
        None => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "exit_code": -1, "stdout": "", "stderr": "",
                        "error": "未找到令牌，请先以管理员运行: AI-Root install"})
                );
            } else {
                eprintln!("[错误] 未找到令牌，请先以管理员运行: AI-Root install");
            }
            return 1;
        }
    };

    let mut file = match protocol::connect_client() {
        Ok(f) => f,
        Err(e) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "exit_code": -1, "stdout": "", "stderr": "",
                        "error": format!("连接失败: {e}")})
                );
            } else {
                eprintln!("[错误] 连接失败: {e}");
            }
            return 1;
        }
    };

    let req = protocol::Request {
        token,
        v: Some(protocol::PROTOCOL_VERSION),
        op: spec.op,
        argv: spec.argv,
        command: spec.command,
        target: spec.target,
        cwd: spec.cwd,
        timeout_ms: Some(spec.timeout_ms),
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[错误] 序列化失败: {e}");
            return 1;
        }
    };
    if let Err(e) = protocol::write_frame(&mut file, &bytes) {
        eprintln!("[错误] 发送失败: {e}");
        return 1;
    }

    // 读超时：必须覆盖「命令执行超时」+ 传输余量。
    // 固定 60s 会让 `--timeout 300000` 的请求在 60s 就被客户端放弃，
    // 而服务端仍在跑（表现为「命令跑了但调用方超时」）。
    let read_budget = spec.timeout_ms.saturating_add(30_000);
    let resp_bytes = match protocol::read_frame_timeout(&mut file, read_budget) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[错误] 读取响应失败: {e}");
            return 1;
        }
    };
    let resp: protocol::Response = match serde_json::from_slice(&resp_bytes) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[错误] 响应解析失败");
            return 1;
        }
    };

    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "ok": resp.ok,
                "exit_code": resp.exit_code,
                "stdout": resp.stdout,
                "stderr": resp.stderr,
                "error": resp.error,
                "truncated": resp.truncated,
                "run_id": resp.run_id,
                "duration_ms": resp.duration_ms,
                "caller": resp.caller,
                "timed_out": resp.timed_out,
                "decision": resp.decision,
                "rule_id": resp.rule_id,
                "reason": resp.reason,
                "confirmation_id": resp.confirmation_id,
            })
        );
        // 需要确认时也给专用退出码，便于 AI 分支处理
        if resp.decision.as_deref() == Some("confirm") {
            return 3;
        }
        return if resp.ok { resp.exit_code } else { 1 };
    }

    let on = color_enabled();

    // 需要确认：把「下一步该做什么」直接打出来，不要让人去翻文档
    if resp.decision.as_deref() == Some("confirm") {
        if let Some(e) = &resp.error {
            eprintln!("{}", c(C_YELLOW, e, on));
        }
        if let Some(rid) = &resp.confirmation_id {
            eprintln!();
            eprintln!(
                "{}",
                c(
                    C_YELLOW,
                    &format!("等待管理员批准后，重新执行同一条命令即可。请求 ID: {rid}"),
                    on
                )
            );
        }
        return 3;
    }

    if !resp.stdout.is_empty() {
        println!("{}", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}", c(C_RED, &resp.stderr, on));
        if !resp.stderr.ends_with('\n') {
            eprintln!();
        }
    }
    if resp.truncated {
        eprintln!("{}", c(C_YELLOW, "[注意] 输出超过 1MB 已被截断", on));
    }
    if resp.timed_out {
        eprintln!(
            "{}",
            c(
                C_YELLOW,
                "[超时] 命令执行超过时限被终止（非命令自身报错）",
                on
            )
        );
    }
    if let Some(e) = &resp.error {
        // 策略拒绝时顺便回显命中的规则，便于定位该调哪条策略
        let extra = resp
            .rule_id
            .as_deref()
            .map(|r| format!(" [规则 {r}]"))
            .unwrap_or_default();
        eprintln!("{}", c(C_RED, &format!("[relay error]{extra} {e}"), on));
        return 1;
    }
    resp.exit_code
}

/// `AI-Root exec [--cwd D] [--timeout MS] [--json] [--] <程序> [参数...]`
///
/// 结构化执行：程序与参数以数组传给 CreateProcess，**不经 cmd.exe**。
/// 策略可以按「程序 + 参数前缀 + 目录」精确授权，因此这是高自主性模式的主路径。
fn parse_exec(args: &[String]) -> Result<i32, String> {
    let mut cwd: Option<String> = None;
    let mut timeout_ms = default_timeout_ms();
    let mut json_mode = false;
    let mut rest: Vec<String> = Vec::new();
    let mut after_dd = false;

    let mut i = 0;
    while i < args.len() {
        if after_dd {
            rest.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_dd = true,
            "--cwd" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --cwd 参数值".into());
                }
                cwd = Some(args[i].clone());
            }
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --timeout 参数值".into());
                }
                timeout_ms = args[i]
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout 参数无效: {}", args[i]))?;
            }
            "--json" => json_mode = true,
            other => rest.push(other.to_string()),
        }
        i += 1;
    }

    if rest.is_empty() {
        return Err("用法: AI-Root exec [--json] [--cwd D] [--timeout MS] -- <程序> [参数...]".into());
    }

    Ok(client_call(
        CallSpec {
            op: Some("run_argv".into()),
            argv: Some(rest),
            command: None,
            target: None,
            cwd,
            timeout_ms,
        },
        json_mode,
    ))
}

/// `AI-Root op <操作名> [--target X] [--json] [--timeout MS]`
///
/// 结构化操作：不涉及命令字符串，参数由服务端按固定语义处理。
fn parse_op(args: &[String]) -> Result<i32, String> {
    let mut json_mode = false;
    let mut target: Option<String> = None;
    let mut timeout_ms = default_timeout_ms();
    let mut name: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json_mode = true,
            "--target" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --target 参数值".into());
                }
                target = Some(args[i].clone());
            }
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --timeout 参数值".into());
                }
                timeout_ms = args[i]
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout 参数无效: {}", args[i]))?;
            }
            other if name.is_none() => name = Some(other.to_string()),
            other => return Err(format!("多余参数: {other}")),
        }
        i += 1;
    }

    let Some(op) = name else {
        return Err(
            "用法: AI-Root op <query_system_info|query_service|restart_service|read_state_file> [--target X]"
                .into(),
        );
    };

    Ok(client_call(
        CallSpec {
            op: Some(op),
            argv: None,
            command: None,
            target,
            cwd: None,
            timeout_ms,
        },
        json_mode,
    ))
}

// ============================================================
// 确认流程（管理员侧）
// ============================================================

/// 当前进程是否已提权（管理员且已通过 UAC）。
///
/// 用 `whoami /groups` 里的完整性级别 SID `S-1-16-12288`（High）判断：
/// 组成员身份与提权是两件事 —— 用户可能属于 Administrators 但进程是 filtered token，
/// 那种情况下仍然**不能**写受 ACL 保护的目录。SID 是 ASCII，不受系统语言影响。
fn is_elevated() -> bool {
    std::process::Command::new("whoami")
        .args(["/groups", "/fo", "csv", "/nh"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("S-1-16-12288"))
        .unwrap_or(false)
}

fn require_admin() -> Result<(), String> {
    if is_elevated() {
        Ok(())
    } else {
        Err("此操作需要管理员权限：请以「管理员身份运行」的终端执行（UAC 提权后重试）".into())
    }
}

fn show_pending() {
    let dir = common::data_dir();
    let list = consent::list(&dir);
    let on = color_enabled();
    if list.is_empty() {
        println!("{}", c(C_DIM, "没有待确认请求", on));
        return;
    }
    println!(
        "{}",
        c(C_BOLD, &format!("待确认请求 {} 条:", list.len()), on)
    );
    for r in list {
        println!();
        println!("  {} {}", c(C_CYAN, "请求 ID:", on), c(C_BOLD, &r.id, on));
        println!("    时间  : {}", r.created_at);
        println!("    发起方: {} (pid={})", r.caller, r.caller_pid);
        println!("    操作  : {}", r.op);
        println!("    参数  : {}", r.params);
        if let Some(t) = &r.target {
            println!("    目标  : {t}");
        }
        println!("    风险  : {}", c(C_YELLOW, &r.risk, on));
        println!(
            "    操作  : {}  /  {}",
            c(C_CYAN, &format!("AI-Root approve {}", r.id), on),
            c(C_CYAN, &format!("AI-Root deny {}", r.id), on)
        );
    }
}

fn do_approve(id: Option<&str>) -> Result<(), String> {
    let Some(id) = id else {
        return Err("用法: AI-Root approve <请求ID>（用 AI-Root pending 查看）".into());
    };
    require_admin()?;
    let dir = common::data_dir();
    let r = consent::approve(&dir, id)?;
    println!("[ok] 已批准请求 {}", r.id);
    println!("  操作  : {}", r.op);
    println!("  参数  : {}", r.params);
    println!("  风险  : {}", r.risk);
    println!("  提示  : 现在重新执行原命令即可（参数必须完全一致，否则会生成新的确认请求）");
    Ok(())
}

fn do_deny(id: Option<&str>) -> Result<(), String> {
    let Some(id) = id else {
        return Err("用法: AI-Root deny <请求ID>".into());
    };
    require_admin()?;
    let dir = common::data_dir();
    consent::deny(&dir, id)?;
    println!("[ok] 已拒绝请求 {id}");
    Ok(())
}

// ============================================================
// 策略 / 签名
// ============================================================

fn show_policy() -> Result<(), String> {
    let paths = config::Paths::resolve();
    let on = color_enabled();
    let eff = config::load_readonly(&paths);
    let p = &eff.policy;

    println!("{}", c(C_BOLD, "AI-Root 授权策略", on));
    println!("  策略文件: {}", paths.policy().display());
    println!("  规则目录: {}", paths.rules_dir().display());
    println!(
        "  用户偏好: {}（{}）",
        paths.user_file().display(),
        eff.layers.user.label()
    );
    println!(
        "  状态    : {}",
        match &p.state {
            policy::LoadState::Loaded => c(C_GREEN, "正常", on),
            policy::LoadState::CreatedDefault => c(C_YELLOW, "已生成默认策略", on),
            policy::LoadState::Degraded(e) =>
                c(C_RED, &format!("降级（仅只读查询可用）：{e}"), on),
        }
    );
    println!("  模式    : {}", p.file.mode.as_str());
    println!("  默认决策: {}", p.file.default_decision.as_str());
    println!(
        "  可信发布者: {}",
        if p.file.trusted_publishers.is_empty() {
            c(C_RED, "（空 → update 一律拒绝）", on)
        } else {
            c(C_GREEN, &format!("{} 个", p.file.trusted_publishers.len()), on)
        }
    );
    if p.file.approved_services.is_empty() {
        println!("  已批准服务: 空（restart_service 需逐次确认）");
    } else {
        println!("  已批准服务: {}", p.file.approved_services.join(", "));
    }
    println!("  规则数  : {}", p.file.rules.len());
    println!();
    println!("{}", c(C_BOLD, "自动允许的程序（argv 模式）:", on));
    let mut allows: Vec<&str> = p
        .file
        .rules
        .iter()
        .filter(|r| r.decision == policy::Decision::Allow)
        .filter_map(|r| r.program.as_deref())
        .collect();
    allows.sort_unstable();
    if allows.is_empty() {
        println!("  （无）");
    } else {
        println!("  {}", allows.join(", "));
    }
    if !eff.notes.is_empty() {
        println!();
        println!("{}", c(C_BOLD, "说明：", on));
        for n in &eff.notes {
            println!("  - {n}");
        }
    }
    println!();
    println!(
        "{}",
        c(
            C_DIM,
            "改配置后执行 `AI-Root config check`；策略/规则文件受 ACL 保护，需管理员编辑",
            on
        )
    );
    Ok(())
}

fn show_signature(path: Option<&str>) -> Result<(), String> {
    let Some(path) = path else {
        return Err("用法: AI-Root signature <exe 路径>".into());
    };
    let p = std::path::Path::new(path);
    // 只读加载：展示签名不应创建策略文件
    let trusted = config::load_readonly(&config::Paths::resolve())
        .policy
        .file
        .trusted_publishers;
    let r = signature::verify_file(p, &trusted);
    let on = color_enabled();
    println!("{}", c(C_BOLD, &format!("签名信息: {}", r.path.display()), on));
    println!(
        "  Windows 信任链: {}",
        if r.chain_trusted {
            c(C_GREEN, "通过（嵌入签名有效）", on)
        } else {
            c(
                C_RED,
                &format!("未通过（{}）", r.trust_error.as_deref().unwrap_or("-")),
                on,
            )
        }
    );
    println!(
        "  签名者      : {}",
        r.signer_subject.as_deref().unwrap_or("<无签名>")
    );
    println!(
        "  证书 SHA-256: {}",
        r.signer_thumbprint.as_deref().unwrap_or("-")
    );
    println!(
        "  在信任列表  : {}",
        if r.publisher_allowed {
            c(C_GREEN, "是", on)
        } else {
            c(C_RED, "否", on)
        }
    );
    if let Some(h) = &r.file_sha256 {
        println!("  文件 SHA-256: {h}");
    }
    if !r.chain_trusted {
        println!();
        println!(
            "{}",
            c(
                C_DIM,
                "说明：catalog 签名（System32 常见）不被接受；更新包必须做 Authenticode 嵌入签名。",
                on
            )
        );
    }
    Ok(())
}

fn do_trust_publisher(path: Option<&str>) -> Result<(), String> {
    let Some(path) = path else {
        return Err("用法: AI-Root trust-publisher <已签名的 AI-Root.exe>".into());
    };
    require_admin()?;
    let p = std::path::Path::new(path);
    let r = signature::verify_file(p, &[]);
    if !r.chain_trusted {
        return Err(format!(
            "该文件未通过签名验证（{}），不能作为信任根。\n\
             提示：必须是 Authenticode 嵌入签名；未签名/catalog 签名均不被接受。",
            r.trust_error.as_deref().unwrap_or("未知原因")
        ));
    }
    let Some(thumb) = r.signer_thumbprint.clone() else {
        return Err("无法提取签名者证书指纹".into());
    };
    let on = color_enabled();
    println!("发布者: {}", r.signer_subject.as_deref().unwrap_or("<未知>"));
    println!("证书 SHA-256: {thumb}");
    let t = config::add_trusted_publisher(&config::Paths::resolve(), &thumb)?;
    println!(
        "{}",
        c(
            C_GREEN,
            &format!("[ok] 已加入可信发布者列表，此后仅接受该证书签名的更新包（{t}）"),
            on
        )
    );
    Ok(())
}

/// 解析 `run` 子命令参数，装载命令文本后交给 client_run。
///
/// 支持调用形态：
///   AI-Root run "单行命令"
///   AI-Root run --file <脚本路径>   读取脚本文件（多行）
///   AI-Root run --stdin            从标准输入读取脚本（多行）
///   AI-Root run --timeout <毫秒>   覆盖默认 60s 超时（上限 10 分钟，服务端钳制）
///   AI-Root run --cwd <目录>       指定命令工作目录（默认 SYSTEM 的 System32）
///   AI-Root run --json             结构化 JSON 输出（AI 消费方推荐）
///   AI-Root run -- <命令>          显式分隔符：`--` 之后全部原样视为命令（含 - 开头参数）
///
/// 含换行符的内容会被服务端识别为「脚本模式」，此时 `&`/`&&`/`|`/`()` 恢复 cmd 正常语义，
/// 这是 AI 批量自治多命令时的推荐路径。
fn parse_and_run(args: &[String]) -> Result<i32, String> {
    use std::io::Read;
    let mut file_path: Option<String> = None;
    let mut stdin_mode = false;
    let mut timeout_ms: u64 = default_timeout_ms();
    let mut json_mode = false;
    let mut cwd: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut after_dashdash = false;

    let mut i = 0;
    while i < args.len() {
        if after_dashdash {
            positional.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => after_dashdash = true, // 之后全部为命令原文
            "-f" | "--file" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 -f/--file 参数".to_string());
                }
                file_path = Some(args[i].clone());
            }
            "--stdin" | "-" => {
                stdin_mode = true;
            }
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --timeout 参数值".to_string());
                }
                timeout_ms = args[i]
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout 参数无效: {}", args[i]))?;
            }
            "--cwd" => {
                i += 1;
                if i >= args.len() {
                    return Err("缺少 --cwd 参数值".to_string());
                }
                cwd = Some(args[i].clone());
            }
            "--json" => json_mode = true,
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let command = if stdin_mode {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("读取标准输入失败: {e}"))?;
        buf
    } else if let Some(p) = file_path {
        std::fs::read_to_string(&p).map_err(|e| format!("读取脚本文件失败: {e}"))?
    } else {
        positional.join(" ")
    };

    // 空命令校验：run 后无命令、或仅带 flag 不带命令、或 --stdin/--file 内容为空，
    // 都会得到空字符串，最终以 SYSTEM 执行 `cmd /C ""`（无害但无意义）。
    // 在此（命令组装完成后）统一拦截，避免无谓的管道连接与 SYSTEM 提权调用。
    if command.trim().is_empty() {
        return Err("命令为空：请在 `run` 后传入命令，或用 --file/--stdin 提供脚本".to_string());
    }

    Ok(client_call(
        CallSpec {
            op: Some("run_shell".into()),
            argv: None,
            command: Some(command),
            target: None,
            cwd,
            timeout_ms,
        },
        json_mode,
    ))
}

/// GUI 子系统程序无控制台，交互式 cmd/PowerShell 终端下 stdout/stderr 不可见
/// （管道与重定向捕获不受影响）。CLI 路径启动时附加父进程控制台解决此问题。
/// 失败静默忽略：ERROR_ACCESS_DENIED=已附加 / ERROR_INVALID_HANDLE=父进程无控制台。
fn attach_parent_console() {
    unsafe {
        let _ = winapi_raw::AttachConsole(winapi_raw::ATTACH_PARENT_PROCESS);
    }
}

/// 从 stdin 读取一个字节（双击场景下等待用户按键，避免终端一闪而过）。
/// 管道/重定向场景 stdin 立即 EOF，read 返回 0，不阻塞 AI 自动化调用。
fn wait_key() {
    use std::io::Read;
    let mut b = [0u8; 1];
    let _ = std::io::stdin().read(&mut b);
}

/// `AI-Root tail` 的事件过滤条件（由 --caller/--type/--since 解析）。
struct TailFilter {
    /// caller 字段包含该子串（大小写不敏感）即匹配
    caller: Option<String>,
    /// type 字段精确等于
    type_: Option<String>,
    /// ts 晚于该时间点才显示（相对时长换算的本地时刻）
    since: Option<chrono::NaiveDateTime>,
}

/// 解析相对时长（如 `5m`/`1h`/`30s`/`1d`）为本地截止时刻。
/// 仅支持相对时长，不支持绝对时间；解析失败返回 None（不过滤）。
fn parse_since(spec: &str) -> Option<chrono::NaiveDateTime> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    // 末位单位字符（ASCII，单字节安全）
    let unit = *s.as_bytes().last()?;
    if !matches!(unit, b's' | b'm' | b'h' | b'd') {
        return None;
    }
    let num_str = &s[..s.len() - 1];
    let n: i64 = num_str.parse().ok()?;
    let secs = match unit {
        b's' => n,
        b'm' => n * 60,
        b'h' => n * 3600,
        b'd' => n * 86400,
        _ => return None,
    };
    let cutoff = chrono::Local::now() - chrono::Duration::seconds(secs);
    Some(cutoff.naive_local())
}

/// 解析 `tail` 的过滤参数（位置参数忽略，仅认 --caller/--type/--since）。
fn parse_tail_filter(args: &[String]) -> TailFilter {
    let mut f = TailFilter {
        caller: None,
        type_: None,
        since: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--caller" => {
                i += 1;
                if i < args.len() {
                    f.caller = Some(args[i].clone());
                }
            }
            "--type" => {
                i += 1;
                if i < args.len() {
                    f.type_ = Some(args[i].clone());
                }
            }
            "--since" => {
                i += 1;
                if i < args.len() {
                    f.since = parse_since(&args[i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    f
}

/// 单条事件是否通过过滤条件（任一条件不满足即不显示）。
fn event_matches(line: &str, f: &TailFilter) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        // 非 JSON 行：caller/type 过滤下直接丢弃；since 下也丢弃（无时间可比）
        return false;
    };
    if let Some(c) = &f.caller {
        let caller = v["caller"].as_str().unwrap_or("");
        if !caller.to_lowercase().contains(&c.to_lowercase()) {
            return false;
        }
    }
    if let Some(t) = &f.type_ {
        if v["type"].as_str().unwrap_or("") != t {
            return false;
        }
    }
    if let Some(cutoff) = f.since {
        if let Some(ts) = v["ts"].as_str() {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
                if dt < cutoff {
                    return false;
                }
            }
        }
    }
    true
}

/// `AI-Root tail`：实时跟随 events.jsonl，终端打印 AI 操作信息流。
/// 先按过滤条件回放历史（过多则只显示最近 TAIL_INITIAL_MAX 行，避免打开即刷屏），
/// 再每 300ms 增量读取新增事件并应用同一过滤，直到 Ctrl+C。
fn tail_events(args: &[String]) {
    let on = color_enabled();
    let filter = parse_tail_filter(args);
    let mut offset: u64 = 0;
    println!(
        "{} {}",
        c(C_DIM, "[AI-Root tail]", on),
        c(C_DIM, "实时事件流（Ctrl+C 退出）", on)
    );
    if let Some(cv) = &filter.caller {
        println!("{} {}", c(C_DIM, "过滤 caller 含:", on), c(C_DIM, cv, on));
    }
    if let Some(t) = &filter.type_ {
        println!("{} {}", c(C_DIM, "过滤 type =", on), c(C_DIM, t, on));
    }
    if let Some(s) = &filter.since {
        println!(
            "{} {}",
            c(C_DIM, "过滤 since >=", on),
            c(C_DIM, &s.format("%Y-%m-%d %H:%M:%S").to_string(), on)
        );
    }
    println!(
        "{} {}",
        c(C_DIM, "日志:", on),
        c(C_DIM, &common::events_path().display().to_string(), on)
    );
    let (hist, new_off) = common::read_events_since(offset);
    offset = new_off;
    // 先过滤再截断：保证「不刷屏」的同时只显示命中条件的最近 N 条
    // 注意：用 into_iter 让 filter 闭包拿到 &String（而非 &&String），
    // 否则 &&String 不会自动解引用到 event_matches 所需的 &str，编译失败。
    let filtered: Vec<String> = hist
        .into_iter()
        .filter(|l| event_matches(l, &filter))
        .collect();
    let total = filtered.len();
    let show = if total > TAIL_INITIAL_MAX {
        &filtered[total - TAIL_INITIAL_MAX..]
    } else {
        &filtered[..]
    };
    if total > TAIL_INITIAL_MAX {
        println!(
            "{}",
            c(
                C_DIM,
                &format!(
                    "… 已省略 {} 条更早历史，仅显示最近 {}",
                    total - TAIL_INITIAL_MAX,
                    TAIL_INITIAL_MAX
                ),
                on
            )
        );
    }
    for l in show {
        println!("{}", event_line_text(l, on));
    }
    loop {
        let (lines, new_off) = common::read_events_since(offset);
        offset = new_off;
        for l in lines.iter() {
            if event_matches(l, &filter) {
                println!("{}", event_line_text(l, on));
            }
        }
        thread::sleep(Duration::from_millis(300));
    }
}

/// events.jsonl 单行 → 终端可读文本（交互式终端带 ANSI 语义色；管道/重定向自动降级）。
/// 时间戳/调用者暗灰，开始青、成功绿、失败红、警告黄、服务事件蓝，命令本身加粗。
/// 若事件含 run_id，在行尾以暗灰 `[rid=...]` 展示，便于与 JSON 响应 / 其他事件配对。
fn event_line_text(line: &str, on: bool) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    let ts = v["ts"].as_str().unwrap_or("--:--:--");
    let ty = v["type"].as_str().unwrap_or("");
    let cmd = v["cmd"].as_str().unwrap_or("").replace('\n', " ");
    let caller = v["caller"].as_str().unwrap_or("");
    let ts_s = c(C_DIM, ts, on);
    let body = match ty {
        "exec_start" => format!(
            "{} {} {} ({}{})",
            ts_s,
            c(C_CYAN, "开始", on),
            c(C_BOLD, &cmd, on),
            c(C_DIM, "caller=", on),
            c(C_DIM, caller, on)
        ),
        "exec_done" => {
            let exit = v["exit"].as_i64().unwrap_or(-1);
            let dur = v["duration_ms"].as_i64().unwrap_or(0);
            let preview: String = v["out_preview"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            let (mark, mcolor) = if exit == 0 {
                ("✔ 完成", C_GREEN)
            } else {
                ("✘ 失败", C_RED)
            };
            let mut s = format!(
                "{} {} {} → exit={} ({}ms, caller={})",
                ts_s,
                c(mcolor, mark, on),
                c(C_BOLD, &cmd, on),
                exit,
                dur,
                caller
            );
            if !preview.is_empty() {
                s.push_str(&format!("  {}", c(C_DIM, &format!("[{preview}]"), on)));
            }
            s
        }
        "token_rotate" => format!(
            "{} {}",
            ts_s,
            c(C_YELLOW, "⚠ 令牌轮换（旧令牌立即失效）", on)
        ),
        "service_start" => format!(
            "{} {} {}",
            ts_s,
            c(C_BLUE, "◇ 服务启动", on),
            c(
                C_DIM,
                &format!("(pipe={})", v["pipe"].as_str().unwrap_or("")),
                on
            )
        ),
        _ => return line.to_string(),
    };
    let run_id = v["run_id"].as_str().unwrap_or("");
    if run_id.is_empty() {
        body
    } else {
        format!("{body}{}", c(C_DIM, &format!(" [rid={run_id}]"), on))
    }
}

/// 用户配置里的默认超时（无配置则用编译期默认）
fn default_timeout_ms() -> u64 {
    config::user_prefs()
        .and_then(|u| u.effective_timeout_ms())
        .unwrap_or(common::DEFAULT_COMMAND_TIMEOUT_MS)
}

// ============================================================
// config 子命令
// ============================================================

fn config_path() {
    let p = config::Paths::resolve();
    let on = color_enabled();
    println!("{}", c(C_BOLD, "AI-Root 配置路径", on));
    println!("  机器级目录  : {}", p.machine.display());
    println!("    授权策略  : {}", p.policy().display());
    println!("    规则目录  : {}", p.rules_dir().display());
    println!("    秘密      : {}", p.secrets().display());
    println!("    日志      : {}", p.logs().display());
    println!("    状态      : {}", p.state().display());
    println!("  用户级目录  : {}", p.user.display());
    println!("    用户偏好  : {}", p.user_file().display());
}

/// 校验所有配置层。**只读**：不创建、不迁移任何文件。
fn config_check() -> Result<(), String> {
    let paths = config::Paths::resolve();
    let eff = config::load_readonly(&paths);
    let on = color_enabled();

    println!("{}", c(C_BOLD, "AI-Root 配置检查", on));
    println!("  机器级: {}", paths.machine.display());
    println!("  用户级: {}", paths.user.display());
    println!();

    let mut failed = false;

    match &eff.layers.admin {
        config::LayerState::Missing => {
            println!(
                "  {} 授权策略    未初始化（首次 install 或 `AI-Root config init` 后生成）",
                c(C_DIM, "[--]  ", on)
            );
        }
        config::LayerState::Error(e) => {
            println!("  {} 授权策略    {}", c(C_RED, "[FAIL]", on), e);
            failed = true;
        }
        other => {
            println!("  {} 授权策略    {}", c(C_GREEN, "[OK]  ", on), other.label());
        }
    }

    if eff.layers.rules.is_empty() {
        println!(
            "  {} 规则目录    无独立规则文件（内联规则仍在策略文件里）",
            c(C_DIM, "[--]  ", on)
        );
    }
    for (name, st) in &eff.layers.rules {
        let ok = st.ok();
        println!(
            "  {} 规则 {:<28} {}",
            if ok { c(C_GREEN, "[OK]  ", on) } else { c(C_RED, "[FAIL]", on) },
            name,
            st.label()
        );
        if !ok {
            failed = true;
        }
    }

    match &eff.layers.user {
        config::LayerState::Missing => {
            println!("  {} 用户偏好    未配置（可选）", c(C_DIM, "[--]  ", on));
        }
        config::LayerState::Error(e) => {
            println!("  {} 用户偏好    {}", c(C_RED, "[WARN]", on), e);
            println!(
                "        {}",
                c(C_DIM, "用户层已被忽略（结果只会更严）；修正后自动生效", on)
            );
        }
        other => {
            println!("  {} 用户偏好    {}", c(C_GREEN, "[OK]  ", on), other.label());
        }
    }

    if config::needs_layout_migration(&paths) {
        println!(
            "  {} 旧布局     检测到 v1.1 扁平布局，执行 `AI-Root config migrate`（管理员）迁移",
            c(C_YELLOW, "[!!]  ", on)
        );
    }

    println!();
    println!("{}", c(C_BOLD, "生效结果：", on));
    println!("  模式        : {}", eff.policy.file.mode.as_str());
    println!("  默认决策    : {}", eff.policy.file.default_decision.as_str());
    println!("  规则数      : {}", eff.policy.file.rules.len());
    println!(
        "  可信发布者  : {}",
        if eff.policy.file.trusted_publishers.is_empty() {
            "（空 → update 一律拒绝）".to_string()
        } else {
            format!("{} 个", eff.policy.file.trusted_publishers.len())
        }
    );
    println!("  确认有效期  : {}s", eff.policy.confirm_timeout_secs());
    if let Some(t) = eff.user.as_ref().and_then(|u| u.effective_timeout_ms()) {
        println!("  默认超时    : {t}ms（来自用户配置）");
    }

    if !eff.notes.is_empty() {
        println!();
        println!("{}", c(C_BOLD, "说明：", on));
        for n in &eff.notes {
            println!("  - {n}");
        }
    }

    println!();
    if failed {
        println!(
            "{}",
            c(
                C_RED,
                "检查未通过：存在配置错误。服务端按「失败即收紧」处理（只读查询以外全部拒绝）。",
                on
            )
        );
        return Err("配置检查未通过".into());
    }
    println!("{}", c(C_GREEN, "检查通过。", on));
    Ok(())
}

/// 显示合并后的生效配置。`--json` 输出机器可消费格式。
fn config_show(json_mode: bool) -> Result<(), String> {
    let paths = config::Paths::resolve();
    let eff = config::load_readonly(&paths);

    if json_mode {
        let out = serde_json::json!({
            "paths": {
                "policy": paths.policy().to_string_lossy(),
                "rules_dir": paths.rules_dir().to_string_lossy(),
                "user": paths.user_file().to_string_lossy(),
                "secrets": paths.secrets().to_string_lossy(),
                "logs": paths.logs().to_string_lossy(),
                "state": paths.state().to_string_lossy(),
            },
            "layers": {
                "admin": eff.layers.admin.label(),
                "user": eff.layers.user.label(),
                "rules": eff.layers.rules.iter()
                    .map(|(n, s)| serde_json::json!({"file": n, "state": s.label()}))
                    .collect::<Vec<_>>(),
            },
            "effective": {
                "mode": eff.policy.file.mode.as_str(),
                "default_decision": eff.policy.file.default_decision.as_str(),
                "rule_count": eff.policy.file.rules.len(),
                "confirm_timeout_secs": eff.policy.confirm_timeout_secs(),
                "approved_services": eff.policy.file.approved_services,
                "trusted_publishers": eff.policy.file.trusted_publishers,
                "allow_legacy_shell": eff.policy.file.allow_legacy_shell,
                "workspace_roots": eff.policy.file.workspace_roots,
            },
            "notes": eff.notes,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }

    let on = color_enabled();
    println!("{}", c(C_BOLD, "AI-Root 生效配置", on));
    println!("  策略文件  : {}", paths.policy().display());
    println!("  规则目录  : {}", paths.rules_dir().display());
    println!("  用户配置  : {}（{}）", paths.user_file().display(), eff.layers.user.label());
    println!();
    println!("  模式        : {}", eff.policy.file.mode.as_str());
    println!("  默认决策    : {}", eff.policy.file.default_decision.as_str());
    println!("  规则数      : {}", eff.policy.file.rules.len());
    println!("  确认有效期  : {}s", eff.policy.confirm_timeout_secs());
    println!(
        "  传统 shell  : {}",
        if eff.policy.file.allow_legacy_shell {
            "免逐次确认"
        } else {
            "逐次确认"
        }
    );
    println!(
        "  已批准服务  : {}",
        if eff.policy.file.approved_services.is_empty() {
            "（空 → restart_service 需确认）".to_string()
        } else {
            eff.policy.file.approved_services.join(", ")
        }
    );
    println!(
        "  可信发布者  : {}",
        if eff.policy.file.trusted_publishers.is_empty() {
            "（空 → update 拒绝）".to_string()
        } else {
            format!("{} 个", eff.policy.file.trusted_publishers.len())
        }
    );
    println!();
    println!("{}", c(C_BOLD, "自动允许的程序（argv）:", on));
    let mut progs: Vec<&str> = eff
        .policy
        .file
        .rules
        .iter()
        .filter(|r| r.decision == policy::Decision::Allow)
        .filter_map(|r| r.program.as_deref())
        .collect();
    progs.sort_unstable();
    println!(
        "  {}",
        if progs.is_empty() { "（无）".to_string() } else { progs.join(", ") }
    );

    if let Some(u) = &eff.user {
        println!();
        println!("{}", c(C_BOLD, "用户配置（只能收紧）：", on));
        println!(
            "  mode={}  confirm_timeout={}  default_timeout={}",
            u.mode.map(|m| m.as_str()).unwrap_or("-"),
            u.confirm_timeout_secs
                .map(|t| format!("{t}s"))
                .unwrap_or_else(|| "-".into()),
            u.effective_timeout_ms()
                .map(|t| format!("{t}ms"))
                .unwrap_or_else(|| "-".into()),
        );
    }

    if !eff.notes.is_empty() {
        println!();
        println!("{}", c(C_BOLD, "说明：", on));
        for n in &eff.notes {
            println!("  - {n}");
        }
    }
    Ok(())
}

/// 生成默认管理员策略（已存在则不覆盖）
fn config_init() -> Result<(), String> {
    require_admin()?;
    let paths = config::Paths::resolve();
    if paths.policy().exists() {
        return Err(format!(
            "策略已存在，未覆盖：{}（如需重置，先备份并手工删除）",
            paths.policy().display()
        ));
    }
    std::fs::create_dir_all(&paths.machine).map_err(|e| format!("创建目录失败: {e}"))?;
    config::write_default_policy(&paths)?;
    let _ = common::set_dir_acl(&paths.machine);
    config::invalidate_cache();
    println!("[ok] 已生成默认策略: {}", paths.policy().display());
    println!("  下一步: `AI-Root config check` 校验，或直接 `AI-Root install`");
    Ok(())
}

/// 迁移旧布局/旧格式
fn config_migrate() -> Result<(), String> {
    require_admin()?;
    let paths = config::Paths::resolve();
    let actions = config::migrate_layout(&paths)?;
    common::set_dir_acl(&paths.machine).map_err(|e| format!("迁移后设置 ACL 失败: {e}"))?;
    config::invalidate_cache();
    println!("[ok] 配置迁移完成，共 {} 步：", actions.len());
    for a in actions {
        println!("  - {a}");
    }
    println!("  提示: 旧版本 AI-Root 无法读取新布局（回滚方法见 docs/配置说明.md）");
    Ok(())
}

fn print_help(on: bool) {
    let title = c(
        C_BOLD,
        &format!(
            "AI-Root v{} —— LocalSystem 受控提权中继（策略 + 确认）",
            env!("CARGO_PKG_VERSION")
        ),
        on,
    );
    let body = r#"
用法:
  AI-Root install      注册为 LocalSystem 自启服务（需管理员，首次一次性）
  AI-Root grant        对已存在服务补发用户 STOP/START 授权（需管理员）
  AI-Root uninstall    卸载服务（需管理员）
  AI-Root rotate-token 轮换共享令牌（服务端自动热加载，旧令牌立即失效）
  AI-Root start        启动服务（install 授权后非提权即可）
  AI-Root stop         停止服务（install 授权后非提权即可）
  AI-Root update <exe> 自主更新（需签名验证 + 信任根；不再接受任意 PE）
  AI-Root exec <程序> [参数...]  结构化执行，不经 shell（策略可精确授权，推荐）
  AI-Root op <操作> [--target X] 结构化操作（query_system_info / query_service /
                                 restart_service / read_state_file）
  AI-Root pending      查看待确认请求（需人工批准的操作）
  AI-Root approve <id> 批准一条待确认请求（需管理员）
  AI-Root deny <id>    拒绝一条待确认请求（需管理员）
  AI-Root policy       查看当前授权策略与自动允许范围
  AI-Root config check 校验全部配置层（默认/用户/管理员/规则目录），有错返回非零
  AI-Root config show  显示合并后的生效配置与路径（--json 供程序消费）
  AI-Root config path  打印各配置文件路径
  AI-Root config init  生成默认管理员策略（管理员，已存在则不覆盖）
  AI-Root config migrate  执行旧布局/旧格式迁移（管理员）
  AI-Root signature <exe>        查看 Authenticode 签名与信任状态
  AI-Root trust-publisher <exe>  把该证书加入 update 信任根（需管理员）
  AI-Root --version    打印版本
  AI-Root status       查询服务状态
  AI-Root server       前台管道服务模式（测试用）
  AI-Root run <命令>             连接管道并执行命令（非提权即可）
  AI-Root run --file <脚本>      以脚本模式执行多行命令（&/&& 正常）
  AI-Root run --stdin            从标准输入读取脚本执行
  AI-Root run --timeout <毫秒>   覆盖默认 60s 超时（上限 10 分钟）
  AI-Root run --cwd <目录>       指定命令工作目录（默认 SYSTEM 的 System32）
  AI-Root run --json             结构化 JSON 输出（AI 消费方推荐）
  AI-Root run -- <命令>          `--` 后全部原样作为命令（含 - 开头参数）
  AI-Root tail                   实时跟随事件流（终端实时查看 AI 操作，Ctrl+C 退出）
  AI-Root tail --type exec_done  仅显示执行完成事件
  AI-Root tail --caller DOMAIN\\user   仅显示指定调用者
  AI-Root tail --since 5m        仅显示最近 5 分钟内的事件（支持 s/m/h/d）
  AI-Root --service    [内部] SCM 派发入口

示例:
  AI-Root run "whoami"                          # 应以 NT AUTHORITY\SYSTEM 身份返回
  AI-Root run "pnputil /enum-devices"
  AI-Root run --file C:\tmp\batch.cmd           # 多行脚本，&/&&/| 均正常
  echo 命令行 & pnputil /enum-devices | AI-Root run --stdin
  AI-Root run --timeout 5000 "ping -n 10 127.0.0.1"   # 5 秒超时杀进程树
  AI-Root run --cwd C:\tmp "dir"                # 在 C:\tmp 下执行
  AI-Root run --json "whoami"                   # JSON 输出（含 run_id/duration_ms/caller/timed_out）
  AI-Root update D:\AI-Root-target\...\AI-Root.exe   # AI 自主更新（零 UAC）
  AI-Root rotate-token                             # 轮换令牌（SYSTEM 凭据，勿入日志）
"#;
    // 小节标题上色（用法/示例），其余正文保持原样。
    let body = body
        .replace("用法:", &c(C_CYAN, "用法:", on))
        .replace("示例:", &c(C_CYAN, "示例:", on));
    println!("{title}{body}");
}
