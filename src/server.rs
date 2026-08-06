//! 命名管道服务端：监听连接、鉴权、执行、回写结果
//!
//! 安全要点：
//! - 管道通过 SDDL 限制连接方为 SYSTEM / Administrators / Interactive
//!   `D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;IU)`
//! - 每个连接须携带正确共享令牌，否则拒绝
//! - 受「启用开关」约束，禁用时不执行

use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::common;
use crate::exec;
use crate::protocol::{self, Request, Response};
use crate::winapi_raw;

/// 鉴权失败节流：30s 窗口内最多失败 5 次，超出则拒绝所有请求 60s（防暴力尝试）。
struct AuthThrottle {
    window_start: Instant,
    fail_count: u32,
    locked_until: Option<Instant>,
}

impl AuthThrottle {
    fn new() -> Self {
        AuthThrottle {
            window_start: Instant::now(),
            fail_count: 0,
            locked_until: None,
        }
    }
    /// 记录一次失败；返回是否已触发锁定
    fn record_fail(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) > Duration::from_secs(30) {
            self.window_start = now;
            self.fail_count = 0;
        }
        self.fail_count += 1;
        if self.fail_count >= 5 {
            self.locked_until = Some(now + Duration::from_secs(60));
            return true;
        }
        false
    }
    /// 是否处于锁定态
    fn is_locked(&self) -> bool {
        match self.locked_until {
            Some(t) if Instant::now() < t => true,
            _ => false,
        }
    }
}

/// 全局鉴权节流器 + 并发信号量计数
static AUTH_THROTTLE: Mutex<Option<AuthThrottle>> = Mutex::new(None);
/// 当前在执行的连接数（并发上限 8，防失控 AI 线程轰炸拖垮 SYSTEM 服务）
static ACTIVE_CONNS: AtomicU32 = AtomicU32::new(0);
const MAX_CONNS: u32 = 8;

/// 创建带安全描述符的命名管道实例，返回原生 HANDLE
///
/// SDDL 由 `common::pipe_sddl()` 动态构造：基础授权 SYSTEM/Admins/Interactive，
/// 并追加安装用户 SID（若存在），使以该用户身份运行的智能体进程可无人值守连接。
fn create_pipe() -> io::Result<winapi_raw::HANDLE> {
    let sddl_w = common::to_wide(&common::pipe_sddl());
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    let converted = unsafe {
        winapi_raw::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("SDDL 转换失败, last_err={}", unsafe { winapi_raw::GetLastError() }),
        ));
    }

    let sa = winapi_raw::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<winapi_raw::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };

    let name_w = common::to_wide(&common::pipe_name());
    let handle = unsafe {
        winapi_raw::CreateNamedPipeW(
            name_w.as_ptr(),
            winapi_raw::PIPE_ACCESS_DUPLEX,
            winapi_raw::PIPE_TYPE_BYTE | winapi_raw::PIPE_READMODE_BYTE | winapi_raw::PIPE_WAIT,
            winapi_raw::PIPE_UNLIMITED_INSTANCES,
            8192,
            8192,
            0,
            &sa as *const winapi_raw::SECURITY_ATTRIBUTES,
        )
    };

    // 安全描述符不再需要
    unsafe {
        winapi_raw::LocalFree(sd);
    }

    if handle == winapi_raw::INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "CreateNamedPipeW 失败, last_err={}",
                unsafe { winapi_raw::GetLastError() }
            ),
        ));
    }
    Ok(handle)
}

