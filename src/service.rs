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

    // 给当前用户 SID 追加服务 STOP/START 权限：智能体进程（用户身份）此后可自主
    // stop/start/更新服务，无需每次手动管理员授权（Windows 服务安全描述符标准机制）。
    // 失败仅降级（更新仍需管理员），不阻断安装。
    if let Err(e) = grant_acl() {
        eprintln!("[warn] 服务 ACL 授权失败: {e}（更新服务二进制将仍需管理员）");
    }

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

/// 给当前用户 SID 追加服务 STOP/START 权限（服务 DACL 标准机制）。
///
/// 权限位：`SERVICE_START`=0x10=SDDL `RP`，`SERVICE_STOP`=0x20=SDDL `WP`。
/// 效果：智能体进程（以安装用户身份运行，非提权）此后可执行
/// `ai-root stop/start/update`，无需管理员 → 二进制更新彻底自治。
/// 失败不致命（install 继续，仅降级为「更新仍需管理员」）。
///
/// 可独立以 `ai-root grant` 调用（需管理员）：用于**对已存在的服务**补发授权，
/// 无需卸载重装（deploy.ps1 会在部署后自动调用一次）。
pub fn grant_acl() -> Result<(), Box<dyn std::error::Error>> {
    let Some(sid) = common::get_user_sid() else {
        return Ok(());
    };
    // 保留默认 ACE（SYSTEM/Administrators 完全控制），追加用户 GRGX。
    // 服务对象 GENERIC 映射：GENERIC_READ = QUERY_CONFIG|QUERY_STATUS|INTERROGATE|READ_CONTROL；
    // GENERIC_EXECUTE = START|STOP|PAUSE_CONTINUE|READ_CONTROL。
    // 必须覆盖 QUERY_STATUS：`status`/`stop`/`start`/`update` 均需以 SERVICE_QUERY_STATUS 打开服务，
    // 缺它会导致非提权用户 OpenService 失败（status 误报未安装、update 报 IO error）。
    // 注意：不要用 RP/WP/LO 等零散位组合——实测 SDDL 字母 LO 并非 SERVICE_QUERY_STATUS(0x04)，
    // 授予 RPLOWP 后 START/STOP 生效但 QUERY_STATUS 仍被拒；GRGX 组合经 sc sdset 实测全部通过。
    let sddl = format!(
        "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCLCSWRPWPDTLOCRRC;;;BA)(A;;GRGX;;;{sid})"
    );
    let sddl_w = common::to_wide(&sddl);
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    let converted = unsafe {
        winapi_raw::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 || sd.is_null() {
        return Err(format!(
            "SDDL 转换失败, last_err={}",
            unsafe { winapi_raw::GetLastError() }
        )
        .into());
    }

    let name_w = common::to_wide(common::SERVICE_NAME);
    let scm = unsafe {
        winapi_raw::OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            winapi_raw::SC_MANAGER_CONNECT,
        )
    };
    if scm.is_null() {
        unsafe { winapi_raw::LocalFree(sd); }
        return Err(format!(
            "OpenSCManagerW 失败, last_err={}",
            unsafe { winapi_raw::GetLastError() }
        )
        .into());
    }
    let svc = unsafe {
        winapi_raw::OpenServiceW(scm, name_w.as_ptr(), winapi_raw::SERVICE_ALL_ACCESS)
    };
    if svc.is_null() {
        unsafe {
            winapi_raw::CloseServiceHandle(scm);
            winapi_raw::LocalFree(sd);
        }
        return Err(format!(
            "OpenServiceW 失败, last_err={}",
            unsafe { winapi_raw::GetLastError() }
        )
        .into());
    }

    let ok = unsafe {
        winapi_raw::SetServiceObjectSecurity(
            svc,
            winapi_raw::DACL_SECURITY_INFORMATION,
            sd,
        )
    };
    unsafe {
        winapi_raw::CloseServiceHandle(svc);
        winapi_raw::CloseServiceHandle(scm);
        winapi_raw::LocalFree(sd);
    }
    if ok == 0 {
        return Err(format!(
            "SetServiceObjectSecurity 失败, last_err={}",
            unsafe { winapi_raw::GetLastError() }
        )
        .into());
    }
    println!("✓ 服务 ACL 已授权用户 STOP/START（智能体可自主更新）");
    Ok(())
}

/// 自主更新服务二进制（需 install 时已对当前用户授权 STOP/START）。
///
/// 流程：停止服务（轮询等待 Stopped）→ 覆盖部署副本 `D:\Kiln\AI-root\ai-root.exe`
/// → 启动服务 → 自检状态。智能体（用户身份）一条命令完成，全程无需管理员。
pub fn update_self(new_exe: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        common::SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;

    // 1) 若在运行，先停止（轮询等待，最多 ~15s；服务优雅停止逻辑见 server.rs CancelIoEx）
    let st = service.query_status()?;
    if st.current_state != ServiceState::Stopped {
        println!("[*] 停止服务...");
        service.stop()?;
        for _ in 0..30 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if service.query_status()?.current_state != ServiceState::Stopped {
            return Err("服务停止超时".into());
        }
    }

    // 2) 覆盖部署副本（与 install 的 deploy_global 同一路径）
    let deploy_dir = std::path::Path::new("D:\\Kiln\\AI-root");
    std::fs::create_dir_all(deploy_dir)?;
    let deploy_exe = deploy_dir.join("ai-root.exe");
    // 自锁处理：update 若由部署 exe 自身运行，直接 copy 会因「映像被占用」报 os error 32。
    // Windows 允许重命名运行中 exe（映像以 FILE_SHARE_DELETE 打开），先改名 .old 腾出路径，
    // 再复制新文件；失败回滚，启动后清理 .old（残留可被下轮 update 清理）。
    let old_exe = deploy_dir.join("ai-root.exe.old");
    let _ = std::fs::remove_file(&old_exe);
    match std::fs::rename(&deploy_exe, &old_exe) {
        Ok(_) => {
            if let Err(e) = std::fs::copy(new_exe, &deploy_exe) {
                let _ = std::fs::rename(&old_exe, &deploy_exe); // 回滚旧文件
                return Err(format!("复制新二进制失败（已回滚旧版本）: {e}").into());
            }
            println!("✓ 已覆盖: {}（旧文件保留为 .old）", deploy_exe.display());
        }
        Err(_) => {
            std::fs::copy(new_exe, &deploy_exe)?;
            println!("✓ 已覆盖: {}", deploy_exe.display());
        }
    }

    // 3) 启动服务
    service.start::<OsString>(&[])?;
    println!("✓ 服务已启动");

    // 4) 自检
    let s = service.query_status()?;
    println!("服务状态: {:?}", s.current_state);

    // 5) 清理 .old（映像句柄允许删除；失败留待下轮 update 清理）
    let _ = std::fs::remove_file(&old_exe);
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
