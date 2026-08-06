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

/// 配置 SCM 失败恢复策略：服务异常终止（7034）后按 60s/300s/600s 间隔自动重启，
/// 防止进程被意外杀死后中继长时间不可用。install 注册服务后调用一次；失败仅降级（无自动重启）。
fn configure_failure_actions(svc: &windows_service::service::Service) {


    let mut actions = [
        winapi_raw::SC_ACTION { Type: winapi_raw::SC_ACTION_RESTART, Delay: 60_000 },
        winapi_raw::SC_ACTION { Type: winapi_raw::SC_ACTION_RESTART, Delay: 300_000 },
        winapi_raw::SC_ACTION { Type: winapi_raw::SC_ACTION_RESTART, Delay: 600_000 },
    ];
    let fa = winapi_raw::SERVICE_FAILURE_ACTIONS {
        dwResetPeriod: 86_400, // 重置失败计数周期：24h
        lpRebootMsg: std::ptr::null(),
        lpCommand: std::ptr::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let ok = unsafe {
        winapi_raw::ChangeServiceConfig2W(
            svc.raw_handle() as winapi_raw::HANDLE,
            winapi_raw::SERVICE_CONFIG_FAILURE_ACTIONS,
            &fa as *const winapi_raw::SERVICE_FAILURE_ACTIONS as *const core::ffi::c_void,
        )
    };
    if ok != 0 {
        println!("✓ 已配置失败自动重启（60s/300s/600s）");
    } else {
        eprintln!(
            "[warn] 配置失败恢复策略失败, last_err={}",
            unsafe { winapi_raw::GetLastError() }
        );
    }
}

/// 注册为 LocalSystem 自启服务（需管理员权限）
pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    // 幂等检测：服务已存在时给出明确指引（重装走 uninstall，更新走 update），
    // 避免 create_service 报笼统的 IO error 让人无从下手。
    if let Ok(m) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        if let Ok(svc) = m.open_service(common::SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            let st = svc.query_status().ok();
            let state = st
                .map(|s| format!("{:?}", s.current_state))
                .unwrap_or_else(|| "unknown".to_string());
            return Err(format!(
                "服务 {} 已存在（状态: {state}）。重装请先 `ai-root uninstall`；仅更新二进制请用 `ai-root update <新exe>`。",
                common::SERVICE_NAME
            )
            .into());
        }
    }

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

    let service = manager.create_service(&info, ServiceAccess::ALL_ACCESS)?;

    // 配置 SCM 失败恢复策略（异常终止自动重启，防 7034 后中继长时间不可用）
    configure_failure_actions(&service);

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

/// 卸载服务（需管理员权限）。删除服务 + 清理部署残留（令牌/部署目录/包装器/用户SID）。
/// 令牌是 SYSTEM 凭据，卸载后必须清除，避免最高权限凭据残留盘上。
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(common::SERVICE_NAME, ServiceAccess::ALL_ACCESS)?;
    service.delete()?;
    println!("✓ 服务已卸载: {}", common::SERVICE_NAME);

    // 清理部署残留（尽力而为，失败仅提示）
    let deploy_dir = std::path::Path::new("D:\\Kiln\\AI-root");
    if deploy_dir.exists() {
        let _ = std::fs::remove_dir_all(deploy_dir);
        println!("✓ 已清理部署目录: {}", deploy_dir.display());
    }
    let wrapper = std::path::Path::new("C:\\Windows\\ai-root.cmd");
    if wrapper.exists() {
        let _ = std::fs::remove_file(wrapper);
        println!("✓ 已清理全局包装器: {}", wrapper.display());
    }
    // 令牌（SYSTEM 凭据）必须清除；数据目录保留日志供审计查阅
    let tok = common::token_path();
    if tok.exists() {
        let _ = std::fs::remove_file(&tok);
        println!("✓ 已清除令牌文件（SYSTEM 凭据不残留）");
    }
    let sid = common::user_sid_path();
    if sid.exists() {
        let _ = std::fs::remove_file(&sid);
        println!("✓ 已清除安装用户 SID 记录");
    }
    println!(
        "提示: 执行日志/事件流保留于 {}（如需彻底清除请手动删除目录）",
        common::data_dir().display()
    );
    Ok(())
}

