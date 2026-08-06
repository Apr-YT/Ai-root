//! AI-root —— 入口与子命令分发
//!
//! 子命令：
//!   install    注册为 LocalSystem 自启服务（需管理员）
//!   uninstall  卸载服务（需管理员）
//!   start      启动服务（需管理员）
//!   stop       停止服务（需管理员）
//!   status     查询服务状态
//!   server     前台管道服务模式（测试用，当前进程直接运行）
//!   run <cmd>  以客户端身份连接管道并执行命令（非提权即可）
//!   gui        打开原生 GUI 控制面板
//!   --service  SCM 派发进入服务主循环（用户勿直接调用）

mod common;
mod exec;
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
        print_help();
        return;
    }

    let cmd = args[1].as_str();
    let result: Result<(), String> = match cmd {
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
        "--test-pipe-server" => {
            // [内部测试] 以服务模式逻辑常驻，10 秒后自动触发 stop，用于本地复现/验证
            // 7034 竞态修复（CancelIoEx 替代 CloseHandle）。非提权亦可运行，便于无 SCM 下验证。
            // 注意：此路径不走 service_run()，不会自动创建 enabled.flag → 显式默认启用，
            // 否则新数据目录下 is_enabled()=false，所有请求返回「中继已禁用」。
            common::set_enabled(true);
            let stop_event = unsafe { winapi_raw::CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            let ev = stop_event as usize;
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(30));
                unsafe { winapi_raw::SetEvent(ev as winapi_raw::HANDLE); }
            });
            server::run_pipe_server(stop_event);
            println!("[test] 服务模式优雅退出（未崩溃）");
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
        "gui" => {
            gui::run();
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
fn client_run(command: &str, timeout_ms: u64) {
    let token = match common::read_token() {
        Some(t) => t,
        None => {
            eprintln!("[错误] 未找到令牌，请先以管理员运行: ai-root install");
            exit(1);
        }
    };

    let mut file = match protocol::connect_client() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[错误] 连接失败: {e}");
            exit(1);
        }
    };

    let req = protocol::Request {
        token,
        command: command.to_string(),
        cwd: None,
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

    if !resp.stdout.is_empty() {
        print!("{}\n", resp.stdout);
    }
    if !resp.stderr.is_empty() {
        eprint!("{}\n", resp.stderr);
    }
    if let Some(e) = resp.error {
        eprintln!("[relay error] {e}");
        exit(1);
    }
    exit(resp.exit_code);
}

/// 解析 `run` 子命令参数，装载命令文本后交给 client_run。
///
/// 支持三种调用形态：
///   ai-root run "单行命令"
///   ai-root run --file <脚本路径>   读取脚本文件（多行）
///   ai-root run --stdin            从标准输入读取脚本（多行）
///   ai-root run --timeout <毫秒>   覆盖默认 60s 超时
///
/// 含换行符的内容会被服务端识别为「脚本模式」，此时 `&`/`&&`/`|`/`()` 恢复 cmd 正常语义，
/// 这是 AI 批量自治多命令时的推荐路径。
fn parse_and_run(args: &[String]) -> Result<(), String> {
    use std::io::Read;
    let mut file_path: Option<String> = None;
    let mut stdin_mode = false;
    let mut timeout_ms: u64 = 60_000;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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

    client_run(&command, timeout_ms);
    Ok(())
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
  ai-root run --timeout <毫秒>   覆盖默认 60s 超时
  ai-root gui          打开 GUI 控制面板
  ai-root --service    [内部] SCM 派发入口

示例:
  ai-root run "whoami"                          # 应以 NT AUTHORITY\SYSTEM 身份返回
  ai-root run "pnputil /enum-devices"
  ai-root run --file C:\tmp\batch.cmd           # 多行脚本，&/&&/| 均正常
  echo 命令行 & pnputil /enum-devices | ai-root run --stdin
  ai-root run --timeout 5000 "ping -n 10 127.0.0.1"   # 5 秒超时杀进程树
  ai-root update D:\wbrelay_target\...\ai-root.exe   # AI 自主更新（零 UAC）
  ai-root rotate-token                             # 轮换令牌（SYSTEM 凭据，勿入日志）
"#
    );
}
