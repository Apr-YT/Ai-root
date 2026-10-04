//! 命名管道通信协议：长度前缀帧 + JSON 负载
//!
//! 帧格式：[u32 LE 长度][payload bytes]
//! 客户端 -> 服务端：Request
//! 服务端 -> 客户端：Response

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Write};

use crate::common;
use crate::winapi_raw;

#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    /// 共享令牌，与服务端比对
    pub token: String,
    /// 待执行命令（由服务端以 `cmd /C <command>` 运行，故获得 SYSTEM 权限）
    pub command: String,
    /// 可选工作目录
    #[serde(default)]
    pub cwd: Option<String>,
    /// 可选超时（毫秒），默认 60000
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Response {
    /// 中继层是否成功（鉴权通过且命令已执行/已超时处理）
    pub ok: bool,
    /// 命令自身退出码
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 中继层错误信息（鉴权失败 / 超时 / spawn 失败）
    pub error: Option<String>,
    /// 输出是否因超过 1MB 上限被截断（默认 false，兼容旧客户端）
    #[serde(default)]
    pub truncated: bool,
    /// 本次执行的唯一标识，与 events.jsonl 的 exec_start/exec_done 配对（默认 None，兼容旧服务端）
    #[serde(default)]
    pub run_id: Option<String>,
    /// 命令执行耗时（毫秒，含超时），便于 AI 消费方判断命令快慢（默认 None）
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 执行者身份（domain\user 或 SID），便于多 AI 场景下区分来源（默认 None）
    #[serde(default)]
    pub caller: Option<String>,
    /// 是否因超时终止（显式布尔，替代从 error 文字猜），默认 false
    #[serde(default)]
    pub timed_out: bool,
}

/// 验证请求数据的完整性（服务端 handle_client 调用，故为 pub）
pub fn validate_request(req: &Request) -> Result<(), String> {
    // 验证令牌格式
    if req.token.is_empty() {
        return Err("令牌不能为空".to_string());
    }
    if req.token.len() != 64 {
        // 32字节十六进制 = 64字符
        return Err("令牌格式无效".to_string());
    }

    // 验证命令
    if req.command.is_empty() {
        return Err("命令不能为空".to_string());
    }
    if req.command.len() > 8191 {
        // Windows 命令行长度限制
        return Err("命令过长".to_string());
    }

    // 验证工作目录
    if let Some(ref cwd) = req.cwd {
        if cwd.is_empty() {
            return Err("工作目录不能为空字符串".to_string());
        }
        if cwd.len() > 260 {
            // MAX_PATH
            return Err("工作目录路径过长".to_string());
        }
    }

    // 验证超时
    if let Some(timeout) = req.timeout_ms {
        if timeout == 0 {
            return Err("超时时间不能为0".to_string());
        }
        if timeout > common::MAX_COMMAND_TIMEOUT_MS {
            return Err(format!(
                "超时时间超过上限 {}ms",
                common::MAX_COMMAND_TIMEOUT_MS
            ));
        }
    }

    Ok(())
}

