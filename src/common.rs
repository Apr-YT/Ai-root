//! 公共常量、路径、令牌管理与 ACL 设置
//!
//! 安全模型：
//! - 共享令牌（token）仅在安装时生成一次，落盘于 `C:\ProgramData\AI-Root\token.txt`
//! - 该目录 ACL 设为「仅 SYSTEM 完全控制 + 当前用户只读」，其余账户不可访问
//! - 命名管道本身也通过 SDDL 限制连接方（见 server.rs）
//! - 客户端必须以正确令牌鉴权，否则服务端拒绝执行

use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

// ============================================================
// 可调常量：取值来源与可调边界集中管理（安全审计 / 调参统一入口）
// 所有跨模块的硬编码阈值、超时、上限统一收敛于此，避免散落各文件难以追溯。
// ============================================================

/// 服务端读取请求帧空闲超时（毫秒）。客户端连接后超时未发数据即释放工作线程，防线程泄漏。
/// 来源：经验值；正常请求 <1s，留 10s 余量；协议层按 payload 放宽绝对上限（见 protocol.rs）。
/// 边界：建议 >= 1000（防慢网误杀），<= 60000（防线程长期占用）。
pub const PIPE_READ_TIMEOUT_MS: u64 = 10_000;

/// 客户端读取响应帧空闲超时（毫秒）。`read_frame` 默认超时，与命令执行超时无关。
/// 来源：经验值；SYSTEM 命令多数较快，兜底 60s。
pub const CLIENT_READ_TIMEOUT_MS: u64 = 60_000;

/// 命令默认执行超时（毫秒）。`AI-Root run` 未传 `--timeout` 时采用。
/// 来源：经验值；覆盖绝大多数系统命令。服务端会钳制到 [0, MAX_COMMAND_TIMEOUT_MS]。
/// 边界：建议 >= 1000。
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 60_000;

/// 命令执行超时硬上限（毫秒）。服务端对 `req.timeout_ms` 取 `.min(本值)`，
/// 防持令牌客户端用超长超时长期占用 LocalSystem 服务。
/// 来源：安全约束；10 分钟足够绝大多数 SYSTEM 运维命令。
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 600_000;

/// 服务端并发连接上限。超出直接拒绝（返回「服务繁忙」），防失控 AI 线程轰炸拖垮服务。
/// 来源：经验值 8，兼顾吞吐与资源占用。
/// 边界：建议 1..=64。
pub const MAX_CONNS: u32 = 8;

/// 客户端等待管道实例就绪超时（毫秒）。`WaitNamedPipeW` 等待窗口；
/// 服务端单实例 accept，并发连接会撞 ERROR_PIPE_BUSY，需短暂等待。
/// 来源：经验值。边界：建议 100..=2000。
pub const PIPE_CONNECT_WAIT_MS: u32 = 500;

/// 客户端连接重试轮询间隔（毫秒）。撞 ERROR_PIPE_BUSY 后 sleep 退避。
/// 来源：经验值。边界：建议 10..=200。
pub const PIPE_CONNECT_RETRY_SLEEP_MS: u64 = 50;

/// 管道读轮询间隔（毫秒）。`PeekNamedPipe` 轮询可用字节的休眠，平衡实时性与 CPU。
/// 来源：经验值。边界：建议 5..=100。
pub const PIPE_POLL_INTERVAL_MS: u64 = 20;

/// 子进程轮询间隔（毫秒）。`try_wait` 轮询退出状态的休眠。
/// 来源：经验值。边界：建议 10..=200。
pub const CHILD_POLL_INTERVAL_MS: u64 = 50;

/// 多行脚本拼接为单行的长度上限（字符）。超过回退文件方案。
/// 来源：Windows cmd 命令行上限 8191 字符的安全余量（拼接含 ` & ` 分隔）。
/// 边界：须 < 8191；建议 <= 8000。
pub const SCRIPT_INLINE_MAX_LEN: usize = 6000;

/// 协议帧最大长度（字节）。超出视为非法帧拒绝，防内存撑爆。
/// 来源：覆盖大输出 JSON 的经验上限。边界：建议 >= 1MB 且 <= 100MB。
pub const FRAME_MAX_SIZE: usize = 16 * 1024 * 1024;

