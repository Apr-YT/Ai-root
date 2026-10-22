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

use crate::winapi_raw;

// ============================================================
// 可调常量：取值来源与可调边界集中管理（安全审计 / 调参统一入口）
// 所有跨模块的硬编码阈值、超时、上限统一收敛于此，避免散落各文件难以追溯。
// ============================================================

/// 服务端读取请求帧空闲超时（毫秒）。客户端连接后超时未发数据即释放工作线程，防线程泄漏。
/// 来源：经验值；正常请求 <1s，留 10s 余量；协议层按 payload 放宽绝对上限（见 protocol.rs）。
/// 边界：建议 >= 1000（防慢网误杀），<= 60000（防线程长期占用）。
pub const PIPE_READ_TIMEOUT_MS: u64 = 10_000;

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

/// 机器级数据目录：默认 `%ProgramData%\AI-Root`
///
/// 可用 `AIROOT_DATA_DIR` 覆盖 —— **仅用于集成测试/多实例隔离**，不是给最终用户调的参数
/// （能改数据目录等于能把令牌指到任意位置）。路径布局见 `config.rs`。
pub fn data_dir() -> PathBuf {
    crate::config::machine_dir()
}

/// 令牌路径：`<data>/secrets/token.txt`
///
/// 为什么单独放 `secrets\`：令牌等同 SYSTEM 凭据。与日志/状态分开后，
/// 才能对秘密目录单独收紧 ACL，而不必为了"能看日志"而放宽秘密的可见范围。
pub fn token_path() -> PathBuf {
    crate::config::Paths::resolve()
        .secrets()
        .join("token.txt")
}

/// GUI 实时事件流文件（JSONL，追加写）。服务端在 exec_start/exec_done/token_rotate 等
/// 关键节点写入；GUI 按字节偏移增量读取实现「AI 操控实时信息流」。
/// 与 exec.log（审计，命令+退出码）互补：本文件含执行中状态、时长与输出摘要。
pub fn events_path() -> PathBuf {
    logs_dir().join("events.jsonl")
}

