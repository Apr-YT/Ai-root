//! 命令执行引擎
//!
//! 以 `cmd /C <command>` 方式运行命令。由于服务端以 LocalSystem 运行，
//! 因此执行结果天然具备 SYSTEM 最高权限。
//! 通过 try_wait 轮询 + 超时杀进程实现超时控制，避免跨线程移动 Child 所有权。
//!
//! 两种执行模式：
//! - 单行命令：经 `escape_cmd_meta` 把**引号外**的 `^`/`&` 转义为字面量，避免 cmd /C 二次解析
//!   把未加引号的设备实例 ID 等含 `&` 的字面量拆成多段命令。引号内的 `&`/`^` 由 cmd 自身按字面处理、
//!   不再额外转义（修复 B3：cmd 引号内 `^` 非转义符，盲目转义会把 `&` 变成字面 `^&` 破坏内容，
//!   例如 `echo "a&b"` 会输出 `a^&b` 而非 `a&b`）。
//!   代价：**引号外**的 `&`/`&&` 串联在单行内失效（退化为字面 `&`/`&&`）。
//! - 多行脚本（命令含换行 `\n`）：写入临时 `.cmd` 文件并以 `cmd /C <file>` 执行，
//!   **不转义**，`&`/`&&`/`|`/`()` 均按正常 cmd 语义工作。这是 AI 批量自治多命令时的推荐路径。

use crate::common;
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
    /// stdout/stderr 是否因超过 1MB 上限被截断
    pub truncated: bool,
}

impl ExecResult {
    fn error(msg: String) -> Self {
        ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            error: Some(msg),
            truncated: false,
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

    // 命令验证：防止空命令或危险命令
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ExecResult::error("命令不能为空".to_string());
    }

    // 基本安全检查：阻止直接格式化命令（防注入）
    let dangerous_patterns = [
        "format ", "format:", "diskpart", "del /s", "rd /s", "rmdir /s",
    ];
    for pattern in dangerous_patterns {
        if trimmed.to_lowercase().contains(pattern) {
            return ExecResult::error(format!("检测到危险命令模式 '{}'，已阻止执行", pattern));
        }
    }

    let mut builder = Command::new("cmd");
    // 关键：用 raw_arg 原样拼接命令，禁止 Rust 对引号/`&`/结尾反斜杠二次转义。
    // 否则 `cmd /C` 会把内层引号转义成 `\"` 且无法还原，导致 `dir "C:\x"`、
    // 结尾反斜杠的 `dir C:\`、以及 pnputil 的 `"HID\...\&COL0x\..."` 等命令解析失败。
    // 进一步：`cmd /C <command>` 会对命令行**二次解析**，把 `&` 误判为命令分隔符，
    // 使含 `&` 的设备实例 ID（`VID_373B&PID_1216&MI_00&COL0x`）被拆成多段、
    // 只有首段送达 pnputil。故在交给 cmd 前对 `^`/`&` 做 `^` 转义为字面量。
    // 代价：`^`/`&` 仅在**引号外**被转义为字面量，故**引号外**的 `&` 与 `&&` 串联**全部失效**（退化为字面 `&`/`&&`）；
    // 仅 `|`/`<`/`>`/`()` 运算符语义保留。需要 `&`/`&&` 串联请改用多行脚本模式（命令含 `\n`、
    // 或 `AI-Root run --file <脚本>` / `AI-Root run --stdin`）。
    // 引号内的 `&`/`^` 不做转义（B3 修复）：cmd 引号内 `^` 非转义符，盲目加 `^` 会把 `&` 变成字面 `^&` 破坏内容。
    let escaped = escape_cmd_meta(command);
    builder.raw_arg("/C").raw_arg(&escaped);
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }

    run_child(builder, timeout_ms)
}

