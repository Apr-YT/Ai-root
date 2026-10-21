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
use crate::config;
use crate::consent;
use crate::exec;
use crate::policy;
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
            &format!("<auth-fail {}>", summarize_request(&req)),
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

    let run_id = next_run_id();
    let data_dir = common::data_dir();

    // ── 授权判定（任务书 §6.1 的判定顺序）──
    // 1) 身份与结构   —— 前面已完成（管道对端身份 + validate_request）
    // 2) 令牌         —— 前面已完成
    // 3) 启用开关     —— 前面已完成
    // 4) 策略判定     —— 此处；内置保护 → deny 规则 → allow 规则 → confirm → 默认
    // 5) 确认流程     —— 需要时走文件型确认
    // 6) 执行 + 审计
    // 带指纹缓存：配置未变时复用上次结果；任一文件变动立即重新加载（保留热生效）
    let cfg = config::load_cached(&config::Paths::resolve());
    let policy = &cfg.policy;
    let protected = protected_paths();
    let action = build_action(&req);
    let verdict = policy.evaluate(&action, &protected);

    let op_str = action.op_kind().as_str().to_string();
    // 审计里的参数必须脱敏（命令可能带 --password=... / API Key）
    let params = common::redact(&action.describe());

    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "policy_decision",
        "run_id": run_id,
        "caller": caller,
        "pid": caller_pid,
        "op": op_str,
        "params": params,
        "decision": verdict.decision.as_str(),
        "rule_id": verdict.rule_id,
        "reason": verdict.reason,
        "policy_state": format!("{:?}", policy.state),
    }));

    let mut confirmed = false;
    match verdict.decision {
        policy::Decision::Deny => {
            common::log_exec(&caller, caller_pid, &format!("<denied {op_str}>"), -403);
            send_decision_err(
                &mut file,
                &format!(
                    "策略拒绝（规则 {}）：{}\n如需放行，请管理员调整 {}\\policy.json",
                    verdict.rule_id, verdict.reason, data_dir.display()
                ),
                "deny",
                &verdict.rule_id,
                None,
            );
            return;
        }
        policy::Decision::Confirm => {
            // 已批准？（参数哈希匹配才认，且一次性消费）
            if let Some(rid) = consent::take_approval(&data_dir, &action) {
                confirmed = true;
                common::log_event(serde_json::json!({
                    "ts": common::now_str(),
                    "type": "confirm_consumed",
                    "run_id": run_id,
                    "rid": rid,
                    "caller": caller,
                    "op": op_str,
                }));
            } else {
                let ttl = policy.confirm_timeout_secs();
                match consent::create(
                    &data_dir,
                    &caller,
                    caller_pid,
                    &action,
                    req.target.as_deref(),
                    ttl,
                ) {
                    Ok(pending) => {
                        let msg = format!(
                            "需要人工确认：\n  请求 ID : {}\n  操作    : {op_str}\n  参数    : {params}\n  \
                             风险    : {}\n  有效期  : {ttl}s（过期作废）\n\n\
                             管理员批准：AI-Root approve {}\n管理员拒绝：AI-Root deny {}\n\
                             批准后重新执行同一条命令即可（参数必须完全一致）。",
                            pending.id, pending.risk, pending.id, pending.id
                        );
                        common::log_exec(
                            &caller,
                            caller_pid,
                            &format!("<confirm {op_str}>"),
                            -409,
                        );
                        send_decision_err(
                            &mut file,
                            &msg,
                            "confirm",
                            &verdict.rule_id,
                            Some(&pending.id),
                        );
                        return;
                    }
                    Err(e) => {
                        // 无法建立确认通道 → 拒绝执行（任务书 §6.1 第 6 条）
                        send_decision_err(
                            &mut file,
                            &format!("需要确认但确认通道不可用：{e}"),
                            "deny",
                            "confirm:unavailable",
                            None,
                        );
                        return;
                    }
                }
            }
        }
        policy::Decision::Allow => {}
    }

    // ── 执行 ──
    let start = std::time::Instant::now();
    let r = match &action {
        policy::Action::Argv { program, args, cwd } => {
            exec::run_argv(program, args, *cwd, timeout_ms)
        }
        policy::Action::Shell { command, cwd } => {
            exec::run_command(command, *cwd, timeout_ms)
        }
        policy::Action::Op { op, target } => run_structured_op(*op, *target, timeout_ms),
    };
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

    // GUI 实时信息流：先截取输出预览（必须在下方所有权移动 r.stdout 之前完成）
    let out_preview: String = r.stdout.chars().take(200).collect();

    let resp = Response {
        ok: r.error.is_none() && !r.timed_out,
        exit_code: r.exit_code,
        stdout: r.stdout,
        stderr: r.stderr,
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
        protocol_version: Some(protocol::PROTOCOL_VERSION),
        // 经人工批准的请求：对调用方报告 allow（并在 reason 里保留原始判定），
        // 审计日志里仍记录 decision=confirm + confirmed=true，两者不矛盾：
        // 「协议层看到的结果」与「为什么允许」分开表达。
        decision: Some(if confirmed {
            "allow".to_string()
        } else {
            verdict.decision.as_str().to_string()
        }),
        rule_id: Some(verdict.rule_id.clone()),
        reason: Some(if confirmed {
            format!(
                "经人工批准执行（原判定 {}：{}）",
                verdict.decision.as_str(),
                verdict.rule_id
            )
        } else {
            verdict.reason.clone()
        }),
        confirmation_id: None,
    };

    common::log_event(serde_json::json!({
        "ts": common::now_str(),
        "type": "exec_done",
        "run_id": run_id.clone(),
        "caller": caller,
        "pid": caller_pid,
        "op": op_str,
        "params": params,
        "decision": verdict.decision.as_str(),
        "rule_id": verdict.rule_id,
        "confirmed": confirmed,
        "exit": r.exit_code,
        "timed_out": r.timed_out,
        "truncated": r.truncated,
        "duration_ms": duration_ms,
        "out_preview": out_preview,
    }));

    if let Ok(bytes) = serde_json::to_vec(&resp) {
        let _ = protocol::write_frame(&mut file, &bytes);
    }

    // 落盘执行日志（含决策与是否经确认）
    common::log_exec(
        &caller,
        caller_pid,
        &format!("<{} {} confirmed={confirmed}>", verdict.rule_id, op_str),
        r.exit_code,
    );

    // 详细审计（不含令牌/密码：params 已脱敏）
    common::log_event(serde_json::json!({
        "type": "command_audit",
        "run_id": run_id,
        "caller": caller,
        "pid": caller_pid,
        "op": op_str,
        "params": params,
        "decision": verdict.decision.as_str(),
        "rule_id": verdict.rule_id,
        "confirmed": confirmed,
        "exit_code": r.exit_code,
        "duration_ms": duration_ms,
        "truncated": r.truncated,
        "timed_out": r.timed_out,
        "timestamp": chrono::Utc::now().to_rfc3339()
    }));
}

