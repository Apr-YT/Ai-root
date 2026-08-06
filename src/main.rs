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
        "uninstall" => service::uninstall().map_err(|e| e.to_string()),
        "start" => service::start_svc().map_err(|e| e.to_string()),
        "stop" => service::stop_svc().map_err(|e| e.to_string()),
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
            if args.len() < 3 {
                eprintln!("用法: ai-root run <命令>");
                return;
            }
            client_run(&args[2..].join(" "));
            return; // client_run 内部决定退出码
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
fn client_run(command: &str) {
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
        timeout_ms: Some(60000),
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

fn print_help() {
    println!(
        r#"
AI-root v1.0.0 —— LocalSystem 提权命令中继

用法:
  ai-root install      注册为 LocalSystem 自启服务（需管理员）
  ai-root uninstall    卸载服务（需管理员）
  ai-root start        启动服务（需管理员）
  ai-root stop         停止服务（需管理员）
  ai-root status       查询服务状态
  ai-root server       前台管道服务模式（测试用）
  ai-root run <命令>   连接管道并执行命令（非提权即可）
  ai-root gui          打开 GUI 控制面板
  ai-root --service    [内部] SCM 派发入口

示例:
  ai-root run "whoami"            # 应以 NT AUTHORITY\SYSTEM 身份返回
  ai-root run "pnputil /enum-devices"
"#
    );
}
