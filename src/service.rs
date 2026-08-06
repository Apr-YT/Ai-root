//! Windows 服务注册、控制与服务主循环
//!
//! 使用 windows-service crate (https://crates.io/crates/windows-service)
//! 服务以 LocalSystem 账户运行（最高本地权限）。

use std::ffi::OsString;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::common;
use crate::server;
use crate::winapi_raw;

/// 将自身部署到 D:\Kiln\AI-root 并在 C:\Windows 写入 ai-root.cmd 包装器，
/// 使 ai-root 在任意目录、任意身份（含智能体非交互进程）下均可直接调用。
/// 返回服务应指向的可执行文件路径（部署成功返回部署副本，失败回退到当前 exe）。
fn deploy_global(src_exe: &std::path::Path) -> std::path::PathBuf {
    let deploy_dir = std::path::Path::new("D:\\Kiln\\AI-root");
    let deploy_exe = deploy_dir.join("ai-root.exe");
    if let Err(e) = std::fs::create_dir_all(deploy_dir) {
        eprintln!("[warn] 无法创建部署目录 {}: {e}", deploy_dir.display());
        return src_exe.to_path_buf();
    }
    match std::fs::copy(src_exe, &deploy_exe) {
        Ok(_) => {
            // 全局包装器：转发所有参数（%*）。C:\Windows 默认在系统 PATH 中，
            // 写入后 ai-root 在任意 shell / 任意身份下均可直接调用。
            let wrapper = std::path::Path::new("C:\\Windows\\ai-root.cmd");
            let content = format!("@echo off\r\n\"{}\" %*\r\n", deploy_exe.display());
            if let Err(e) = std::fs::write(wrapper, content) {
                eprintln!("[warn] 无法写入全局包装器 {}: {e}", wrapper.display());
            } else {
                println!("✓ 已部署全局命令: ai-root (C:\\Windows\\ai-root.cmd)");
            }
            deploy_exe
        }
        Err(e) => {
            eprintln!("[warn] 无法复制可执行文件到 {}: {e}", deploy_exe.display());
            src_exe.to_path_buf()
        }
    }
}

/// 注册为 LocalSystem 自启服务（需管理员权限）
pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;
    let src_exe = std::env::current_exe()?;
    // 部署全局副本并写入包装器（服务指向部署副本，便于后续统一升级）
    let exe_path = deploy_global(&src_exe);

    let info = ServiceInfo {
        name: OsString::from(common::SERVICE_NAME),
        display_name: OsString::from("AI-root (SYSTEM 提权中继)"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        account_name: None,      // None = 以 LocalSystem 运行（最高本地权限）
        account_password: None,
        dependencies: vec![],
    };

    let _service = manager.create_service(&info, ServiceAccess::ALL_ACCESS)?;

    // 安装时即生成令牌并设置 ACL，确保服务启动后可用
    common::ensure_token()?;
    // 持久化安装用户 SID，注入管道 SDDL，使智能体（该用户身份）可无人值守连管道拿 SYSTEM
    if let Some(sid) = common::get_user_sid() {
        common::save_user_sid(&sid);
    }
    common::set_enabled(true);

    println!("✓ 服务已注册: {} (LocalSystem, 自启)", common::SERVICE_NAME);
    println!("启动命令: ai-root start");
    Ok(())
}

/// 卸载服务（需管理员权限）
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(common::SERVICE_NAME, ServiceAccess::ALL_ACCESS)?;
    service.delete()?;
    println!("✓ 服务已卸载: {}", common::SERVICE_NAME);
    Ok(())
}

pub fn start_svc() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        common::SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.start::<OsString>(&[])?;
    println!("✓ 服务已启动");
    Ok(())
}

pub fn stop_svc() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        common::SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    service.stop()?;
    println!("✓ 服务已停止");
    Ok(())
}

/// 查询服务状态；服务不存在时返回 Err
pub fn query_status() -> Result<ServiceState, Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(common::SERVICE_NAME, ServiceAccess::QUERY_STATUS)?;
    let status = service.query_status()?;
    Ok(status.current_state)
}

/// 从 SCM 派发进入服务主循环（main 中检测到 --service 时调用）
pub fn run_as_service() -> Result<(), Box<dyn std::error::Error>> {
    windows_service::service_dispatcher::start(common::SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = service_run() {
        eprintln!("[ai-root] 服务运行错误: {e}");
    }
}

fn service_run() -> Result<(), Box<dyn std::error::Error>> {
    // 停止事件：控制处理器置位它，管道主循环的监控线程据此中断阻塞并退出。
    // 用于解决 ConnectNamedPipe 阻塞导致 Stop-Service 无法终止服务的问题。
    let stop_event = unsafe { winapi_raw::CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop_event == winapi_raw::INVALID_HANDLE_VALUE {
        return Err("CreateEventW 失败".into());
    }
    // HANDLE 不是 Send，无法直接跨线程；以 usize 整数形式交给控制处理器闭包（原事件指向同一内核对象）
    let svc_stop_event = stop_event as usize;

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                unsafe { winapi_raw::SetEvent(svc_stop_event as winapi_raw::HANDLE); }
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NoError,
        }
    };

    let status_handle = service_control_handler::register(common::SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // 确保令牌与默认启用
    common::ensure_token().ok();
    common::set_enabled(true);

    // 运行管道服务，直到收到 stop 事件（监控线程会中断阻塞的 ConnectNamedPipe）
    server::run_pipe_server(stop_event);

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    unsafe { winapi_raw::CloseHandle(stop_event); }

    Ok(())
}