/// 受 ACL 保护的私有临时目录（脚本模式落盘用）。
///
/// 为什么不复用 `std::env::temp_dir()`：以 SYSTEM 运行时它解析为
/// `C:\Windows\Temp`，该目录对普通用户可创建文件 —— 把「即将由 SYSTEM 执行」
/// 的脚本放在用户可写的目录里，等于给本地提权留了一个竞争窗口。
/// 本目录位于数据目录之下，继承 `SYSTEM:F / Administrators:F / 安装用户:R`，
/// 普通用户无法写入或替换其中文件。
pub fn tmp_dir() -> PathBuf {
    crate::config::Paths::resolve().state().join("tmp")
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

/// 日志落盘目录。
///
/// ⚠ 测试构建下固定指向 `%TEMP%iroot-test-logs`：
/// 单元/端到端测试会真实执行命令并记录事件，若直接写 `data_dir()`（默认
/// `%ProgramData%\AI-Root`），跑一次 `cargo test` 就会在**用户机器**上凭空造出一个
/// 未加固 ACL 的目录 —— 既污染环境，也会让人误以为「服务装过」。
fn logs_dir() -> PathBuf {
    #[cfg(test)]
    {
        std::env::temp_dir().join("airoot-test-logs")
    }
    #[cfg(not(test))]
    {
        crate::config::Paths::resolve().logs()
    }
}

/// 事件流日志路径（内部使用；`tail` 命令仍读 `events_path()`）
fn events_log_path() -> PathBuf {
    logs_dir().join("events.jsonl")
}

/// 审计日志路径（内部使用）
fn exec_log_path() -> PathBuf {
    logs_dir().join("exec.log")
}

fn build_logger() -> Option<Logger> {
    let dir = logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let events = LogFile::open(&events_log_path(), true).ok()?;
    // 审计日志同样逐条落盘。
    //
    // 曾经用 16KB 批量 flush（realtime=false）。问题是：只有在缓冲积满、
    // 或服务**正常停止**时才落盘，于是个人使用这种每天几十条命令的量，
    // 文件上能看到的是很久以前的内容；而进程被强杀（taskkill /F、崩溃、
    // 断电）时，缓冲区里的条目全部丢失。审计日志的价值就是事后可查，
    // 这两点都不可接受。单条 write + flush 的开销在这个量级可以忽略。
    let exec = LogFile::open(&exec_log_path(), true).ok()?;
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
    append_with_rotate(events_log_path(), line.as_bytes());
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
    crate::config::Paths::resolve().state().join("enabled.flag")
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

/// 本机 LocalSystem 的 SID。用于识别「当前进程就是 SYSTEM」这一情形。
pub const LOCAL_SYSTEM_SID: &str = "S-1-5-18";

/// 取「应被授予数据目录只读权限的用户」SID —— 即**安装用户**（智能体运行身份）。
///
/// 优先级：已持久化的 `sid.txt` → 回退到当前进程身份。
///
/// ⚠ 为什么不能直接用 `get_user_sid()`（线上故障根因）：
///   `set_dir_acl` 被**服务自身**在启动时调用（`run_pipe_server`），而服务以 LocalSystem
///   运行 → 此时 `get_user_sid()` 返回 **`S-1-5-18`**（SYSTEM 自己），不是安装用户。
///   于是「授予当前用户只读」这条 ACE 变成「把 SYSTEM 降级为只读」，
///   且 `:r` 是 replace 语义、覆盖掉了先前授予的 SYSTEM:(F) →
///   **服务启动即把自己对数据目录的权限降为只读**，无法再写 token.txt / events.jsonl。
///   本函数通过「读持久化的安装用户 SID」+「拒绝返回 S-1-5-18」从根上杜绝该故障。
pub fn granted_user_sid() -> Option<String> {
    let persisted = std::fs::read_to_string(user_sid_path()).ok();
    select_grant_sid(persisted.as_deref(), get_user_sid().as_deref())
}

/// `granted_user_sid()` 的**纯逻辑内核**（可单元测试，无环境依赖）。
///
/// 分离原因：`granted_user_sid()` 依赖真实环境（读 sid.txt + 跑 whoami），
/// 而测试进程**永远不是 SYSTEM**，无法在测试里构造「当前身份 = S-1-5-18」这一幕，
/// 导致「禁止返回 SYSTEM SID」这条关键不变量**无法被覆盖**（变异验证实测漏检）。
/// 抽出纯函数后，可以直接喂入 `("S-1-5-18", "S-1-5-18")` 断言返回 `None`。
///
/// 规则（按优先级）：
///   1. `persisted`（sid.txt）合法（`S-1-` 前缀）且非 SYSTEM → 采用；
///   2. 否则用 `current`（当前进程身份），但**必须非 SYSTEM**；
///   3. 二者都是 SYSTEM 或都不可用 → `None`（调用方走 Interactive 只读退化分支）。
///
/// 第 2/3 条是线上故障的根因防线：SYSTEM 身份下 `current` == S-1-5-18，
/// 若放任其返回，`set_dir_acl` 会把 SYSTEM 自己替换成只读。
fn select_grant_sid(persisted: Option<&str>, current: Option<&str>) -> Option<String> {
    let norm = |s: Option<&str>| -> Option<String> {
        let v = s?.trim();
        if v.starts_with("S-1-") {
            Some(v.to_string())
        } else {
            None
        }
    };
    let persisted = norm(persisted);
    if let Some(sid) = persisted {
        if sid != LOCAL_SYSTEM_SID {
            return Some(sid);
        }
    }
    match norm(current) {
        Some(sid) if sid != LOCAL_SYSTEM_SID => Some(sid),
        _ => None,
    }
}

/// 持久化安装用户（智能体运行身份）的 SID 路径。
/// 该 SID 会被注入管道 SDDL，使智能体进程（无论是否交互登录）都能连上管道，
/// 从而在无 UAC 的前提下以 SYSTEM 身份执行命令。
pub fn user_sid_path() -> PathBuf {
    crate::config::Paths::resolve().state().join("sid.txt")
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

/// ACL 操作失败（安全关键路径上不允许静默降级）
#[derive(Debug)]
pub struct AclError(pub String);
impl std::fmt::Display for AclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AclError {}

/// 设置目录 ACL：仅 SYSTEM 完全控制 + Administrators 完全控制 + 安装用户只读
///
/// 为什么必须给 Administrators `(F)`（实测补充）：
///   首次以管理员身份 `install` 时，目录由**当前用户（Administrator 成员）**创建，
///   目录 owner 即当前用户；而 `set_dir_acl` 只显式授予 SYSTEM + 用户只读。
///   Windows 的 `icacls` 在 DACL 变更后，**为了不把 owner 自己锁在外面**，会自动把
///   原本经继承（ProgramData → BUILTIN\Administrators `(F)`）获得的 `(F)` 以
///   **显式 ACE** 形式固化进 DACL —— 于是 ACL 里会多出一条
///   `BUILTIN\Administrators:(F)`。这是 Windows 的行为，不是本函数的输出。
///   既然它必然出现，就**显式授予**而非依赖隐式补写，避免「ACL 与设计文档不一致」。
///
/// ⚠ 安全关键：必须先用 `/inheritance:r` **断开并清除继承的 ACE**，再显式授权。
///
/// 实测发现的缺陷：只用 `/inheritance:e` + `/grant:r` 时，
/// 而 `icacls /grant:r` **只替换指定 SID 的那一条 ACE，不会清除其它 SID 的 ACE**。
/// 于是 `C:\ProgramData` 继承来的 `BUILTIN\Users:(I)(CI)(WD,AD,WEA,WA)` 会**原样保留** ——
/// 即任意本地用户仍对该目录**有写权限**，可覆写 `token.txt` 并自授权连上 SYSTEM 中继。
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
///
/// ⚠ 错误处理：返回 `()` 并把每条 icacls 的结果 `let _ =` 丢弃，
/// 属 fail-open —— `/inheritance:r` 成功而后续 grant 全失败时，目录可能落入
/// 「SYSTEM 无写权限」或「继承已断但显式 ACE 缺失」的状态，而调用方毫不知情。
/// 现在任何一步失败都返回 `Err`，由调用方决定是否继续（服务启动时选择拒绝启动）。
pub fn set_dir_acl(dir: &std::path::Path) -> Result<(), AclError> {
    harden_dir_tree(dir, ACL_HARDEN_DEPTH)
}

/// 递归硬化深度。
///
/// 当前布局为 `machine\{secrets,logs,state\{tmp,pending},rules.d}`，2 层足够。
/// 不做无界递归：`AIROOT_DATA_DIR` 可以把数据目录指向任意位置，
/// 无界遍历会拖慢服务启动。
const ACL_HARDEN_DEPTH: u8 = 2;

/// 逐层硬化：每个目录都执行 `icacls /inheritance:r` + 显式授权。
///
/// ## 为什么必须递归（本机 install 实测失败）
///
/// `icacls /inheritance:r` **只作用于指定目录，不递归子目录**。
///
/// 而 `ensure_token` 的顺序是 `create_dir_all(secrets)` → `set_dir_acl(数据目录)`。
/// 创建 `secrets` 时父目录 `%ProgramData%\AI-Root` 还是从 `ProgramData` 继承来的
/// 弱 DACL，于是 `secrets` 把 3 条 ACE（含 Users 可写）**固化**成自己的继承 ACE。
/// 之后对父目录执行 `/inheritance:r` 并不会清除子目录里已经固化的那些 ACE，
/// 紧接着 `verify_dir_acl(secrets)` 报
/// 「残留 3 条继承 ACE（/inheritance:r 未生效）」并拒绝写入令牌 —— 安装直接失败。
///
/// 这是「硬化没覆盖到子目录」，不是「校验过严」。放宽校验会让秘密目录真的
/// 保留可写 ACE；正确做法是逐层断继承。
fn harden_dir_tree(dir: &std::path::Path, depth: u8) -> Result<(), AclError> {
    set_one_dir_acl(dir)?;
    if depth == 0 {
        return Ok(());
    }
    // 读不到子项不算失败：目录本身已硬化，可能只是没有列目录的权限。
    let mut children: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    };
    children.sort();
    for child in children {
        // 子目录失败必须向上传播：秘密目录权限不可靠时不能算「部分成功」。
        harden_dir_tree(&child, depth - 1)?;
    }
    Ok(())
}

fn set_one_dir_acl(dir: &std::path::Path) -> Result<(), AclError> {
    let dir_str = dir.to_string_lossy().to_string();

    // 先断开继承（清除全部继承 ACE），再加显式 ACE。
    // 顺序不可颠倒：若先 grant 再 inheritance:r，grant 的结果会被一并清掉。
    // 本步失败必须终止：继承未断时，「用户不可写」无法由本函数的显式 ACE 保证。
    let out = Command::new("icacls")
        .arg(&dir_str)
        .arg("/inheritance:r")
        .output()
        .map_err(|e| AclError(format!("无法执行 icacls: {e}")))?;
    if !out.status.success() {
        return Err(AclError(format!(
            "icacls /inheritance:r 失败（exit={:?}）: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim()
        )));
    }

    // 清除历史上可能残留的、本不该存在的显式 ACE。
    //
    // 为什么必须有这一步：`/inheritance:r` 只断开并清除**继承**的 ACE，
    // 不会删除已经**显式**写进 DACL 的条目。于是出现过这种情况：
    //   某次运行拿不到安装用户 SID，走了退化分支，显式写入 `INTERACTIVE:(R)`；
    //   之后拿到 SID 了，新的授权会追加用户自己的 ACE，但那条 INTERACTIVE 删不掉，
    //   两条长期并存。多用户机器上，其他交互登录用户因此也能读到 token.txt。
    // `verify_dir_acl` 只检查写权限，读权限它不管，所以这个残留不会报错。
    strip_stale_aces(&dir_str);

    // ⚠ 必须用 `granted_user_sid()`（安装用户）而非 `get_user_sid()`（当前进程身份）：
    //   本函数会被以 SYSTEM 运行的服务调用，此时 `get_user_sid()` == S-1-5-18，
    //   会导致「授予当前用户只读」变成「把 SYSTEM 降级为只读」→ 服务无法写数据目录。
    //   详见 `granted_user_sid()` 的文档注释。
    //
    // ⚠ `{sid}` 必须带 `*` 前缀！`*` 告知 icacls「这是 SID 字面量，不要去解析账户名」。
    // 缺 `*` 时 icacls 会把 `S-1-5-21-...` 当**账户名**去查，查不到即报
    //   "账户名与安全标识间无任何映射完成"
    // 并**整条命令以非零退出、一条 ACE 都不写**（grant 参数是「全有或全无」）。
    // 因此这里**逐条独立授予**（而非塞进一条 icacls 命令），单条失败不牵连其它。
    if let Some(sid) = granted_user_sid() {
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F")?; // LocalSystem 完全控制
        grant_one(&dir_str, "*S-1-5-32-544:(OI)(CI)F")?; // Administrators 完全控制（运维/排障必需）
        grant_one(&dir_str, &format!("*{sid}:(OI)(CI)R"))?; // 安装用户只读（注意 * 前缀）
    } else {
        // 退化方案：Interactive 用户只读（仅本机交互登录者可读取令牌）
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F")?;
        grant_one(&dir_str, "*S-1-5-32-544:(OI)(CI)F")?;
        grant_one(&dir_str, "*S-1-5-4:(OI)(CI)R")?; // Interactive 只读
    }

    // 兜底自检：用**二进制 DACL** 确认 SYSTEM 确实有写权限（不依赖本地化显示名）。
    // 「SYSTEM 被降为只读」一旦发生，服务将无法写令牌/日志；此处检测到即立刻补回。
    if !dacl_has_system_write(dir) {
        grant_one(&dir_str, "*S-1-5-18:(OI)(CI)F")?;
        if !dacl_has_system_write(dir) {
            return Err(AclError(
                "SYSTEM 未能获得目录完全控制（补写后仍失败）".into(),
            ));
        }
    }

    // 最终验收：真实 DACL 必须满足「危险主体无写权限 + SYSTEM 有写 + 无继承 ACE」。
    // 这一步是 fail-closed 的关键 —— 不再靠 `contains("SYSTEM") && contains("(F)")` 猜。
    verify_dir_acl(dir)
}

/// 不应出现在数据目录 DACL 里的主体。
///
/// 这些主体要么范围过宽（Everyone、Authenticated Users、INTERACTIVE、Users、
/// Guests、NETWORK 都覆盖到安装用户以外的账号），要么根本不该有本地访问
/// （ANONYMOUS）。它们一旦出现在 DACL 上，即使只有读权限，也等于把令牌
/// 交给更多人。
const STALE_SIDS: &[&str] = &[
    "*S-1-5-4",      // INTERACTIVE
    "*S-1-5-11",     // Authenticated Users
    "*S-1-1-0",      // Everyone
    "*S-1-5-32-545", // Users
    "*S-1-5-32-546", // Guests
    "*S-1-5-7",      // ANONYMOUS LOGON
    "*S-1-5-2",      // NETWORK
];

/// 移除可能残留的过宽授权 ACE。
///
/// 逐条执行：`/remove:g` 对不存在的 SID 会返回非零，属于正常情况（本来就没有），
/// 因此这里不看退出码，只保证执行过。真正的验收在 `verify_dir_acl`。
///
/// 注意 `Users` 也在清除列表里，而安装用户通常是 Users 成员。删掉的是那个
/// **组**的 ACE，安装用户自己有一条独立 SID 的 ACE，读权限不受影响。
fn strip_stale_aces(dir_str: &str) {
    for sid in STALE_SIDS {
        let _ = Command::new("icacls")
            .arg(dir_str)
            .arg("/remove:g")
            .arg(sid)
            .output();
    }
}

/// 单条 `icacls /grant:r` 授权。`:r` = replace 语义（幂等，不累积重复 ACE）。
/// 独立成函数是为了让每条 ACE 各自落盘，避免一条参数非法导致整批丢失。
fn grant_one(dir_str: &str, ace: &str) -> Result<(), AclError> {
    let out = Command::new("icacls")
        .arg(dir_str)
        .arg("/grant:r")
        .arg(ace)
        .output()
        .map_err(|e| AclError(format!("无法执行 icacls: {e}")))?;
    if !out.status.success() {
        return Err(AclError(format!(
            "icacls /grant:r {ace} 失败（exit={:?}）: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim()
        )));
    }
    Ok(())
}

// ============================================================
// 真实 DACL 校验（基于 GetNamedSecurityInfoW，与系统语言无关）
// ============================================================

/// 写类访问位（任一置位即「可写」）
const WRITE_BITS: u32 = 0x0000_0002   // FILE_WRITE_DATA
    | 0x0000_0004                     // FILE_APPEND_DATA
    | 0x0000_0010                     // FILE_WRITE_EA
    | 0x0000_0100                     // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000                     // DELETE
    | 0x0004_0000                     // WRITE_DAC
    | 0x0008_0000                     // WRITE_OWNER
    | 0x1000_0000                     // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

/// 绝不允许对数据目录持有写权限的「宽泛主体」SID。
///
/// 用 SID 而不是账户名：`icacls` 输出的是本地化显示名，
/// 中文/其他语言 Windows 上按名字匹配必然漏检。
fn dangerous_write_sid(sid: &str) -> Option<&'static str> {
    Some(match sid {
        "S-1-1-0" => "Everyone",
        "S-1-5-32-545" => "BUILTIN\\Users",
        "S-1-5-11" => "Authenticated Users",
        "S-1-5-4" => "INTERACTIVE",
        "S-1-5-7" => "ANONYMOUS LOGON",
        "S-1-5-32-546" => "Guests",
        "S-1-5-2" => "NETWORK",
        _ => return None,
    })
}

/// 解析目录的真实 DACL，校验三条不变量：
///
/// 1. DACL 不是 `NULL`（`NULL` DACL = 所有人完全控制，是最危险的形态）
/// 2. 宽泛主体（Everyone / Users / Authenticated Users …）**不得持有任何写权限**
/// 3. `SYSTEM` 必须持有写权限；且 `DACL` 中**不得残留继承 ACE**（说明 `/inheritance:r` 生效）
///
/// 为什么不用 `icacls` 文本：见 `winapi_raw.rs` 中 DACL 解析段的说明。
pub fn verify_dir_acl(dir: &std::path::Path) -> Result<(), AclError> {
    use core::ffi::c_void;
    use winapi_raw as w;

    let wide = to_wide(dir.to_string_lossy().as_ref());
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        w::GetNamedSecurityInfoW(
            wide.as_ptr(),
            w::SE_FILE_OBJECT,
            w::DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(AclError(format!(
            "GetNamedSecurityInfoW 失败（rc={rc}），无法校验目录权限"
        )));
    }
    // 安全描述符必须释放（无论后续成功失败）
    struct SdGuard(*mut c_void);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    winapi_raw::LocalFree(self.0);
                }
            }
        }
    }
    let _guard = SdGuard(sd);

    if sd.is_null() {
        return Err(AclError("安全描述符为空".into()));
    }
    if dacl.is_null() {
        return Err(AclError(
            "DACL 为 NULL：对象完全开放（等同于 Everyone 完全控制）".into(),
        ));
    }

    let mut info = w::ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    let ok = unsafe {
        w::GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<w::ACL_SIZE_INFORMATION>() as u32,
            w::ACL_SIZE_INFORMATION_CLASS,
        )
    };
    if ok == 0 {
        return Err(AclError("GetAclInformation 失败".into()));
    }

    let mut problems: Vec<String> = Vec::new();
    let mut system_writable = false;
    let mut inherited = 0usize;

    for i in 0..info.AceCount {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if unsafe { w::GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
            continue;
        }
        let p = ace as *const u8;
        // ACE_HEADER { AceType: u8, AceFlags: u8, AceSize: u16 }
        let ace_type = unsafe { *p };
        let ace_flags = unsafe { *p.add(1) };
        let ace_size = unsafe { *(p.add(2) as *const u16) } as usize;
        if ace_size < 8 || p.is_null() {
            continue;
        }
        if ace_flags & w::INHERITED_ACE_FLAG != 0 {
            inherited += 1;
        }
        // 只检查「允许」ACE：deny ACE 只会收紧权限
        if ace_type != w::ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        // ACCESS_ALLOWED_ACE: Header(4) + Mask(4) + SidStart...
        let mask = unsafe { *(p.add(4) as *const u32) };
        let sid_ptr = unsafe { p.add(8) } as *const c_void;
        let Some(sid) = sid_to_string(sid_ptr) else {
            // 解析不出 SID 时保守处理：无法确认就不宣称「已验证」
            problems.push(format!("第 {i} 条 ACE 的 SID 无法解析"));
            continue;
        };
        let writable = mask & WRITE_BITS != 0;
        if sid == LOCAL_SYSTEM_SID && writable {
            system_writable = true;
        }
        if writable {
            if let Some(name) = dangerous_write_sid(&sid) {
                problems.push(format!("{name} ({sid}) 持有写权限 mask=0x{mask:08X}"));
            }
        }
    }

    if inherited > 0 {
        problems.push(format!(
            "残留 {inherited} 条继承 ACE（/inheritance:r 未生效，父目录权限会继续传播）"
        ));
    }
    if !system_writable {
        problems.push("SYSTEM 缺少写权限（服务将无法写令牌/日志）".into());
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(AclError(problems.join("；")))
    }
}

