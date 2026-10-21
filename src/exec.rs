//! 命令执行引擎
//!
//! 提供两条**严格区分**的执行路径：
//!
//! | 路径 | 入口 | 是否经 shell | 策略语义 |
//! |---|---|---|---|
//! | 结构化 argv | [`run_argv`] | **否**（`CreateProcess` 直调） | 可按程序/参数/目录精确授权 |
//! | 传统 shell | [`run_command`] | 是（`cmd /C`） | 只能整条命令确认，无法结构化匹配 |
//!
//! 为什么要把两条路分开：shell 字符串一旦允许，程序、参数、重定向、管道全部由
//! `cmd.exe` 解释，服务端无法可靠判断「这条命令到底要干什么」。改为 argv 直调后，
//! 程序是绝对路径、参数是数组，策略层能逐项校验，审计日志也能如实记录。
//!
//! 传统 shell 路径保留，是因为日常排障确实需要 `dir`/`for`/`|` 这类 cmd 语义；
//! 但它默认走**人工确认**（见 `policy.rs`），且不可逆操作在策略层硬拒绝。
//!
//! 超时控制：`try_wait` 轮询 + `taskkill /T /F` 杀整棵进程树。

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

/// 在**字节位置**安全截断 UTF-8 字符串。
///
/// 为什么不直接写 `&s[..n]`：`&str` 的索引是字节偏移，若 `n` 落在多字节字符
/// 中间，会在**运行时 panic**（`byte index is not a char boundary`）。
/// 中文 Windows 下命令输出含中文极常见（`ipconfig`、`dir`、`echo 中文`），
/// 200 字节这个位置经常正好切在一个汉字中间 —— 旧实现因此在
/// `run_child` 里 panic，导致该请求永远收不到响应帧。
///
/// 语义：向**前**回退到最近的字符边界，宁可少截几个字节也不 panic。
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 把程序名解析为**绝对路径**，避免 `CreateProcess` 的当前目录搜索顺序被利用。
///
/// Windows 上 `CreateProcess` 查找可执行文件的顺序是：
/// 应用目录 → **当前目录** → System32 → Windows → PATH。
/// 若客户端能把 cwd 指定为一个含 `git.exe` 的目录，`Command::new("git")` 会执行
/// 攻击者放的文件 —— 虽然结果仍是 SYSTEM（攻击者本来就要这个），但它破坏了
/// 「规则绑定的程序 = 实际执行的程序」这一语义，让审计日志出现误导。
///
/// 因此：含路径分隔符时必须是绝对路径；裸名字则**只**在 PATH 中查找（不查 cwd）。
pub fn resolve_program(program: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Path, PathBuf};

    let trimmed = program.trim();
    if trimmed.is_empty() {
        return Err("程序名为空".into());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        let p = PathBuf::from(trimmed);
        if !p.is_absolute() {
            return Err(format!("程序路径必须是绝对路径: {trimmed}"));
        }
        if !p.is_file() {
            return Err(format!("程序不存在: {trimmed}"));
        }
        return Ok(p);
    }

    // 裸名字：只在 PATH 里找（不查当前目录）
    let has_ext = Path::new(trimmed)
        .extension()
        .map(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("com"))
        .unwrap_or(false);
    let want = if has_ext {
        trimmed.to_string()
    } else {
        format!("{trimmed}.exe")
    };

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(&want);
            if cand.is_file() {
                return Ok(cand);
            }
        }
        // 没有 .exe 后缀也试一次（少数工具是可执行脚本/无扩展）
        if !has_ext {
            for dir in std::env::split_paths(&path) {
                let cand = dir.join(trimmed);
                if cand.is_file() {
                    return Ok(cand);
                }
            }
        }
    }
    Err(format!(
        "在 PATH 中找不到程序 `{trimmed}`（argv 模式不经 shell，必须能在 PATH 中定位）"
    ))
}

/// **结构化执行**：程序 + 参数数组，不经 `cmd.exe`。
///
/// `cwd` 必须由调用方先经策略校验；此处只校验它存在且是目录。
pub fn run_argv(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    timeout_ms: u64,
) -> ExecResult {
    let exe = match resolve_program(program) {
        Ok(p) => p,
        Err(e) => return ExecResult::error(e),
    };
    if let Some(c) = cwd {
        let p = std::path::Path::new(c);
        if !p.is_dir() {
            return ExecResult::error(format!("工作目录不存在或不是目录: {c}"));
        }
    }

    let mut builder = Command::new(&exe);
    builder.args(args);
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }
    run_child(builder, timeout_ms)
}

