// 构建脚本：为 Windows 目标嵌入应用图标（assets/ai-root.ico）
// 通过 embed-resource 调用 rc.exe（MSVC 构建环境需配置 vcvars/INCLUDE/LIB）
fn main() {
    // 仅 Windows 目标嵌入资源；其他平台（CI 交叉检查等）跳过
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/ai-root.rc", embed_resource::NONE);
    }
}
