//! 命名管道服务端：监听连接、鉴权、执行、回写结果
//!
//! 安全要点：
//! - 管道通过 SDDL 限制连接方为 SYSTEM / Administrators / Interactive
//!   `D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;IU)`
//! - 每个连接须携带正确共享令牌，否则拒绝
//! - 受「启用开关」约束，禁用时不执行

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::common;
use crate::exec;
use crate::protocol::{self, Request, Response};
use crate::winapi_raw;

/// 鉴权失败节流：双层模型。
///
/// 早期实现是单一全局计数器（30s 窗口内 5 次失败 → 全局锁 60s）。缺陷：
/// 攻击者可开 8 条并发连接（MAX_CONNS）同时试探，每次失败都累加到**同一**计数器，
/// 计数器很快打满并触发全局锁——但这反而让**合法客户端**被连坐锁定；
/// 且试探方在窗口重置后立刻恢复，限流被并发稀释。
///
/// 现改为：
/// - **全局熔断**（本结构）：统计全局失败总数，超过阈值则整体锁定一段时间，
///   用于兜底「无差别暴力尝试」；锁定期间拒绝一切请求。
/// - **单连接窗口**（由 handle_client 内的 per-conn 计数 + 读超时共同约束）：
///   单次连接内只允许极少数失败，超限即断开该连接，避免单连接无限次试探。
struct AuthThrottle {
    window_start: Instant,
    fail_count: u32,
    locked_until: Option<Instant>,
}

/// 全局熔断参数：窗口 30s 内累计失败达到 20 次（≈ MAX_CONNS×2.5）触发，锁 120s。
const AUTH_WINDOW_SECS: u64 = 30;
const AUTH_GLOBAL_FAIL_THRESHOLD: u32 = 20;
const AUTH_LOCK_SECS: u64 = 120;
/// 单个调用方 SID 在 60s 窗口内允许的最大鉴权失败次数，超出即定向拒绝 300s。
const MAX_AUTH_FAILS_PER_SID: u32 = 5;

/// 按调用方 SID 的定向失败计数表（SID → (窗口起点, 失败次数, 封禁截止)）。
/// 用于防「同一身份反复试探令牌」——这是本服务一连接一请求模型下唯一有意义的
/// 定向防护维度（连接级计数无意义，详见 handle_client 内注释）。
static SID_FAILS: Mutex<Option<HashMap<String, (Instant, u32, Option<Instant>)>>> =
    Mutex::new(None);

/// 记录一次「按 SID 的鉴权失败」，返回 (该 SID 当前失败次数, 是否已被定向拒绝)。
/// 窗口 60s 内失败达到 MAX_AUTH_FAILS_PER_SID 即封禁该 SID 300s（自动过期）。
/// 表规模上限 256，超出时清理过期项，防内存无限增长。
fn record_sid_fail(sid: &str) -> (u32, bool) {
    let now = Instant::now();
    let mut guard = SID_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);

    // 惰性清理：表过大时清掉所有已过期条目（超时未再犯者不留痕）
    if map.len() > 256 {
        map.retain(|_, (start, _, until)| {
            let active = now.duration_since(*start) <= Duration::from_secs(60)
                || until.map(|u| now < u).unwrap_or(false);
            active
        });
    }

    let entry = map
        .entry(sid.to_string())
        .or_insert_with(|| (now, 0, None));
    // 窗口过期且未封禁 → 重置窗口
    if now.duration_since(entry.0) > Duration::from_secs(60) && entry.2.is_none() {
        entry.0 = now;
        entry.1 = 0;
    }
    // 已被定向封禁 → 直接拒绝
    if let Some(until) = entry.2 {
        if now < until {
            return (entry.1, true);
        }
        // 封禁已过期：清除并重新开始计数
        entry.2 = None;
        entry.0 = now;
        entry.1 = 0;
    }
    entry.1 += 1;
    if entry.1 >= MAX_AUTH_FAILS_PER_SID {
        entry.2 = Some(now + Duration::from_secs(300));
        return (entry.1, true);
    }
    (entry.1, false)
}