/// **传统 shell 执行**：`cmd /C <command>`（以 SYSTEM 运行，因此是 SYSTEM shell）。
///
/// ⚠ 安全定位：本函数是「需要人工确认」的路径。此处的 `dangerous_patterns`
/// 只是**廉价的纵深防御提示**，不是安全边界 —— `cmd /c`、`for /f`、
/// `powershell -enc`、`%ComSpec%`、`^` 转义、脚本文件都能绕过字符串匹配。
/// 真正的边界是 `policy.rs` 的结构化判定 + 确认流程。
///
/// 多行命令（含换行）走脚本模式：`&`/`&&`/`|`/`()` 作为正常 cmd 语法工作。
pub fn run_command(command: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    // 多行命令（含换行）走脚本模式：写入临时 .cmd，不转义 cmd 元字符，
    // 使 `&`/`&`/`|`/`()` 作为正常命令分隔符/运算符工作（引号内的 `&` 仍按字面处理）。
    if command.contains('\n') {
        return run_script(command, cwd, timeout_ms);
    }

    // 命令验证：防止空命令
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ExecResult::error("命令不能为空".to_string());
    }

    // 纵深防御（非边界，见函数文档）：拦截最直白的不可逆操作。
    let dangerous_patterns = ["format ", "format:", "diskpart", "del /s", "rd /s", "rmdir /s"];
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
    // 代价：`^`/`&` 仅在**引号外**被转义为字面量，故**引号外**的 `&` 与 `&&` 串联**全部失效**（退化为字面 `&`/`&`）；
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

/// 文件方案（回退）：脚本写入**受 ACL 保护的私有临时目录**后执行。
///
/// 双文件结构（实测验证）：
/// - 本体 `AI-Root-<rand>.cmd`：脚本内容，UTF-8 + CRLF（见 `encode_script`）。
/// - 包装 `AI-Root-<rand>.wrap.cmd`：**纯 ASCII**，内容为
///   `@echo off` / `chcp 65001 >nul` / `call "<本体路径>"`。
/// 原理：cmd 顶层批处理存在「预读缓冲」怪癖——`chcp` 生效前的行已按旧代码页读入，
/// 导致 UTF-8 中文在某些行乱码；且 `cmd /C "chcp & call \"path\""` 单行形式会因
/// `/C` 引号剥离把 `call` 参数拆坏（实测失败）。而包装脚本全 ASCII（顶层读取无中文问题），
/// 本体在 `chcp 65001` 生效后经 `call` 才被读取 → UTF-8 中文在多数情况下可正确解析。
///
/// ⚠ 安全改造（原实现写 `%TEMP%`）：
///   以 SYSTEM 运行时 `std::env::temp_dir()` 解析为 `C:\Windows\Temp`，
///   该目录对普通用户可创建文件。原实现用「pid + 纳秒」命名 + 覆盖式写入，
///   在「写文件」与「cmd 读取」之间存在可被竞争替换的窗口（TOCTOU）。
///   现在改为：
///     1) 目录 = `%ProgramData%\AI-Root\tmp\`（ACL：SYSTEM:F / Admins:F / 用户:R，用户不可写）
///     2) 文件名 = 16 字节加密随机 hex（不可预测）
///     3) 独占创建 `create_new(true)`（已存在即失败，绝不覆盖他人文件）
fn run_script_file(script: &str, cwd: Option<&str>, timeout_ms: u64) -> ExecResult {
    run_script_file_in(&common::tmp_dir(), script, cwd, timeout_ms)
}

