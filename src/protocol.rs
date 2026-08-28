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

/// 验证请求数据的完整性
fn validate_request(req: &Request) -> Result<(), String> {
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
