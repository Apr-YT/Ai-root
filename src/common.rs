//! 公共常量、路径、令牌管理与 ACL 设置
//!
//! 安全模型：
//! - 共享令牌（token）仅在安装时生成一次，落盘于 `C:\ProgramData\ai-root\token.txt`
//! - 该目录 ACL 设为「仅 SYSTEM 完全控制 + 当前用户只读」，其余账户不可访问
//! - 命名管道本身也通过 SDDL 限制连接方（见 server.rs）
//! - 客户端必须以正确令牌鉴权，否则服务端拒绝执行

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::SystemTime;

/// 命名管道名称（Windows 格式：\\.\pipe\ai-root）
pub const PIPE_NAME: &str = "\\\\.\\pipe\\ai-root";

/// 实际使用的管道名：默认 PIPE_NAME；可用 AIROOT_PIPE_NAME 环境变量覆盖
/// （集成测试隔离用，避免与生产服务争用同名管道导致串扰）。
pub fn pipe_name() -> String {
    if let Ok(n) = std::env::var("AIROOT_PIPE_NAME") {
        if !n.trim().is_empty() {
            return n;
        }
    }
    PIPE_NAME.to_string()
}


/// 服务名（需与 service_dispatcher 注册名一致）
pub const SERVICE_NAME: &str = "ai-root";

/// 数据目录：默认 C:\ProgramData\ai-root；可用 AIROOT_DATA_DIR 环境变量覆盖（集成测试隔离用）
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIROOT_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    PathBuf::from(base).join("ai-root")
}

pub fn token_path() -> PathBuf {
    data_dir().join("token.txt")
}

pub fn log_path() -> PathBuf {
    data_dir().join("exec.log")
}

/// 启用开关文件：存在即「启用」，删除即「禁用」
pub fn enabled_flag_path() -> PathBuf {
    data_dir().join("enabled.flag")
}

/// 进程内令牌缓存：懒加载 + 每次请求比对 token.txt 的 mtime，文件变化即重读。
/// 这样 `ai-root rotate-token`（管理员写盘）后，服务端无需重启即可热加载新令牌。
/// 键为 `(文件mtime, 令牌)`；mtime 读取失败时视为 None（每次请求都重读，兜底正确性）。
static TOKEN_CACHE: Mutex<Option<(Option<SystemTime>, String)>> = Mutex::new(None);

/// 将 Rust 字符串转为 UTF-16 宽字符串（含结尾 0）
pub fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 生成 32 字节随机十六进制令牌
fn gen_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 通过 whoami 获取当前用户 SID（用于精细化 ACL 与管道授权）
pub fn get_user_sid() -> Option<String> {
    let out = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    // CSV 第二列为 SID: "DOMAIN\user","S-1-5-21-...","..."
    let field = s.split(',').nth(1)?;
    let sid = field.trim().trim_matches('"').to_string();
    if sid.starts_with("S-1-") {
        Some(sid)
    } else {
        None
    }
}

/// 持久化安装用户（智能体运行身份）的 SID 路径。
/// 该 SID 会被注入管道 SDDL，使智能体进程（无论是否交互登录）都能连上管道，
/// 从而在无 UAC 的前提下以 SYSTEM 身份执行命令。
pub fn user_sid_path() -> PathBuf {
    data_dir().join("sid.txt")
}

/// 持久化安装用户的 SID（仅在 install 阶段、以管理员身份解析，此时 whoami 返回的是真实用户，
/// 而非 LocalSystem）。服务运行时（SYSTEM 身份）禁止调用本函数，否则会错误写入 S-1-5-18。
pub fn save_user_sid(sid: &str) {
    let _ = std::fs::write(user_sid_path(), sid);
}

/// 构造管道安全描述符（SDDL）。
/// 基础授权：SYSTEM / Administrators / Interactive 均可连。
/// 若已持久化安装用户 SID，则追加该 SID 的完全访问，确保智能体（以该用户身份运行，
/// 可能非交互登录）也能稳定连接，实现无人值守的 SYSTEM 命令中继。
pub fn pipe_sddl() -> String {
    let mut s = String::from("D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;IU)");
    if let Ok(sid) = std::fs::read_to_string(user_sid_path()) {
        let sid = sid.trim();
        if sid.starts_with("S-1-") {
            s.push_str(&format!("(A;;FA;;;{})", sid));
        }
    }
    s
}