/// 接受单个客户端连接，成功后返回封装的 File
fn accept_one() -> io::Result<File> {
    let handle = create_pipe()?;
    let connected = unsafe { winapi_raw::ConnectNamedPipe(handle, std::ptr::null_mut()) };
    if connected == 0 {
        let err = unsafe { winapi_raw::GetLastError() };
        // ERROR_PIPE_CONNECTED：连接在 CreateNamedPipe 与 ConnectNamedPipe 之间已建立
        if err != winapi_raw::ERROR_PIPE_CONNECTED {
            unsafe { winapi_raw::CloseHandle(handle) };
            return Err(io::Error::new(io::ErrorKind::Other, "ConnectNamedPipe 失败"));
        }
    }
    // 转移句柄所有权给 Rust File
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// 获取管道对端进程身份：GetNamedPipeClientProcessId → OpenProcess → TokenUser → LookupAccountSid。
/// 返回 (domain\user, pid)。任一步失败降级返回仅 PID 或 None（不影响执行，仅日志降级）。
fn client_identity(file: &File) -> Option<(String, u32)> {
    use core::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    let handle = file.as_raw_handle() as winapi_raw::HANDLE;
    let mut pid: u32 = 0;
    if unsafe { winapi_raw::GetNamedPipeClientProcessId(handle, &mut pid) } == 0 || pid == 0 {
        return None;
    }

    let proc = unsafe {
        winapi_raw::OpenProcess(winapi_raw::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
    };
    if proc.is_null() {
        return Some((format!("pid={pid}"), pid));
    }
    let mut token: winapi_raw::HANDLE = std::ptr::null_mut();
    let tok_ok = unsafe { winapi_raw::OpenProcessToken(proc, winapi_raw::TOKEN_QUERY, &mut token) };
    unsafe { winapi_raw::CloseHandle(proc); }
    if tok_ok == 0 || token.is_null() {
        return Some((format!("pid={pid}"), pid));
    }

    // 先查 TokenUser 所需缓冲大小（期望 ERROR_INSUFFICIENT_BUFFER + need）
    let mut need: u32 = 0;
    unsafe {
        winapi_raw::GetTokenInformation(token, winapi_raw::TOKEN_USER, std::ptr::null_mut(), 0, &mut need);
    }
    let mut buf = vec![0u8; need.max(1) as usize];
    let got = unsafe {
        winapi_raw::GetTokenInformation(
            token,
            winapi_raw::TOKEN_USER,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut need,
        )
    };
    unsafe { winapi_raw::CloseHandle(token); }
    if got == 0 {
        return Some((format!("pid={pid}"), pid));
    }

    let tu = unsafe { &*(buf.as_ptr() as *const winapi_raw::TOKEN_USER) };
    let sid = tu.User.Sid;
    if sid.is_null() {
        return Some((format!("pid={pid}"), pid));
    }

    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain_len = domain.len() as u32;
    let mut use_: u32 = 0;
    let lok = unsafe {
        winapi_raw::LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    if lok == 0 {
        return Some((format!("pid={pid}"), pid));
    }
    let user = String::from_utf16_lossy(&name[..name_len as usize]);
    let dom = String::from_utf16_lossy(&domain[..domain_len as usize]);
    Some((format!("{dom}\\{user}"), pid))
}

/// 恒定时间字符串比较（防时序侧信道逐字符猜测令牌）。
/// 逐字节 XOR 累加，任意位置差异都会使结果非零；长度差异视为不等。
fn ct_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..ab.len() {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

/// 处理单个客户端：读请求 -> 鉴权 -> 执行 -> 回写
fn handle_client(mut file: File) {
    // 并发信号量：超出上限直接拒绝（防线程轰炸）
    if ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response("服务繁忙（并发超限），请稍后重试")).unwrap(),
        );
        return;
    }
    // 作用域结束自动释放并发计数
    struct ConnGuard;
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _guard = ConnGuard;

    let token = common::cached_token();
    // 审计：先解析管道对端调用者身份（PID + 用户名）
    let (caller, caller_pid) = match client_identity(&file) {
        Some((u, p)) => (u, p),
        None => ("unknown".to_string(), 0),
    };

    // 鉴权节流：锁定期间拒绝一切请求
    {
        let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
        if th.get_or_insert_with(AuthThrottle::new).is_locked() {
            let _ = protocol::write_frame(
                &mut file,
                &serde_json::to_vec(&protocol::err_response(
                    "鉴权失败次数过多，已临时锁定（60s），请稍后再试",
                ))
                .unwrap(),
            );
            common::log_exec(&caller, caller_pid, "<locked>", -403);
            return;
        }
    }

    // 读请求（带 10s 读超时，防客户端连接后不发数据导致线程泄漏）
    let frame = match protocol::read_frame_timeout(&mut file, 10_000) {
        Ok(f) => f,
        Err(_) => return,
    };
    let req: Request = match serde_json::from_slice(&frame) {
        Ok(r) => r,
        Err(_) => {
            let _ = protocol::write_frame(
                &mut file,
                &serde_json::to_vec(&protocol::err_response("请求解析失败")).unwrap(),
            );
            return;
        }
    };

    // 令牌鉴权（恒定时间比较）
    if !ct_eq(&req.token, &token) {
        // 记录失败 + 审计（暴力尝试必须可追溯）
        let locked = {
            let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
            th.get_or_insert_with(AuthThrottle::new).record_fail()
        };
        common::log_exec(&caller, caller_pid, &format!("<auth-fail cmd={}>", req.command), -401);
        let msg = if locked {
            "令牌鉴权失败（多次失败已锁定 60s）".to_string()
        } else {
            "令牌鉴权失败".to_string()
        };
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response(&msg)).unwrap(),
        );
        return;
    }
    // 鉴权成功后重置失败计数（防正常用户被历史失败误锁）
    {
        let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = th.as_mut() {
            if !t.is_locked() {
                t.fail_count = 0;
            }
        }
    }

    // 启用开关
    if !common::is_enabled() {
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response("中继已禁用")).unwrap(),
        );
        return;
    }

    // 超时钳制：客户端可传超时，但上限 10 分钟（防持令牌客户端长期占用 SYSTEM 服务）
    let timeout_ms = req.timeout_ms.unwrap_or(60_000).min(600_000);

    // GUI 实时信息流：执行开始事件
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "exec_start",
        "caller": caller,
        "pid": caller_pid,
        "cmd": req.command,
        "timeout_ms": timeout_ms,
    }));

    // 执行
    let start = std::time::Instant::now();
    let r = exec::run_command(&req.command, req.cwd.as_deref(), timeout_ms);
    let duration_ms = start.elapsed().as_millis() as u64;

    let resp = Response {
        ok: r.error.is_none() && !r.timed_out,
        exit_code: r.exit_code,
        stdout: r.stdout.clone(),
        stderr: r.stderr.clone(),
        error: if r.timed_out {
            Some("执行超时".to_string())
        } else {
            r.error.clone()
        },
        truncated: r.truncated,
    };

    // GUI 实时信息流：执行完成事件
    let out_preview: String = r.stdout.chars().take(200).collect();
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "exec_done",
        "caller": caller,
        "pid": caller_pid,
        "cmd": req.command,
        "exit": r.exit_code,
        "timed_out": r.timed_out,
        "truncated": r.truncated,
        "duration_ms": duration_ms,
        "out_preview": out_preview,
    }));

    if let Ok(bytes) = serde_json::to_vec(&resp) {
        let _ = protocol::write_frame(&mut file, &bytes);
    }

    // 落盘执行日志
    common::log_exec(&caller, caller_pid, &req.command, r.exit_code);
}