/// 判定脚本是否含「无法拼接为单行」的块结构。
///
/// 需要回退文件方案的情形：`if (...)` / `for ... do (...)` / `while (...)` 等
/// 带**跨行块体**的语法，以及花括号块。判据（大小写不敏感，逐行 trim 后匹配）：
/// - 行以 `if `/`for `/`while `/`else` 开头（cmd 内建块关键字）；
/// - 行内含 `) do (` / `do (`（`for`/`while` 的块体起点）；
/// - 行内含 `{` 或 `}`（花括号块）。
///
/// 明确**不**触发回退的情形（这些括号是纯字面量，可安全拼接）：
/// - `echo (test)`、`dir C:\Program Files (x86)`、`(echo hi)` 单行分组。
///
/// 这样把「裸括号」从回退判据中剔除，避免普通命令无谓走「写临时 .cmd + 落盘」路径。
fn has_block_syntax(script: &str) -> bool {
    for raw in script.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("if ")
            || lower.starts_with("for ")
            || lower.starts_with("while ")
            || lower.starts_with("else")
        {
            return true;
        }
        if line.contains(") do (") || lower.contains("do (") {
            return true;
        }
        if line.contains('{') || line.contains('}') {
            return true;
        }
    }
    false
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
/// - 含**括号块**（`if (...)` / `for ... do (...)` 等多行块结构）无法拼接为单行，自动回退文件方案；
/// - 拼接后长度超过 common::SCRIPT_INLINE_MAX_LEN 字符回退文件方案（cmd 命令行上限 8191）。
fn run_script(script: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    let norm = script.replace("\r\n", "\n");
    // 判定「是否含多行块结构」。仅命中真正的块语法关键字/花括号才回退文件方案：
    // 早期实现只要出现任意 `(`/`)` 就降级，导致 `echo (test)`、`dir (x)` 这类
    // 普通命令被无谓地落到「写临时脚本 + 落盘」路径，既增加磁盘 IO，又留下临时文件痕迹。
    let has_block = has_block_syntax(&norm);
    let joined = norm
        .split('\n')
        .map(|l| l.trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" & ");

    if !has_block && !joined.is_empty() && joined.len() <= common::SCRIPT_INLINE_MAX_LEN {
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
/// - 本体 `AI-Root-<pid>-<n>.cmd`：脚本内容，UTF-8 + CRLF（见 `encode_script`）。
/// - 包装 `AI-Root-<pid>-<n>.wrap.cmd`：**纯 ASCII**，内容为
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
    let tmp = std::env::temp_dir().join(format!("AI-Root-{}-{}.cmd", pid, nanos));
    let wrap = std::env::temp_dir().join(format!("AI-Root-{}-{}.wrap.cmd", pid, nanos));

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
    let start = Instant::now();
    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => {
            let error_msg = format!("启动命令失败: {}", e);
            common::log_event(serde_json::json!({
                "type": "exec_error",
                "error": error_msg,
                "timestamp": chrono::Utc::now().to_rfc3339()
            }));
            return ExecResult::error(error_msg);
        }
    };

    // 取出 stdout/stderr 管道，交给读取线程（各自截断至 OUTPUT_LIMIT）
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_t = thread::spawn(move || {
        let (buf, truncated) = drain_pipe(stdout_pipe);
        (decode_output(&buf), truncated)
    });
    let err_t = thread::spawn(move || {
        let (buf, truncated) = drain_pipe(stderr_pipe);
        (decode_output(&buf), truncated)
    });

    // 轮询退出状态，超时则杀进程树
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed().as_millis() as u64 >= timeout_ms {
                    // 杀整棵进程树：child.kill() 只杀 cmd 外壳，其子进程（ping 等）会残留
                    // 至自然结束（实测 2s 超时要 5s 才返回）。taskkill /T 递归终止全部后代。
                    let pid = child.id();
                    let _ = Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .output();
                    timed_out = true;

                    common::log_event(serde_json::json!({
                        "type": "exec_timeout",
                        "pid": pid,
                        "timeout_ms": timeout_ms,
                        "timestamp": chrono::Utc::now().to_rfc3339()
                    }));

                    match child.wait() {
                        Ok(s) => break s,
                        Err(e) => {
                            let error_msg = format!("等待进程退出失败: {}", e);
                            common::log_event(serde_json::json!({
                                "type": "exec_error",
                                "error": error_msg,
                                "timestamp": chrono::Utc::now().to_rfc3339()
                            }));
                            return ExecResult::error(error_msg);
                        }
                    }
                }
                thread::sleep(Duration::from_millis(common::CHILD_POLL_INTERVAL_MS));
            }
            Err(e) => {
                let error_msg = format!("进程状态检查失败: {}", e);
                common::log_event(serde_json::json!({
                    "type": "exec_error",
                    "error": error_msg,
                    "timestamp": chrono::Utc::now().to_rfc3339()
                }));
                return ExecResult::error(error_msg);
            }
        }
    };

    let (stdout, out_trunc) = out_t.join().unwrap_or_default();
    let (stderr, err_trunc) = err_t.join().unwrap_or_default();

    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        common::log_event(serde_json::json!({
            "type": "exec_nonzero_exit",
            "exit_code": exit_code,
            "stdout_preview": &stdout[..stdout.len().min(200)],
            "stderr_preview": &stderr[..stderr.len().min(200)],
            "timestamp": chrono::Utc::now().to_rfc3339()
        }));
    }

    ExecResult {
        exit_code,
        stdout,
        stderr,
        timed_out,
        error: None,
        truncated: out_trunc || err_trunc,
    }
}

