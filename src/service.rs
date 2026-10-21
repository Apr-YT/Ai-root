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
use crate::config;
use crate::server;
use crate::winapi_raw;

/// 解析部署目录（服务副本与全局包装器的落地位置）。
///
/// 优先级：
///   1) 环境变量 `AIROOT_DEPLOY_DIR`（便于自定义/多实例/CI）
///   2) 通用默认值 `%ProgramData%\AI-Root`（机器级、非用户级、SYSTEM 可写）
///
/// 之所以不写死在源码里某个盘符路径：部署位置属于**环境相关配置**，
/// 硬编码会把某台机器的目录习惯带进发布物，也让他人无法按需调整。
fn deploy_dir() -> std::path::PathBuf {
    // 部署位置必须与配置/令牌/日志同目录（都在受 ACL 保护的机器级目录下），
    // 否则「服务二进制受保护」与「配置受保护」会分裂成两套边界。
    crate::config::machine_dir()
}

/// 将自身部署到 [`deploy_dir`] 并在 C:\Windows 写入 AI-Root.cmd 包装器，
/// 使 AI-Root 在任意目录、任意身份（含智能体非交互进程）下均可直接调用。
/// 返回服务应指向的可执行文件路径（部署成功返回部署副本，失败回退到当前 exe）。
fn deploy_global(src_exe: &std::path::Path) -> std::path::PathBuf {
    let dir = deploy_dir();
    let deploy_dir = dir.as_path();
    let deploy_exe = deploy_dir.join("AI-Root.exe");
    if let Err(e) = std::fs::create_dir_all(deploy_dir) {
        eprintln!("[warn] 无法创建部署目录 {}: {e}", deploy_dir.display());
        return src_exe.to_path_buf();
    }
    match std::fs::copy(src_exe, &deploy_exe) {
        Ok(_) => {
            // 全局包装器：转发所有参数（%*）。C:\Windows 默认在系统 PATH 中，
            // 写入后 AI-Root 在任意 shell / 任意身份下均可直接调用。
            let wrapper = std::path::Path::new("C:\\Windows\\AI-Root.cmd");
            let content = format!("@echo off\r\n\"{}\" %*\r\n", deploy_exe.display());
            if let Err(e) = std::fs::write(wrapper, content) {
                eprintln!("[warn] 无法写入全局包装器 {}: {e}", wrapper.display());
            } else {
                println!("✓ 已部署全局命令: AI-Root (C:\\Windows\\AI-Root.cmd)");
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
        winapi_raw::SC_ACTION {
            Type: winapi_raw::SC_ACTION_RESTART,
            Delay: 60_000,
        },
        winapi_raw::SC_ACTION {
            Type: winapi_raw::SC_ACTION_RESTART,
            Delay: 300_000,
        },
        winapi_raw::SC_ACTION {
            Type: winapi_raw::SC_ACTION_RESTART,
            Delay: 600_000,
        },
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
        eprintln!("[warn] 配置失败恢复策略失败, last_err={}", unsafe {
            winapi_raw::GetLastError()
        });
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
                "服务 {} 已存在（状态: {state}）。重装请先 `AI-Root uninstall`；仅更新二进制请用 `AI-Root update <新exe>`。",
                common::SERVICE_NAME
            )
            .into());
        }
    }

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;
    let src_exe = std::env::current_exe()?;
    // 部署全局副本并写入包装器（服务指向部署副本，便于后续统一升级）
    let exe_path = deploy_global(&src_exe);

    let info = ServiceInfo {
        name: OsString::from(common::SERVICE_NAME),
        display_name: OsString::from("AI-Root (SYSTEM 提权中继)"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: vec![OsString::from("--service")],
        account_name: None, // None = 以 LocalSystem 运行（最高本地权限）
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

    // 安装时即生成令牌并设置 ACL，确保服务启动后可用。
    // 任何一步失败都必须终止安装（fail closed）：宁可不装，也不留一个
    // 「服务已注册但权限可疑」的中间状态。
    common::ensure_token()?;
    // 持久化安装用户 SID，注入管道 SDDL，使智能体（该用户身份）可无人值守连管道拿 SYSTEM
    if let Some(sid) = common::get_user_sid() {
        common::save_user_sid(&sid);
    }
    common::set_enabled(true)?;

    println!("✓ 服务已注册: {} (LocalSystem, 自启)", common::SERVICE_NAME);
    println!("启动命令: AI-Root start");
    Ok(())
}

/// 卸载服务（需管理员权限）。
///
/// ## 清理边界（2026-10-21 修正）
///
/// 只清理**程序文件**与**凭据**，保留**用户数据**：
///
/// | 保留（重新安装后继续生效） | 删除（可再生成 / 必须清除） |
/// |---|---|
/// | `policy.toml` 管理员授权策略 | `AI-Root.exe` / `.old` / `.new` |
/// | `rules.d\` 自定义自动化规则 | `secrets\token.txt`（SYSTEM 凭据） |
/// | `logs\` 审计与事件日志 | `state\sid.txt`（安装用户身份记录） |
/// | `state\policy.initialized` 初始化标记 | `state\tmp\`（临时脚本） |
/// | `%LOCALAPPDATA%\AI-Root\user.toml` | `state\pending\`（待确认请求） |
/// |  | `state\enabled.flag`（服务已不存在） |
///
/// 为什么不再 `remove_dir_all`：旧实现删掉整个数据目录，紧接着却打印
/// 「执行日志/事件流保留于 …」，函数注释也写着「数据目录保留日志供审计查阅」——
/// 实现与声明**自相矛盾**。用户重装后会发现授权策略退回默认、审计记录消失，
/// 且没有任何提示。卸载不该是「静默重置全部配置」的同义词。
///
/// 需要彻底清除时用 `scripts/uninstall.ps1 -Purge`（它会先做完整备份）。
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(common::SERVICE_NAME, ServiceAccess::ALL_ACCESS)?;

    // ⚠ 必须先停止，再删除。
    //
    // `delete()` 对**运行中**的服务只做「标记删除」：进程会继续运行到自行退出，
    // SCM 则立即把它置为 DISABLED 且不再响应查询/启动。实测后果是
    // `-Force` 重装后：部署目录里的 exe 仍被占用删不掉（os error 5）、
    // 服务还在跑旧二进制、令牌也没重建，而调用方以为已经换版。
    // 先 stop 并等待 STOPPED，后续删文件才可能成功。
    if let Ok(st) = service.query_status() {
        if st.current_state != ServiceState::Stopped {
            let _ = service.stop();
            for _ in 0..20 {
                if let Ok(s) = service.query_status() {
                    if s.current_state == ServiceState::Stopped {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }

    service.delete()?;
    println!("✓ 服务已卸载: {}", common::SERVICE_NAME);

    let dir = deploy_dir();

    // ── 1. 程序文件（部署副本与更新暂存/备份）──
    let mut removed_program = 0usize;
    for name in ["AI-Root.exe", "AI-Root.exe.old", "AI-Root.exe.new"] {
        let p = dir.join(name);
        if !p.exists() {
            continue;
        }
        // 服务 delete 后进程退出是**异步**的，文件可能仍被占用。
        // 实测 -Force 重装时旧 exe 删除失败（os error 5），导致部署目录里
        // 残留上一版二进制、服务仍在跑旧代码。此处重试后再放弃。
        match remove_file_retry(&p, 6) {
            Ok(()) => {
                removed_program += 1;
                println!("✓ 已删除程序文件: {}", p.display());
            }
            // 重试仍失败才提示（例如被杀软长期锁定）。这不是安全问题
            // （凭据与配置已按下面的范围处理），如实说明而不是静默吞掉。
            Err(e) => eprintln!("[warn] 删除失败（可能仍被占用）{}: {e}", p.display()),
        }
    }

    // ── 2. 运行期残留（可安全重建）──
    let state = dir.join(config::STATE_DIR);
    for name in ["tmp", "pending"] {
        let p = state.join(name);
        if p.exists() {
            match std::fs::remove_dir_all(&p) {
                Ok(()) => println!("✓ 已清理运行期残留: {}", p.display()),
                Err(e) => eprintln!("[warn] 清理失败 {}: {e}", p.display()),
            }
        }
    }
    // 服务已不存在，启用标志留着只会让「是否启用」的判断产生歧义。
    let flag = state.join("enabled.flag");
    if flag.exists() {
        let _ = std::fs::remove_file(&flag);
    }

    let wrapper = std::path::Path::new("C:\\Windows\\AI-Root.cmd");
    if wrapper.exists() {
        match std::fs::remove_file(wrapper) {
            Ok(()) => println!("✓ 已清理全局包装器: {}", wrapper.display()),
            Err(e) => eprintln!("[warn] 清理包装器失败 {}: {e}", wrapper.display()),
        }
    }

    // ── 3. 凭据（必须清除，不留最高权限凭据残盘）──
    let tok = common::token_path();
    if tok.exists() {
        match std::fs::remove_file(&tok) {
            Ok(()) => println!("✓ 已清除令牌文件（SYSTEM 凭据不残留）"),
            Err(e) => eprintln!("[warn] 令牌删除失败（请手工确认）{}: {e}", tok.display()),
        }
    }
    let sid = common::user_sid_path();
    if sid.exists() {
        match std::fs::remove_file(&sid) {
            Ok(()) => println!("✓ 已清除安装用户 SID 记录"),
            Err(e) => eprintln!("[warn] SID 记录删除失败 {}: {e}", sid.display()),
        }
    }

    // ── 4. 明确告知保留了什么（避免用户以为配置被删）──
    let paths = config::Paths::resolve();
    let mut kept: Vec<std::path::PathBuf> = Vec::new();
    for p in [paths.policy(), paths.rules_dir(), paths.logs(), paths.user_file()] {
        if p.exists() {
            kept.push(p);
        }
    }
    if kept.is_empty() {
        println!("提示: 未发现需要保留的用户数据（可能为首次安装后立即卸载）。");
    } else {
        println!("以下用户数据已保留（重新安装后继续生效）:");
        for p in &kept {
            println!("  - {}", p.display());
        }
        println!("彻底清除请用: scripts\\uninstall.ps1 -Purge（会先备份）");
    }
    println!("提示: 本次删除 {removed_program} 个程序文件。");
    Ok(())
}

/// 删除文件并在失败时重试。
///
/// 服务 `delete()` 只把服务标记为删除，**进程退出是异步的**。紧接着删 exe 往往
/// 因文件仍被占用而失败（实测 `-Force` 重装时 os error 5），结果部署目录里留下
/// 上一版二进制、服务继续运行旧代码，而调用方以为已经换版。
fn remove_file_retry(p: &std::path::Path, attempts: u32) -> std::io::Result<()> {
    let mut last: Option<std::io::Error> = None;
    for i in 0..attempts {
        match std::fs::remove_file(p) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "删除失败且无错误信息")
    }))
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
    println!(
        "✓ 启动指令已下发（服务状态: {:?}）",
        service.query_status()?.current_state
    );
    Ok(())
}

/// 给当前用户 SID 追加服务 STOP/START 权限（服务 DACL 标准机制）。
///
/// 权限位：`SERVICE_START`=0x10=SDDL `RP`，`SERVICE_STOP`=0x20=SDDL `WP`。
/// 效果：智能体进程（以安装用户身份运行，非提权）此后可执行
/// `AI-Root stop/start/update`，无需管理员 → 二进制更新彻底自治。
/// 失败不致命（install 继续，仅降级为「更新仍需管理员」）。
///
/// 可独立以 `AI-Root grant` 调用（需管理员）：用于**对已存在的服务**补发授权，
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
        return Err(format!("SDDL 转换失败, last_err={}", unsafe {
            winapi_raw::GetLastError()
        })
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
        unsafe {
            winapi_raw::LocalFree(sd);
        }
        return Err(format!("OpenSCManagerW 失败, last_err={}", unsafe {
            winapi_raw::GetLastError()
        })
        .into());
    }
    let svc =
        unsafe { winapi_raw::OpenServiceW(scm, name_w.as_ptr(), winapi_raw::SERVICE_ALL_ACCESS) };
    if svc.is_null() {
        unsafe {
            winapi_raw::CloseServiceHandle(scm);
            winapi_raw::LocalFree(sd);
        }
        return Err(format!("OpenServiceW 失败, last_err={}", unsafe {
            winapi_raw::GetLastError()
        })
        .into());
    }

    let ok = unsafe {
        winapi_raw::SetServiceObjectSecurity(svc, winapi_raw::DACL_SECURITY_INFORMATION, sd)
    };
    unsafe {
        winapi_raw::CloseServiceHandle(svc);
        winapi_raw::CloseServiceHandle(scm);
        winapi_raw::LocalFree(sd);
    }
    if ok == 0 {
        return Err(
            format!("SetServiceObjectSecurity 失败, last_err={}", unsafe {
                winapi_raw::GetLastError()
            })
            .into(),
        );
    }
    println!("✓ 服务 ACL 已授权用户 STOP/START（智能体可自主更新）");
    Ok(())
}