/// 前台（测试）模式：持续接受连接，直到进程被终止
pub fn run_pipe_server_forever() {
    let tok = common::read_or_create_token();
    common::cache_token(tok);
    common::set_enabled(true); // 前台模式默认启用
    println!("[ai-root] 前台管道服务已启动: {}", common::pipe_name());
    loop {
        match accept_one() {
            Ok(file) => {
                thread::spawn(|| handle_client(file));
            }
            Err(e) => {
                eprintln!("[ai-root] accept 错误: {e}");
                thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

/// 将已连接的管道句柄移交工作线程处理（句柄所有权转移给 File，由线程负责读写）。
/// 用 catch_unwind 包裹，确保任何命令处理 panic 都被限制在单连接线程内，
/// 不会导致整个中继服务进程崩溃（SCM 记 7034）。
fn dispatch_connection(handle: winapi_raw::HANDLE) {
    let file = unsafe { File::from_raw_handle(handle as RawHandle) };
    thread::spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle_client(file)));
    });
}

/// 服务模式：持续接受连接，可被 stop_event 随时优雅中断退出。
///
/// 关键问题：ConnectNamedPipe 是阻塞调用，服务空闲时长时间卡在此处，
/// 若仅在循环顶部轮询 shutdown 信道，则停止信号永远无法被“看到”，
/// 导致 Stop-Service 超时、服务卡在 Stop Pending、exe 被文件锁占用。
///
/// 早期修复：监控线程等待 stop 后 `CloseHandle` 当前管道句柄，使阻塞的
/// ConnectNamedPipe 立即失败返回。但该做法存在竞态——主循环可能在 CloseHandle
/// 与 ConnectNamedPipe 返回之间的窗口把同一句柄 `dispatch` 给工作线程，造成
/// use-after-free（句柄被关闭后仍在读写）→ 段错误 → SCM 记「意外停止」(7034)。
///
/// 现采用 `CancelIoEx`：stop 时仅取消阻塞中的 I/O（不关闭句柄），ConnectNamedPipe
/// 返回 ERROR_OPERATION_ABORTED；主循环据此退出并自行关闭句柄。句柄所有权始终
/// 只属于主循环或单一工作线程，杜绝双重关闭 / use-after-free。
///
/// 注意：HANDLE（*mut c_void）不是 Send，跨线程传递统一以 usize 整数形式，
/// 使用时再 cast 回 HANDLE。Mutex 取锁统一用 `unwrap_or_else(|e| e.into_inner())`
/// 防止 poison 导致主循环 panic 拖垮进程。
pub fn run_pipe_server(stop_event: winapi_raw::HANDLE) {
    let tok = common::read_or_create_token();
    common::cache_token(tok);

    // 重新固化目录 ACL（含 /inheritance:e），确保 SYSTEM 创建的事件文件可被 GUI 用户读取
    let dir = common::data_dir();
    std::fs::create_dir_all(&dir).ok();
    common::set_dir_acl(&dir);

    // 服务启动即写一条事件——确保 events.jsonl 按当前（含继承）DACL 被创建
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "service_start",
        "pipe": common::pipe_name(),
    }));

    let stop_event_usize = stop_event as usize;

    // 当前正被 ConnectNamedPipe 阻塞的管道句柄（usize）；stop 时由监控线程 CancelIoEx 以中断阻塞
    let current_pipe: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));

    // 监控线程：等待 stop 信号，触发后 CancelIoEx 当前管道句柄（不关闭），中断阻塞的 ConnectNamedPipe
    let monitor_pipe = current_pipe.clone();
    let monitor = thread::spawn(move || {
        unsafe { winapi_raw::WaitForSingleObject(stop_event_usize as winapi_raw::HANDLE, winapi_raw::INFINITE); }
        if let Some(h) = monitor_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            unsafe { winapi_raw::CancelIoEx(h as winapi_raw::HANDLE, std::ptr::null_mut()); }
        }
    });

    loop {
        // 非阻塞轮询 stop 信号
        if unsafe { winapi_raw::WaitForSingleObject(stop_event, 0) } == winapi_raw::WAIT_OBJECT_0 {
            break;
        }

        let handle = match create_pipe() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[ai-root] create_pipe 错误: {e}");
                thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        *current_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle as usize);

        let connected = unsafe { winapi_raw::ConnectNamedPipe(handle, std::ptr::null_mut()) };
        // 连接已返回（成功或失败），清空 current（取走），避免监控线程误 Cancel 已移交的句柄
        let _ = current_pipe.lock().unwrap_or_else(|e| e.into_inner()).take();

        // stop 已触发：CancelIoEx 已中断本次 ConnectNamedPipe；放弃本连接并退出（不 dispatch）
        if unsafe { winapi_raw::WaitForSingleObject(stop_event, 0) } == winapi_raw::WAIT_OBJECT_0 {
            unsafe { winapi_raw::CloseHandle(handle); }
            break;
        }

        if connected != 0
            || unsafe { winapi_raw::GetLastError() } == winapi_raw::ERROR_PIPE_CONNECTED
        {
            // 客户端已连接：移交工作线程处理（句柄所有权转移，主循环不再触碰）
            dispatch_connection(handle);
        } else {
            // 真实连接错误：关闭后重试
            unsafe { winapi_raw::CloseHandle(handle); }
            thread::sleep(Duration::from_millis(300));
        }
    }

    let _ = monitor.join();
}
