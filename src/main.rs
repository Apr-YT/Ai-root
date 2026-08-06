//! AI-root —— 入口与子命令分发
//!
//! 子命令：
//!   install    注册为 LocalSystem 自启服务（需管理员）
//!   uninstall  卸载服务（需管理员）
//!   start      启动服务（需管理员）
//!   stop       停止服务（需管理员）
//!   status     查询服务状态
//!   server     前台管道服务模式（当前进程直接运行，演练/多实例用）
//!   run <cmd>  以客户端身份连接管道并执行命令（非提权即可）
//!   gui        打开原生 GUI 控制面板
//!   --service  SCM 派发进入服务主循环（用户勿直接调用）

// GUI 变体声明为 Windows GUI 子系统：双击启动时不弹黑色终端窗口。
// headless（--no-default-features）保持 console 子系统，CLI 输出正常；
// GUI 版的 CLI 输出可见性由 attach_parent_console()（AttachConsole）兜底。
#![cfg_attr(feature = "gui", windows_subsystem = "windows")]

mod common;
mod exec;
#[cfg(feature = "gui")]
mod gui;
mod protocol;
mod server;
mod service;
mod winapi_raw;

use std::process::exit;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        // 无参数（典型：双击 exe）：默认打开 GUI 控制台。
        // 本程序为 GUI 子系统（无控制台），老行为 print_help() 输出不可见、进程静默退出，
        // 双击「无反应」体验极差；默认进 GUI 才是成品行为。帮助请用 `ai-root -h`。
        #[cfg(feature = "gui")]
        {
            gui::run();
            return;
        }
        #[cfg(not(feature = "gui"))]
        {
            // headless 版无参数（典型：双击）：打印帮助并等待按键，防止终端一闪而过。
            // 管道/重定向场景 stdin 为 EOF，read 立即返回，不阻塞 AI 调用。
            println!();
            println!("ai-root headless 版：无 GUI。请双击 ai-root-gui.exe 打开可视化控制台；");
            println!("命令行用法见下方帮助；实时查看 AI 操作可用 `ai-root tail`。");
            println!();
            print_help();
            println!();
            println!("按任意键退出...");
            wait_key();
            return;
        }
    }

    let cmd = args[1].as_str();
    // GUI 子系统程序无控制台，交互式终端下 println/eprintln 输出不可见。
    // CLI 命令启动时附加父进程控制台使输出可见；管道/重定向场景标准句柄有效，
    // AttachConsole 不会破坏；父进程无控制台（如从 GUI 启动、SCM 服务）时静默忽略。
    attach_parent_console();
    let result: Result<(), String> = match cmd {
        "-h" | "--help" | "help" => {
            print_help();
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
                    println!("✓ 令牌已轮换（服务端自动热加载，旧令牌立即失效）");
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
                eprintln!("用法: ai-root update <新exe路径>");
                exit(1);
            }
            service::update_self(std::path::Path::new(&args[2])).map_err(|e| e.to_string())
        }
        "status" => {
            match service::query_status() {
                Ok(state) => println!("服务状态: {:?}", state),
                Err(_) => println!("服务状态: 未安装 / 不可访问"),
            }
            Ok(())
        }
        "server" => {
            server::run_pipe_server_forever();
            Ok(())
        }
        "run" => {
            match parse_and_run(&args[2..]) {
                Ok(()) => return, // client_run 内部决定退出码
                Err(e) => {
                    eprintln!("[错误] {e}");
                    exit(1);
                }
            }
        }
        #[cfg(feature = "gui")]
        "gui" => {
            gui::run();
            Ok(())
        }
        #[cfg(not(feature = "gui"))]
        "gui" => Err(
            "本构建为 headless（--no-default-features），未包含 GUI。请使用默认构建的 ai-root.exe 运行 `ai-root gui`"
                .to_string(),
        ),
        "tail" => {
            // 实时跟随 events.jsonl（增量读），终端实时显示 AI 操作（exec_start/exec_done 等）。
            // Ctrl+C 退出；Ctrl+C 无法在 Rust 中优雅捕获，直接终止即可（无副作用）。
            tail_events();
            Ok(())
        }
        _ => {
            print_help();
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("[错误] {e}");
        exit(1);
    }
}