/// 设置目录 ACL：仅 SYSTEM 完全控制 + 指定用户只读
/// 失败则退化为「SYSTEM + Interactive 只读」，保证至少可用
fn set_dir_acl(dir: &std::path::Path) {
    let dir_str = dir.to_string_lossy().to_string();
    if let Some(sid) = get_user_sid() {
        let _ = Command::new("icacls")
            .arg(&dir_str)
            .arg("/inheritance:r")
            .arg("/grant")
            .arg(format!("*S-1-5-18:(OI)(CI)F")) // LocalSystem 完全控制
            .arg("/grant")
            .arg(format!("{sid}:(OI)(CI)R")) // 当前用户只读
            .output();
    } else {
        // 退化方案：Interactive 用户只读（仅本机交互登录者可读取令牌）
        let _ = Command::new("icacls")
            .arg(&dir_str)
            .arg("/inheritance:r")
            .arg("/grant")
            .arg("*S-1-5-18:(OI)(CI)F")
            .arg("/grant")
            .arg("*S-1-5-4:(OI)(CI)R") // Interactive 只读
            .output();
    }
}

/// 确保令牌存在：不存在则生成并写盘、设置 ACL
pub fn ensure_token() -> io::Result<String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).ok();
    let tp = token_path();
    if tp.exists() {
        return Ok(std::fs::read_to_string(&tp)?.trim().to_string());
    }
    let tok = gen_token();
    std::fs::write(&tp, &tok)?;
    set_dir_acl(&dir);
    Ok(tok)
}

/// 读取已存在的令牌；不存在返回 None
pub fn read_token() -> Option<String> {
    let tp = token_path();
    if tp.exists() {
        std::fs::read_to_string(&tp).ok().map(|s| s.trim().to_string())
    } else {
        None
    }
}

/// 读取或创建令牌（服务启动时调用，保证自恢复）
pub fn read_or_create_token() -> String {
    ensure_token().unwrap_or_default()
}

/// 填充进程内令牌缓存（兼容旧调用；服务启动时调用，缓存 mtime 与文件一致）
pub fn cache_token(tok: String) {
    let mtime = std::fs::metadata(token_path()).and_then(|m| m.modified()).ok();
    if let Ok(mut c) = TOKEN_CACHE.lock() {
        *c = Some((mtime, tok));
    }
}

/// 取进程内令牌（客户端鉴权比对用）。
/// 每次请求比对 token.txt 的 mtime：`rotate-token` 写盘后自动重读新值 → 服务端热加载。
pub fn cached_token() -> String {
    let tp = token_path();
    let mtime = std::fs::metadata(&tp).and_then(|m| m.modified()).ok();
    if let Ok(mut cache) = TOKEN_CACHE.lock() {
        if let Some((t, tok)) = cache.as_ref() {
            if *t == mtime {
                return tok.clone();
            }
        }
        let tok = std::fs::read_to_string(&tp).unwrap_or_default().trim().to_string();
        *cache = Some((mtime, tok.clone()));
        tok
    } else {
        // 缓存锁不可用（poison）：直接读文件，兜底正确性
        std::fs::read_to_string(&tp).unwrap_or_default().trim().to_string()
    }
}

/// 轮换令牌：生成新随机令牌写盘并失效进程缓存，服务端下一请求自动加载新值，旧令牌立即失效。
/// 需对 token.txt 有写权限（SYSTEM / 管理员 / 安装用户）；调用方应妥善传递新令牌（勿入日志）。
pub fn rotate_token() -> io::Result<String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).ok();
    let tok = gen_token();
    std::fs::write(token_path(), &tok)?;
    set_dir_acl(&dir);
    if let Ok(mut c) = TOKEN_CACHE.lock() {
        *c = None; // 失效缓存，下一次 cached_token() 按新 mtime 重读
    }
    Ok(tok)
}

/// 是否启用（启用标志文件存在即启用，缺省启用）
pub fn is_enabled() -> bool {
    enabled_flag_path().exists()
}

/// 设置启用状态
pub fn set_enabled(enabled: bool) {
    let flag = enabled_flag_path();
    if enabled {
        std::fs::write(&flag, b"1").ok();
    } else {
        std::fs::remove_file(&flag).ok();
    }
    // 确保目录 ACL（首次可能产生）
    set_dir_acl(&data_dir());
}

/// 当前时间戳字符串（本地时间）
pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 追加一行执行日志：时间 | 调用者(domain\user) | 客户端PID | 命令 | 退出码
/// 调用者身份来自管道对端进程（GetNamedPipeClientProcessId 审计），非服务自身环境变量。
pub fn log_exec(caller: &str, pid: u32, command: &str, exit_code: i32) {
    let line = format!(
        "[{}] caller={} pid={} exit={} cmd={}\n",
        now_str(),
        caller,
        pid,
        exit_code,
        command
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// 读取最近 N 行执行日志（GUI 用）
pub fn read_log_tail(n: usize) -> String {
    match std::fs::read_to_string(log_path()) {
        Ok(s) => {
            let lines: Vec<&str> = s.lines().collect();
            let start = lines.len().saturating_sub(n);
            lines[start..].join("\n")
        }
        Err(_) => String::new(),
    }
}
