// 构建脚本：为 Windows 目标嵌入应用图标（assets/AI-Root.rc + AI-Root.ico）
//
// 设计原则：**资源嵌入是可选增强，绝不能阻断构建**。
//   - 资源文件缺失（assets/AI-Root.rc 未随仓库分发）时直接跳过，仅打印告警。
//   - 资源编译器不可用时（gnu 目标缺 windres、msvc 目标缺 rc.exe）降级跳过，
//     仅缺失图标，不影响功能与编译 —— 保证任何环境都能产出可运行的 exe。
//
// 分支说明：
//   - msvc 目标：优先调用 rc.exe 生成 .lib 并 emit cargo:rustc-link-arg-bins，
//     与 embed-resource 的 windows_msvc 分支行为一致，避免依赖 windres。
//   - gnu 目标：仍走 embed-resource（windres），但捕获其 panic / 缺失降级。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // 资源文件不存在：跳过（仓库可能未含 assets，或构建环境精简）
    let rc_path = "assets/AI-Root.rc";
    if !std::path::Path::new(rc_path).exists() {
        eprintln!("cargo:warning=未找到 {rc_path}，跳过图标嵌入（不影响功能）");
        return;
    }

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        let out_dir = std::env::var("OUT_DIR").expect("No OUT_DIR env var");
        let out_file = format!("{}\\AI-Root.lib", out_dir);
        match std::process::Command::new("rc.exe")
            .args(["/fo", &out_file, "/I", &out_dir])
            .arg(rc_path)
            .status()
        {
            Ok(status) if status.success() => {
                println!("cargo:rustc-link-arg-bins={}", out_file);
            }
            Ok(status) => {
                eprintln!(
                    "cargo:warning=rc.exe 编译资源失败(status={:?})，跳过图标嵌入",
                    status
                );
            }
            Err(e) => {
                eprintln!("cargo:warning=未找到 rc.exe({e})，跳过图标嵌入（不影响功能）");
            }
        }
    } else {
        // gnu 目标：embed-resource 依赖 windres；缺失时其内部会 panic，
        // 这里用 catch_unwind 兜底，把「资源编译失败」降级为告警而非构建失败。
        // 说明：embed-resource 的 panic 发生在 build script 阶段，无法在库外拦截，
        // 故先探测 windres 是否可用，不可用则直接跳过（比依赖 catch_unwind 更可靠）。
        if !tool_available("windres") {
            eprintln!("cargo:warning=未找到 windres，跳过图标嵌入（不影响功能）");
            return;
        }
        let res = std::panic::catch_unwind(|| {
            embed_resource::compile(rc_path, embed_resource::NONE);
        });
        if res.is_err() {
            eprintln!("cargo:warning=embed-resource 编译资源失败，跳过图标嵌入（不影响功能）");
        }
    }
}

/// 探测某个可执行文件是否在 PATH 中可用（仅判断能否启动）。
fn tool_available(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
