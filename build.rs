// 构建脚本：为 Windows 目标嵌入应用图标（assets/AI-Root.ico）
//
// 问题：build 脚本本身编译于「host」三元组（本机默认 gnu），embed-resource 据此走
// windows_not_msvc 分支去调 windres（本机缺 as.exe / windres），导致资源编译失败。
// 解决：当 *目标* 环境为 msvc 时，直接用 rc.exe 生成 .lib（与 embed-resource 的
// windows_msvc 分支行为一致：rc.exe /fo {OUT_DIR}\AI-Root.lib，再 emit
// cargo:rustc-link-arg-bins），避免依赖 windres。gnu 目标仍回退 embed-resource。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        if target_env == "msvc" {
            let out_dir = std::env::var("OUT_DIR").expect("No OUT_DIR env var");
            let out_file = format!("{}\\AI-Root.lib", out_dir);
            // 优先用 rc.exe 嵌入图标；rc.exe 不可用时（精简环境 / SDK 未就绪）降级跳过，
            // 仅缺失图标，不影响功能与编译。正常构建环境（含 Windows SDK）仍会嵌入图标。
            match std::process::Command::new("rc.exe")
                .args(["/fo", &out_file, "/I", &out_dir])
                .arg("assets/AI-Root.rc")
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
                    eprintln!(
                        "cargo:warning=未找到 rc.exe({})，跳过图标嵌入（不影响功能）",
                        e
                    );
                }
            }
        } else {
            embed_resource::compile("assets/AI-Root.rc", embed_resource::NONE);
        }
    }
}
