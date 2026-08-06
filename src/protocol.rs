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
}

/// 从文件（包装的管道句柄）读取一帧
pub fn read_frame(file: &mut File) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    file.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
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
        if err != winapi_raw::ERROR_PIPE_BUSY {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("无法连接管道（last_err={err}），中继服务是否运行？"),
            ));
        }
        // 等待服务端创建可用实例；管道已消失则快速失败
        let waited = unsafe { winapi_raw::WaitNamedPipeW(name.as_ptr(), 500) };
        if waited == 0 && attempt >= 5 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "连接管道超时（持续忙），中继服务是否运行？",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "无法连接管道，中继服务是否运行？",
    ))
}

/// 构造标准错误响应
pub fn err_response(msg: &str) -> Response {
    Response {
        ok: false,
        exit_code: -1,
        stdout: String::new(),
        stderr: String::new(),
        error: Some(msg.to_string()),
    }
}