impl AuthThrottle {
    fn new() -> Self {
        AuthThrottle {
            window_start: Instant::now(),
            fail_count: 0,
            locked_until: None,
        }
    }
    /// 记录一次失败；返回是否已触发全局熔断
    fn record_fail(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) > Duration::from_secs(AUTH_WINDOW_SECS) {
            self.window_start = now;
            self.fail_count = 0;
        }
        self.fail_count += 1;
        if self.fail_count >= AUTH_GLOBAL_FAIL_THRESHOLD {
            self.locked_until = Some(now + Duration::from_secs(AUTH_LOCK_SECS));
            return true;
        }
        false
    }
    /// 是否处于锁定态（锁定已过期则自动清理并返回 false）
    fn is_locked(&mut self) -> bool {
        match self.locked_until {
            Some(t) if Instant::now() < t => true,
            Some(_) => {
                // 锁定已过期：清理状态，避免长期停留在锁定分支
                self.locked_until = None;
                self.fail_count = 0;
                self.window_start = Instant::now();
                false
            }
            None => false,
        }
    }
}

/// 全局鉴权节流器 + 并发信号量计数
static AUTH_THROTTLE: Mutex<Option<AuthThrottle>> = Mutex::new(None);
/// 当前在执行的连接数（并发上限见 `common::MAX_CONNS`，防失控 AI 线程轰炸拖垮 SYSTEM 服务）
static ACTIVE_CONNS: AtomicU32 = AtomicU32::new(0);

