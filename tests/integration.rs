//! ai-root 集成测试套件
//!
//! 底座：spawn 构建产物 `ai-root.exe --test-pipe-server`（非提权即可运行完整服务端逻辑，
//! 30 秒后自动优雅退出），并通过 `AIROOT_DATA_DIR` 指向独立临时目录，与生产数据完全隔离。
//! 所有用例经全局互斥锁串行执行（管道名固定 `\\.\pipe\ai-root`，并行会互相干扰）。
//!
//! 运行：`cargo test --release`（bin-only crate 下 `CARGO_BIN_EXE_ai-root` 指向构建产物）

use std::io::{Read, Write};

use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 全局串行锁：管道名固定，测试间不得并行
static LOCK: Mutex<()> = Mutex::new(());

// ---- 最小 Win32 绑定（仅客户端连接用）----
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const OPEN_EXISTING: u32 = 3;
/// CreateFileW 连接命名管道时所有实例均忙（服务端单线程 accept 尚未轮到）返回此错误码
const ERROR_PIPE_BUSY: u32 = 231;

extern "system" {
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *const core::ffi::c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn CloseHandle(hObject: *mut core::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
    fn WaitNamedPipeW(lpNamedPipeName: *const u16, nTimeOut: u32) -> i32;
}

/// 连接命名管道并返回句柄；遇 ERROR_PIPE_BUSY 时按标准流程 WaitNamedPipeW 等待实例就绪后重试，
/// 非 busy 的瞬态错误（如服务端实例创建间隙）也短退避重试。连接失败返回 null（由调用方决定）。
unsafe fn connect_pipe_raw() -> *mut core::ffi::c_void {
    let pipe_name = std::env::var("AIROOT_PIPE_NAME").unwrap_or_else(|_| r"\\.\pipe\ai-root".into());
    let name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    for _ in 0..20 {
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if !h.is_null() && h != usize::MAX as *mut _ {
            return h;
        }
        let err = GetLastError();
        if err != ERROR_PIPE_BUSY {
            // 瞬态非 busy 错误：短退避后继续（服务端单线程 accept 的创建间隙）
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }
        WaitNamedPipeW(name.as_ptr(), 300);
    }
    std::ptr::null_mut()
}

/// 测试服务守卫：闭包 panic 时也确保 kill 进程、清理临时目录（防残留占用管道名）
struct ServerGuard {
    child: std::process::Child,
    dir: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 启动一个隔离的测试服务实例，就绪后调用闭包；退出（含 panic）时回收进程与临时目录。
fn with_server(f: impl FnOnce(&Path)) {
    // 随机后缀：管道名/目录每次唯一，避免上一轮残留进程或同 pid 复用冲突
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("airoot_it_{}_{}", std::process::id(), nonce));
    std::fs::create_dir_all(&dir).expect("创建测试数据目录");
    // 唯一管道名：避免与生产服务(\.\pipe\ai-root)串扰。
    // 显式 set_var(供测试进程内直连) + 显式 env(传给 server 子进程, 不依赖继承)。
    // 注意：Windows 管道名必须为双反斜杠前缀 \\.\pipe\，单反斜杠 \.\pipe\ 会导致
    // CreateNamedPipeW 返回 ERROR_INVALID_NAME (123)。
    let pipe = format!(r"\\.\pipe\ai-root-it-{nonce}");
    std::env::set_var("AIROOT_PIPE_NAME", &pipe);

    let exe = env!("CARGO_BIN_EXE_ai-root");
    let err_log = dir.join("server_err.log");
    let err_file = std::fs::File::create(&err_log).expect("创建错误日志");
    let child = Command::new(exe)
        .args(["--test-pipe-server"])
        .env("AIROOT_DATA_DIR", &dir)
        .env("AIROOT_PIPE_NAME", &pipe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(err_file))
        .spawn()
        .expect("spawn --test-pipe-server");
    let mut guard = ServerGuard { child, dir: dir.clone() };

    // 轮询等管道就绪：必须满足「管道可连 且 服务进程仍存活」才算就绪
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let alive = guard
            .child
            .try_wait()
            .map(|s| s.is_none())
            .unwrap_or(false);
        let probe = connect_probe();
        if alive && probe {
            std::thread::sleep(Duration::from_millis(300));
            break;
        }
        if Instant::now() >= deadline {
            eprintln!("[dbg] server stderr: {:?}", std::fs::read_to_string(&err_log));
            assert!(false, "测试服务 6s 内未就绪");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    f(&dir);
    drop(guard);
}

/// 仅探测管道是否可连接（服务端 accept 已就绪即成功）
fn connect_probe() -> bool {
    unsafe {
        let h = connect_pipe_raw();
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// 直连管道发送一帧请求（[u32 LE 长度][JSON]），返回解析后的响应
fn send_request(_data_dir: &Path, token: &str, command: &str, cwd: Option<&str>, timeout_ms: u64) -> Value {
    unsafe {
        let h = connect_pipe_raw();
        assert!(!h.is_null(), "CreateFileW 失败(含 ERROR_PIPE_BUSY 等待重试后仍无法连接)");
        let mut file = std::fs::File::from_raw_handle(h as RawHandle);

        let payload = serde_json::to_vec(&json!({
            "token": token,
            "command": command,
            "cwd": cwd,
            "timeout_ms": timeout_ms,
        }))
        .unwrap();
        file.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&payload).unwrap();
        file.flush().unwrap();

        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf).unwrap();
        let n = u32::from_le_bytes(len_buf) as usize;
        assert!(n <= 16 * 1024 * 1024, "响应帧过大");
        let mut body = vec![0u8; n];
        file.read_exact(&mut body).unwrap();
        drop(file); // 关闭句柄
        serde_json::from_slice(&body).expect("响应 JSON 解析失败")
    }
}

/// 读取测试数据目录中的令牌
fn read_token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("token.txt"))
        .expect("token.txt 缺失")
        .trim()
        .to_string()
}

