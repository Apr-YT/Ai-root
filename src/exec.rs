//! 命令执行引擎
//!
//! 以 `cmd /C <command>` 方式运行命令。由于服务端以 LocalSystem 运行，
//! 因此执行结果天然具备 SYSTEM 最高权限。
//! 通过 try_wait 轮询 + 超时杀进程实现超时控制，避免跨线程移动 Child 所有权。
//!
//! 两种执行模式：
//! - 单行命令：经 `escape_cmd_meta` 把 `^`/`&` 转义为字面量，避免 cmd /C 二次解析
//!   把设备实例 ID 等含 `&` 的字面量拆成多段命令。代价：`&`/`&&` 串联在单行内失效。
//! - 多行脚本（命令含换行 `\n`）：写入临时 `.cmd` 文件并以 `cmd /C <file>` 执行，
//!   **不转义**，`&`/`&&`/`|`/`()` 均按正常 cmd 语义工作。这是 AI 批量自治多命令时的推荐路径。

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub error: Option<String>,
}

impl ExecResult {
    fn error(msg: String) -> Self {
        ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            error: Some(msg),
        }
    }
}

/// 执行命令，带超时（毫秒）。超时则终止进程。
pub fn run_command(command: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    // 多行命令（含换行）走脚本模式：写入临时 .cmd，不转义 cmd 元字符，
    // 使 `&`/`&&`/`|`/`()` 作为正常命令分隔符/运算符工作（引号内的 `&` 仍按字面处理）。
    if command.contains('\n') {
        return run_script(command, cwd, timeout_ms);
    }

    let mut builder = Command::new("cmd");
    // 关键：用 raw_arg 原样拼接命令，禁止 Rust 对引号/`&`/结尾反斜杠二次转义。
    // 否则 `cmd /C` 会把内层引号转义成 `\"` 且无法还原，导致 `dir "C:\x"`、
    // 结尾反斜杠的 `dir C:\`、以及 pnputil 的 `"HID\...\&COL0x\..."` 等命令解析失败。
    // 进一步：`cmd /C <command>` 会对命令行**二次解析**，把 `&` 误判为命令分隔符，
    // 使含 `&` 的设备实例 ID（`VID_373B&PID_1216&MI_00&COL0x`）被拆成多段、
    // 只有首段送达 pnputil。故在交给 cmd 前对 `^`/`&` 做 `^` 转义为字面量。
    // 代价：`^`/`&` 均被转义为字面量，故单行内 `&` 与 `&&` 串联**全部失效**（退化为字面 `&`/`&&`）；
    // 仅 `|`/`<`/`>`/`()` 运算符语义保留。需要 `&`/`&&` 串联请改用多行脚本模式（命令含 `\n`、
    // 或 `ai-root run --file <脚本>` / `ai-root run --stdin`）。
    let escaped = escape_cmd_meta(command);
    builder.raw_arg("/C").raw_arg(&escaped);
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }

    run_child(builder, timeout_ms)
}

/// 脚本模式：将多行命令写入临时 `.cmd`（GBK 编码），以 `cmd /C "<file>"` 执行，不转义。
fn run_script(script: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("ai-root-{}-{}.cmd", std::process::id(), nanos));
    // 以 GBK（系统 OEM/ANSI 代码页）编码写入，确保 cmd 正确读取含中文的脚本
    let bytes = encode_gbk(script);
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return ExecResult::error(format!("写临时脚本失败: {e}"));
    }
    let path = tmp.to_string_lossy().to_string();
    let mut builder = Command::new("cmd");
    // 文件路径用引号包裹，兼容含空格的路径；此处不调用 escape_cmd_meta
    builder.raw_arg("/C").raw_arg(format!("\"{path}\""));
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }
    let res = run_child(builder, timeout_ms);
    let _ = std::fs::remove_file(&tmp);
    res
}