/// 策略判定的受保护路径：AI-Root 自身的数据目录（含 exe / token / policy / sid / 日志）。
///
/// 任何参数指向这里的请求都会被内置保护直接拒绝 —— 这一条**不可由策略文件覆盖**，
/// 否则「改策略 → 放开自改 → 改 exe」就成了环。
fn protected_paths() -> Vec<std::path::PathBuf> {
    vec![common::data_dir()]
}

/// 请求摘要（用于日志，不展开完整命令）
fn summarize_request(req: &protocol::Request) -> String {
    if let Some(argv) = &req.argv {
        format!(
            "op=run_argv argv[{}]={}",
            argv.len(),
            argv.first().cloned().unwrap_or_default()
        )
    } else if let Some(cmd) = &req.command {
        let preview: String = cmd.chars().take(60).collect();
        format!("op={} cmd={preview}", req.op.as_deref().unwrap_or("shell"))
    } else {
        format!("op={:?}", req.op)
    }
}

/// op 字符串 → 枚举（`validate_request` 已保证取值合法）
fn op_from_str(s: &str) -> policy::OpKind {
    match s {
        "query_system_info" => policy::OpKind::QuerySystemInfo,
        "query_service" => policy::OpKind::QueryService,
        "restart_service" => policy::OpKind::RestartService,
        "read_state_file" => policy::OpKind::ReadStateFile,
        "install_package" => policy::OpKind::InstallPackage,
        "update_self" => policy::OpKind::UpdateSelf,
        "run_argv" => policy::OpKind::RunArgv,
        _ => policy::OpKind::RunShell,
    }
}

/// 由请求构造待判定动作（借用 req 的字段，不复制大字符串）
fn build_action(req: &protocol::Request) -> policy::Action<'_> {
    if let Some(argv) = req.argv.as_deref() {
        let program = argv.first().map(|s| s.as_str()).unwrap_or("");
        return policy::Action::Argv {
            program,
            args: &argv[1..],
            cwd: req.cwd.as_deref(),
        };
    }
    match req.op.as_deref() {
        Some("run_shell") | None => policy::Action::Shell {
            command: req.command.as_deref().unwrap_or(""),
            cwd: req.cwd.as_deref(),
        },
        Some(other) => policy::Action::Op {
            op: op_from_str(other),
            target: req.target.as_deref(),
        },
    }
}

/// 发送带决策信息的错误响应
fn send_decision_err(
    file: &mut File,
    msg: &str,
    decision: &str,
    rule_id: &str,
    confirmation_id: Option<&str>,
) {
    let mut resp = protocol::err_response(msg);
    resp.decision = Some(decision.to_string());
    resp.rule_id = Some(rule_id.to_string());
    resp.reason = Some(msg.to_string());
    resp.confirmation_id = confirmation_id.map(|s| s.to_string());
    if let Ok(bytes) = serde_json::to_vec(&resp) {
        let _ = protocol::write_frame(file, &bytes);
    }
}