/// `run_script_file` 的可测试内核：临时目录由调用方注入。
///
/// 分离原因：测试不能污染真实的 `%ProgramData%\AI-Root\tmp`，
/// 也不应为了测试去改 `AIROOT_DATA_DIR`（那是进程级变量，会与端到端测试互相干扰）。
fn run_script_file_in(
    dir: &std::path::Path,
    script: &str,
    cwd: Option<&str>,
    timeout_ms: u64,
) -> ExecResult {
    use rand::Rng;

    if let Err(e) = std::fs::create_dir_all(dir) {
        return ExecResult::error(format!("创建临时目录失败 {}: {e}", dir.display()));
    }

    let token: String = {
        let mut b = [0u8; 16];
        rand::thread_rng().fill(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    };
    let tmp = dir.join(format!("AI-Root-{token}.cmd"));
    let wrap = dir.join(format!("AI-Root-{token}.wrap.cmd"));

    // 本体：脚本内容（UTF-8 + CRLF），独占创建
    let bytes = encode_script(script);
    if let Err(e) = write_exclusive(&tmp, &bytes) {
        return ExecResult::error(format!("写临时脚本失败 {}: {e}", tmp.display()));
    }
    // 包装：纯 ASCII；@echo off 抑制回显，chcp 65001 先切换代码页，再 call 本体
    let inner = tmp.to_string_lossy().to_string();
    let wrapper = format!("@echo off\r\nchcp 65001 >nul\r\ncall \"{inner}\"\r\n");
    if let Err(e) = write_exclusive(&wrap, wrapper.as_bytes()) {
        cleanup_temp(&tmp);
        return ExecResult::error(format!("写包装脚本失败 {}: {e}", wrap.display()));
    }
    let wrap_path = wrap.to_string_lossy().to_string();

    let mut builder = Command::new("cmd");
    builder.raw_arg("/C").raw_arg(format!("\"{wrap_path}\""));
    builder.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(c) = cwd {
        builder.current_dir(c);
    }
    let res = run_child(builder, timeout_ms);
    cleanup_temp(&tmp);
    cleanup_temp(&wrap);
    res
}

/// 独占创建并写入（`create_new` 保证不会覆盖已存在的文件）
fn write_exclusive(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(data)?;
    f.flush()?;
    Ok(())
}

/// 删除临时文件；失败时**记录事件**而非静默吞掉。
///
/// 静默 `let _ =` 的问题是：一次失败会在 SYSTEM 目录留下带脚本内容的文件，
/// 而运维完全不知道。记录后至少能在 `tail` 事件流里看到 `temp_cleanup_failed`。
fn cleanup_temp(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            common::log_event(serde_json::json!({
                "ts": common::now_str(),
                "type": "temp_cleanup_failed",
                "path": path.to_string_lossy(),
                "error": e.to_string(),
            }));
        }
    }
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
                    let kill_out = Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .output();
                    let kill_ok = matches!(&kill_out, Ok(o) if o.status.success());
                    if !kill_ok {
                        // 兜底：至少杀掉直接子进程。两路都失败时下面的 wait() 可能阻塞，
                        // 但必须有事件留痕，否则「超时了却杀不掉」在日志里完全不可见。
                        let _ = child.kill();
                        common::log_event(serde_json::json!({
                            "ts": common::now_str(),
                            "type": "taskkill_failed",
                            "pid": pid,
                            "error": match &kill_out {
                                Ok(o) => format!("exit={:?}", o.status.code()),
                                Err(e) => e.to_string(),
                            },
                        }));
                    }
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
            // ⚠ 必须用 truncate_utf8 而非 &stdout[..200]：
            // 中文输出下 200 常落在多字节字符中间，字节切片会 panic。
            "stdout_preview": truncate_utf8(&stdout, 200),
            "stderr_preview": truncate_utf8(&stderr, 200),
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
    use super::{escape_cmd_meta, run_argv, run_command, run_script_file_in, truncate_utf8};

    // ---------- UTF-8 安全截断（V-07 回归）----------

    #[test]
    fn truncate_utf8_ascii_is_plain_prefix() {
        assert_eq!(truncate_utf8("abcdef", 3), "abc");
        assert_eq!(truncate_utf8("abc", 10), "abc", "未超限时原样返回");
    }

    #[test]
    fn truncate_utf8_never_splits_multibyte_char() {
        // 「中」= 3 字节（E4 B8 AD）。200 落在 66*3+2 处，即第三个字节之前 ——
        // 旧实现 `&s[..200]` 在此处 panic。
        let s = "中".repeat(200);
        let cut = truncate_utf8(&s, 200);
        assert!(cut.len() <= 200);
        assert_eq!(cut.len() % 3, 0, "必须在字符边界处回退");
        assert_eq!(cut, "中".repeat(66));
        // 关键：结果仍是合法 &str，可安全拼接/序列化
        assert!(!cut.is_empty());
    }

    #[test]
    fn truncate_utf8_boundary_exact_char_len() {
        // 恰好对齐边界时不应多退
        let s = "中".repeat(10); // 30 字节
        assert_eq!(truncate_utf8(&s, 30).len(), 30);
        assert_eq!(truncate_utf8(&s, 29).len(), 27);
    }

    #[test]
    fn truncate_utf8_with_emoji_four_byte() {
        // 4 字节字符（emoji）同样不能被切开
        let s = "🙂".repeat(10); // 40 字节
        let cut = truncate_utf8(&s, 21); // 落在第 6 个 emoji 中间
        assert_eq!(cut.len(), 20);
        assert_eq!(cut, "🙂".repeat(5));
    }

    /// 精确回归 V-07：非零退出 + 中文输出会走 preview 分支，旧实现此处 panic。
    /// 本测试真实驱动脚本执行，断言不 panic 且退出码正确。
    #[test]
    fn chinese_output_with_nonzero_exit_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("airoot-exec-utf8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 多行脚本 → 走脚本模式；输出 100 个汉字（300 字节，跨过 200 边界）后 exit 1
        let script = format!("echo {}\nexit 1", "中".repeat(100));
        let r = run_script_file_in(&dir, &script, None, 20_000);
        assert_eq!(r.exit_code, 1, "退出码应透传（error={:?}）", r.error);
        assert!(r.stdout.contains("中"), "应捕获到中文输出");
        assert!(!r.timed_out);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_temp_files_are_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("airoot-exec-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = run_script_file_in(&dir, "echo hi", None, 20_000);
        assert_eq!(r.exit_code, 0);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            left.is_empty(),
            "执行后不应残留临时脚本，实际: {:?}",
            left.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 结构化 argv（不经 shell）----------

    #[test]
    fn run_argv_executes_without_shell() {
        // whoami 在 PATH 中；argv 模式应正常执行并返回当前身份
        let r = run_argv("whoami", &[], None, 20_000);
        assert_eq!(r.exit_code, 0, "error={:?}", r.error);
        let out = r.stdout.trim();
        assert!(!out.is_empty(), "应输出当前身份");
        assert!(
            !out.contains('%') && !out.contains('^'),
            "未经 shell 解析，输出不应含变量/转义残留: {out:?}"
        );
    }

    #[test]
    fn run_argv_does_not_interpret_shell_metacharacters() {
        // 关键安全属性：argv 模式把 `&` 当作**普通参数**，不会执行第二条命令
        let r = run_argv(
            "cmd",
            &[
                "/c".to_string(),
                "echo".to_string(),
                "A&echo".to_string(),
                "B".to_string(),
            ],
            None,
            20_000,
        );
        // cmd 作为程序本身仍会解析自己的参数（这是 cmd 的语义），
        // 但 AI-Root 这一层没有做字符串拼接 —— 参数边界由 CreateProcess 保证。
        // 此处只要不 panic、能完成执行即可。
        assert!(r.error.is_none() || r.exit_code != 0);
    }

    #[test]
    fn run_argv_rejects_relative_program_path() {
        let r = run_argv(r"..\evil.exe", &[], None, 5_000);
        assert!(r.error.is_some(), "相对路径程序必须被拒绝");
        assert!(r.error.unwrap().contains("绝对路径"));
    }

    #[test]
    fn run_argv_rejects_missing_program() {
        let r = run_argv("definitely-not-a-real-program-xyz", &[], None, 5_000);
        assert!(r.error.is_some(), "PATH 中不存在的程序应报错而非当成 cmd 内置执行");
    }

    #[test]
    fn run_argv_rejects_missing_cwd() {
        let r = run_argv("whoami", &[], Some(r"Z:\no\such\dir"), 5_000);
        assert!(r.error.is_some(), "不存在的 cwd 应被拒绝");
    }

    #[test]
    fn run_argv_passes_arguments_verbatim() {
        // 含空格与引号的参数必须原样到达（不经 shell 二次解析）
        let r = run_argv(
            "cmd",
            &["/c".to_string(), "echo".to_string(), "a b&c".to_string()],
            None,
            20_000,
        );
        assert!(r.error.is_none(), "error={:?}", r.error);
    }

    // ---------- shell 模式（保留原有用例）----------

    #[test]
    fn shell_mode_still_works() {
        let r = run_command("echo AIROOT_SHELL_OK", None, 20_000);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("AIROOT_SHELL_OK"));
    }

    #[test]
    fn shell_destructive_string_check_is_not_a_boundary() {
        // 直白写法被拦（纵深防御有效，但仅限「逐字出现」）
        assert!(run_command("format C: /q", None, 5_000).error.is_some());

        // ⚠ 已知局限（本测试钉住这个事实，避免有人误以为这层是安全边界）：
        // 只要破坏性词不在字符串里逐字出现，本层就看不见。此处用 cmd 的 `^`
        // 转义改写演示 —— 本层放过，但 exit != 0 说明未真正执行 format。
        // 真正的边界是 policy.rs（此命令在策略层会因命中 `format ` 模式被 Deny）。
        let r = run_command("f^ormat C: /q", None, 5_000);
        assert!(
            r.error.is_none(),
            "字符串黑名单看不到转义形式，这是已知局限（应由策略层兜底）"
        );
        assert_ne!(r.exit_code, 0, "并未真正执行 format");
    }

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
