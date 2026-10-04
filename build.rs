// 构建脚本：为 Windows 目标嵌入应用图标（assets/AI-Root.rc + AI-Root.ico）
//
// 设计原则：**资源嵌入是可选增强，绝不能阻断构建**。
//   - 资源文件缺失（assets/AI-Root.rc 未随仓库分发）时直接跳过，仅打印告警。
//   - 资源编译器不可用时（gnu 目标缺 windres、msvc 目标缺 rc.exe）降级跳过，
//     仅缺失图标，不影响功能与编译 —— 保证任何环境都能产出可运行的 exe。
//
// ★ 为什么 gnu 分支**不用** embed-resource（重要，CI 上踩过的坑）
//   embed-resource 2.x 在 `#[cfg(target_env = "msvc")]` 下选模块 —— 而该 cfg 指的是
//   **编译 build script 的 host**，不是 cargo `--target` 指定的目标。于是：
//     msvc host + gnu target（CI 的 cross-compile 场景）
//     → 编译进 windows_msvc 分支 → 调 rc.exe 产出 **MSVC 格式 .lib**
//     → 链接用 gnu 的 x86_64-w64-mingw32-gcc → "file format not recognized" 失败。
//   即 embed-resource 会**按 host 而非 target 选工具**，混杂配置下必然出错。
//   故 gnu 目标下我们**自己显式调 windres** 生成 .o（COFF 目标文件），
//   绕开其 host 探测 —— 这样无论 host 是 gnu 还是 msvc 都正确。
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
    let out_dir = std::env::var("OUT_DIR").expect("No OUT_DIR env var");

    if target_env == "msvc" {
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
        // gnu 目标：显式调 windres 生成 COFF 目标文件，再由链接器直接消费。
        // 说明：不依赖 embed-resource（原因见文件头）。windres 缺失则降级跳过。
        if !tool_available("windres") {
            eprintln!("cargo:warning=未找到 windres，跳过图标嵌入（不影响功能）");
            return;
        }
        // 目标架构：gnu 下我们只关心 x64（本项目仅发布 x86_64-pc-windows-gnu）。
        // windres 默认按自身前缀推导位数，显式指明更稳。
        let res_obj = format!("{}\\AI-Root-res.o", out_dir);
        match std::process::Command::new("windres")
            .args(["--include-dir", &out_dir, "-I", "assets"])
            .arg(rc_path)
            .arg("-O").arg("coff")
            .arg(&res_obj)
            .status()
        {
            Ok(status) if status.success() => {
                // 直接把 .o 交给链接器；rustc 会把它并入最终链接。
                println!("cargo:rustc-link-arg-bins={}", res_obj);
            }
            Ok(status) => {
                eprintln!(
                    "cargo:warning=windres 编译资源失败(status={:?})，跳过图标嵌入",
                    status
                );
            }
            Err(e) => {
                eprintln!("cargo:warning=调用 windres 失败({e})，跳过图标嵌入（不影响功能）");
            }
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