/// 读取 PE 的 Machine 字段（0x8664 = x64 / 0x014C = x86 / 0xAA64 = ARM64）。
/// 非 PE 或头损坏返回 None。
fn pe_machine(path: &std::path::Path) -> Option<u16> {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 0x40];
    f.read_exact(&mut head).ok()?;
    if &head[0..2] != b"MZ" {
        return None;
    }
    let pe_off = u32::from_le_bytes([head[0x3C], head[0x3D], head[0x3E], head[0x3F]]) as usize;
    if pe_off == 0 || pe_off > 0x1000 {
        return None; // 异常偏移防御
    }
    let mut pe = [0u8; 0x18];
    f.seek(std::io::SeekFrom::Start(pe_off as u64)).ok()?;
    f.read_exact(&mut pe).ok()?;
    if &pe[0..4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([pe[4], pe[5]]))
}

/// 服务二进制的期望架构（必须与当前进程一致）
fn expected_machine() -> u16 {
    if cfg!(target_arch = "x86_64") {
        0x8664
    } else if cfg!(target_arch = "aarch64") {
        0xAA64
    } else {
        0x014C
    }
}

/// 校验文件是有效 PE **且架构与服务一致**。
///
/// 注：`update_self` 内部直接比较 `pe_machine` 以获得更精确的错误信息，
/// 本函数保留给单元测试与将来的调用方使用。
///
/// 改造点：旧实现接受 x64/x86 任意一种 —— 用 x86 二进制替换 x64 服务会让服务
/// 直接起不来（甚至留下无法回滚的坏状态）。架构必须精确匹配。
#[allow(dead_code)] // 见上方说明：update 内部用 pe_machine 直接比较
fn is_valid_pe(path: &std::path::Path) -> bool {
    pe_machine(path) == Some(expected_machine())
}