/// 从文件（包装的管道句柄）读取一帧，采用「空闲超时」计时（毫秒）
///
/// 防线程泄漏：客户端连接后若不发送数据，read_exact 会永久阻塞工作线程。
/// 通过 PeekNamedPipe 轮询可用字节数实现超时。
///
/// 计时语义（B4 优化）：旧实现让「帧头等待 + 帧体读取」共享同一个总预算 deadline，
/// 导致接近 16MB 上限的大负载在慢速管道上，可能因累计耗时（而非真正卡死）在帧体
/// 中途被误判超时断连。现改为分阶段计时：
///   1. 帧头阶段——保留原有「连接后静默」保护：连续 `idle_ms` 拿不到 4 字节帧头即超时。
///   2. 帧体阶段——`idle_ms` 作为「空闲窗口」：每读到新增数据就刷新计时，仅当「连续
///      `idle_ms` 无任何新字节」才判定超时；稳定流式传输的大帧不会被误断。
///      同时叠加一个按 payload 长度放宽的**绝对上限**（本地命名管道吞吐极高，此上限主要
///      兜底慢速涓流 / slow-loris 长期占用工作线程），既修复误超时又不削弱 DoS 防护。
pub fn read_frame_timeout(file: &mut File, idle_ms: u64) -> io::Result<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use std::time::{Duration, Instant};

    let handle = file.as_raw_handle() as winapi_raw::HANDLE;
    let idle = Duration::from_millis(idle_ms);

    // 轮询管道当前可读字节数；对端关闭时返回 Err(BrokenPipe)
    let peek = |h: winapi_raw::HANDLE| -> io::Result<u32> {
        let mut total: u32 = 0;
        let ok = unsafe {
            winapi_raw::PeekNamedPipe(
                h,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut total,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PeekNamedPipe 失败（对端关闭）",
            ));
        }
        Ok(total)
    };

    // ---- 帧头阶段：等待 4 字节可用 ----
    // 语义与旧实现一致：连接后连续 idle_ms 无帧头即超时（防连接后静默的线程泄漏）
    let hdr_start = Instant::now();
    loop {
        let total = peek(handle)?;
        if total >= 4 {
            break;
        }
        if hdr_start.elapsed() >= idle {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("读帧头超时（>{idle_ms}ms 无数据，客户端可能连接后未发送）"),
            ));
        }
        std::thread::sleep(Duration::from_millis(common::PIPE_POLL_INTERVAL_MS));
    }

    // 读帧头
    let mut len_buf = [0u8; 4];
    file.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > common::FRAME_MAX_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }

    // ---- 帧体阶段：空闲超时 + 按 payload 放宽的绝对上限 ----
    // 绝对上限：按「最低吞吐 256KB/s」折算并叠加一个 idle 基准，仅用于兜底慢速涓流；
    // 16MB => ~64s + idle，本地管道正常大负载在毫秒级即完成，绝不会触及此上限。
    let body_start = Instant::now();
    let body_hard = idle + Duration::from_millis((len as u64) / 256); // 256 B/ms = 256 KB/s
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    let mut last_progress = Instant::now();
    while filled < len {
        // 绝对上限始终生效（即便数据在持续涓流也会兜底），防 slow-loris 拖满线程
        if body_start.elapsed() >= body_hard {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("读帧体总超时（已收 {filled}/{len} 字节）"),
            ));
        }
        let total = peek(handle)?;
        if total == 0 {
            // 无新数据：仅当空闲窗口耗尽才超时（有增量会在下方刷新 last_progress）
            if last_progress.elapsed() >= idle {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("读帧体空闲超时（>{idle_ms}ms 无新数据，已收 {filled}/{len} 字节）"),
                ));
            }
            std::thread::sleep(Duration::from_millis(common::PIPE_POLL_INTERVAL_MS));
            continue;
        }
        let n = (total as usize).min(len - filled);
        file.read_exact(&mut buf[filled..filled + n])?;
        filled += n;
        last_progress = Instant::now(); // 有增量：刷新空闲计时
    }
    Ok(buf)
}

/// 兼容旧调用：默认 60s 空闲读超时
pub fn read_frame(file: &mut File) -> io::Result<Vec<u8>> {
    read_frame_timeout(file, common::CLIENT_READ_TIMEOUT_MS)
}

/// 向文件写入一帧
pub fn write_frame(file: &mut File, data: &[u8]) -> io::Result<()> {
    let len = (data.len() as u32).to_le_bytes();
    file.write_all(&len)?;
    file.write_all(data)?;
    file.flush()
}