/// 通过 CLI 子进程执行 run，返回 (exit_code, stdout)
fn run_cli(data_dir: &Path, args: &[&str], stdin: Option<&str>) -> (i32, String) {
    let exe = env!("CARGO_BIN_EXE_ai-root");
    let mut cmd = Command::new(exe);
    cmd.arg("run").args(args).env("AIROOT_DATA_DIR", data_dir);
    // stdout/stderr 必须始终 piped：wait_with_output 只能捕获 piped 输出，
    // 否则子进程输出继承测试进程 stdout（stdin 为 None 时漏设，导致 out 恒为空）
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().expect("spawn run");
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().expect("wait run");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr)); // 客户端错误(如"执行超时")走 stderr
    (out.status.code().unwrap_or(-1), text)
}

// ================= 用例 =================

#[test]
fn t_01_auth_and_exec() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let tok = read_token(dir);
        // 错误令牌：连接层通过，应用层拒绝
        let r = send_request(dir, "f".repeat(64).as_str(), "whoami", None, 5000);
        assert_eq!(r["ok"], false);
        assert_eq!(r["exit_code"], -1);
        assert!(r["error"].as_str().unwrap().contains("令牌鉴权失败"));
        // 正确令牌：执行成功（test server 为当前用户身份，非 SYSTEM）
        let r = send_request(dir, &tok, "whoami", None, 5000);
        assert_eq!(r["ok"], true);
        assert!(!r["stdout"].as_str().unwrap().trim().is_empty());
        // CLI 形态同样工作
        let (code, out) = run_cli(dir, &["whoami"], None);
        assert_eq!(code, 0);
        assert!(!out.trim().is_empty());
    });
}

#[test]
fn t_02_script_mode_and_escape() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        // 多行脚本：& 串联、变量（call 展开）正常
        let script = "echo S1\necho A & call echo V=%X%";
        let r = send_request(dir, &read_token(dir), script, None, 5000);
        assert_eq!(r["ok"], true);
        let out = r["stdout"].as_str().unwrap();
        assert!(out.contains("S1"));
        // 单行 & 转义：字面量输出
        let r = send_request(dir, &read_token(dir), "echo a & echo b", None, 5000);
        assert!(r["stdout"].as_str().unwrap().contains("a & echo b"));
    });
}

#[test]
fn t_03_exit_code_and_timeout() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let tok = read_token(dir);
        // 退出码传播
        let r = send_request(dir, &tok, "exit /b 7", None, 5000);
        assert_eq!(r["exit_code"], 7);
        // 超时：2s 超时杀进程树（taskkill /T），应较快返回且 ok=false
        let t0 = Instant::now();
        let r = send_request(dir, &tok, "ping -n 8 127.0.0.1", None, 1500);
        let elapsed = t0.elapsed();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"].as_str().unwrap(), "执行超时");
        // 修复前实测 2s 超时要 ~5s 返回（子进程残留）；杀树后应在 ~3s 内返回
        assert!(elapsed.as_secs() < 4, "超时返回过慢: {elapsed:?}");
    });
}