fn op_ok(text: String) -> exec::ExecResult {
    exec::ExecResult {
        exit_code: 0,
        stdout: text,
        stderr: String::new(),
        timed_out: false,
        error: None,
        truncated: false,
    }
}

fn op_err(msg: String) -> exec::ExecResult {
    exec::ExecResult {
        exit_code: -1,
        stdout: String::new(),
        stderr: String::new(),
        timed_out: false,
        error: Some(msg),
        truncated: false,
    }
}

/// 执行结构化操作。
///
/// 这些操作**不走 shell**，参数与目标都由服务端按固定语义处理 ——
/// 这是「最小权限 + 结构化」的落点：能表达清楚意图的，就不要交给字符串解释器。
fn run_structured_op(
    op: policy::OpKind,
    target: Option<&str>,
    timeout_ms: u64,
) -> exec::ExecResult {
    use policy::OpKind;

    match op {
        // 只读系统信息：直接用标准库，不起子进程（更快、更省资源、无注入面）
        OpKind::QuerySystemInfo => {
            let info = serde_json::json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "family": std::env::consts::FAMILY,
                "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
                "hostname": std::env::var("COMPUTERNAME").unwrap_or_default(),
                "user": std::env::var("USERNAME").unwrap_or_default(),
                "pid": std::process::id(),
                "exe": std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            });
            op_ok(serde_json::to_string_pretty(&info).unwrap_or_default())
        }

        // 查询服务状态（只读）：target 为服务名
        OpKind::QueryService => {
            let Some(name) = target else {
                return op_err("query_service 需要 target（服务名）".into());
            };
            match query_service_state(name) {
                Ok(s) => op_ok(serde_json::json!({"service": name, "state": s}).to_string()),
                Err(e) => op_err(format!("查询服务 {name} 失败: {e}")),
            }
        }

        // 重启指定服务：名单由策略 approved_services 约束（策略层已判过，此处再兜一层）
        OpKind::RestartService => {
            let Some(name) = target else {
                return op_err("restart_service 需要 target（服务名）".into());
            };
            restart_service(name)
        }

        // 读取状态文件：只允许数据目录下的白名单，绝不返回 token.txt
        OpKind::ReadStateFile => {
            const ALLOWED: &[&str] = &["policy.json"];
            let Some(name) = target else {
                return op_err("read_state_file 需要 target（文件名）".into());
            };
            if !ALLOWED.contains(&name) {
                return op_err(format!(
                    "不允许读取 {name}（白名单只有 {}）",
                    ALLOWED.join(", ")
                ));
            }
            let p = common::data_dir().join(name);
            match std::fs::read_to_string(&p) {
                Ok(s) if s.len() <= 64 * 1024 => op_ok(s),
                Ok(_) => op_err("文件超过 64KB，拒绝返回".into()),
                Err(e) => op_err(format!("读取失败: {e}")),
            }
        }

        // 安装软件：由 run_argv + winget/choco/scoop 白名单承担，此 op 仅作提示
        OpKind::InstallPackage => op_err(
            "install_package 请改用结构化 argv（例：AI-Root exec winget install <包名>），\
             以便策略按程序与参数精确授权"
                .into(),
        ),

        // 更新自身：必须走 CLI 的签名门控流程，管道里永远不做
        OpKind::UpdateSelf => op_err(
            "update_self 不能经管道执行；请在本地运行 AI-Root update <已签名exe>（需签名与信任根）"
                .into(),
        ),

        OpKind::RunArgv | OpKind::RunShell => {
            // 由 build_action 映射到 Argv/Shell 分支，不会走到这里
            let _ = timeout_ms;
            op_err("内部错误：run_* 不应以结构化 op 方式执行".into())
        }
    }
}

/// 查询服务状态（文本）
fn query_service_state(name: &str) -> Result<String, String> {
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| e.to_string())?;
    let svc = m
        .open_service(name, ServiceAccess::QUERY_STATUS)
        .map_err(|e| e.to_string())?;
    let st = svc.query_status().map_err(|e| e.to_string())?;
    let s = match st.current_state {
        ServiceState::Running => "running",
        ServiceState::Stopped => "stopped",
        ServiceState::StartPending => "start_pending",
        ServiceState::StopPending => "stop_pending",
        ServiceState::Paused => "paused",
        ServiceState::PausePending => "pause_pending",
        ServiceState::ContinuePending => "continue_pending",
    };
    Ok(s.to_string())
}