/// 单流输出截断上限（字节）。stdout/stderr 超出仅返回前 N 字节并标记 truncated，
/// 不影响子进程生命周期（仍排空管道）。防 `dir /s C:\` 等拖垮 SYSTEM 内存。
/// 来源：经验值 1MB，与客户端「输出超过 1MB 已被截断」提示对应。
/// 边界：建议 >= 64KB 且 <= FRAME_MAX_SIZE（须能完整回传）。
pub const OUTPUT_LIMIT: usize = 1024 * 1024;

/// 命名管道名称（Windows 格式：\\.\pipe\AI-Root）
pub const PIPE_NAME: &str = "\\\\.\\pipe\\AI-Root";

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
pub const SERVICE_NAME: &str = "AI-Root";

/// 数据目录：默认 C:\ProgramData\AI-Root；可用 AIROOT_DATA_DIR 环境变量覆盖（集成测试隔离用）
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIROOT_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    PathBuf::from(base).join("AI-Root")
}

pub fn token_path() -> PathBuf {
    data_dir().join("token.txt")
}

pub fn log_path() -> PathBuf {
    data_dir().join("exec.log")
}

/// GUI 实时事件流文件（JSONL，追加写）。服务端在 exec_start/exec_done/token_rotate 等
/// 关键节点写入；GUI 按字节偏移增量读取实现「AI 操控实时信息流」。
/// 与 exec.log（审计，命令+退出码）互补：本文件含执行中状态、时长与输出摘要。
pub fn events_path() -> PathBuf {
    data_dir().join("events.jsonl")
}

/// 单个日志文件：持有常驻句柄 + 行缓冲，避免每次写都 Open+Append（高频命令下开销显著）。
///
/// 并发安全：每个文件一个 `Mutex`，服务端 MAX_CONNS=8 并发连接下多线程序列化写同一句柄，
/// 杜绝交错/撕裂写。轮转（>2MB）时 flush 当前缓冲 → 改名 .1 → 原地重开新句柄，
/// 句柄所有权始终归本结构，不泄露给外部。
struct LogFile {
    writer: std::io::BufWriter<File>,
    path: PathBuf,
    /// true=每条即时 flush（events.jsonl，GUI 实时信息流要求低延迟）；
    /// false=阈值批量 flush（exec.log 审计，平衡吞吐与实时性）。
    realtime: bool,
}

impl LogFile {
    fn open(path: &Path, realtime: bool) -> io::Result<Self> {
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(LogFile {
            // 64KB 内部缓冲：单条大写入可批量；阈值 flush 时仍有足够余量
            writer: std::io::BufWriter::with_capacity(64 * 1024, f),
            path: path.to_path_buf(),
            realtime,
        })
    }

    /// 轮转：先 flush 现有缓冲落盘（避免丢失），再把旧文件改名 .1，最后原地重开新句柄。
    fn rotate(&mut self) {
        let _ = self.writer.flush();
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".1");
        let backup = PathBuf::from(name);
        let _ = std::fs::rename(&self.path, &backup);
        if let Ok(f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.writer = std::io::BufWriter::with_capacity(64 * 1024, f);
        }
    }

    /// 写入一行（含换行）。写前判断是否需轮转：以「磁盘已有 + 缓冲未落 + 本次」为准，
    /// 防止缓冲导致轮转判定偏差。realtime 或缓冲超阈值即 flush，保证实时性与落盘下限。
    fn write_line(&mut self, data: &[u8]) {
        const MAX_LOG: u64 = 2 * 1024 * 1024;
        const FLUSH_THRESHOLD: usize = 16 * 1024; // exec.log 每积满 16KB 落盘一次
        let on_disk = self
            .writer
            .get_ref()
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);
        let buffered = self.writer.buffer().len() as u64;
        if on_disk + buffered + data.len() as u64 > MAX_LOG {
            self.rotate();
        }
        let _ = self.writer.write_all(data);
        if self.realtime || self.writer.buffer().len() >= FLUSH_THRESHOLD {
            let _ = self.writer.flush();
        }
    }

    /// 强制落盘当前缓冲（进程退出/服务停止时调用，避免缓冲中审计条目丢失）。
    fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