/// 探测二进制自报的版本（`<exe> --version` → 取最后一段版本号）。
///
/// 仅在**签名验证通过之后**调用：此时该文件已被确认由可信发布者签名，
/// 执行它取版本不会引入新的信任假设。
fn probe_version(exe: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(exe).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .next()?
        .split_whitespace()
        .last()
        .map(|v| v.trim().to_string())
}

/// 版本号 → 可比较元组（缺失段按 0）
fn version_tuple(v: &str) -> (u64, u64, u64) {
    let mut it = v.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// 轮询等待服务进入 Running（秒）
fn wait_running(svc: &windows_service::service::Service, secs: u64) -> bool {
    for _ in 0..(secs * 2) {
        if let Ok(st) = svc.query_status() {
            if st.current_state == ServiceState::Running {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// 自主更新服务二进制。
///
/// ## 安全模型（2026-10-21 重做）
///
/// 旧实现的防护是「PE 头校验 + 打印 SHA-256」。这在安全上等于零：
/// 任何能执行 `AI-Root update` 的人都能把任意 PE 变成 LocalSystem 服务，
/// 而 `install` 又主动给安装用户授予了服务 STOP/START 权限 ——
/// 于是「非提权用户 → SYSTEM」是一条不需要令牌、不需要读文件的直路。
///
/// 现在替换为**三道门控**，任一不满足即拒绝：
///
/// 1. **签名**：`WinVerifyTrust` 验证 Authenticode 嵌入签名与证书链完整性；
/// 2. **发布者绑定**：签名者证书 SHA-256 必须命中策略 `trusted_publishers`
///    （只验「有签名」不够 —— 任何人都能自签一张证书）；
/// 3. **目标固定**：写入目标取自 SCM 的实际 `ImagePath`，并强制它位于受 ACL
///    保护的数据目录内；不再用环境变量 `AIROOT_DEPLOY_DIR` 推算（那既会写错位置，
///    也是一个「以 SYSTEM 身份向任意目录写文件」的原语）。
///
/// 另加：架构必须与当前进程一致；拒绝版本降级；复制后二次校验 SHA-256；
/// 启动失败自动回滚到 `.old`。
///
/// ## 流程
/// 校验输入 → 加载策略/信任根 → 验签 → 定位 ImagePath → 版本比对 →
/// 停服务 → 复制到 `.new` → 校验 → 换名（旧文件留 `.old`）→ 启动 → 健康检查
/// → 成功删 `.old` / 失败回滚。
pub fn update_self(new_exe: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use crate::policy;
    use crate::signature;

    // ── 0. 输入基本校验 ──
    if !new_exe.is_absolute() {
        return Err(format!("必须提供绝对路径: {}", new_exe.display()).into());
    }
    if !new_exe.is_file() {
        return Err(format!("文件不存在: {}", new_exe.display()).into());
    }
    let machine = pe_machine(new_exe)
        .ok_or_else(|| format!("不是有效的 PE 可执行文件: {}", new_exe.display()))?;
    if machine != expected_machine() {
        return Err(format!(
            "架构不匹配：目标 Machine=0x{machine:04X}，服务要求 0x{:04X}",
            expected_machine()
        )
        .into());
    }

    // ── 1. 信任根 + 签名验证（不可绕过）──
    let data_dir = common::data_dir();
    let pol = config::load(&config::Paths::resolve()).policy;
    if pol.is_degraded() {
        return Err("策略文件不可用（降级状态），拒绝更新".into());
    }
    if pol.file.trusted_publishers.is_empty() {
        return Err(
            "未配置可信发布者，拒绝更新。\n\
             建立信任根（需管理员，对已发布版本执行一次）：\n\
              AI-Root trust-publisher <已签名的新版本 AI-Root.exe>\n\
             注意：只接受 Authenticode **嵌入签名**；catalog 签名（System32 常见）不被接受。"
                .into(),
        );
    }
    let report = signature::verify_file(new_exe, &pol.file.trusted_publishers);
    if !report.is_acceptable() {
        common::log_event(serde_json::json!({
            "ts": common::now_str(),
            "type": "update_rejected",
            "path": new_exe.to_string_lossy(),
            "chain_trusted": report.chain_trusted,
            "trust_error": report.trust_error,
            "signer": report.signer_subject,
            "thumbprint": report.signer_thumbprint,
            "publisher_allowed": report.publisher_allowed,
        }));
        return Err(format!(
            "签名验证失败，拒绝更新：\n  信任链: {}\n  失败码: {}\n  签名者: {}\n  证书指纹: {}\n  是否命中信任列表: {}\n\
             （只接受由 trusted_publishers 中证书签名的二进制）",
            if report.chain_trusted { "通过" } else { "未通过" },
            report.trust_error.as_deref().unwrap_or("-"),
            report.signer_subject.as_deref().unwrap_or("<无签名>"),
            report.signer_thumbprint.as_deref().unwrap_or("-"),
            if report.publisher_allowed { "是" } else { "否" },
        )
        .into());
    }
    // 二次确认：策略侧独立再判一次发布者（纵深防御 ——
    // 即使 verify_file 内部的比对逻辑被改坏，这里也能拦住）
    if let Some(thumb) = &report.signer_thumbprint {
        if !pol.publisher_trusted(thumb) {
            return Err(format!(
                "发布者证书 {thumb} 不在 trusted_publishers 中，拒绝更新"
            )
            .into());
        }
    } else {
        return Err("无法提取签名者证书指纹，拒绝更新".into());
    }

    println!(
        "✓ 签名验证通过：{}",
        report.signer_subject.as_deref().unwrap_or("<未知发布者>")
    );
    if let Some(h) = &report.file_sha256 {
        println!("  新二进制 SHA256: {h}");
    }

    // ── 2. 目标路径 = 服务实际注册的 ImagePath（不用环境变量推算）──
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        common::SERVICE_NAME,
        ServiceAccess::QUERY_CONFIG
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::STOP
            | ServiceAccess::START,
    )?;
    let cfg = service.query_config()?;
    let deploy_exe = cfg.executable_path.clone();
    let deploy_dir = deploy_exe
        .parent()
        .ok_or("无法从服务 ImagePath 确定部署目录")?
        .to_path_buf();

    // 目标必须落在受 ACL 保护的数据目录内。否则说明 ImagePath 被改到了
    // 用户可写位置（或部署方式不是本程序 install 的）—— 此时更新等于把
    // 一个 SYSTEM 服务指向不可信目录，必须拒绝并提示重新安装。
    let norm_target = policy::normalize_win_path(&deploy_exe.to_string_lossy());
    let norm_data = policy::normalize_win_path(&data_dir.to_string_lossy());
    if !(norm_target == norm_data || norm_target.starts_with(&format!("{norm_data}/"))) {
        return Err(format!(
            "服务镜像路径不在受保护数据目录内，拒绝更新：\n  ImagePath: {}\n  期望前缀: {}\n\
             （可能被篡改，或由旧版本以其它路径安装；请以管理员重新执行 install）",
            deploy_exe.display(),
            data_dir.display()
        )
        .into());
    }
    println!("  目标路径: {}", deploy_exe.display());

    // ── 3. 版本比对（拒绝降级）──
    let new_ver = probe_version(new_exe);
    let cur_exe = std::env::current_exe()?;
    let cur_ver = probe_version(&cur_exe);
    if let (Some(n), Some(c)) = (&new_ver, &cur_ver) {
        println!("  版本: {c} → {n}");
        if version_tuple(n) < version_tuple(c) {
            return Err(format!(
                "拒绝版本降级：当前 {c}，目标 {n}。\n\
                 如需回滚，请在离线环境手动替换 .old 并重启服务。"
            )
            .into());
        }
    } else {
        println!("  [warn] 无法探测版本（跳过降级检查，签名已验证）");
    }

    // ── 4. 停服务（轮询等待，最多 ~15s）──
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
            return Err(
                "服务停止超时（15s 内未变为 Stopped）。现有二进制尚未改动，可安全重试。".into(),
            );
        }
    }

    // ── 5. 分阶段替换：先复制到 .new 并复核，再换名（旧文件留 .old）──
    let staged = deploy_dir.join("AI-Root.exe.new");
    let backup = deploy_dir.join("AI-Root.exe.old");
    let _ = std::fs::remove_file(&staged);
    if let Err(e) = std::fs::copy(new_exe, &staged) {
        return Err(format!("复制到暂存文件失败（服务已停止，原文件未动）: {e}").into());
    }
    // 二次校验：防止复制被截断或暂存文件在复制后被替换
    let staged_sha = signature::sha256_file(&staged).ok();
    if staged_sha != report.file_sha256 {
        let _ = std::fs::remove_file(&staged);
        let _ = service.start::<OsString>(&[]);
        return Err("复制后校验失败：暂存文件 SHA-256 与源文件不一致（已中止并重启旧版本）".into());
    }

    let _ = std::fs::remove_file(&backup);
    // 先改名旧文件（Windows 允许重命名运行中的 exe；失败则说明被占用 → 中止且未破坏）
    if let Err(e) = std::fs::rename(&deploy_exe, &backup) {
        let _ = std::fs::remove_file(&staged);
        let _ = service.start::<OsString>(&[]);
        return Err(format!("无法移开旧二进制（可能被占用），已中止并重启旧版本: {e}").into());
    }
    if let Err(e) = std::fs::rename(&staged, &deploy_exe) {
        // 回滚：把旧文件放回去
        let _ = std::fs::rename(&backup, &deploy_exe);
        let _ = service.start::<OsString>(&[]);
        return Err(format!("替换失败（已回滚到旧版本）: {e}").into());
    }
    println!("✓ 已替换: {}", deploy_exe.display());

    // ── 6. 启动 + 健康检查；失败自动回滚 ──
    let started = service.start::<OsString>(&[]).is_ok();
    let healthy = started && wait_running(&service, 20);
    if !healthy {
        common::log_event(serde_json::json!({
            "ts": common::now_str(),
            "type": "update_rollback",
            "reason": "新版本启动失败或未通过健康检查",
            "deploy": deploy_exe.to_string_lossy(),
        }));
        let _ = service.stop();
        std::thread::sleep(Duration::from_millis(500));
        let _ = std::fs::remove_file(&deploy_exe);
        let _ = std::fs::rename(&backup, &deploy_exe);
        let _ = service.start::<OsString>(&[]);
        return Err(
            "新版本未能在 20s 内进入 Running，已回滚到上一版本并重启。请检查事件日志。".into(),
        );
    }
    println!("✓ 服务已启动并通过健康检查");

    // ── 7. 清理备份（失败仅提示，可被下轮更新覆盖）──
    if let Err(e) = std::fs::remove_file(&backup) {
        println!("  [note] 旧版本备份未能删除（下轮更新会覆盖）: {e}");
    }
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "update_done",
        "from": cur_ver,
        "to": new_ver,
        "sha256": report.file_sha256,
        "signer": report.signer_subject,
    }));
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
        eprintln!("[AI-Root] 服务运行错误: {e}");
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
                unsafe {
                    winapi_raw::SetEvent(svc_stop_event as winapi_raw::HANDLE);
                }
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
    // 服务启动：令牌与 ACL 不可用时**拒绝启动**（fail closed）。
    // 旧实现用 `.ok()` 吞掉错误，等于「权限异常也照常提供 SYSTEM 中继」。
    common::ensure_token()
        .map_err(|e| format!("令牌/ACL 初始化失败，拒绝启动：{e}"))?;
    common::set_enabled(true).map_err(|e| format!("启用标志/ACL 初始化失败，拒绝启动：{e}"))?;

    // 运行管道服务，直到收到 stop 事件（监控线程会中断阻塞的 ConnectNamedPipe）
    server::run_pipe_server(stop_event).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // 服务停止前强制落盘缓冲日志（events.jsonl 实时 flush；exec.log 审计可能仍在缓冲），
    // 防止常驻句柄上的审计条目因进程退出未 flush 而丢失。
    common::flush_logs();

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    unsafe {
        winapi_raw::CloseHandle(stop_event);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expected_machine, is_valid_pe, pe_machine, version_tuple};

    #[test]
    fn pe_machine_reads_current_exe() {
        // 当前进程自身必然是可执行文件，且架构与 expected_machine 一致
        let exe = std::env::current_exe().expect("取当前 exe 失败");
        assert_eq!(
            pe_machine(&exe),
            Some(expected_machine()),
            "当前 exe 的 Machine 必须与编译目标架构一致"
        );
        assert!(is_valid_pe(&exe));
    }

    #[test]
    fn pe_machine_rejects_non_pe() {
        let p = std::env::temp_dir().join(format!("airoot-nonpe-{}", std::process::id()));
        std::fs::write(&p, b"#!/bin/sh
echo not a pe
").unwrap();
        assert_eq!(pe_machine(&p), None);
        assert!(!is_valid_pe(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pe_machine_rejects_truncated_file() {
        let p = std::env::temp_dir().join(format!("airoot-trunc-{}", std::process::id()));
        std::fs::write(&p, b"MZ").unwrap();
        assert_eq!(pe_machine(&p), None, "只有 MZ 的短文件必须被拒");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn version_tuple_parses_and_orders() {
        assert_eq!(version_tuple("1.0.0"), (1, 0, 0));
        assert_eq!(version_tuple("1.2"), (1, 2, 0));
        assert_eq!(version_tuple("2.0.0-beta"), (2, 0, 0));
        assert!(version_tuple("1.10.0") > version_tuple("1.9.9"));
        assert!(version_tuple("1.0.0") == version_tuple("1.0.0"));
    }
}