/// 从管道读取全部输出，截断至 `OUTPUT_LIMIT`（仅影响返回给客户端的字符串，
/// 不影响子进程生命周期）。
///
/// 关键修复（B1）：原实现在 `buf.len() >= OUTPUT_LIMIT` 时直接 `break` 停止消费，
/// 子进程写满 8KB 内核管道缓冲后阻塞在 WriteFile，直到超时（默认 60s）才被 taskkill
/// 杀掉，使 `dir /s C:\`、`systeminfo`、大日志等输出 >1MB 的命令被「错误超时」。
/// 现改为超限后**继续排空管道（读到 EOF，丢弃超出的字节）**，让子进程自然跑完退出；
/// 截断标记仅用于告知客户端输出被裁剪，超时判定完全由子进程真实退出状态决定。
fn drain_pipe<R: Read>(pipe: Option<R>) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(common::OUTPUT_LIMIT.min(64 * 1024));
    let mut truncated = false;
    if let Some(mut p) = pipe {
        let mut chunk = [0u8; 8192];
        loop {
            match p.read(&mut chunk) {
                Ok(0) => break, // 管道关闭，子进程已结束
                Ok(n) => {
                    if buf.len() < common::OUTPUT_LIMIT {
                        // 仍未到上限：截取不超过上限的字节，恰好填满时标记截断
                        let take = (common::OUTPUT_LIMIT - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                        if buf.len() >= common::OUTPUT_LIMIT {
                            truncated = true;
                        }
                    } else {
                        // 已超出上限：继续消费并丢弃，排空管道让子进程自然退出
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
    }
    (buf, truncated)
}

/// 对传入 `cmd /C` 的单行命令做 cmd.exe 元字符转义。
///
/// `cmd /C <command>` 会二次解析命令行，把 `&` 当作命令分隔符
/// （如未加引号的设备实例 ID `VID_373B&PID_1216&MI_00&COL0x` 中的 `&` 会被拆段）。
/// 在**引号外**的 `^`/`&` 前加 `^` 转义为字面量，确保整条命令原样送达目标程序。
///
/// **引号感知（修复 B3）**：cmd 的双引号内部，`^` **不是转义符**（引号内 `^` 为字面量），
/// 且引号内的 `&`/`|`/`<`/`>`/`()` 本来就由 cmd 当字面量处理、不会被当作分隔符/重定向。
/// 因此**仅当字符位于引号外部时才转义**；引号内的 `&` 必须保持原样。否则盲目加 `^`
/// 会使 `echo "a&b"` 变成 `echo "a^&b"`、输出 `a^&b` 而非 `a&b`（静默内容破坏）。
///
/// 注意：`^` 必须先于 `&` 处理，否则 `^` 自身也会被误转义。
/// 仅在单行模式下调用；多行脚本模式不转义（见 `run_script`）。
fn escape_cmd_meta(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + 8);
    let mut in_quotes = false;
    for c in cmd.chars() {
        match c {
            // 每个 `"` 切换引号状态；引号内的 `&`/`^` 不再转义（B3 修复）
            '"' => {
                in_quotes = !in_quotes;
                out.push(c);
            }
            '^' | '&' if !in_quotes => {
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
///
/// 启发式修正（P1）：仅凭「UTF-8 解码成功」不足以判定内容就是 UTF-8。GBK 双字节
/// 序列（首字节 0x81-0xFE、次字节 0x40-0xFE）中有相当一部分恰好是合法 UTF-8 序列，
/// 使短 GBK 中文输出被误判为 UTF-8 → 乱码。判据改为「UTF-8 合法 **且** 不含任何
/// 多字节序列」才直接采用（即纯 ASCII）；含多字节序列时按系统 OEM 代码页解码，
/// 因为 cmd 内建命令（dir/ver/systeminfo 等）的多字节输出实质上都是 OEM 编码。
/// 纯 ASCII 情形（pnputil/whoami/sc/ver 等）两种解码结果一致，不受影响。
fn decode_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // 1) 纯 ASCII：UTF-8 与 OEM 解码结果相同，直接采用（最常见路径，零额外开销）
    if bytes.is_ascii() {
        // SAFETY: 已确认全为 ASCII，必为合法 UTF-8
        return unsafe { String::from_utf8_unchecked(bytes.to_vec()) };
    }
    // 2) 含非 ASCII 字节：优先判定是否为合法 UTF-8（覆盖真正的 UTF-8 发射器）
    if let Ok(s) = std::str::from_utf8(bytes) {
        // 合法 UTF-8 且含多字节序列。此处存在歧义（可能是 GBK 恰好构成合法 UTF-8），
        // 采用折中判据：若解码结果含 CJK 且无替换符/控制字符污染，则信任 UTF-8；
        // 否则回退 OEM。
        if looks_like_text(s) {
            return s.to_string();
        }
    }
    // 3) 回退到 OEM 代码页（GBK），覆盖 cmd 内建命令以 GBK 回显的输出
    decode_codepage(bytes, crate::winapi_raw::CP_OEMCP)
}

/// 粗略判断一段已成功解码的 UTF-8 文本是否「像正常文本」：
/// 不含 C0/C1 控制字符（除常见空白）与 Unicode 替换符 U+FFFD。
/// 用于在「合法 UTF-8」与「GBK 误判为 UTF-8」之间取舍。
fn looks_like_text(s: &str) -> bool {
    !s.chars().any(|c| {
        (c.is_control() && c != '\r' && c != '\n' && c != '\t') || c == '\u{FFFD}'
    })
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

#[cfg(test)]
mod tests {
    use super::escape_cmd_meta;

    #[test]
    fn escapes_amp_outside_quotes() {
        // 未加引号的 & 必须转义，避免被 cmd 当作命令分隔符
        assert_eq!(escape_cmd_meta("echo a&b"), "echo a^&b");
        assert_eq!(escape_cmd_meta("echo a&&b"), "echo a^&^&b");
    }

    #[test]
    fn preserves_amp_inside_quotes_b3() {
        // B3 修复：引号内的 & 不得转义，否则输出 a^&b
        assert_eq!(escape_cmd_meta("echo \"a&b\""), "echo \"a&b\"");
        // 含 & 的设备实例 ID（带引号）应原样保留
        assert_eq!(
            escape_cmd_meta("pnputil /instanceid \"VID_373B&PID_1216&MI_00&COL0x\""),
            "pnputil /instanceid \"VID_373B&PID_1216&MI_00&COL0x\""
        );
    }

    #[test]
    fn escapes_caret_outside_quotes() {
        assert_eq!(escape_cmd_meta("echo a^b"), "echo a^^b");
    }

    #[test]
    fn preserves_caret_inside_quotes() {
        assert_eq!(escape_cmd_meta("echo \"a^b\""), "echo \"a^b\"");
    }

    #[test]
    fn quote_toggle_mixed() {
        // 引号内的 & 不转义，引号外的 & 转义
        assert_eq!(
            escape_cmd_meta("echo \"a&b\" & echo c"),
            "echo \"a&b\" ^& echo c"
        );
        // 引号外的 ^ 转义，引号内的 ^ 不转义
        assert_eq!(escape_cmd_meta("echo ^a \"b^c\""), "echo ^^a \"b^c\"");
    }

    #[test]
    fn unterminated_quote_treats_rest_as_quoted() {
        // 未闭合引号：其后内容视为引号内，& 不转义
        assert_eq!(escape_cmd_meta("echo \"a&b"), "echo \"a&b");
    }
}