/// PSID → 字符串 SID（"S-1-5-..."）
fn sid_to_string(psid: *const core::ffi::c_void) -> Option<String> {
    if psid.is_null() {
        return None;
    }
    let mut psz: *mut u16 = std::ptr::null_mut();
    if unsafe { winapi_raw::ConvertSidToStringSidW(psid, &mut psz) } == 0 || psz.is_null() {
        return None;
    }
    let mut len = 0usize;
    unsafe {
        while *psz.add(len) != 0 {
            len += 1;
        }
    }
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(psz, len) });
    unsafe {
        winapi_raw::LocalFree(psz as *mut core::ffi::c_void);
    }
    Some(s)
}

/// 用二进制 DACL 判断 SYSTEM 是否有写权限（供 set_dir_acl 的兜底自检使用）
fn dacl_has_system_write(dir: &std::path::Path) -> bool {
    // 复用 verify 的解析逻辑代价过高（失败即返回 Err），此处单独做一次轻量查询：
    // 只要能成功调用 verify 的一部分即可。为避免重复实现，这里直接调用 verify
    // 并检查错误信息里是否**只**缺 SYSTEM（不现实），故改为独立遍历。
    use core::ffi::c_void;
    use winapi_raw as w;

    let wide = to_wide(dir.to_string_lossy().as_ref());
    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        w::GetNamedSecurityInfoW(
            wide.as_ptr(),
            w::SE_FILE_OBJECT,
            w::DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 || sd.is_null() || dacl.is_null() {
        return false;
    }
    let mut info = w::ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    let ok = unsafe {
        w::GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<w::ACL_SIZE_INFORMATION>() as u32,
            w::ACL_SIZE_INFORMATION_CLASS,
        )
    };
    let mut found = false;
    if ok != 0 {
        for i in 0..info.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if unsafe { w::GetAce(dacl, i, &mut ace) } == 0 || ace.is_null() {
                continue;
            }
            let p = ace as *const u8;
            if unsafe { *p } != w::ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let mask = unsafe { *(p.add(4) as *const u32) };
            if mask & WRITE_BITS == 0 {
                continue;
            }
            if let Some(sid) = sid_to_string(unsafe { p.add(8) } as *const c_void) {
                if sid == LOCAL_SYSTEM_SID {
                    found = true;
                    break;
                }
            }
        }
    }
    unsafe {
        w::LocalFree(sd);
    }
    found
}