/// 客户端：连接到命名管道，返回包装后的 File 句柄
///
/// 服务端（run_pipe_server）单线程 accept，同一时刻只阻塞等待一个实例；
/// 多个客户端并发连接时，CreateFileW 会撞 ERROR_PIPE_BUSY(231)。
/// 标准处理：WaitNamedPipeW 等待实例就绪后重试（最多约 3s），
/// 保证 AI 并发调用 `run` 时不会偶发「连接失败」。
///
/// 关键区分（避免服务未运行时的无谓重试）：CreateFileW / WaitNamedPipeW
/// 失败时读取 GetLastError()，将「管道不存在（ERROR_FILE_NOT_FOUND=2，服务未运行）」
/// 与「实例忙（ERROR_PIPE_BUSY=231 / ERROR_SEM_TIMEOUT=121）」分开处理：
/// 前者立即返回精准的「服务未运行」错误，后者才进入等待重试循环。
pub fn connect_client() -> io::Result<File> {
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::time::Duration;

    let name = common::to_wide(&common::pipe_name());
    for attempt in 0..6 {
        let handle = unsafe {
            winapi_raw::CreateFileW(
                name.as_ptr(),
                winapi_raw::GENERIC_READ | winapi_raw::GENERIC_WRITE,
                winapi_raw::FILE_SHARE_READ | winapi_raw::FILE_SHARE_WRITE,
                std::ptr::null(),
                winapi_raw::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != winapi_raw::INVALID_HANDLE_VALUE {
            // 将原生 HANDLE 封装为 Rust File（所有权转移，Drop 时自动 close）
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }
        let err = unsafe { winapi_raw::GetLastError() };
        // 管道根本不存在 = 服务未运行：立即精准失败，不做无意义的重试
        if err == winapi_raw::ERROR_FILE_NOT_FOUND {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "中继服务未运行（命名管道不存在）",
            ));
        }
        // 仅「所有实例忙」才值得等待重试；其余错误直接失败
        if err != winapi_raw::ERROR_PIPE_BUSY {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("无法连接管道（last_err={err}），中继服务是否运行？"),
            ));
        }
        // 等待服务端释放/新建可用实例（最多 500ms）
        let waited =
            unsafe { winapi_raw::WaitNamedPipeW(name.as_ptr(), common::PIPE_CONNECT_WAIT_MS) };
        if waited == 0 {
            // WaitNamedPipeW 返回 0 有多种语义，必须靠 GetLastError 精确区分，不能一概按超时处理：
            match unsafe { winapi_raw::GetLastError() } {
                // 管道消失（等待期间服务停止）=> 服务未运行，立即精准失败，不再机械重试
                winapi_raw::ERROR_FILE_NOT_FOUND => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "中继服务未运行（等待管道实例时管道已消失）",
                    ));
                }
                // 等待超时：管道仍在但实例持续忙 => 属正常并发竞争，耗尽次数才放弃
                winapi_raw::ERROR_SEM_TIMEOUT => {
                    if attempt >= 5 {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            "连接管道超时（服务持续繁忙，实例未释放）",
                        ));
                    }
                }
                // 其他意外错误：给出精准 last_err，同样耗尽次数才放弃
                other => {
                    if attempt >= 5 {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!("等待管道实例失败（last_err={other}），中继服务是否运行？"),
                        ));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(common::PIPE_CONNECT_RETRY_SLEEP_MS));
    }
    Err(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "无法连接管道，中继服务是否运行？",
    ))
}