#[test]
fn t_04_large_output_and_cwd() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let tok = read_token(dir);
        // 大输出：200KB 文本（远超 8KB 管道缓冲）
        let big = dir.join("big.txt");
        let mut content = String::with_capacity(200 * 1024);
        for i in 0..2048 {
            content.push_str(&format!("LINE_{i:05} {}\n", "x".repeat(85)));
        }
        std::fs::write(&big, &content).unwrap();
        let r = send_request(dir, &tok, &format!("type {}", big.display()), None, 15000);
        assert_eq!(r["ok"], true);
        let out = r["stdout"].as_str().unwrap();
        assert!(out.len() > 180 * 1024, "输出不完整: {}", out.len());
        assert!(out.contains("LINE_02047"));
        // cwd 参数：%CD% 应为指定目录
        let r = send_request(dir, &tok, "echo %CD%", Some(dir.to_str().unwrap()), 5000);
        assert!(r["stdout"].as_str().unwrap().contains(dir.to_str().unwrap()));
    });
}

#[test]
fn t_05_rotate_hot_reload() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let old = read_token(dir);
        let r = send_request(dir, &old, "whoami", None, 5000);
        assert_eq!(r["ok"], true, "旧令牌应可用");

        // 模拟 rotate-token：生成新令牌写盘（服务端按 mtime 自动热加载）
        let new = "0123456789abcdef0123456789abcdef";
        std::fs::write(dir.join("token.txt"), new).unwrap();

        // 旧令牌应立即失效
        let r = send_request(dir, &old, "whoami", None, 5000);
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("令牌鉴权失败"));
        // 新令牌生效（无需重启服务）
        let r = send_request(dir, new, "whoami", None, 5000);
        assert_eq!(r["ok"], true, "新令牌应热加载生效");
    });
}

#[test]
fn t_06_enabled_flag() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let tok = read_token(dir);
        // 默认启用
        let r = send_request(dir, &tok, "whoami", None, 5000);
        assert_eq!(r["ok"], true);
        // 删除 flag → 拒绝
        std::fs::remove_file(dir.join("enabled.flag")).unwrap();
        let r = send_request(dir, &tok, "whoami", None, 5000);
        assert_eq!(r["ok"], false);
        assert!(r["error"].as_str().unwrap().contains("中继已禁用"));
        // 恢复 flag → 重新可用
        std::fs::write(dir.join("enabled.flag"), "1").unwrap();
        let r = send_request(dir, &tok, "whoami", None, 5000);
        assert_eq!(r["ok"], true);
    });
}

#[test]
fn t_07_cli_timeout_and_script() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        // --timeout 参数生效（1.5s 超时杀进程树）
        let (code, out) = run_cli(dir, &["--timeout", "1500", "ping -n 8 127.0.0.1"], None);
        assert_ne!(code, 0);
        assert!(out.contains("执行超时"));
        // --stdin 多行脚本
        let (code, out) = run_cli(dir, &["--stdin"], Some("echo CLI_STDIN_OK\necho DONE"));
        assert_eq!(code, 0);
        assert!(out.contains("CLI_STDIN_OK"));
    });
}

#[test]
fn t_08_concurrent_10() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    with_server(|dir| {
        let tok = read_token(dir);
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let tok = tok.clone();
                let d = dir.to_path_buf();
                std::thread::spawn(move || {
                    let r = send_request(&d, &tok, &format!("echo REQ_{i}"), None, 10000);
                    (i, r)
                })
            })
            .collect();
        for h in handles {
            let (i, r) = h.join().unwrap();
            assert_eq!(r["ok"], true, "REQ_{i} 失败: {r}");
            assert!(r["stdout"].as_str().unwrap().contains(&format!("REQ_{i}")));
        }
    });
}

/// 确保测试产物未污染生产环境（生产目录必须保持原样）
#[test]
fn t_09_no_production_touch() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prod = PathBuf::from(std::env::var("ProgramData").unwrap())
        .join("ai-root")
        .join("token.txt");
    // 生产环境已安装时：token 应存在且非测试值
    if prod.exists() {
        let t = std::fs::read_to_string(&prod).unwrap();
        assert_ne!(t.trim(), "0123456789abcdef0123456789abcdef", "测试令牌写入生产目录！");
    }
}