/// 全局常驻日志器：events.jsonl（实时流）+ exec.log（审计）各持一缓冲句柄。
/// 用 `OnceLock` 懒初始化：首次写日志时按当前 data_dir() 打开句柄。
/// 初始化失败（极端：目录不可建）时返回 None，调用方回退旧逻辑逐次 Open+Append，不丢日志。
struct Logger {
    events: Mutex<LogFile>,
    exec: Mutex<LogFile>,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

fn build_logger() -> Option<Logger> {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let events = LogFile::open(&events_path(), true).ok()?;
    let exec = LogFile::open(&log_path(), false).ok()?;
    Some(Logger {
        events: Mutex::new(events),
        exec: Mutex::new(exec),
    })
}

/// 取全局日志器（懒初始化）。返回 None 表示句柄不可用，调用方应回退旧逻辑。
fn logger() -> Option<&'static Logger> {
    LOGGER.get_or_init(build_logger).as_ref()
}

/// 强制落盘全部缓冲（服务停止/进程退出时调用，防止审计条目滞留内存丢失）。
/// 句柄锁异常（poison）时静默跳过，不阻断退出。
pub fn flush_logs() {
    if let Some(lg) = logger() {
        if let Ok(mut e) = lg.events.lock() {
            e.flush();
        }
        if let Ok(mut x) = lg.exec.lock() {
            x.flush();
        }
    }
}

/// 追加一条 GUI 实时事件（调用方构造 JSON 对象；禁止写入令牌明文/命令输出全文）。
/// 超 2MB 自动轮转（改名 .1）。优先走常驻缓冲句柄（无逐次 Open+Append 开销），
/// 句柄不可用时回退旧逻辑逐次写。
pub fn log_event(event: serde_json::Value) {
    let mut line = serde_json::to_string(&event).unwrap_or_default();
    line.push('\n');
    if let Some(lg) = logger() {
        if let Ok(mut w) = lg.events.lock() {
            w.write_line(line.as_bytes());
            return;
        }
    }
    append_with_rotate(events_path(), line.as_bytes());
}

/// 兜底：逐次打开并追加（仅当常驻句柄不可用 / 锁 poison 时走此路径，保证不丢日志）。
fn append_with_rotate(path: PathBuf, data: &[u8]) {
    const MAX_LOG: u64 = 2 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG {
            let mut name = path.as_os_str().to_os_string();
            name.push(".1");
            let backup = PathBuf::from(name);
            let _ = std::fs::rename(&path, &backup);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(data);
    }
}

/// 从 offset 字节处读取 events.jsonl 新增行，返回（新增行, 新的文件长度）。
/// `AI-Root tail` 每 500ms 调用一次实现增量实时跟随；offset 越界/文件被截断时安全回退。
pub fn read_events_since(offset: u64) -> (Vec<String>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let path = events_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return (Vec::new(), 0);
    };
    let size = meta.len();
    if size <= offset {
        return (Vec::new(), size);
    }
    let Ok(mut f) = std::fs::File::open(&path) else {
        return (Vec::new(), size);
    };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), size);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (Vec::new(), size);
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    (lines, size)
}

/// 启用开关文件：存在即「启用」，删除即「禁用」
pub fn enabled_flag_path() -> PathBuf {
    data_dir().join("enabled.flag")
}

/// 进程内令牌缓存：懒加载 + 每次请求比对 token.txt 的 mtime，文件变化即重读。
/// 这样 `AI-Root rotate-token`（管理员写盘）后，服务端无需重启即可热加载新令牌。
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