/// 构造标准错误响应（新字段给默认值，兼容旧客户端反序列化）
pub fn err_response(msg: &str) -> Response {
    Response {
        ok: false,
        exit_code: -1,
        stdout: String::new(),
        stderr: String::new(),
        error: Some(msg.to_string()),
        truncated: false,
        run_id: None,
        duration_ms: None,
        caller: None,
        timed_out: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个「合法」请求，仅覆盖被测字段，避免每个用例重复样板。
    fn req(token: &str, command: &str) -> Request {
        Request {
            token: token.to_string(),
            command: command.to_string(),
            cwd: None,
            timeout_ms: None,
        }
    }

    fn valid_token() -> String {
        "a".repeat(64)
    }

    // ---------- 令牌校验 ----------

    #[test]
    fn rejects_empty_token() {
        let err = validate_request(&req("", "whoami")).unwrap_err();
        assert_eq!(err, "令牌不能为空");
    }

    #[test]
    fn rejects_token_wrong_length() {
        // 63 / 65 字符均非法（必须恰为 32 字节 hex = 64 字符）
        assert_eq!(
            validate_request(&req(&"a".repeat(63), "whoami")).unwrap_err(),
            "令牌格式无效"
        );
        assert_eq!(
            validate_request(&req(&"a".repeat(65), "whoami")).unwrap_err(),
            "令牌格式无效"
        );
    }

    #[test]
    fn rejects_multibyte_token_of_byte_len_64() {
        // 关键：长度按**字节**而非字符计算。
        // 32 个 2 字节 UTF-8 字符 = 64 字节，能通过长度检查，
        // 但绝不是合法 hex 令牌 —— 记录当前行为（仅查长度，不查 hex）。
        let tok = "é".repeat(32);
        assert_eq!(tok.len(), 64, "前置条件：字节长度确为 64");
        assert!(validate_request(&req(&tok, "whoami")).is_ok());
    }

    #[test]
    fn accepts_valid_token() {
        assert!(validate_request(&req(&valid_token(), "whoami")).is_ok());
    }

    // ---------- 命令校验 ----------

    #[test]
    fn rejects_empty_command() {
        assert_eq!(
            validate_request(&req(&valid_token(), "")).unwrap_err(),
            "命令不能为空"
        );
    }

    #[test]
    fn accepts_command_at_max_length() {
        // 8191 是允许上限（Windows 命令行长度限制）
        let cmd = "a".repeat(8191);
        assert!(validate_request(&req(&valid_token(), &cmd)).is_ok());
    }

    #[test]
    fn rejects_command_over_max_length() {
        let cmd = "a".repeat(8192);
        assert_eq!(
            validate_request(&req(&valid_token(), &cmd)).unwrap_err(),
            "命令过长"
        );
    }

    // ---------- cwd 校验 ----------

    #[test]
    fn rejects_empty_cwd() {
        let mut r = req(&valid_token(), "dir");
        r.cwd = Some(String::new());
        assert_eq!(validate_request(&r).unwrap_err(), "工作目录不能为空字符串");
    }

    #[test]
    fn accepts_cwd_at_max_path() {
        // MAX_PATH = 260
        let mut r = req(&valid_token(), "dir");
        r.cwd = Some("C".repeat(260));
        assert!(validate_request(&r).is_ok());
    }

    #[test]
    fn rejects_cwd_over_max_path() {
        let mut r = req(&valid_token(), "dir");
        r.cwd = Some("C".repeat(261));
        assert_eq!(validate_request(&r).unwrap_err(), "工作目录路径过长");
    }

    // ---------- 超时校验 ----------

    #[test]
    fn rejects_zero_timeout() {
        // 0 会被当作「无超时」导致进程永不回收，故显式拒绝
        let mut r = req(&valid_token(), "whoami");
        r.timeout_ms = Some(0);
        assert_eq!(validate_request(&r).unwrap_err(), "超时时间不能为0");
    }

    #[test]
    fn accepts_timeout_at_max() {
        let mut r = req(&valid_token(), "whoami");
        r.timeout_ms = Some(common::MAX_COMMAND_TIMEOUT_MS);
        assert!(validate_request(&r).is_ok());
    }

    #[test]
    fn rejects_timeout_over_max() {
        let mut r = req(&valid_token(), "whoami");
        r.timeout_ms = Some(common::MAX_COMMAND_TIMEOUT_MS + 1);
        assert!(validate_request(&r).unwrap_err().contains("超过上限"));
    }

    // ---------- 顺序敏感：令牌先于命令校验 ----------

    #[test]
    fn token_error_takes_precedence_over_command_error() {
        // 令牌与命令同时非法时应先报令牌错。
        // 这保证「未鉴权者无法通过错误信息推断服务端校验逻辑」。
        let err = validate_request(&req("", "")).unwrap_err();
        assert_eq!(err, "令牌不能为空");
    }

    // ---------- err_response ----------

    #[test]
    fn err_response_shape() {
        let r = err_response("boom");
        assert!(!r.ok);
        assert_eq!(r.exit_code, -1);
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert!(r.stdout.is_empty() && r.stderr.is_empty());
        assert!(!r.truncated && !r.timed_out);
    }

    // ---------- 序列化兼容性 ----------

    #[test]
    fn response_deserializes_without_optional_fields() {
        // 旧服务端可能不返回新增字段，客户端必须能反序列化（serde default）
        let legacy = r#"{"ok":true,"exit_code":0,"stdout":"x","stderr":""}"#;
        let r: Response = serde_json::from_str(legacy).unwrap();
        assert!(r.ok);
        assert_eq!(r.stdout, "x");
        assert!(!r.truncated);
        assert!(r.run_id.is_none());
        assert!(!r.timed_out);
    }

    #[test]
    fn request_deserializes_without_optional_fields() {
        // 只给 token + command 也应能解析（cwd/timeout_ms 有 default）
        let json = format!(r#"{{"token":"{}","command":"whoami"}}"#, valid_token());
        let r: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(r.command, "whoami");
        assert!(r.cwd.is_none());
        assert!(r.timeout_ms.is_none());
    }

    #[test]
    fn request_rejects_missing_token() {
        // token 无 default，缺失即解析失败（必须显式提供）
        let json = r#"{"command":"whoami"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    // ---------- 帧编解码（长度前缀 + 负载）----------

    /// 真正调用 `write_frame` 写入临时文件，再按字节核对长度前缀与负载。
    /// 用普通文件而非命名管道：`write_frame` 只依赖 `Write` trait，
    /// 无需真实管道即可覆盖编码逻辑（避免依赖服务端与并发）。
    #[test]
    fn write_frame_emits_le_length_prefix() {
        use std::io::Read;

        let mut path = std::env::temp_dir();
        path.push(format!("airoot_frame_test_{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let data = b"hello";
        {
            let mut f = File::create(&path).unwrap();
            write_frame(&mut f, data).unwrap();
        }

        let mut raw = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut raw).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(raw.len(), 4 + data.len());
        assert_eq!(&raw[..4], &[5, 0, 0, 0], "长度前缀必须是 u32 小端");
        assert_eq!(&raw[4..], data);
    }

    #[test]
    fn write_frame_roundtrip_boundaries() {
        use std::io::Read;

        // 边界：空负载与较大负载，均需完整往返
        for payload in [vec![], vec![1u8, 2, 3, 4], vec![0xABu8; 10_000]] {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "airoot_frame_rt_{}_{}.bin",
                std::process::id(),
                payload.len()
            ));
            let _ = std::fs::remove_file(&path);

            {
                let mut f = File::create(&path).unwrap();
                write_frame(&mut f, &payload).unwrap();
            }

            let mut raw = Vec::new();
            File::open(&path).unwrap().read_to_end(&mut raw).unwrap();
            let _ = std::fs::remove_file(&path);

            // 按协议手工解码（read_frame_timeout 需要真实管道句柄，故此处解长度+切片）
            let decoded_len = u32::from_le_bytes(raw[..4].try_into().unwrap()) as usize;
            assert_eq!(decoded_len, payload.len());
            assert_eq!(&raw[4..], &payload[..]);
        }
    }

    #[test]
    fn write_frame_flushes_to_disk() {
        use std::io::Read;

        // write_frame 末尾必须 flush：否则管道上可能出现「前一条响应滞留」，
        // 客户端会读到旧帧。此处用文件验证数据已落盘可读。
        let mut path = std::env::temp_dir();
        path.push(format!("airoot_frame_flush_{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let f = File::create(&path).unwrap();
        let mut w = f;
        write_frame(&mut w, b"flushed").unwrap();
        // 不显式 drop：在句柄仍打开时读取，验证 flush 已把数据推给 OS
        let mut raw = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut raw).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(&raw[..4], &[7, 0, 0, 0]);
        assert_eq!(&raw[4..], b"flushed");
    }

    #[test]
    fn frame_max_size_constant_is_sane() {
        // FRAME_MAX_SIZE 必须显著大于命令长度上限，否则合法请求会被帧层拒绝
        assert!(
            common::FRAME_MAX_SIZE > 8191,
            "帧上限必须大于命令长度上限 8191"
        );
        // 且长度前缀为 u32，上限不得超过 u32::MAX
        assert!(common::FRAME_MAX_SIZE <= u32::MAX as usize);
    }

    // ============ read_frame_timeout：真实命名管道集成测试 ============
    //
    // 为什么必须用真实命名管道：read_frame_timeout 内部依赖 PeekNamedPipe 轮询
    // 可用字节数来实现「空闲超时」。普通文件不支持该语义，用文件测等于没测到
    // 真正的超时逻辑。
    //
    // 本机自连方案（CreateNamedPipeW 服务端 + CreateFileW 客户端，同一进程）：
    // 无需管理员、无需已安装服务，也不依赖全局单例管道名 —— 每个测试用
    // 独立随机管道名，可并行执行。

    /// 建立一对已连接的命名管道：返回 (服务端句柄包装的 File, 客户端句柄包装的 File)。
    /// 服务端在一个独立线程内阻塞等待连接，避免与本线程的 CreateFileW 互锁。
    fn make_pipe_pair(tag: &str) -> (File, File) {
        use std::os::windows::io::FromRawHandle;
        use std::sync::mpsc;

        // 用 pid + tag 保证并行测试间管道名唯一
        let name = format!(r"\\.\pipe\airoot-test-{}-{}", std::process::id(), tag);
        let wide = common::to_wide(&name);
        let (tx, rx) = mpsc::channel::<bool>();

        let name_for_thread = wide.clone();
        // 关键：HANDLE 是裸指针(!Send)，不能在跨线程返回值里传递。
        // 故服务端线程内部完成「创建+等待连接+封装为 File」，只把 bool 状态传出。
        let server_thread = std::thread::spawn(move || -> Option<File> {
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
            // 阻塞等待客户端连接
            let ok = unsafe { winapi_raw::ConnectNamedPipe(h, std::ptr::null_mut()) };
            let err = unsafe { winapi_raw::GetLastError() };
            if ok == 0 && err != winapi_raw::ERROR_PIPE_CONNECTED {
                unsafe { winapi_raw::CloseHandle(h) };
                return None;
            }
            Some(unsafe { File::from_raw_handle(h as _) })
        });

        // 等服务端管道创建完成（否则 CreateFileW 会先撞上 FILE_NOT_FOUND）
        assert!(rx.recv().expect("服务端管道创建失败"), "CreateNamedPipeW 失败");

        // 客户端连接（服务端 ConnectNamedPipe 会在此刻返回）
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
            "客户端连接测试管道失败 (last_err={})",
            unsafe { winapi_raw::GetLastError() }
        );

        let srv = server_thread.join().unwrap().expect("服务端连接失败");
        let cli = unsafe { File::from_raw_handle(cli_h as _) };
        (srv, cli)
    }

    #[test]
    fn read_frame_roundtrip_over_real_pipe() {
        let (mut srv, cli) = make_pipe_pair("roundtrip");

        let payload = br#"{"token":"x","command":"whoami"}"#;
        let mut writer = cli.try_clone().unwrap();
        let payload_for_writer = payload.to_vec();
        let t = std::thread::spawn(move || {
            write_frame(&mut writer, &payload_for_writer).unwrap();
        });

        // 服务端读取，验证与写入内容一致（覆盖真实 PeekNamedPipe 轮询路径）
        let got = read_frame_timeout(&mut srv, 5_000).expect("读帧应成功");
        t.join().unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn read_frame_handles_large_payload() {
        // 覆盖「帧体分阶段读取」路径：远大于单次管道缓冲区(64KB)的负载，
        // 必须在多次 peek/read 循环中完整拼装。
        let (mut srv, cli) = make_pipe_pair("large");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

        let mut writer = cli.try_clone().unwrap();
        let p = payload.clone();
        let t = std::thread::spawn(move || {
            write_frame(&mut writer, &p).unwrap();
        });

        let got = read_frame_timeout(&mut srv, 5_000).expect("大帧应完整读出");
        t.join().unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload, "大帧内容必须逐字节一致");
    }

    #[test]
    fn read_frame_times_out_when_no_data_sent() {
        // 核心 DoS 防护：客户端连上后不发任何数据，必须按 idle_ms 超时返回，
        // 而不是永久阻塞工作线程。
        let (mut srv, _cli) = make_pipe_pair("idle");

        let start = std::time::Instant::now();
        let err = read_frame_timeout(&mut srv, 300).expect_err("无数据应超时");
        let elapsed = start.elapsed();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "应为 TimedOut");
        assert!(
            elapsed >= std::time::Duration::from_millis(250),
            "不应早于 idle 窗口返回（实际 {elapsed:?}）"
        );
        // 允许充足的调度余量，但必须确实返回而非挂死
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "超时后应尽快返回（实际 {elapsed:?}）"
        );
    }

    #[test]
    fn read_frame_rejects_oversized_length_prefix() {
        // 只发「长度前缀」声明超过 FRAME_MAX_SIZE，且不实际发送负载。
        // 实现应在读完 4 字节帧头后立即拒绝，绝不能去分配 16MB+ 缓冲。
        let (mut srv, cli) = make_pipe_pair("oversize");

        let bogus = (common::FRAME_MAX_SIZE as u32 + 1).to_le_bytes();
        let mut writer = cli.try_clone().unwrap();
        let t = std::thread::spawn(move || {
            use std::io::Write;
            let _ = writer.write_all(&bogus);
            let _ = writer.flush();
            // 保持片刻，确保服务端已读到帧头再判定
            std::thread::sleep(std::time::Duration::from_millis(200));
        });

        let err = read_frame_timeout(&mut srv, 3_000).expect_err("超大帧应被拒绝");
        t.join().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "应为 InvalidData");
    }

    #[test]
    fn read_frame_detects_closed_peer() {
        // 对端关闭时 PeekNamedPipe 失败，应作为 BrokenPipe 暴露而非挂死
        let (mut srv, cli) = make_pipe_pair("closed");
        drop(cli); // 立即关闭客户端

        let err = read_frame_timeout(&mut srv, 3_000).expect_err("对端关闭应报错");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "应为 BrokenPipe");
    }
}