/// spawn 子进程、读取 stdout/stderr、轮询退出状态并在超时时杀进程
fn run_child(mut builder: Command, timeout_ms: u64) -> ExecResult {
    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecResult::error(format!("spawn 失败: {e}"));
        }
    };

    // 取出 stdout/stderr 管道，交给读取线程
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_t = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        decode_output(&buf)
    });
    let err_t = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        decode_output(&buf)
    });

    // 轮询退出状态，超时则杀进程
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed().as_millis() as u64 >= timeout_ms {
                    let _ = child.kill();
                    timed_out = true;
                    match child.wait() {
                        Ok(s) => break s,
                        Err(e) => {
                            return ExecResult::error(format!("wait 失败: {e}"));
                        }
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return ExecResult::error(format!("try_wait 失败: {e}"));
            }
        }
    };

    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    ExecResult {
        exit_code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        timed_out,
        error: None,
    }
}

/// 对传入 `cmd /C` 的单行命令做 cmd.exe 元字符转义。
///
/// `cmd /C <command>` 会二次解析命令行，把 `&` 当作命令分隔符
/// （如设备实例 ID `VID_373B&PID_1216&MI_00&COL0x` 中的 `&` 会被拆段）。
/// 在 `^`/`&` 前加 `^` 转义为字面量，确保整条命令原样送达目标程序。
/// 注意：`^` 必须先于 `&` 处理，否则 `^` 自身也会被误转义。
/// 仅在单行模式下调用；多行脚本模式不转义（见 `run_script`）。
fn escape_cmd_meta(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + 8);
    for c in cmd.chars() {
        match c {
            '^' | '&' => {
                out.push('^');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// 将命令输出的原始字节还原为 Rust String。
///
/// 同一 `cmd /C` 进程内输出编码并不统一：
/// - `dir`/`ver` 等内建命令按系统 OEM 代码页（中文 Windows = GBK 936）回显；
/// - `echo 中文` 则原样回显命令行字面量（经 CreateProcessW 以 UTF-16 传入后转回 OEM，或保留 UTF-8）。
/// 单一代码页无法同时正确解码两者。故采用双解码策略：
///   1) 先严格按 UTF-8 解码（覆盖 UTF-8 发射器及全部 ASCII 内容）；
///   2) 失败再回退到系统 OEM 代码页（GBK）解码。
/// 两种情形都能正确还原；ASCII 命令（pnputil/whoami/sc/ver）完全不受影响。
fn decode_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // 1) 严格 UTF-8：字节合法则直接采用（避免将合法 UTF-8 误当 GBK 解码产生乱码）
    if let Ok(s) = String::from_utf8(bytes.to_vec()) {
        return s;
    }
    // 2) 回退到 OEM 代码页（GBK），覆盖 cmd 内建命令以 GBK 回显的输出
    decode_codepage(bytes, crate::winapi_raw::CP_OEMCP)
}

/// 按指定代码页把多字节原始字节解码为 UTF-16 → Rust String（lossy 兜底）。
fn decode_codepage(bytes: &[u8], codepage: u32) -> String {
    unsafe {
        let needed = crate::winapi_raw::MultiByteToWideChar(
            codepage,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if needed <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; needed as usize];
        let written = crate::winapi_raw::MultiByteToWideChar(
            codepage,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            needed,
        );
        if written <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide[..written as usize])
    }
}

/// 将脚本内容以系统 OEM/ANSI 代码页（GBK）编码为字节，供写入临时 `.cmd` 文件。
/// 使 cmd.exe 以系统代码页正确读取脚本（含中文不乱码）。
fn encode_gbk(s: &str) -> Vec<u8> {
    let wide: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        let needed = crate::winapi_raw::WideCharToMultiByte(
            crate::winapi_raw::CP_OEMCP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if needed <= 0 {
            return s.as_bytes().to_vec();
        }
        let mut out = vec![0u8; needed as usize];
        let written = crate::winapi_raw::WideCharToMultiByte(
            crate::winapi_raw::CP_OEMCP,
            0,
            wide.as_ptr(),
            wide.len() as i32,
            out.as_mut_ptr(),
            needed,
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if written <= 0 {
            return s.as_bytes().to_vec();
        }
        out.truncate(written as usize);
        out
    }
}