/// 重启服务：停 → 等 Stopped（≤15s）→ 起 → 等 Running（≤15s）
fn restart_service(name: &str) -> exec::ExecResult {
    use std::ffi::OsString;
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let m = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(m) => m,
        Err(e) => return op_err(format!("打开 SCM 失败: {e}")),
    };
    let svc = match m.open_service(
        name,
        ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(s) => s,
        Err(e) => return op_err(format!("打开服务 {name} 失败: {e}")),
    };

    if let Ok(st) = svc.query_status() {
        if st.current_state != ServiceState::Stopped {
            if let Err(e) = svc.stop() {
                return op_err(format!("停止 {name} 失败: {e}"));
            }
            let mut ok = false;
            for _ in 0..30 {
                if let Ok(s) = svc.query_status() {
                    if s.current_state == ServiceState::Stopped {
                        ok = true;
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            if !ok {
                return op_err(format!("{name} 15s 内未停止"));
            }
        }
    }

    if let Err(e) = svc.start::<OsString>(&[]) {
        return op_err(format!("启动 {name} 失败: {e}"));
    }
    for _ in 0..30 {
        if let Ok(s) = svc.query_status() {
            if s.current_state == ServiceState::Running {
                return op_ok(format!("服务 {name} 已重启（running）"));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    op_err(format!("{name} 启动后 15s 内未进入 running"))
}

/// 前台（测试）模式：持续接受连接，直到进程被终止
pub fn run_pipe_server_forever() {
    let tok = common::read_or_create_token();
    common::cache_token(tok);
    if let Err(e) = common::set_enabled(true) {
        // 前台模式：启用标志/ACL 失败也必须显式告警（原先静默吞错）
        eprintln!("[AI-Root] 警告：启用标志或目录 ACL 设置失败: {e}");
    }
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
pub fn run_pipe_server(stop_event: winapi_raw::HANDLE) -> Result<(), String> {
    let tok = common::read_or_create_token();
    common::cache_token(tok);

    // 重新固化目录 ACL（/inheritance:r 断开继承 + 显式授予 SYSTEM:F / Admins:F / 安装用户:R），
    // 确保 SYSTEM 创建的事件文件可被安装用户读取。
    // ⚠ 服务以 LocalSystem 运行，此处 set_dir_acl 内部的 granted_user_sid() 会读 sid.txt
    //   取到**安装用户** SID，而不是当前进程的 S-1-5-18 —— 否则会把 SYSTEM 自降为只读
    //   （2026-10-04 线上故障，见 common.rs::select_grant_sid 文档）。
    // ⚠ fail-closed：ACL 是令牌保密性的唯一边界。设置或校验失败时**不进入主循环**，
    // 让服务启动失败（SCM 记 7034/1053），而不是带着可疑权限继续监听管道。
    let dir = common::data_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let msg = format!("创建数据目录失败 {}: {e}", dir.display());
        common::log_event(serde_json::json!({
            "ts": common::now_str(), "type": "acl_setup_failed", "error": msg,
        }));
        return Err(msg);
    }
    if let Err(e) = common::set_dir_acl(&dir) {
        let msg = format!("设置数据目录 ACL 失败（拒绝启动）：{e}");
        common::log_event(serde_json::json!({
            "ts": common::now_str(), "type": "acl_setup_failed", "error": msg,
        }));
        return Err(msg);
    }

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
    Ok(())
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)] // README 记载的数值契约，变更应是有意识决策
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

    // ================= handle_client 端到端流程 =================
    //
    // 这一组测试真实驱动 `handle_client`（鉴权 → 执行 → 回写全链路），
    // 而非只测它的某个纯函数片段。此前该流程**只有手工验证、零自动化覆盖**，
    // 是最容易在重构中静默损坏的部分。
    //
    // ⚠ 测试环境隔离（三个必须处理的全局状态）：
    //   1. `common::data_dir()` → 用 `AIROOT_DATA_DIR` 指向临时目录，
    //      `cached_token()` / `is_enabled()` 才会读到测试令牌与 flag；
    //   2. `AUTH_THROTTLE` / `SID_FAILS` / `ACTIVE_CONNS` 是**进程级 static**，
    //      cargo 默认多线程跑测试 → 必须用一把全局测试锁把本组用例**串行化**，
    //      否则一个用例的失败计数会污染另一个用例的鉴权结果；
    //   3. `LOGGER`（OnceLock）首次写日志时按当时的 `data_dir()` 绑定句柄，
    //      一旦绑定就不再改 → 本组共享同一个 `AIROOT_DATA_DIR`，日志落同一处。
    static E2E_LOCK: Mutex<()> = Mutex::new(());

    /// 端到端测试专用数据目录（进程内唯一；本组共享，保证 LOGGER 绑定一致）。
    fn e2e_data_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("airoot-e2e-{}", std::process::id()))
    }

    /// 建立一条「服务端 File ←→ 客户端 File」的真实命名管道对，两端在**同一进程内**连接。
    ///
    /// 管道名固定为本测试专用名：`handle_client` 要用 `GetNamedPipeClientProcessId` 取对端
    /// PID，本进程即是"客户端"，故能取到真实 PID（非 0），从而走到真实身份解析分支。
    fn e2e_pipe_pair(tag: &str) -> (File, File) {
        use std::sync::mpsc;
        let name = format!(r"\\.\pipe\airoot-e2e-{}-{}", std::process::id(), tag);
        let wide = common::to_wide(&name);
        let (tx, rx) = mpsc::channel::<bool>();

        let name_for_thread = wide.clone();
        let srv_thread = std::thread::spawn(move || -> Option<File> {
            let h = unsafe {
                winapi_raw::CreateNamedPipeW(
                    name_for_thread.as_ptr(),
                    winapi_raw::PIPE_ACCESS_DUPLEX,
                    winapi_raw::PIPE_TYPE_BYTE | winapi_raw::PIPE_READMODE_BYTE,
                    winapi_raw::PIPE_UNLIMITED_INSTANCES,
                    65536,
                    65536,
                    0,
                    std::ptr::null(),
                )
            };
            if h == winapi_raw::INVALID_HANDLE_VALUE {
                tx.send(false).unwrap();
                return None;
            }
            tx.send(true).unwrap();
            let ok = unsafe { winapi_raw::ConnectNamedPipe(h, std::ptr::null_mut()) };
            let err = unsafe { winapi_raw::GetLastError() };
            if ok == 0 && err != winapi_raw::ERROR_PIPE_CONNECTED {
                unsafe { winapi_raw::CloseHandle(h) };
                return None;
            }
            Some(unsafe { File::from_raw_handle(h as RawHandle) })
        });

        assert!(rx.recv().expect("管道创建失败"), "CreateNamedPipeW 失败");
        let cli_h = unsafe {
            winapi_raw::CreateFileW(
                wide.as_ptr(),
                winapi_raw::GENERIC_READ | winapi_raw::GENERIC_WRITE,
                winapi_raw::FILE_SHARE_READ | winapi_raw::FILE_SHARE_WRITE,
                std::ptr::null(),
                winapi_raw::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(
            cli_h != winapi_raw::INVALID_HANDLE_VALUE,
            "客户端连接失败 (last_err={})",
            unsafe { winapi_raw::GetLastError() }
        );
        let srv = srv_thread.join().unwrap().expect("服务端连接失败");
        let cli = unsafe { File::from_raw_handle(cli_h as RawHandle) };
        (srv, cli)
    }

    /// 准备端到端环境：设置数据目录、写测试令牌/启用标志/**TOML 测试策略**。
    /// 返回令牌。调用方**必须**持有 `E2E_LOCK`。
    ///
    /// 布局与生产一致（v1.2 分层）：`secrets/token.txt`、`state/enabled.flag`、`policy.toml`。
    fn e2e_setup() -> String {
        let dir = e2e_data_dir();
        std::fs::create_dir_all(dir.join(config::SECRETS_DIR)).expect("创建 secrets 失败");
        std::fs::create_dir_all(dir.join(config::STATE_DIR)).expect("创建 state 失败");
        std::env::set_var("AIROOT_DATA_DIR", &dir);
        // 用户配置也隔离，避免读到开发机上的真实 %LOCALAPPDATA%\AI-Root\user.toml
        let user_dir = std::env::temp_dir().join(format!("airoot-e2e-user-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&user_dir);
        std::env::set_var("AIROOT_USER_DIR", &user_dir);

        let token = "0123456789abcdef".repeat(4);
        std::fs::write(dir.join(config::SECRETS_DIR).join("token.txt"), &token)
            .expect("写测试令牌失败");
        std::fs::write(
            dir.join(config::STATE_DIR).join("enabled.flag"),
            b"1",
        )
        .expect("写启用标志失败");
        write_test_policy(&dir, true);
        config::invalidate_cache();
        token
    }

    /// 写测试策略（TOML）。`allow_shell=true` 时额外放行传统 shell。
    ///
    /// 用生产默认策略作基线，而不是空规则表 —— 否则 argv 用例会误判成「白名单不放行」。
    fn write_test_policy(dir: &std::path::Path, allow_shell: bool) {
        let mut f = policy::PolicyFile::default_autonomy();
        f.allow_legacy_shell = allow_shell;
        if allow_shell {
            f.rules.push(policy::Rule {
                id: "t-allow-shell".into(),
                decision: policy::Decision::Allow,
                op: Some(policy::OpKind::RunShell),
                program: None,
                program_path: None,
                arg_prefix: vec![],
                arg_forbid: vec![],
                target: None,
                cwd_roots: vec![],
                note: Some("测试专用：放行传统 shell".into()),
            });
        }
        f.validate().expect("测试策略必须合法");
        let body = toml::to_string_pretty(&f).expect("序列化 TOML 失败");
        std::fs::write(dir.join(config::POLICY_FILE), body).expect("写测试策略失败");
        std::fs::write(
            dir.join(config::STATE_DIR).join(config::POLICY_MARKER),
            b"1",
        )
        .expect("写初始化标记失败");
        config::invalidate_cache();
    }

    /// 一次完整对话：服务端线程跑 `handle_client`，主线程当客户端收发帧。返回响应。
    fn e2e_roundtrip(tag: &str, req_json: &str) -> Response {
        let (srv, mut cli) = e2e_pipe_pair(tag);

        let srv_thread = std::thread::spawn(move || {
            handle_client(srv);
        });

        protocol::write_frame(&mut cli, req_json.as_bytes()).expect("写请求帧失败");
        let resp_bytes =
            protocol::read_frame_timeout(&mut cli, 10_000).expect("读响应帧应成功");
        let resp: Response = serde_json::from_slice(&resp_bytes).expect("响应应为合法 JSON");

        srv_thread.join().expect("服务端线程 panic");
        resp
    }

    /// 端到端正向：正确令牌 + 简单命令 → ok=true、exit_code=0、stdout 含预期输出。
    /// 这是整条链路的主干契约；破坏鉴权/执行/回写任一环，本测试必须 FAILED。
    #[test]
    fn e2e_handle_client_executes_command_with_valid_token() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let mark = format!("AIROOT_E2E_OK_{}", std::process::id());
        let req = serde_json::json!({
            "token": token,
            "command": format!("echo {}", mark),
        })
        .to_string();

        let resp = e2e_roundtrip("ok", &req);

        assert!(resp.ok, "有效令牌的请求应成功，实际 error={:?}", resp.error);
        assert_eq!(resp.exit_code, 0, "echo 应返回 0");
        assert!(
            resp.stdout.contains(&mark),
            "stdout 应包含预期标记 {mark}，实际 stdout={:?}",
            resp.stdout
        );
        assert!(resp.error.is_none(), "成功响应不应带 error");
    }

    /// 端到端反向：错误令牌 → ok=false、exit_code=-1、error 非空，且**不执行命令**。
    /// 变异验证：把 `ct_eq(&req.token, &token)` 改成恒真（取消鉴权），本测试必须 FAILED。
    #[test]
    fn e2e_handle_client_rejects_wrong_token() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        e2e_setup();

        let mark = format!("AIROOT_E2E_LEAK_{}", std::process::id());
        let req = serde_json::json!({
            "token": "deadbeefdeadbeef".repeat(4), // 长度合法、字形接近，但不是真实令牌
            "command": format!("echo {}", mark),
        })
        .to_string();

        let resp = e2e_roundtrip("badtoken", &req);

        assert!(!resp.ok, "错误令牌必须被拒绝");
        assert_eq!(resp.exit_code, -1, "鉴权失败 exit_code 应为 -1");
        assert!(resp.error.is_some(), "鉴权失败必须带 error 说明");
        assert!(
            !resp.stdout.contains(&mark),
            "鉴权失败绝不能执行命令（stdout 泄露了输出：{:?}）",
            resp.stdout
        );
    }

    /// 端到端：中继被禁用（无 enabled.flag）→ 拒绝执行，即使令牌正确。
    /// 变异验证：去掉 `is_enabled()` 检查，本测试必须 FAILED。
    #[test]
    fn e2e_handle_client_respects_disabled_flag() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        // v1.2 布局：启用标志在 state\ 下
        let flag = e2e_data_dir().join(config::STATE_DIR).join("enabled.flag");
        let _ = std::fs::remove_file(&flag);

        let req = serde_json::json!({
            "token": token,
            "command": "echo should_not_run",
        })
        .to_string();
        let resp = e2e_roundtrip("disabled", &req);

        // 恢复标志，避免影响同组后续用例
        std::fs::write(&flag, b"1").expect("恢复启用标志失败");

        assert!(!resp.ok, "禁用状态下必须拒绝");
        assert!(
            resp.error.as_deref().unwrap_or("").contains("禁用"),
            "错误信息应说明中继已禁用，实际 {:?}",
            resp.error
        );
        assert!(!resp.stdout.contains("should_not_run"));
    }

    /// 端到端：退出码透传（`cmd /c exit 7` → exit_code=7，ok=true 表示中继层成功）。
    /// 区分「中继层成功」与「被执命令成功」是协议关键语义（README §3.6）。
    #[test]
    fn e2e_handle_client_passes_through_exit_code() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let req = serde_json::json!({
            "token": token,
            "command": "cmd /c exit 7",
        })
        .to_string();
        let resp = e2e_roundtrip("exitcode", &req);

        assert!(resp.ok, "中继层应视为成功（命令已执行完）");
        assert_eq!(resp.exit_code, 7, "被执命令退出码应原样透传");
    }

    /// 端到端：非法请求（令牌长度不对）在**执行前**被 `validate_request` 拦截。
    #[test]
    fn e2e_handle_client_rejects_invalid_request_before_exec() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        e2e_setup();

        let req = serde_json::json!({
            "token": "tooshort",
            "command": "echo should_not_run",
        })
        .to_string();
        let resp = e2e_roundtrip("invalid", &req);

        assert!(!resp.ok);
        assert_eq!(resp.exit_code, -1);
        assert!(
            resp.error.as_deref().unwrap_or("").contains("验证失败"),
            "应为请求验证失败，实际 {:?}",
            resp.error
        );
        assert!(!resp.stdout.contains("should_not_run"));
    }

    // ============================================================
    // 授权层端到端（v2 策略 / 确认流程）
    // ============================================================

    /// 默认策略下，传统 shell 必须变成「需要确认」，且**绝不能执行**。
    #[test]
    fn e2e_shell_requires_confirmation_and_does_not_execute() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        // 换成「不放行 shell」的策略
        write_test_policy(&e2e_data_dir(), false);

        let mark = format!("AIROOT_CONFIRM_LEAK_{}", std::process::id());
        let req = serde_json::json!({
            "token": token,
            "command": format!("echo {mark}"),
        })
        .to_string();
        let resp = e2e_roundtrip("confirm", &req);

        assert_eq!(resp.decision.as_deref(), Some("confirm"), "应为待确认");
        assert!(!resp.ok, "待确认不得视为成功");
        assert!(
            resp.confirmation_id.is_some(),
            "必须返回请求 ID，否则用户无法批准"
        );
        assert!(
            !resp.stdout.contains(&mark),
            "待确认的请求绝不能被执行（stdout 泄露: {:?}）",
            resp.stdout
        );
        // 恢复，避免影响同组其它用例
        write_test_policy(&e2e_data_dir(), true);
    }

    /// 结构化 argv 命中默认白名单 → 直接执行，不需要确认。
    #[test]
    fn e2e_argv_allowed_by_policy_runs_without_confirmation() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let req = serde_json::json!({
            "token": token,
            "v": 2,
            "op": "run_argv",
            "argv": ["whoami"],
        })
        .to_string();
        let resp = e2e_roundtrip("argvok", &req);

        assert!(resp.ok, "argv 白名单应直接允许，error={:?}", resp.error);
        assert_eq!(resp.decision.as_deref(), Some("allow"));
        assert_eq!(resp.exit_code, 0);
        assert!(!resp.stdout.trim().is_empty(), "应拿到 whoami 输出");
    }

    /// argv 模式下的 shell 宿主被内置保护拒绝（不能靠规则放开）。
    #[test]
    fn e2e_argv_shell_host_is_denied() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let req = serde_json::json!({
            "token": token,
            "v": 2,
            "op": "run_argv",
            "argv": ["cmd", "/c", "echo should_not_run"],
        })
        .to_string();
        let resp = e2e_roundtrip("shellhost", &req);

        assert_eq!(resp.decision.as_deref(), Some("deny"));
        assert!(!resp.ok);
        assert_eq!(resp.rule_id.as_deref(), Some("builtin:shell-host"));
        assert!(!resp.stdout.contains("should_not_run"));
    }

    /// 破坏性 shell 命令：Deny（确认也不能放行）。
    #[test]
    fn e2e_destructive_shell_is_denied() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let req = serde_json::json!({
            "token": token,
            "command": "format D: /q",
        })
        .to_string();
        let resp = e2e_roundtrip("destructive", &req);

        assert_eq!(resp.decision.as_deref(), Some("deny"));
        assert_eq!(resp.rule_id.as_deref(), Some("builtin:destructive"));
    }

    /// 完整确认流程：待确认 → 管理员批准 → 重放同一条命令 → 执行成功。
    /// 这是任务书 §7 的核心验收：确认与请求绑定，且只有批准后才执行。
    #[test]
    fn e2e_confirm_then_replay_executes() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        write_test_policy(&e2e_data_dir(), false);

        let mark = format!("AIROOT_APPROVED_{}", std::process::id());
        let req = serde_json::json!({
            "token": token,
            "command": format!("echo {mark}"),
        })
        .to_string();

        // 第 1 次：得到待确认 + rid
        let first = e2e_roundtrip("cfm1", &req);
        assert_eq!(first.decision.as_deref(), Some("confirm"));
        let rid = first.confirmation_id.clone().expect("应有请求 ID");
        assert!(!first.stdout.contains(&mark), "批准前不得执行");

        // 管理员批准
        let dir = e2e_data_dir();
        consent::approve(&dir, &rid).expect("批准应成功");

        // 第 2 次：同一条命令 → 应执行（复用 token，不重置策略）
        let req2 = serde_json::json!({
            "token": token,
            "command": format!("echo {mark}"),
        })
        .to_string();
        let second = e2e_roundtrip("cfm2", &req2);
        assert!(second.ok, "批准后应执行，error={:?}", second.error);
        assert_eq!(
            second.decision.as_deref(),
            Some("allow"),
            "批准后的执行应报告 allow（reason 中注明经人工批准）"
        );
        assert!(
            second.stdout.contains(&mark),
            "批准后应看到命令输出，实际 {:?}",
            second.stdout
        );

        // 第 3 次：批准已被消费 → 回到待确认（不能无限复用）
        let req3 = serde_json::json!({
            "token": token,
            "command": format!("echo {mark}"),
        })
        .to_string();
        let third = e2e_roundtrip("cfm3", &req3);
        assert_eq!(
            third.decision.as_deref(),
            Some("confirm"),
            "一份批准只能消费一次"
        );
        assert!(!third.stdout.contains(&mark));

        write_test_policy(&dir, true);
    }

    /// 批准 A 命令不能让 B 命令通过（参数绑定）。
    #[test]
    fn e2e_approval_is_bound_to_exact_command() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        write_test_policy(&e2e_data_dir(), false);
        let dir = e2e_data_dir();

        let a = serde_json::json!({"token": token, "command": "echo AAA" }).to_string();
        let first = e2e_roundtrip("bind-a", &a);
        let rid = first.confirmation_id.clone().unwrap();
        consent::approve(&dir, &rid).unwrap();

        // 换一条命令：必须仍然待确认
        let b = serde_json::json!({
            "token": token,
            "command": "echo BBB",
        })
        .to_string();
        let resp = e2e_roundtrip("bind-b", &b);
        assert_eq!(
            resp.decision.as_deref(),
            Some("confirm"),
            "批准 A 不能放行 B（参数必须完全一致）"
        );

        write_test_policy(&dir, true);
    }

    /// 未知字段的请求必须被拒绝（不能靠多带字段表达状态）。
    #[test]
    fn e2e_unknown_field_is_rejected() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();

        let req = serde_json::json!({
            "token": token,
            "command": "echo should_not_run",
            "confirmed": true,
        })
        .to_string();
        let resp = e2e_roundtrip("unknownfield", &req);

        assert!(!resp.ok);
        assert!(
            resp.error.as_deref().unwrap_or("").contains("解析失败"),
            "未知字段应导致解析失败，实际 {:?}",
            resp.error
        );
    }
    // ============================================================
    // 配置分层：接入真实授权流程（v1.2）
    // ============================================================

    /// 用户配置收紧到 disabled：真实请求必须被拒（验证用户层确实接入了判定流程）。
    #[test]
    fn e2e_user_config_can_tighten_effective_mode() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        let user_dir =
            std::env::temp_dir().join(format!("airoot-e2e-user-{}", std::process::id()));
        let user_cfg = user_dir.join(config::USER_CONFIG_FILE);
        std::fs::write(&user_cfg, "version = 2\nmode = \"disabled\"\n").expect("写用户配置");
        config::invalidate_cache();

        let req = serde_json::json!({
            "token": token,
            "v": 2,
            "op": "run_argv",
            "argv": ["whoami"],
        })
        .to_string();
        let resp = e2e_roundtrip("usertight", &req);

        // 清理，避免影响后续用例
        let _ = std::fs::remove_file(&user_cfg);
        config::invalidate_cache();

        assert_eq!(
            resp.decision.as_deref(),
            Some("deny"),
            "用户配置把模式收紧为 disabled 后，argv 白名单必须失效（error={:?}）",
            resp.error
        );
        assert_eq!(resp.rule_id.as_deref(), Some("mode:disabled"));
        assert!(!resp.ok);
    }

    /// 用户在 user.toml 里写授权规则 → 整个用户层被忽略（管理员策略照旧生效）。
    ///
    /// 这条断言的意义：user.toml **不是**绕过点。它既不能放宽，也不能通过
    /// `deny_unknown_fields` 之外的字段影响判定（写错就整层忽略，不是静默部分生效）。
    #[test]
    fn e2e_user_config_cannot_inject_rules() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        let user_dir =
            std::env::temp_dir().join(format!("airoot-e2e-user-{}", std::process::id()));
        let user_cfg = user_dir.join(config::USER_CONFIG_FILE);
        std::fs::write(
            &user_cfg,
            "version = 2\nmode = \"autonomy\"\n\n[[rule]]\nid = \"evil\"\ndecision = \"allow\"\nop = \"run_argv\"\nprogram = \"cmd\"\n",
        )
        .expect("写用户配置");
        config::invalidate_cache();

        let req = serde_json::json!({
            "token": token,
            "v": 2,
            "op": "run_argv",
            "argv": ["whoami"],
        })
        .to_string();
        let resp = e2e_roundtrip("userinject", &req);

        let _ = std::fs::remove_file(&user_cfg);
        config::invalidate_cache();

        // 用户层被整层忽略 → 走管理员策略（默认允许 whoami）
        assert!(
            resp.ok,
            "用户层非法不影响管理员策略，whoami 仍应放行（error={:?}）",
            resp.error
        );
        assert_eq!(resp.decision.as_deref(), Some("allow"));
    }

    /// 管理员策略缺失但曾初始化：真实请求必须被拒（不是回落默认策略）。
    #[test]
    fn e2e_missing_policy_after_init_denies_requests() {
        let _g = E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = e2e_setup();
        let dir = e2e_data_dir();
        std::fs::remove_file(dir.join(config::POLICY_FILE)).expect("删除策略");
        config::invalidate_cache();

        let req = serde_json::json!({
            "token": token,
            "v": 2,
            "op": "run_argv",
            "argv": ["whoami"],
        })
        .to_string();
        let resp = e2e_roundtrip("nopolicy", &req);

        // 恢复策略，避免污染同组用例
        write_test_policy(&dir, true);

        assert_eq!(
            resp.decision.as_deref(),
            Some("deny"),
            "策略被删除后必须降级（只读查询以外全拒），不能回落默认"
        );
        assert_eq!(resp.rule_id.as_deref(), Some("degraded:deny"));
    }
}