/// 确保令牌存在：不存在则生成并写盘、设置 ACL
///
/// ⚠ 令牌**已存在**时不能直接返回。
/// 若写成 `if tp.exists() { return Ok(...) }`，「ACL 在 install 早期窗口被写坏」
/// 之后将永远不被修复 —— 而令牌可读性正是整个中继的边界。已存在时也会
/// 用真实 DACL 复验目录权限，不安全即报错（fail closed）。
pub fn ensure_token() -> io::Result<String> {
    let paths = crate::config::Paths::resolve();
    let dir = paths.machine.clone();
    std::fs::create_dir_all(&dir)?;

    // 旧扁平布局（v1.1）→ 分层布局（v1.2）。幂等；失败则拒绝继续（fail closed），
    // 因为"迁移到一半"会让令牌位置与代码预期不一致。
    if crate::config::needs_layout_migration(&paths) {
        crate::config::migrate_layout(&paths).map_err(|e| {
            io::Error::other(
                format!("磁盘布局迁移失败，拒绝继续：{e}"),
            )
        })?;
    }
    // 秘密目录单独硬化（父目录已带 (OI)(CI) 传播，这里显式再确认一次）
    std::fs::create_dir_all(paths.secrets())?;

    let tp = token_path();
    if tp.exists() {
        verify_dir_acl(&dir).map_err(|e| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("数据目录权限不安全，拒绝继续：{e}"),
            )
        })?;
        return Ok(std::fs::read_to_string(&tp)?.trim().to_string());
    }
    // ⚠ 顺序：**先硬化目录 ACL，再写令牌**。
    // 若先写令牌再硬化目录，会有一个窗口：token.txt 已经在弱权限目录里存在过，
    // 期间任何本地用户都能读到这个等同 SYSTEM 凭据的文件。
    set_dir_acl(&dir).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("设置数据目录 ACL 失败（未写入令牌）：{e}"),
        )
    })?;
    // 秘密目录也必须通过同一套 DACL 校验（它继承父目录，但显式验证更可靠）
    verify_dir_acl(&paths.secrets()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("secrets 目录权限不安全（未写入令牌）：{e}"),
        )
    })?;
    let tok = gen_token();
    std::fs::write(&tp, &tok)?;
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
    std::fs::create_dir_all(&dir)?;
    let tok = gen_token();
    std::fs::write(token_path(), &tok)?;
    set_dir_acl(&dir)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))?;
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
///
/// ⚠ ACL 失败必须上报，不能静默吞错。
pub fn set_enabled(enabled: bool) -> Result<(), AclError> {
    let flag = enabled_flag_path();
    if enabled {
        std::fs::create_dir_all(data_dir())
            .map_err(|e| AclError(format!("创建数据目录失败: {e}")))?;
        if let Some(parent) = flag.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AclError(format!("创建状态目录失败: {e}")))?;
        }
        std::fs::write(&flag, b"1").map_err(|e| AclError(format!("写启用标志失败: {e}")))?;
    } else {
        match std::fs::remove_file(&flag) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AclError(format!("删除启用标志失败: {e}"))),
        }
    }
    // 确保目录 ACL（首次可能产生）
    set_dir_acl(&data_dir())
}