pub fn start_svc() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        common::SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.start::<OsString>(&[])?;
    // 轮询等待 Running（最多 15s，防「报成功但未真正起来」）
    for _ in 0..30 {
        if service.query_status()?.current_state == ServiceState::Running {
            println!("✓ 服务已启动");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("✓ 启动指令已下发（服务状态: {:?}）", service.query_status()?.current_state);
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
    // 保留默认 ACE（SYSTEM/Administrators 完整权限），追加用户 GRGX。
    // 服务对象 GENERIC 映射：GENERIC_READ = QUERY_CONFIG|QUERY_STATUS|INTERROGATE|READ_CONTROL；
    // GENERIC_EXECUTE = START|STOP|PAUSE_CONTINUE|READ_CONTROL。
    // 必须覆盖 QUERY_STATUS：`status`/`stop`/`start`/`update` 均需以 SERVICE_QUERY_STATUS 打开服务，
    // 缺它会导致非提权用户 OpenService 失败（status 误报未安装、update 报 IO error）。
    // 注意：不要用 RP/WP/LO 等零散位组合——实测 SDDL 字母 LO 并非 SERVICE_QUERY_STATUS(0x04)，
    // 授予 RPLOWP 后 START/STOP 生效但 QUERY_STATUS 仍被拒；GRGX 组合经 sc sdset 实测全部通过。
    // BA 必须保持 CCDCLCSWRPWPDTLOCRSDRCWDWO（Administrators 标准权限，含 DELETE/DC/WD/WO）：
    // 若像 SYSTEM 一样只写 CCLCSWRPWPDTLOCRRC，会降级 BA 导致二次 grant / uninstall 被拒(last_err=5)。
    let sddl = format!(
        "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;GRGX;;;{sid})"
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

/// 校验文件是否为有效 PE 可执行文件（MZ 头 + PE 签名 + 机器码段）。
/// update 前调用，防止把非可执行文件（损坏/纯数据）覆盖成 SYSTEM 服务。
fn is_valid_pe(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 0x40];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    // DOS 头 MZ 签名
    if &head[0..2] != b"MZ" {
        return false;
    }
    // e_lfanew 偏移（0x3C 处 4 字节 LE）→ PE 签名
    let pe_off = u32::from_le_bytes([head[0x3C], head[0x3D], head[0x3E], head[0x3F]]) as usize;
    if pe_off > 0x1000 {
        return false; // 异常偏移防御
    }
    let mut pe = [0u8; 0x18];
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    use std::io::Seek;
    if f.seek(std::io::SeekFrom::Start(pe_off as u64)).is_err() {
        return false;
    }
    if f.read_exact(&mut pe).is_err() {
        return false;
    }
    // PE\0\0 签名 + Machine（0x8664=x64 / 0x014C=x86）
    if &pe[0..4] != b"PE\0\0" {
        return false;
    }
    let machine = u16::from_le_bytes([pe[4], pe[5]]);
    matches!(machine, 0x8664 | 0x014C)
}

/// 计算文件 SHA-256（certutil -hashfile），返回 hex 字符串；失败返回空。
/// 供 update 自检输出，便于审计新二进制指纹。
fn file_sha256(path: &std::path::Path) -> String {
    let out = match std::process::Command::new("certutil")
        .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return String::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout);
    // certutil 输出格式：文件 + 空行 + 64 hex + 空行
    for line in s.lines() {
        let t = line.trim();
        if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return t.to_lowercase();
        }
    }
    String::new()
}

/// 自主更新服务二进制（需 install 时已对当前用户授权 STOP/START）。
///
/// ⚠ 安全说明：本命令将任意 exe 替换为 SYSTEM 服务并启动，等效于 SYSTEM 代码执行。
/// 仅应授予可信进程（如本机智能体）。防护仅为 PE 有效性校验 + SHA256 指纹输出，
/// 如需强校验请对发布物做 Authenticode 签名并在外部流程中核验。
///
/// 流程：停止服务（轮询等待 Stopped）→ 覆盖部署副本 → 启动服务 → 自检状态。
pub fn update_self(new_exe: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // 0) 新 exe 有效性校验（防覆盖成垃圾文件导致服务无法启动）
    if !is_valid_pe(new_exe) {
        return Err(format!(
            "拒绝更新：{} 不是有效的 PE 可执行文件（MZ/PE 头校验失败）",
            new_exe.display()
        )
        .into());
    }
    let sha = file_sha256(new_exe);
    if !sha.is_empty() {
        println!("新二进制 SHA256: {sha}");
    }

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