/// 客户端执行：连接管道、鉴权、打印结果、传播退出码
/// json_mode=true 时输出结构化 JSON（stdout/stderr/exit_code/error 分离，AI 消费方推荐）
fn client_run(command: &str, timeout_ms: u64, json_mode: bool, cwd: Option<&str>) {
    let token = match common::read_token() {
        Some(t) => t,
        None => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "exit_code": -1, "stdout": "", "stderr": "", "error": "未找到令牌，请先以管理员运行: ai-root install"})
                );
            } else {
                eprintln!("[错误] 未找到令牌，请先以管理员运行: ai-root install");
            }
            exit(1);
        }
    };

    let mut file = match protocol::connect_client() {
        Ok(f) => f,
        Err(e) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "exit_code": -1, "stdout": "", "stderr": "", "error": format!("连接失败: {e}")})
                );
            } else {
                eprintln!("[错误] 连接失败: {e}");
            }
            exit(1);
        }
    };

    let req = protocol::Request {
        token,
        command: command.to_string(),
        cwd: cwd.map(|s| s.to_string()),
        timeout_ms: Some(timeout_ms),
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[错误] 序列化失败: {e}");
            exit(1);
        }
    };

    if let Err(e) = protocol::write_frame(&mut file, &bytes) {
        eprintln!("[错误] 发送失败: {e}");
        exit(1);
    }

    let resp_bytes = match protocol::read_frame(&mut file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[错误] 读取失败: {e}");
            exit(1);
        }
    };

    let resp: protocol::Response = match serde_json::from_slice(&resp_bytes) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[错误] 响应解析失败");
            exit(1);
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
            })
        );
    } else {
        if !resp.stdout.is_empty() {
            print!("{}\n", resp.stdout);
        }
        if !resp.stderr.is_empty() {
            eprint!("{}\n", resp.stderr);
        }
        if resp.truncated {
            eprintln!("[注意] 输出超过 1MB 已被截断");
        }
        if let Some(e) = resp.error {
            eprintln!("[relay error] {e}");
            exit(1);
        }
    }
    exit(resp.exit_code);
}