/// 生成 32 字节随机十六进制令牌（加密安全随机数）
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
///
/// ⚠ 安全关键：必须先用 `/inheritance:r` **断开并清除继承的 ACE**，再显式授权。
///
/// 2026-10-04 实测发现的缺陷：原实现只用 `/inheritance:e` + `/grant:r`，
/// 而 `icacls /grant:r` **只替换指定 SID 的那一条 ACE，不会清除其它 SID 的 ACE**。
/// 于是 `C:\ProgramData` 继承来的 `BUILTIN\Users:(I)(CI)(WD,AD,WEA,WA)` 会**原样保留** ——
/// 即任意本地用户仍对该目录**有写权限**，可覆写 `token.txt` 并自授权连上 SYSTEM 中继。
/// 这与本函数「仅 SYSTEM 完全控制 + 用户只读」的声明语义完全背离。
/// 实测对照（父目录含可继承的 Users 写 ACE）：
///   `/inheritance:e` → `BUILTIN\Users:(I)(OI)(CI)(WD,AD)` 残留 ❌
///   `/inheritance:r` → 仅 `SYSTEM:(OI)(CI)(F)` + 用户只读 ✅
///
/// 为何 `/inheritance:r` 不会破坏「SYSTEM 建的文件用户可读」：本函数随后显式授予的
/// ACE 均带 `(OI)(CI)` 传播标志，由**本目录自身**的显式 ACE 向下继承，
/// 不依赖上级目录 → 子文件仍得到 `SYSTEM:F + Admins:F + 用户:R`。
///
/// 幂等性：必须使用 `/grant:r`（replace 语义）而非 `/grant`（append 语义）。
/// `icacls /grant` 不会对同一 SID 去重，每次调用都会向 DACL 追加一条 `(OI)(CI)` ACE，
/// 服务启动 / `ensure_token` / `rotate_token` / `set_enabled` 反复调用会导致 ACE 条目无限膨胀。
/// `:r` 会先移除该 SID 的全部既有 ACE 再写入唯一一条，既保证幂等，也顺带清理历史累积的重复 ACE。
pub fn set_dir_acl(dir: &std::path::Path) {
    let dir_str = dir.to_string_lossy().to_string();
    // 先断开继承（清除全部继承 ACE），再加显式 ACE。
    // 顺序不可颠倒：若先 grant 再 inheritance:r，grant 的结果会被一并清掉。
    let _ = Command::new("icacls")
        .arg(&dir_str)
        .arg("/inheritance:r")
        .output();
    if let Some(sid) = get_user_sid() {
        // ⚠ `{sid}` 必须带 `*` 前缀！`*` 告知 icacls「这是 SID 字面量，不要去解析账户名」。
        // 缺 `*` 时 icacls 会把 `S-1-5-21-...` 当**账户名**去查，查不到即报
        //   "账户名与安全标识间无任何映射完成"
        // 并**整条命令以非零退出、一条 ACE 都不写**（grant 参数是「全有或全无」）。
        // 叠加同函数的 `/inheritance:r` 后后果极严重：继承 ACE 已清除、显式 ACE 又写入失败
        // → 目录 DACL 变空 → SYSTEM 自己都进不去，服务彻底锁死。
        // 该缺陷原本被 `/inheritance:e` 掩盖（继承来的 SYSTEM:F 兜住了访问），
        // 属"两个 bug 互相遮蔽"，2026-10-04 修复继承缺陷时才暴露。
        //
        // 因此这里**逐条独立授予**（而非塞进一条 icacls 命令）：icacls 的 grant 参数是
        // 「全有或全无」，一条坏参数会让同命令内其它正当 ACE 一起丢失。
        // 逐条执行后，即使某条失败，SYSTEM 的 ACE 也已独立落盘，不会出现空 DACL。
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F"); // LocalSystem 完全控制
        grant_one(&dir_str, "*S-1-5-32-544:(OI)(CI)F"); // Administrators 完全控制（运维/排障必需）
        grant_one(&dir_str, &format!("*{sid}:(OI)(CI)R")); // 当前用户只读（注意 * 前缀）
    } else {
        // 退化方案：Interactive 用户只读（仅本机交互登录者可读取令牌）
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F");
        grant_one(&dir_str, "*S-1-5-32-544:(OI)(CI)F");
        grant_one(&dir_str, "*S-1-5-4:(OI)(CI)R"); // Interactive 只读
    }
    // 兜底自检：确认 SYSTEM 的完全控制 ACE 确实写入成功。
    // 本函数先 `/inheritance:r` 清空继承 ACE 再重新授权，若期间授权失败，目录 DACL 会变空
    // → SYSTEM 自己都进不去，服务彻底锁死且难以恢复。此处检测到即立刻补一条，
    // 把「空 DACL 锁死」这一失败模式从结构上消除。
    if !acl_has_system_full(&dir_str) {
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F");
    }
}

