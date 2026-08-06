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
use std::sync::OnceLock;

/// 命名管道名称（Windows 格式：\\.\pipe\ai-root）
pub const PIPE_NAME: &str = "\\\\.\\pipe\\ai-root";

/// 服务名（需与 service_dispatcher 注册名一致）
pub const SERVICE_NAME: &str = "ai-root";

/// 数据目录：C:\ProgramData\ai-root
pub fn data_dir() -> PathBuf {
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

/// 进程内令牌缓存（服务启动时填充）
static TOKEN: OnceLock<String> = OnceLock::new();

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

/// 填充进程内令牌缓存
pub fn cache_token(tok: String) {
    let _ = TOKEN.set(tok);
}

/// 取进程内令牌（客户端鉴权比对用）
pub fn cached_token() -> String {
    TOKEN.get().cloned().unwrap_or_default()
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

/// 追加一行执行日志：时间 | 用户 | 命令 | 退出码
pub fn log_exec(command: &str, exit_code: i32) {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
    let line = format!("[{}] user={} exit={} cmd={}\n", now_str(), user, exit_code, command);
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