/// 解析 `run` 子命令参数，装载命令文本后交给 client_run。
///
/// 支持调用形态：
///   ai-root run "单行命令"
///   ai-root run --file <脚本路径>   读取脚本文件（多行）
///   ai-root run --stdin            从标准输入读取脚本（多行）
///   ai-root run --timeout <毫秒>   覆盖默认 60s 超时（上限 10 分钟，服务端钳制）
///   ai-root run --cwd <目录>       指定命令工作目录（默认 SYSTEM 的 System32）
///   ai-root run --json             结构化 JSON 输出（AI 消费方推荐）
///   ai-root run -- <命令>          显式分隔符：`--` 之后全部原样视为命令（含 - 开头参数）
///
/// 含换行符的内容会被服务端识别为「脚本模式」，此时 `&`/`&&`/`|`/`()` 恢复 cmd 正常语义，
/// 这是 AI 批量自治多命令时的推荐路径。
fn parse_and_run(args: &[String]) -> Result<(), String> {
    use std::io::Read;
    let mut file_path: Option<String> = None;
    let mut stdin_mode = false;
    let mut timeout_ms: u64 = 60_000;
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

    client_run(&command, timeout_ms, json_mode, cwd.as_deref());
    Ok(())
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
/// 仅 headless 无参数分支（`#[cfg(not(feature = "gui"))]`）使用；GUI 变体下允许 dead_code。
#[cfg_attr(feature = "gui", allow(dead_code))]
fn wait_key() {
    use std::io::Read;
    let mut b = [0u8; 1];
    let _ = std::io::stdin().read(&mut b);
}

/// `ai-root tail`：实时跟随 events.jsonl，终端打印 AI 操作信息流。
/// 先打印已有事件（历史），再每 500ms 增量读取新增事件，直到 Ctrl+C。
fn tail_events() {
    let mut offset: u64 = 0;
    println!("[ai-root tail] 实时事件流（Ctrl+C 退出）: {}", common::events_path().display());
    loop {
        let (lines, new_off) = common::read_events_since(offset);
        offset = new_off;
        for l in lines {
            println!("{}", event_line_text(&l));
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// events.jsonl 单行 → 终端可读文本（headless 无 GUI 着色，用符号区分状态）
fn event_line_text(line: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return line.to_string();
    };
    let ts = v["ts"].as_str().unwrap_or("--:--:--");
    let ty = v["type"].as_str().unwrap_or("");
    let cmd = v["cmd"].as_str().unwrap_or("").replace('\n', " ");
    let caller = v["caller"].as_str().unwrap_or("");
    match ty {
        "exec_start" => format!("{ts} ▶ 开始  {cmd}   (caller={caller})"),
        "exec_done" => {
            let exit = v["exit"].as_i64().unwrap_or(-1);
            let dur = v["duration_ms"].as_i64().unwrap_or(0);
            let preview: String = v["out_preview"].as_str().unwrap_or("").chars().take(40).collect();
            let mark = if exit == 0 { "✔ 完成" } else { "✘ 失败" };
            let mut s = format!("{ts} {mark}  {cmd}   → exit={exit} ({dur}ms, caller={caller})");
            if !preview.is_empty() {
                s.push_str(&format!("   [{}]", preview));
            }
            s
        }
        "token_rotate" => format!("{ts} ⚠ 令牌轮换（旧令牌立即失效）"),
        "service_start" => format!("{ts} ◇ 服务启动 (pipe={})", v["pipe"].as_str().unwrap_or("")),
        _ => line.to_string(),
    }
}

fn print_help() {
    println!(
        r#"
AI-root v1.0.0 —— LocalSystem 提权命令中继

用法:
  ai-root install      注册为 LocalSystem 自启服务（需管理员，首次一次性）
  ai-root grant        对已存在服务补发用户 STOP/START 授权（需管理员）
  ai-root uninstall    卸载服务（需管理员）
  ai-root rotate-token 轮换共享令牌（服务端自动热加载，旧令牌立即失效）
  ai-root start        启动服务（install 授权后非提权即可）
  ai-root stop         停止服务（install 授权后非提权即可）
  ai-root update <exe> 自主更新服务二进制（停服务→覆盖→启服务，无需管理员）
  ai-root status       查询服务状态
  ai-root server       前台管道服务模式（测试用）
  ai-root run <命令>             连接管道并执行命令（非提权即可）
  ai-root run --file <脚本>      以脚本模式执行多行命令（&/&& 正常）
  ai-root run --stdin            从标准输入读取脚本执行
  ai-root run --timeout <毫秒>   覆盖默认 60s 超时（上限 10 分钟）
  ai-root run --cwd <目录>       指定命令工作目录（默认 SYSTEM 的 System32）
  ai-root run --json             结构化 JSON 输出（AI 消费方推荐）
  ai-root run -- <命令>          `--` 后全部原样作为命令（含 - 开头参数）
  ai-root tail                   实时跟随事件流（终端实时查看 AI 操作，Ctrl+C 退出）
  ai-root gui          打开 GUI 控制面板
  ai-root --service    [内部] SCM 派发入口

示例:
  ai-root run "whoami"                          # 应以 NT AUTHORITY\SYSTEM 身份返回
  ai-root run "pnputil /enum-devices"
  ai-root run --file C:\tmp\batch.cmd           # 多行脚本，&/&&/| 均正常
  echo 命令行 & pnputil /enum-devices | ai-root run --stdin
  ai-root run --timeout 5000 "ping -n 10 127.0.0.1"   # 5 秒超时杀进程树
  ai-root run --cwd C:\tmp "dir"                # 在 C:\tmp 下执行
  ai-root run --json "whoami"                   # JSON 输出（ok/exit_code/stdout/stderr）
  ai-root update D:\ai-root-target\...\ai-root.exe   # AI 自主更新（零 UAC）
  ai-root rotate-token                             # 轮换令牌（SYSTEM 凭据，勿入日志）
"#
    );
}