/// 检查目录 ACL 中是否存在 SYSTEM 的完全控制 ACE（用于硬化后的兜底自检）。
fn acl_has_system_full(dir_str: &str) -> bool {
    let Ok(out) = Command::new("icacls").arg(dir_str).output() else {
        return false;
    };
    // 注意 icacls 输出为 OEM/ANSI 编码，但 ACE 权限位字符（如 "(F)"）与 SID 名均为 ASCII，
    // 足以判断；Windows 下 SYSTEM 显示名恒为 "NT AUTHORITY\\SYSTEM"。
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .any(|l| l.contains("SYSTEM") && l.contains("(F)"))
}

/// 单条 `icacls /grant:r` 授权。`:r` = replace 语义（幂等，不累积重复 ACE）。
/// 独立成函数是为了让每条 ACE 各自落盘，避免一条参数非法导致整批丢失。
fn grant_one(dir_str: &str, ace: &str) {
    let _ = Command::new("icacls")
        .arg(dir_str)
        .arg("/grant:r")
        .arg(ace)
        .output();
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
        std::fs::read_to_string(&tp)
            .ok()
            .map(|s| s.trim().to_string())
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
    let mtime = std::fs::metadata(token_path())
        .and_then(|m| m.modified())
        .ok();
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
        let tok = std::fs::read_to_string(&tp)
            .unwrap_or_default()
            .trim()
            .to_string();
        *cache = Some((mtime, tok.clone()));
        tok
    } else {
        // 缓存锁不可用（poison）：直接读文件，兜底正确性
        std::fs::read_to_string(&tp)
            .unwrap_or_default()
            .trim()
            .to_string()
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
    // GUI 实时信息流：令牌轮换事件（绝不记录令牌明文）
    log_event(serde_json::json!({
        "ts": now_str(),
        "type": "token_rotate",
    }));
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
/// 超 2MB 自动轮转（改名 .1，避免无限增长）。优先走常驻缓冲句柄（无逐次 Open+Append 开销），
/// 句柄不可用时回退旧逻辑逐次写。
pub fn log_exec(caller: &str, pid: u32, command: &str, exit_code: i32) {
    let line = format!(
        "[{}] caller={} pid={} exit={} cmd={}\n",
        now_str(),
        caller,
        pid,
        exit_code,
        command
    );
    if let Some(lg) = logger() {
        if let Ok(mut w) = lg.exec.lock() {
            w.write_line(line.as_bytes());
            return;
        }
    }
    append_with_rotate(log_path(), line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 运行 icacls 并返回 stdout（用于断言 ACL 内容）。
    fn icacls_show(dir: &std::path::Path) -> String {
        let out = Command::new("icacls")
            .arg(dir.to_string_lossy().to_string())
            .output()
            .expect("icacls 执行失败");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// 取当前用户的 `DOMAIN\user` 形式名（与 icacls 输出中的显示形式一致）。
    /// 用 `whoami`（无参数）取，是本机唯一无需额外依赖的方式。
    fn current_user_name() -> Option<String> {
        let out = Command::new("whoami").output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let n = s.trim().to_string();
        if n.is_empty() {
            None
        } else {
            Some(n)
        }
    }

    /// 安全回归：`set_dir_acl` 必须清除**继承来的** `BUILTIN\Users` 写权限。
    ///
    /// 背景（2026-10-04 实测发现的真实缺陷）：原实现用 `/inheritance:e` + `/grant:r`，
    /// 而 `icacls /grant:r` 只替换指定 SID 那一条 ACE，**不清除其它 SID 的 ACE**。
    /// `C:\ProgramData` 继承来的 `BUILTIN\Users:(I)(CI)(WD,AD,WEA,WA)` 会原样保留，
    /// 使任意本地用户仍可写该目录 → 覆写 `token.txt` 自授权连 SYSTEM 中继。
    ///
    /// 本测试真实构造「父目录授予可继承的 Users 写权限」这一危险前提，
    /// 再调用**真实的** `set_dir_acl`，断言 Users 的写权限已被剥离。
    /// 若把实现改回 `/inheritance:e`，本测试必须 FAILED（变异验证见提交说明）。
    #[test]
    fn set_dir_acl_strips_inherited_users_write() {
        // 唯一测试目录（pid + 测试名，避免并行冲突）
        let base = std::env::temp_dir().join(format!("airoot-acl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let child = base.join("data");
        std::fs::create_dir_all(&child).expect("创建测试目录失败");

        // 1) 父目录授予 Users 可继承的写权限（复制 ProgramData 的危险前提）
        let grant = Command::new("icacls")
            .arg(base.to_string_lossy().to_string())
            .arg("/grant")
            .arg("Users:(OI)(CI)(WD,AD)")
            .output()
            .expect("icacls 授权父目录失败");
        assert!(
            grant.status.success(),
            "测试前提构造失败：无法给父目录授予 Users 写权限"
        );

        // 2) 确认前提成立：子目录确实继承了 Users 写权限（含 (I) 标记）
        let before = icacls_show(&child);
        let inherited_write = before
            .lines()
            .any(|l| l.contains("Users") && l.contains("(I)") && (l.contains("WD") || l.contains("AD")));
        assert!(
            inherited_write,
            "测试前提不成立：子目录未继承到 Users 写权限，本测试失去意义。\n实际 ACL:\n{before}"
        );

        // 3) 调用真实被测函数
        set_dir_acl(&child);

        // 4) 断言：Users 不再持有任何写权限位（WD/AD/WEA/WA/写类权限）
        let after = icacls_show(&child);
        let users_has_write = after.lines().any(|l| {
            l.contains("Users")
                && (l.contains("WD") || l.contains("AD") || l.contains("WEA") || l.contains("WA"))
        });
        assert!(
            !users_has_write,
            "安全缺陷回归：set_dir_acl 之后 Users 仍持有写权限，\
             本地任意用户可覆写 token.txt 自授权。\n实际 ACL:\n{after}"
        );

        // 5) 断言：SYSTEM 仍为完全控制（不能把服务自己锁在外面）
        let sys_full = after
            .lines()
            .any(|l| l.contains("SYSTEM") && l.contains("(F)"));
        assert!(
            sys_full,
            "修复过度：SYSTEM 失去完全控制，服务将无法写日志/令牌。\n实际 ACL:\n{after}"
        );

        // 6) 断言：**当前用户仍保有只读 ACE**。
        // 这条防的是「bare SID 缺 `*` 前缀」缺陷：icacls 把裸 SID 当账户名解析失败
        // （报"账户名与安全标识间无任何映射完成"），该条 grant 静默失败。
        // 变异验证实测：去掉 `*` 前缀后前 5 条断言全部通过 → 属真实漏检，故补此断言。
        // 后果：智能体（用户身份）读不到 token.txt，SYSTEM 中继整体不可用。
        //
        // icacls 显示的是**账户名**（如 `KING\64391`）而非 SID，故需先解析出账户名。
        // ⚠ 大小写不敏感比较：`whoami` 返回小写 `king\64391`，而 `icacls` 打印大写
        // `KING\64391` —— Windows 账户名本身大小写不敏感，比较时必须归一化，
        // 否则会在代码完全正确时误报失败（实测踩过）。
        let user_name = current_user_name()
            .expect("测试环境应能取到当前用户名")
            .to_lowercase();
        let user_readable = after.lines().any(|l| {
            let low = l.to_lowercase();
            low.contains(&user_name) && l.contains("(R)")
        });
        assert!(
            user_readable,
            "当前用户 ({user_name}) 失去只读 ACE。\
             典型原因：icacls 的 SID 参数缺少 `*` 前缀，被当账户名解析失败而静默丢弃。\n实际 ACL:\n{after}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `set_dir_acl` 必须幂等：重复调用不产生重复 ACE（防止服务反复启动时 ACE 膨胀）。
    #[test]
    fn set_dir_acl_is_idempotent() {
        let base = std::env::temp_dir().join(format!("airoot-acl-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");

        set_dir_acl(&base);
        set_dir_acl(&base);
        set_dir_acl(&base);

        let acl = icacls_show(&base);
        // 统计 SYSTEM 的 ACE 条数：幂等实现下应恰好 1 条（不受继承干扰，已 /inheritance:r）
        let sys_count = acl.lines().filter(|l| l.contains("SYSTEM")).count();
        assert_eq!(
            sys_count, 1,
            "set_dir_acl 不幂等：SYSTEM ACE 出现 {sys_count} 次（期望 1）。\
             /grant:r 的 replace 语义被破坏会导致 ACE 无限膨胀。\n实际 ACL:\n{acl}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