/// 全局单调 run_id 生成器：UNIX毫秒-counter，保证进程内唯一且可粗略按时间排序。
/// 用于把同一次执行的 exec_start / exec_done 事件配对，并回传给客户端 JSON 响应，
/// 使 AI 消费方能在 tail 事件流里精确锁定「某条命令的开始与结束」。
static RUN_ID: AtomicU64 = AtomicU64::new(0);
fn next_run_id() -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = RUN_ID.fetch_add(1, Ordering::SeqCst);
    format!("{now_ms:x}-{n}")
}

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
            format!("SDDL 转换失败, last_err={}", unsafe {
                winapi_raw::GetLastError()
            }),
        ));
    }

    let sa = winapi_raw::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<winapi_raw::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };

    let name_w = common::to_wide(&common::pipe_name());
    // 优化：增加管道缓冲区大小以提高大输出命令的吞吐量
    const PIPE_BUFFER_SIZE: u32 = 64 * 1024; // 64KB 缓冲区
    let handle = unsafe {
        winapi_raw::CreateNamedPipeW(
            name_w.as_ptr(),
            winapi_raw::PIPE_ACCESS_DUPLEX,
            winapi_raw::PIPE_TYPE_BYTE | winapi_raw::PIPE_READMODE_BYTE | winapi_raw::PIPE_WAIT,
            winapi_raw::PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE, // 输出缓冲区
            PIPE_BUFFER_SIZE, // 输入缓冲区
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
            format!("CreateNamedPipeW 失败, last_err={}", unsafe {
                winapi_raw::GetLastError()
            }),
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
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "ConnectNamedPipe 失败",
            ));
        }
    }
    // 转移句柄所有权给 Rust File
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// SID 字符串 → 可读账户名 的进程内缓存（C2 优化核心）。
///
/// `LookupAccountSidW` 在域环境下可能触发域控（DC）网络查询，延迟高且不稳定。
/// 同一主体（用户 / 服务账户）的 SID→名称映射恒定不变，故解析一次后缓存复用。
/// 机器上会「成功鉴权」的主体数量极少（通常 < 20），缓存自然有界，无需淘汰策略。
/// 取锁统一用 `unwrap_or_else(|e| e.into_inner())`，防 poison 拖垮工作线程。
static SID_NAME_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// 【低成本身份获取，鉴权前调用】管道对端 PID + 主 SID 字符串。
///
/// 全程仅本地调用（GetNamedPipeClientProcessId → OpenProcess → OpenProcessToken →
/// GetTokenInformation → ConvertSidToStringSidW），**不含 `LookupAccountSidW`，无域控往返**。
/// 因此即便对暴力尝试 / 无效连接执行本函数，也不会引入域环境下的每连接延迟，
/// 同时保留精确的审计标识（SID 全局唯一，可离线反解为账户名）。
///
/// 返回 `(显示串, pid, Option<sid_string>)`：
/// - 正常：显示串 = SID 字符串（如 `S-1-5-21-...`），第三项 `Some(sid)` 供鉴权成功后升级为账户名；
/// - 逐级降级：取不到 SID 显示 `pid=N`，取不到 PID 显示 `unknown`，第三项 `None`。
fn client_identity_cheap(file: &File) -> (String, u32, Option<String>) {
    use core::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    let handle = file.as_raw_handle() as winapi_raw::HANDLE;
    let mut pid: u32 = 0;
    if unsafe { winapi_raw::GetNamedPipeClientProcessId(handle, &mut pid) } == 0 || pid == 0 {
        return ("unknown".to_string(), 0, None);
    }

    let proc =
        unsafe { winapi_raw::OpenProcess(winapi_raw::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if proc.is_null() {
        return (format!("pid={pid}"), pid, None);
    }
    let mut token: winapi_raw::HANDLE = std::ptr::null_mut();
    let tok_ok = unsafe { winapi_raw::OpenProcessToken(proc, winapi_raw::TOKEN_QUERY, &mut token) };
    unsafe {
        winapi_raw::CloseHandle(proc);
    }
    if tok_ok == 0 || token.is_null() {
        return (format!("pid={pid}"), pid, None);
    }

    // 先查 TokenUser 所需缓冲大小（期望 ERROR_INSUFFICIENT_BUFFER + need）
    let mut need: u32 = 0;
    unsafe {
        winapi_raw::GetTokenInformation(
            token,
            winapi_raw::TOKEN_USER,
            std::ptr::null_mut(),
            0,
            &mut need,
        );
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
    unsafe {
        winapi_raw::CloseHandle(token);
    }
    if got == 0 {
        return (format!("pid={pid}"), pid, None);
    }

    let tu = unsafe { &*(buf.as_ptr() as *const winapi_raw::TOKEN_USER) };
    let sid = tu.User.Sid;
    if sid.is_null() {
        return (format!("pid={pid}"), pid, None);
    }

    // SID → 字符串（纯本地格式化，无 DC 往返）：既作鉴权前的审计标识，也作名称解析缓存键
    let mut psz: *mut u16 = std::ptr::null_mut();
    if unsafe { winapi_raw::ConvertSidToStringSidW(sid, &mut psz) } == 0 || psz.is_null() {
        return (format!("pid={pid}"), pid, None);
    }
    let sid_str = unsafe { wide_ptr_to_string(psz) };
    unsafe {
        winapi_raw::LocalFree(psz as *mut c_void);
    }
    (sid_str.clone(), pid, Some(sid_str))
}

/// 【昂贵身份解析，仅鉴权成功后调用】SID 字符串 → 可读账户名（`domain\user`），带进程内缓存。
///
/// 命中缓存直接返回；未命中才执行 `ConvertStringSidToSidW` + `LookupAccountSidW`
/// （后者在域环境可能查询域控）。解析失败时回退返回原 SID 字符串（仍可追溯）。
fn resolve_sid_display(sid: &str) -> String {
    // 命中缓存：跳过潜在的域控查询
    {
        let cache = SID_NAME_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = cache.as_ref() {
            if let Some(name) = map.get(sid) {
                return name.clone();
            }
        }
    }
    // 未命中：真正解析（可能触发 DC 查询），失败回退为 SID 字符串
    let name = lookup_account_name(sid).unwrap_or_else(|| sid.to_string());
    {
        let mut cache = SID_NAME_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get_or_insert_with(HashMap::new)
            .insert(sid.to_string(), name.clone());
    }
    name
}

/// SID 字符串 → `domain\user`。内部先把字符串 SID 还原为 PSID，再调用 LookupAccountSidW。
/// 任一步失败返回 None（由调用方决定回退显示）。
fn lookup_account_name(sid_str: &str) -> Option<String> {
    use core::ffi::c_void;

    let sid_w = common::to_wide(sid_str);
    let mut psid: *mut c_void = std::ptr::null_mut();
    if unsafe { winapi_raw::ConvertStringSidToSidW(sid_w.as_ptr(), &mut psid) } == 0
        || psid.is_null()
    {
        return None;
    }

    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain_len = domain.len() as u32;
    let mut use_: u32 = 0;
    let lok = unsafe {
        winapi_raw::LookupAccountSidW(
            std::ptr::null(),
            psid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    unsafe {
        winapi_raw::LocalFree(psid);
    }
    if lok == 0 {
        return None;
    }
    let user = String::from_utf16_lossy(&name[..name_len as usize]);
    let dom = String::from_utf16_lossy(&domain[..domain_len as usize]);
    Some(format!("{dom}\\{user}"))
}

/// 读取以 NUL 结尾的宽字符串指针为 String（用于系统分配的字符串 SID）。
unsafe fn wide_ptr_to_string(p: *const u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
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
    if ACTIVE_CONNS.fetch_add(1, Ordering::SeqCst) >= common::MAX_CONNS {
        ACTIVE_CONNS.fetch_sub(1, Ordering::SeqCst);
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response("服务繁忙（并发超限），请稍后重试"))
                .unwrap(),
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

    // 优化：设置连接超时，防止连接无限期占用资源
    let start_time = Instant::now();
    let max_connection_duration = Duration::from_secs(300); // 5分钟最大连接时间

    let token = common::cached_token();
    // 审计（C2 优化）：鉴权前仅做「低成本」身份解析——PID + SID 字符串（纯本地，无域控往返）。
    // 昂贵的 SID→账户名解析（LookupAccountSidW，域环境可能查询域控）推迟到鉴权成功后再做，
    // 避免暴力尝试 / 无效连接放大每连接延迟；失败尝试仍以 SID 精确留痕（可离线反解）。
    let (mut caller, caller_pid, caller_sid) = client_identity_cheap(&file);

    // 鉴权节流：锁定期间拒绝一切请求
    {
        let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
        if th.get_or_insert_with(AuthThrottle::new).is_locked() {
            let _ = protocol::write_frame(
                &mut file,
                &serde_json::to_vec(&protocol::err_response(
                    "鉴权失败次数过多，已临时锁定（120s），请稍后再试",
                ))
                .unwrap(),
            );
            common::log_exec(&caller, caller_pid, "<locked>", -403);
            return;
        }
    }

    // 读请求（10s 空闲读超时：连续 10s 无新数据才断开；大请求按 payload 放宽绝对上限，
    // 既防客户端连接后不发数据导致的线程泄漏，又不误伤慢速管道上的大负载。见 protocol.rs）
    let frame = match protocol::read_frame_timeout(&mut file, common::PIPE_READ_TIMEOUT_MS) {
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

    // 验证请求数据
    if let Err(err_msg) = protocol::validate_request(&req) {
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response(&format!(
                "请求验证失败: {}",
                err_msg
            )))
            .unwrap(),
        );
        return;
    }

    // 令牌鉴权（恒定时间比较）
    if !ct_eq(&req.token, &token) {
        // 定向限流：按调用方 SID 记账（本服务是一连接一请求模型，无「连接内计数」概念；
        // 真正防定向暴力尝试的维度是**身份**，故按 SID 累计失败次数）。
        let sid_key = caller_sid.clone().unwrap_or_else(|| format!("pid:{caller_pid}"));
        let (sid_fails, sid_blocked) = record_sid_fail(&sid_key);
        // 全局熔断（无差别兜底）
        let locked = {
            let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
            th.get_or_insert_with(AuthThrottle::new).record_fail()
        };
        common::log_exec(
            &caller,
            caller_pid,
            &format!("<auth-fail cmd={}>", req.command),
            -401,
        );
        if sid_blocked {
            let _ = protocol::write_frame(
                &mut file,
                &serde_json::to_vec(&protocol::err_response(
                    "该调用方鉴权失败次数过多，已临时拒绝",
                ))
                .unwrap(),
            );
            return;
        }
        let msg = if locked {
            "令牌鉴权失败（全局熔断已触发，临时锁定 120s）".to_string()
        } else if sid_fails >= MAX_AUTH_FAILS_PER_SID {
            "令牌鉴权失败（该调用方失败次数已达上限）".to_string()
        } else {
            "令牌鉴权失败".to_string()
        };
        let _ = protocol::write_frame(
            &mut file,
            &serde_json::to_vec(&protocol::err_response(&msg)).unwrap(),
        );
        return;
    }
    // 鉴权成功后重置全局失败计数（防正常用户被历史失败误锁）。
    // 仅在未处于锁定态时清零，避免把正在进行的熔断窗口「重置」，使熔断形同虚设。
    {
        let mut th = AUTH_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = th.as_mut() {
            if t.locked_until.is_none() {
                t.fail_count = 0;
                t.window_start = Instant::now();
            }
        }
    }

    // C2 优化：鉴权成功后才把 SID 升级为可读账户名（LookupAccountSidW + 进程内缓存），
    // 供审计日志与 GUI 实时事件流展示。失败 / 锁定路径已在上方以 SID 留痕，不触发此昂贵解析。
    if let Some(sid) = caller_sid.as_deref() {
        caller = resolve_sid_display(sid);
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
    let timeout_ms = req
        .timeout_ms
        .unwrap_or(common::DEFAULT_COMMAND_TIMEOUT_MS)
        .min(common::MAX_COMMAND_TIMEOUT_MS);

    // GUI 实时信息流：执行开始事件
    let run_id = next_run_id();
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "exec_start",
        "run_id": run_id.clone(),
        "caller": caller,
        "pid": caller_pid,
        "cmd": req.command,
        "timeout_ms": timeout_ms,
    }));

    // 执行
    let start = std::time::Instant::now();
    let r = exec::run_command(&req.command, req.cwd.as_deref(), timeout_ms);
    let duration_ms = start.elapsed().as_millis() as u64;

    // 优化：检查连接时间，防止长时间占用
    if start_time.elapsed() > max_connection_duration {
        common::log_event(serde_json::json!({
            "type": "connection_timeout",
            "caller": caller,
            "pid": caller_pid,
            "timestamp": chrono::Utc::now().to_rfc3339()
        }));
        return;
    }

    // GUI 实时信息流：先截取输出预览（必须在下方所有权移动 r.stdout 之前完成，
    // 否则会触发「移动后借用」编译错误）
    let out_preview: String = r.stdout.chars().take(200).collect();

    // 优化：减少不必要的克隆，直接使用所有权
    let resp = Response {
        ok: r.error.is_none() && !r.timed_out,
        exit_code: r.exit_code,
        stdout: r.stdout, // 直接使用所有权，避免克隆
        stderr: r.stderr, // 直接使用所有权，避免克隆
        error: if r.timed_out {
            Some("执行超时".to_string())
        } else {
            r.error
        },
        truncated: r.truncated,
        run_id: Some(run_id.clone()),
        duration_ms: Some(duration_ms),
        caller: Some(caller.clone()),
        timed_out: r.timed_out,
    };

    // GUI 实时信息流：执行完成事件
    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "exec_done",
        "run_id": run_id.clone(),
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

    // 记录详细审计信息
    common::log_event(serde_json::json!({
        "type": "command_audit",
        "caller": caller,
        "pid": caller_pid,
        "command": req.command,
        "exit_code": r.exit_code,
        "duration_ms": duration_ms,
        "truncated": r.truncated,
        "timed_out": r.timed_out,
        "cwd": req.cwd,
        "timestamp": chrono::Utc::now().to_rfc3339()
    }));
}

/// 前台（测试）模式：持续接受连接，直到进程被终止
pub fn run_pipe_server_forever() {
    let tok = common::read_or_create_token();
    common::cache_token(tok);
    common::set_enabled(true); // 前台模式默认启用
    println!("[AI-Root] 前台管道服务已启动: {}", common::pipe_name());
    loop {
        match accept_one() {
            Ok(file) => {
                thread::spawn(|| handle_client(file));
            }
            Err(e) => {
                eprintln!("[AI-Root] accept 错误: {e}");
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
        unsafe {
            winapi_raw::WaitForSingleObject(
                stop_event_usize as winapi_raw::HANDLE,
                winapi_raw::INFINITE,
            );
        }
        if let Some(h) = monitor_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            unsafe {
                winapi_raw::CancelIoEx(h as winapi_raw::HANDLE, std::ptr::null_mut());
            }
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
                eprintln!("[AI-Root] create_pipe 错误: {e}");
                thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        *current_pipe.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle as usize);

        let connected = unsafe { winapi_raw::ConnectNamedPipe(handle, std::ptr::null_mut()) };
        // 连接已返回（成功或失败），清空 current（取走），避免监控线程误 Cancel 已移交的句柄
        let _ = current_pipe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // stop 已触发：CancelIoEx 已中断本次 ConnectNamedPipe；放弃本连接并退出（不 dispatch）
        if unsafe { winapi_raw::WaitForSingleObject(stop_event, 0) } == winapi_raw::WAIT_OBJECT_0 {
            unsafe {
                winapi_raw::CloseHandle(handle);
            }
            break;
        }

        if connected != 0
            || unsafe { winapi_raw::GetLastError() } == winapi_raw::ERROR_PIPE_CONNECTED
        {
            // 客户端已连接：移交工作线程处理（句柄所有权转移，主循环不再触碰）
            dispatch_connection(handle);
        } else {
            // 真实连接错误：关闭后重试
            unsafe {
                winapi_raw::CloseHandle(handle);
            }
            thread::sleep(Duration::from_millis(300));
        }
    }

    let _ = monitor.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================= ct_eq：恒定时间令牌比较 =================

    #[test]
    fn ct_eq_matches_identical() {
        assert!(ct_eq("abc", "abc"));
        assert!(ct_eq("", ""), "两个空串应视为相等");
    }

    #[test]
    fn ct_eq_rejects_different_length() {
        // 长度不同直接判不等（不进入逐字节循环，故长度本身会泄露 —— 见下方说明）
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "a"));
    }

    #[test]
    fn ct_eq_detects_single_byte_difference() {
        let a = "a".repeat(64);
        let mut b = a.clone();
        // 逐位置翻转：任意单字节差异都必须被发现
        for i in 0..a.len() {
            let mut bytes = b.as_bytes().to_vec();
            bytes[i] = if bytes[i] == b'a' { b'b' } else { b'a' };
            b = String::from_utf8(bytes).unwrap();
            assert!(!ct_eq(&a, &b), "第 {i} 字节差异未被检出");
            b = a.clone();
        }
    }

    #[test]
    fn ct_eq_detects_difference_at_first_and_last_byte() {
        // 首字节 / 末字节差异是最容易被「提前 return」实现漏掉的位置
        let base = format!("{}Z", "a".repeat(63));
        let mut first = base.clone().into_bytes();
        first[0] = b'b';
        let first = String::from_utf8(first).unwrap();
        let mut last = base.clone().into_bytes();
        let n = last.len() - 1;
        last[n] = b'Y';
        let last = String::from_utf8(last).unwrap();

        assert!(!ct_eq(&base, &first));
        assert!(!ct_eq(&base, &last));
    }

    #[test]
    fn ct_eq_hex_tokens_like_real_use() {
        // 贴近真实用法：64 位 hex 令牌
        let t1 = "0123456789abcdef".repeat(4);
        let t2 = "0123456789abcdee".repeat(4); // 倒数第 2 字符不同（每轮相同）
        assert_eq!(t1.len(), 64);
        assert!(ct_eq(&t1, &t1));
        assert!(!ct_eq(&t1, &t2));
    }

    // ================= AuthThrottle：全局熔断 =================
    // AuthThrottle 是局部 struct（AUTH_THROTTLE 只是承载它的 Mutex），
    // 故可直接构造实例测试，不受全局状态污染影响。

    #[test]
    fn auth_throttle_starts_unlocked() {
        let mut t = AuthThrottle::new();
        assert!(!t.is_locked());
    }

    #[test]
    fn auth_throttle_locks_at_threshold() {
        // 注意：这里刻意**硬编码** 20，而非引用 AUTH_GLOBAL_FAIL_THRESHOLD。
        // 若测试引用常量，则「改常量」这个变异无法被检出（测试会跟着一起变）。
        // 这些数值是对外承诺的行为契约（README 有记载），变更应是有意识的决策。
        const EXPECTED_THRESHOLD: u32 = 20;
        assert_eq!(
            AUTH_GLOBAL_FAIL_THRESHOLD, EXPECTED_THRESHOLD,
            "全局熔断阈值变更需同步更新 README 与客户端预期"
        );

        let mut t = AuthThrottle::new();
        // 前 N-1 次不触发
        for i in 1..EXPECTED_THRESHOLD {
            assert!(!t.record_fail(), "第 {i} 次失败不应触发熔断（阈值 {EXPECTED_THRESHOLD}）");
        }
        // 第 N 次触发
        assert!(t.record_fail(), "第 {EXPECTED_THRESHOLD} 次失败应触发熔断");
        assert!(t.is_locked(), "触发后应立即处于锁定态");
    }

    #[test]
    fn auth_window_and_lock_durations_match_contract() {
        // 同样硬编码，防止常量被默默改动（README 记载为 30s 窗口 / 120s 锁定）
        assert_eq!(AUTH_WINDOW_SECS, 30, "统计窗口变更需同步文档");
        assert_eq!(AUTH_LOCK_SECS, 120, "锁定时长变更需同步文档");
    }

    #[test]
    fn auth_throttle_counts_failures() {
        let mut t = AuthThrottle::new();
        for _ in 0..5 {
            t.record_fail();
        }
        assert_eq!(t.fail_count, 5);
    }

    #[test]
    fn auth_throttle_expired_lock_auto_clears() {
        // 直接把 locked_until 设为过去，验证 is_locked 会自动清理并复位计数
        let mut t = AuthThrottle::new();
        t.fail_count = AUTH_GLOBAL_FAIL_THRESHOLD;
        t.locked_until = Some(Instant::now() - Duration::from_secs(1));

        assert!(!t.is_locked(), "已过期的锁定应返回未锁定");
        assert!(t.locked_until.is_none(), "过期锁定应被清理");
        assert_eq!(t.fail_count, 0, "过期后失败计数应复位");
    }

    #[test]
    fn auth_throttle_window_resets_fail_count() {
        // 窗口过期后重新计数（把 window_start 推到窗口之外）
        let mut t = AuthThrottle::new();
        t.fail_count = 5;
        t.window_start = Instant::now() - Duration::from_secs(AUTH_WINDOW_SECS + 1);

        assert!(!t.record_fail(), "窗口过期后计数应重置，不应立刻熔断");
        assert_eq!(t.fail_count, 1, "重置后本次失败应记为第 1 次");
    }

    // ================= record_sid_fail：按 SID 定向封禁 =================
    // 该函数使用全局 static SID_FAILS，故每个用例必须用**唯一 SID 键**隔离，
    // 避免测试并行执行时互相污染（cargo 默认多线程跑测试）。

    /// 生成本次调用唯一的 SID 键，保证测试间隔离。
    fn uniq_sid(tag: &str) -> String {
        format!("S-1-5-21-TEST-{tag}-{}", std::process::id())
    }

    #[test]
    fn sid_fail_blocks_at_threshold() {
        // 硬编码而非引用常量：见 auth_throttle_locks_at_threshold 的说明
        const EXPECTED: u32 = 5;
        assert_eq!(
            MAX_AUTH_FAILS_PER_SID, EXPECTED,
            "单 SID 封禁阈值变更需同步文档与客户端预期"
        );

        let sid = uniq_sid("blocks");
        // 前 N-1 次返回「未封禁」，第 N 次起返回「已封禁」
        for i in 1..EXPECTED {
            let (count, blocked) = record_sid_fail(&sid);
            assert_eq!(count, i, "第 {i} 次失败计数不符");
            assert!(!blocked, "第 {i} 次失败不应封禁（阈值 {EXPECTED}）");
        }
        let (count, blocked) = record_sid_fail(&sid);
        assert_eq!(count, EXPECTED);
        assert!(blocked, "达到阈值应封禁该 SID");
    }

    #[test]
    fn sid_fail_stays_blocked_on_subsequent_calls() {
        const EXPECTED: u32 = 5;
        let sid = uniq_sid("stay");
        for _ in 0..EXPECTED {
            record_sid_fail(&sid);
        }
        // 封禁期内再次尝试：仍应拒绝，且不增加计数
        let (count, blocked) = record_sid_fail(&sid);
        assert!(blocked, "封禁期内应持续拒绝");
        assert_eq!(
            count, EXPECTED,
            "封禁期内不应继续累加计数（否则计数无上限增长）"
        );
    }

    #[test]
    fn sid_fail_isolates_different_sids() {
        // 一个 SID 被封禁不应影响另一个 SID
        let blocked_sid = uniq_sid("iso-a");
        let other_sid = uniq_sid("iso-b");

        for _ in 0..5 {
            record_sid_fail(&blocked_sid);
        }
        let (_, bad_blocked) = record_sid_fail(&blocked_sid);
        assert!(bad_blocked, "前置条件：a 已被封禁");

        let (count, blocked) = record_sid_fail(&other_sid);
        assert_eq!(count, 1, "b 应是独立计数");
        assert!(!blocked, "a 被封禁不应牵连 b");
    }

    #[test]
    fn sid_fail_window_expiry_resets() {
        let sid = uniq_sid("window");
        record_sid_fail(&sid);
        record_sid_fail(&sid);
        assert!(SID_FAILS.lock().unwrap().as_ref().unwrap().contains_key(&sid));

        // 手工把窗口起点推前（模拟超过 60s 窗口），且未封禁
        {
            let mut guard = SID_FAILS.lock().unwrap();
            let map = guard.as_mut().unwrap();
            if let Some(e) = map.get_mut(&sid) {
                e.0 = Instant::now() - Duration::from_secs(61);
            }
        }
        let (count, blocked) = record_sid_fail(&sid);
        assert_eq!(count, 1, "窗口过期后应重新从 1 计数");
        assert!(!blocked);
    }

    // ================= 并发信号量常量自洽性 =================

    #[test]
    fn concurrency_and_throttle_constants_are_sane() {
        assert!(common::MAX_CONNS > 0, "并发上限必须为正");
        assert!(
            AUTH_GLOBAL_FAIL_THRESHOLD > common::MAX_CONNS,
            "全局熔断阈值应大于并发上限，否则单个客户端即可轻易触发全局锁定"
        );
        assert!(
            AUTH_LOCK_SECS >= AUTH_WINDOW_SECS,
            "锁定时长不应短于统计窗口，否则窗口内可反复触发锁定"
        );
        assert!(MAX_AUTH_FAILS_PER_SID > 0);
    }
}