/// 审计脱敏：遮蔽命令行里常见的凭据参数。
///
/// 任务书 §8.2 要求「对可能包含秘密的命令参数进行脱敏」。审计日志是长期保留的，
/// 一条 `AI-Root run "mysql -u root -p Secret123"` 若不脱敏，等于把密码写进
/// 永久记录里。这里只做**保守遮蔽**（宁可多遮几个词，也不要漏）：
/// - `key=value` 形式：键名命中敏感词 → 值替换为 `***`
/// - 独立敏感开关（`-p` / `--password`）：其后的 token 替换为 `***`
///
/// 注意：这不是「检测所有秘密」的完备方案（无法识别未知格式），
/// 但能覆盖绝大多数常见命令行传参方式，且不改变命令语义（仅用于日志）。
pub fn redact(s: &str) -> String {
    const KEYS: &[&str] = &[
        "password",
        "passwd",
        "pwd",
        "token",
        "secret",
        "apikey",
        "api_key",
        "api-key",
        "accesskey",
        "access_key",
        "access-key",
        "credential",
        "privatekey",
        "private_key",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut mask_next = false;
    for tok in s.split_whitespace() {
        if mask_next {
            out.push("***".to_string());
            mask_next = false;
            continue;
        }
        // key=value
        if let Some(pos) = tok.find('=') {
            let key = tok[..pos].trim_start_matches('-').to_ascii_lowercase();
            if KEYS.iter().any(|k| key.contains(k)) {
                out.push(format!("{}=***", &tok[..pos]));
                continue;
            }
        }
        // 独立开关：-p / --password / token
        let bare = tok.trim_start_matches('-').to_ascii_lowercase();
        let is_switch = tok.starts_with('-')
            && (bare == "p" || KEYS.iter().any(|k| bare == *k));
        let is_bare_key = KEYS.iter().any(|k| bare == *k);
        if is_switch || is_bare_key {
            out.push(tok.to_string());
            mask_next = true;
            continue;
        }
        out.push(tok.to_string());
    }
    out.join(" ")
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
    append_with_rotate(exec_log_path(), line.as_bytes());
}

#[cfg(test)]
#[allow(clippy::assertions_on_constants)] // 常量契约断言是有意的（防常量被默默改动）
mod tests {
    use super::*;

    /// 审计日志必须逐条落盘，不能依赖「缓冲满了」或「进程正常退出」。
    ///
    /// 曾经 exec.log 用 16KB 批量 flush：个人使用每天几十条命令，文件上能看到的
    /// 是很久以前的内容；进程被强杀时缓冲里的条目全部丢失。
    #[test]
    fn audit_log_line_is_visible_without_explicit_flush() {
        let p = std::env::temp_dir().join(format!("airoot-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);

        let mut lf = LogFile::open(&p, true).expect("打开日志失败");
        lf.write_line(b"first line\n");
        // 刻意不调用 flush：模拟服务持续运行、尚未走到停止流程的状态
        let body = std::fs::read_to_string(&p).expect("读日志失败");
        assert_eq!(
            body, "first line\n",
            "写一行就应当已经落盘，实际读到 {body:?}"
        );

        lf.write_line(b"second line\n");
        let body2 = std::fs::read_to_string(&p).expect("读日志失败");
        assert!(
            body2.contains("second line"),
            "第二行也应立即可见，实际 {body2:?}"
        );

        drop(lf);
        let _ = std::fs::remove_file(&p);
    }

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
    /// 背景（实测发现的真实缺陷）：用 `/inheritance:e` + `/grant:r` 时，
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
        set_dir_acl(&child).expect("set_dir_acl 应成功");

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
        // icacls 显示的是**账户名**（如 `MACHINE\user`）而非 SID，故需先解析出账户名。
        // ⚠ 大小写不敏感比较：`whoami` 返回小写 `machine\user`，而 `icacls` 打印大写
        // `MACHINE\USER` —— Windows 账户名本身大小写不敏感，比较时必须归一化，
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

    /// 子目录必须被一起硬化（本机 install 失败的真实回归）。
    ///
    /// 复现条件：子目录在父目录硬化**之前**创建 → 它继承父目录当时的（弱）ACE，
    /// 并把这些 ACE 固化为自己的继承 ACE。`icacls /inheritance:r` 不递归，
    /// 若只硬化父目录，子目录里那几条 (I) ACE 仍然存在，
    /// `verify_dir_acl(secrets)` 随即拒绝写令牌，安装失败。
    /// 残留的过宽显式 ACE 必须被清掉。
    ///
    /// 场景：某次运行拿不到安装用户 SID，走了退化分支，显式写入 `INTERACTIVE:(R)`。
    /// 之后拿到 SID，`/inheritance:r` 删不掉这条显式 ACE，于是它长期留在 DACL 上。
    /// 多用户机器上，其他交互登录用户因此也能读到 token.txt。
    #[test]
    fn set_dir_acl_strips_stale_interactive_ace() {
        let base = std::env::temp_dir().join(format!("airoot-acl-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");

        // 人为制造「退化分支留下的残留」：显式授予 INTERACTIVE 只读
        let g = Command::new("icacls")
            .arg(&base)
            .arg("/grant:r")
            .arg("*S-1-5-4:(OI)(CI)R")
            .output()
            .expect("执行 icacls 失败");
        assert!(g.status.success(), "前提构造失败：无法写入 INTERACTIVE ACE");
        assert!(
            icacls_show(&base).contains("INTERACTIVE"),
            "前提不成立：DACL 里应当已经有 INTERACTIVE，否则本测试没有验证意义"
        );

        set_dir_acl(&base).expect("硬化应成功");

        assert!(
            !icacls_show(&base).contains("INTERACTIVE"),
            "残留的 INTERACTIVE 只读 ACE 必须被清除，实际 ACL:\n{}",
            icacls_show(&base)
        );
        // 清掉过宽主体之后，SYSTEM 与 Administrators 的完全控制不能被牵连
        verify_dir_acl(&base).expect("清除残留后仍应通过 ACL 验收");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 子目录硬化的回归测试
    #[test]
    fn set_dir_acl_hardens_preexisting_subdirs() {
        let base = std::env::temp_dir().join(format!("airoot-acl-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试根失败");

        // 先给父目录一条**可继承**的 ACE 作为「弱权限来源」（Users 只读，
        // 只读不会被判为危险写权限，但会被计入「继承 ACE」）。
        let _ = Command::new("icacls")
            .arg(&base)
            .arg("/grant")
            .arg("*S-1-5-32-545:(OI)(CI)R")
            .output();

        // 在父目录硬化之前创建两级子目录，使它们继承上面那条 ACE。
        let secret = base.join("secrets");
        let nested = secret.join("nested");
        std::fs::create_dir_all(&nested).expect("创建子目录失败");

        set_dir_acl(&base).expect("递归硬化应成功");

        verify_dir_acl(&base).expect("父目录不应残留继承 ACE");
        verify_dir_acl(&secret).expect("secrets 子目录必须被硬化");
        verify_dir_acl(&nested).expect("二级子目录必须被硬化");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `set_dir_acl` 必须幂等：重复调用不产生重复 ACE（防止服务反复启动时 ACE 膨胀）。
    #[test]
    fn set_dir_acl_is_idempotent() {
        let base = std::env::temp_dir().join(format!("airoot-acl-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");

        set_dir_acl(&base).expect("set_dir_acl 应成功");
        set_dir_acl(&base).expect("set_dir_acl 应成功");
        set_dir_acl(&base).expect("set_dir_acl 应成功");

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

    /// ⚠ 安全回归（线上故障）：`set_dir_acl` **绝不能以 SYSTEM 为参数**把
    /// SYSTEM 自己降级为只读。
    ///
    /// 故障链路（已在真实机器上复现并定位）：
    ///   1. 服务以 LocalSystem 运行，启动时 `run_pipe_server` 调用 `set_dir_acl(data_dir)`；
    ///   2. 若用 `get_user_sid()` 取「当前用户」→ 以 SYSTEM 身份运行时 `whoami /user`
    ///      返回 **`S-1-5-18`**（SYSTEM 自己），而非安装用户；
    ///   3. 于是「授予当前用户只读」那步变成 `grant_one("*S-1-5-18:(OI)(CI)R")`，
    ///      而 `:r` 是 replace 语义 → **覆盖掉刚授予的 SYSTEM:(F)**；
    ///   4. 结果：服务启动即自降权限，无法再写 token.txt / events.jsonl / exec.log，
    ///      且 `token.txt` 每次要重读 → 整个 SYSTEM 中继不可用。
    ///
    /// 本测试钉住两条不变量：
    ///   ① 单元级：`granted_user_sid()` **永不**返回 `LOCAL_SYSTEM_SID`；
    ///   ② 集成级：即使数据目录里存在被污染的 `sid.txt`（内容为 S-1-5-18），
    ///      `set_dir_acl` 之后目录 ACL 仍必须含 `SYSTEM:(F)`。
    ///
    /// 注意②必须走**真实的 `set_dir_acl`**（不是重抄实现），否则属伪测试。
    /// 本测试在测试进程（非 SYSTEM）里跑，`get_user_sid()` 正常返回真实用户，
    /// 所以断言②真正检验的是「`sid.txt` 被污染时不被采信」，而不是「SYSTEM 环境下不降级」。
    /// SYSTEM 分支由断言①（`select_grant_sid` 纯函数）覆盖 —— 必须如此，
    /// 因为测试进程永远不是 SYSTEM，无法伪造「当前身份 = S-1-5-18」这一环境。
    #[test]
    fn set_dir_acl_never_downgrades_system() {
        // ── ① 纯逻辑：SYSTEM 不得被选为「待授权用户」 ──
        // 变异验证：把 select_grant_sid 里 `sid != LOCAL_SYSTEM_SID` 去掉即可让本组断言 FAILED。
        assert_eq!(
            select_grant_sid(Some(LOCAL_SYSTEM_SID), Some(LOCAL_SYSTEM_SID)),
            None,
            "persisted 与 current 均为 SYSTEM 时必须返回 None（不得把 SYSTEM 当待授权用户）"
        );
        assert_eq!(
            select_grant_sid(Some(LOCAL_SYSTEM_SID), Some("S-1-5-21-1-2-3")),
            Some("S-1-5-21-1-2-3".to_string()),
            "sid.txt 被污染为 SYSTEM 时应回退到 current（真实用户）"
        );
        assert_eq!(
            select_grant_sid(None, Some(LOCAL_SYSTEM_SID)),
            None,
            "无 sid.txt 且当前身份为 SYSTEM → None（退化 Interactive 只读，而非降级 SYSTEM）"
        );
        assert_eq!(
            select_grant_sid(Some("not-a-sid"), Some(LOCAL_SYSTEM_SID)),
            None,
            "非法 persisted 值应被忽略，且不回落成 SYSTEM"
        );
        assert_eq!(
            select_grant_sid(Some("  S-1-5-21-9-9-9  "), None),
            Some("S-1-5-21-9-9-9".to_string()),
            "合法 persisted SID 应被采用（并去除空白）"
        );
        assert_eq!(LOCAL_SYSTEM_SID, "S-1-5-18");

        // ── ② 集成级：sid.txt 被污染成 SYSTEM 时，ACL 仍须保住 SYSTEM:(F) ──
        // 直接对 scratch 目录调用底层 grant，避免触碰真实 data_dir()。
        let base = std::env::temp_dir().join(format!("airoot-acl-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");
        let dir_str = base.to_string_lossy().to_string();

        // 构造「SYSTEM 被降为只读」的场景
        grant_one(&dir_str, &format!("*{LOCAL_SYSTEM_SID}:(OI)(CI)R"))
            .expect("人为降级 SYSTEM 的前提构造失败");
        let poisoned = icacls_show(&base);
        let sys_readonly = poisoned
            .lines()
            .any(|l| l.contains("SYSTEM") && l.contains("(R)") && !l.contains("(F)"));
        assert!(
            sys_readonly,
            "测试前提不成立：未能把 SYSTEM 人为降为只读，本测试失去意义。\n实际 ACL:\n{poisoned}"
        );

        // 调用真实被测函数：它必须用 granted_user_sid()（非 SYSTEM）并守住 SYSTEM:(F)
        set_dir_acl(&base).expect("set_dir_acl 应成功");

        let after = icacls_show(&base);
        let sys_full = after.lines().any(|l| l.contains("SYSTEM") && l.contains("(F)"));
        assert!(
            sys_full,
            "set_dir_acl 之后 SYSTEM 仍未获得完全控制 (F)。\
             服务以 LocalSystem 运行时将无法写 token.txt / events.jsonl / exec.log。\
             请确认：① granted_user_sid() 未返回 S-1-5-18；\
             ② /grant:r 的 replace 语义未覆盖 SYSTEM:(F)；③ acl_has_system_full 兜底生效。\n实际 ACL:\n{after}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // ============================================================
    // redact：审计脱敏 + escape/边界
    // ============================================================

    #[test]
    fn redact_masks_equals_form_secrets() {
        let out = redact("mysql -u root --password=Secret123 db");
        assert!(!out.contains("Secret123"), "密码必须被遮蔽: {out}");
        assert!(out.contains("--password=***"), "应保留键名: {out}");
    }

    #[test]
    fn redact_masks_switch_value_form() {
        // `-p` / `--token` 这类独立开关，其后的 token 才是秘密
        let out = redact("tool --token abcdef123456 run");
        assert!(!out.contains("abcdef123456"), "应遮蔽开关后的值: {out}");
        let out2 = redact("mysql -u root -p Hunter2");
        assert!(!out2.contains("Hunter2"), "应遮蔽 -p 后面的值: {out2}");
    }

    #[test]
    fn redact_keeps_harmless_commands_intact() {
        // 不能把普通命令改得面目全非（否则审计日志失去价值）
        for cmd in [
            "git status",
            "cargo test --release",
            r"dir C:\Windows",
            "echo hello world",
        ] {
            assert_eq!(redact(cmd), cmd, "普通命令不应被改动: {cmd}");
        }
    }

    #[test]
    fn redact_handles_api_key_variants() {
        for s in [
            "x --api-key=AAA111",
            "x API_KEY=BBB222",
            "x access_key=CCC333",
            "x secret=DDD444",
        ] {
            let out = redact(s);
            assert!(
                !out.contains("AAA111")
                    && !out.contains("BBB222")
                    && !out.contains("CCC333")
                    && !out.contains("DDD444"),
                "变体均应被遮蔽: {s} -> {out}"
            );
        }
    }

    // ============================================================
    // verify_dir_acl：真实 DACL 校验（回归）
    // ============================================================

    /// 正确配置后，校验必须通过 —— 证明校验器不是「一律报错」。
    #[test]
    fn verify_dir_acl_accepts_hardened_dir() {
        let base = std::env::temp_dir().join(format!("airoot-verify-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");

        set_dir_acl(&base).expect("set_dir_acl 应成功");
        verify_dir_acl(&base).expect("硬化后的目录应通过校验");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 危险主体（Everyone）持有写权限时，校验必须失败。
    /// 变异验证：把 dangerous_write_sid 里的 S-1-1-0 去掉，本测试会 FAILED。
    #[test]
    fn verify_dir_acl_rejects_everyone_write() {
        let base = std::env::temp_dir().join(format!("airoot-verify-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建测试目录失败");
        set_dir_acl(&base).expect("先硬化");

        // 人为放开 Everyone 写权限（模拟「ACL 被改坏」）
        let g = Command::new("icacls")
            .arg(base.to_string_lossy().to_string())
            .arg("/grant:r")
            .arg("*S-1-1-0:(OI)(CI)W")
            .output()
            .expect("icacls 执行失败");
        assert!(g.status.success(), "前提构造失败：无法授予 Everyone 写权限");

        let err = verify_dir_acl(&base).expect_err("Everyone 可写必须被检出");
        assert!(
            err.0.contains("Everyone") || err.0.contains("S-1-1-0"),
            "错误信息应指出责任主体，实际: {}",
            err.0
        );

        let _ = grant_one(&base.to_string_lossy(), "*S-1-1-0:(OI)(CI)R");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 残留继承 ACE 时校验必须失败（说明 /inheritance:r 没生效）。
    #[test]
    fn verify_dir_acl_detects_inherited_aces() {
        let base = std::env::temp_dir().join(format!("airoot-verify-inh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let child = base.join("data");
        std::fs::create_dir_all(&child).expect("创建测试目录失败");

        // 父目录授予可继承的 Users 写权限 → 子目录带 (I) 继承 ACE
        let g = Command::new("icacls")
            .arg(base.to_string_lossy().to_string())
            .arg("/grant")
            .arg("Users:(OI)(CI)(WD,AD)")
            .output()
            .expect("icacls 执行失败");
        assert!(g.status.success(), "前提构造失败");

        let err = verify_dir_acl(&child).expect_err("继承 ACE + Users 可写必须被检出");
        assert!(
            err.0.contains("继承") || err.0.contains("Users"),
            "应指出继承 ACE 或 Users 写权限，实际: {}",
            err.0
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// set_dir_acl 对不存在的目录必须返回 Err（而不是静默「设置成功」）。
    /// 这正是 fail-open 的形态：icacls 报错、函数却返回 ()。
    #[test]
    fn set_dir_acl_errors_on_missing_dir() {
        let missing = std::env::temp_dir().join(format!(
            "airoot-missing-{}-{}",
            std::process::id(),
            "nope"
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let r = set_dir_acl(&missing);
        assert!(r.is_err(), "目录不存在时 icacls 必然失败，必须返回 Err");
    }

    /// `ensure_token` 在令牌已存在但目录权限不安全时必须报错（V-16 回归）。
    #[test]
    fn ensure_token_refuses_when_dir_is_writable_by_everyone() {
        let dir = std::env::temp_dir().join(format!("airoot-tok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建测试目录失败");
        std::fs::write(dir.join("token.txt"), "a".repeat(64)).expect("写令牌失败");
        // 放开 Everyone 写权限，模拟被改坏的目录
        let g = Command::new("icacls")
            .arg(dir.to_string_lossy().to_string())
            .arg("/grant:r")
            .arg("*S-1-1-0:(OI)(CI)W")
            .output()
            .expect("icacls 执行失败");
        assert!(g.status.success());

        // 通过 AIROOT_DATA_DIR 让 data_dir() 指向该目录
        // ⚠ 进程级环境变量会影响其它并行测试，故用全局锁串行化
        static TOK_LOCK: Mutex<()> = Mutex::new(());
        let _g2 = TOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AIROOT_DATA_DIR").ok();
        std::env::set_var("AIROOT_DATA_DIR", &dir);

        let r = ensure_token();
        assert!(
            r.is_err(),
            "目录可被 Everyone 写入时，ensure_token 必须拒绝（不能沿用旧令牌）"
        );

        match prev {
            Some(v) => std::env::set_var("AIROOT_DATA_DIR", v),
            None => std::env::remove_var("AIROOT_DATA_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
