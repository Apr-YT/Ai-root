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

/// 脚本模式：将多行命令以 `cmd /C` **直接传参**执行（首选，中文 100% 可靠），
/// 仅对含括号块或超长脚本回退到文件方案。
///
/// 首选方案（已实测，Python subprocess 验证）：
/// 把多行命令用 ` & ` 连接成单行，经 CreateProcessW（UTF-16）直接交给 cmd——
/// **不落盘、不读文件**。cmd 读 `.cmd` 文件时代码页继承父进程且不稳定（UTF-8/GBK
/// 反复翻转，包装脚本 + chcp 也只能做到「大概率对」）是历史死结；而命令行以 UTF-16
/// 传递，中文命令解析 100% 正确。输出编码由 `decode_output`（UTF-8 优先→GBK 回退）兜底。
///
/// 注意（脚本作者须知，README 已写明）：
/// - `&`/`&&`/`|`/`()` 均为 cmd 正常语义（不转义）；同一行内 `set x=1 & echo %x%`
///   的 `%x%` 在解析期展开为空，需写 `& call echo %x%`；
/// - 含 `(`/`)` 的括号块（`if (...)` / `for ... do (...)`）无法拼接为单行，自动回退文件方案；
/// - 拼接后长度超过 6000 字符回退文件方案（cmd 命令行上限 8191）。
fn run_script(script: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    let norm = script.replace("\r\n", "\n");
    let has_paren = norm.contains('(') || norm.contains(')');
    let joined = norm
        .split('\n')
        .map(|l| l.trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" & ");

    if !has_paren && !joined.is_empty() && joined.len() <= 6000 {
        let mut builder = Command::new("cmd");
        // raw_arg 原样传参、不 escape_cmd_meta：脚本模式中 `&` 是命令分隔符（cmd 原生语义），
        // 中文经 UTF-16 命令行传递无乱码
        builder.raw_arg("/C").raw_arg(&joined);
        builder.stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(c) = cwd {
            builder.current_dir(c);
        }
        return run_child(builder, timeout_ms);
    }

    // 回退：文件方案（括号块 / 超长脚本；中文可靠性与主路径不同，属尽力而为）
    run_script_file(script, cwd, timeout_ms)
}

/// 文件方案（回退）：本体 UTF-8 写入临时 `.cmd`，经「ASCII 包装脚本」以 `cmd /C <wrap>` 执行。
///
/// 双文件结构（实测验证）：
/// - 本体 `ai-root-<pid>-<n>.cmd`：脚本内容，UTF-8 + CRLF（见 `encode_script`）。
/// - 包装 `ai-root-<pid>-<n>.wrap.cmd`：**纯 ASCII**，内容为
///   `@echo off` / `chcp 65001 >nul` / `call "<本体路径>"`。
/// 原理：cmd 顶层批处理存在「预读缓冲」怪癖——`chcp` 生效前的行已按旧代码页读入，
/// 导致 UTF-8 中文在某些行乱码；且 `cmd /C "chcp & call \"path\""` 单行形式会因
/// `/C` 引号剥离把 `call` 参数拆坏（实测失败）。而包装脚本全 ASCII（顶层读取无中文问题），
/// 本体在 `chcp 65001` 生效后经 `call` 才被读取 → UTF-8 中文在多数情况下可正确解析。
fn run_script_file(script: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let tmp = std::env::temp_dir().join(format!("ai-root-{}-{}.cmd", pid, nanos));
    let wrap = std::env::temp_dir().join(format!("ai-root-{}-{}.wrap.cmd", pid, nanos));

    // 本体：脚本内容（UTF-8 + CRLF）
    let bytes = encode_script(script);
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return ExecResult::error(format!("写临时脚本失败: {e}"));
    }
    // 包装：纯 ASCII；@echo off 抑制回显，chcp 65001 先切换代码页，再 call 本体
    let inner = tmp.to_string_lossy().to_string();
    let wrapper = format!("@echo off\r\nchcp 65001 >nul\r\ncall \"{inner}\"\r\n");
    if let Err(e) = std::fs::write(&wrap, wrapper.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return ExecResult::error(format!("写包装脚本失败: {e}"));
    }
    let wrap_path = wrap.to_string_lossy().to_string();

    let mut builder = Command::new("cmd");
    builder.raw_arg("/C").raw_arg(format!("\"{wrap_path}\""));
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }
    let res = run_child(builder, timeout_ms);
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&wrap);
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

/// 将脚本内容编码为字节，供写入临时 `.cmd` 文件。
///
/// 方案：写**纯 UTF-8（无 BOM）+ CRLF 换行**，不注入 chcp、不依赖系统代码页探测。
/// 实测结论（本机 ACP=65001，即开启「使用 Unicode UTF-8 提供全球语言支持」）：
/// - cmd 原生以 UTF-8 读取 .cmd 文件 → 纯 UTF-8 脚本的中文命令正确解析、stdout 亦为 UTF-8；
/// - GBK 编码脚本反而乱码（cmd 按 UTF-8 读 GBK 字节）；
/// - `@chcp 65001` 前置会触发 cmd 批处理「预读缓冲」怪癖，部分行被按旧代码页读入而乱码；
/// - UTF-8 BOM 在部分 cmd 版本下会被当 GBK 读成「锘」前缀，同样不可靠。
/// 统一 CRLF 换行，避免 cmd 对 LF 的兼容性问题。
fn encode_script(s: &str) -> Vec<u8> {
    let normalized: String = s.replace("\r\n", "\n").replace('\n', "\r\n");
    normalized.into_bytes()
}
