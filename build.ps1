# AI-Root 一键构建脚本(本机与 CI 通用)
#
# 用法:  powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
# 产物:  <仓库>/target/<target>/release/AI-Root.exe
#
# 设计原则(2026-10-04 重构, 从 207 行削至约 90 行):
#   一、与 build.sh 共享 .cargo/config.toml, 不再各自维护 target-dir/链接器配置。
#   二、产物落**仓库内默认 target/**。仓库已迁至纯 ASCII 路径, 不再需要外部目录。
#       (此前路径含中文「工具」时, MinGW 的 ld 按 ANSI 代码页解释路径导致
#        "cannot find ...\工具\...\libsyn-xxx.rlib" —— 文件存在却找不到。)
#   三、本脚本只承担 build.sh 无法承担的两件事:
#       (a) msvc/gnu 目标自动选择(CI 走 msvc, 本机 gnu-only 走 gnu);
#       (b) Windows 原生语义下调用 cargo(避开 Git Bash 的 PATH 形式问题)。
#
# 目标自动选择:
#   1) 已装 msvc 工具链且能找到 link.exe  → msvc(CI 走此路径)
#   2) 否则已装 gnu 工具链               → gnu + 注入完整 MinGW binutils
#
# gnu 链接期必须用外部完整 binutils: rustup 的 self-contained 目录实测
# 只带 dlltool.exe/ld.exe, **缺 as.exe**, 无法独立完成链接。
# binutils 探测顺序:
#   a) $env:MINGW_BINUTILS_BIN (显式指定)
#   b) D:\Kiln\mingw64\bin (本机当前实际位置)
#   c) D:\Kiln\mingw-binutils\bin (旧位置, 保留兼容)

$ErrorActionPreference = "Stop"
# cargo 把编译进度写到 stderr; PowerShell 在 "Stop" 下会把原生命令的 stderr 首行
# 包装成 NativeCommandError 并中断脚本(CI 会因此误判失败)。
# 放宽为 Continue, 靠 $LASTEXITCODE 判断真实成败。
$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"

# ---- 0) 定位 rustup 根目录 ----
# ⚠ 候选必须**逐个校验 toolchains 目录确实存在**后才采用:
#   本机曾把 rust 从 D:\software\rust 迁到 D:\Kiln\rust, 用户级环境变量已更新,
#   但**已在运行的会话仍带迁移前的旧 $env:RUSTUP_HOME**(陈旧继承)。
#   若不校验直接采信, 会指向已不存在的目录 → 构建失败且报错无提示。
function Resolve-RustupRoot {
    $candidates = @()
    if ($env:AIROOT_RUST_ROOT) { $candidates += (Join-Path $env:AIROOT_RUST_ROOT "rustup") }
    if ($env:RUSTUP_HOME)      { $candidates += $env:RUSTUP_HOME }
    $candidates += (Join-Path $env:USERPROFILE ".rustup")
    $candidates += "D:\Kiln\rust\rustup"        # 本机当前实际位置
    $candidates += "D:\software\rust\rustup"    # 旧位置, 保留兼容
    foreach ($c in $candidates) {
        if ($c -and (Test-Path (Join-Path $c "toolchains"))) { return $c }
    }
    return $null
}

$rustupRoot = Resolve-RustupRoot
if (-not $rustupRoot) {
    Write-Error @"
未找到 rustup 根目录(含 toolchains 子目录)。
已尝试: AIROOT_RUST_ROOT, RUSTUP_HOME, $env:USERPROFILE\.rustup, D:\Kiln\rust\rustup, D:\software\rust\rustup
可用 AIROOT_RUST_ROOT 环境变量指定 rust 根目录(含 cargo\ 与 rustup\)。
"@
}
$toolchainsDir = Join-Path $rustupRoot "toolchains"

# ⚠ 必须把解析出的根目录**回写** CARGO_HOME / RUSTUP_HOME, 不能只读取后不管。
#   实测缺陷: 会话带着陈旧的 $env:CARGO_HOME 时, 旧脚本只用它推导 rustupRoot,
#   却没导出修正值 → cargo 继承旧值 → 该目录不存在就从 index.crates.io
#   **重新下载全套 crate**(rsproxy 镜像配置失效), 于是 build.ps1 与 build.sh
#   用了两套 registry(源路径不同 → 产物字节不一致)。
$rustRoot = Split-Path -Parent $rustupRoot
$env:RUSTUP_HOME = $rustupRoot
$env:CARGO_HOME  = Join-Path $rustRoot "cargo"

# ---- 1) 选择 target ----
$msvcDir = Join-Path $toolchainsDir "stable-x86_64-pc-windows-msvc"
$gnuDir  = Join-Path $toolchainsDir "stable-x86_64-pc-windows-gnu"

$TARGET = $null
if (Test-Path $msvcDir) {
    # msvc 需 link.exe 才能真正链接; 仅工具链存在但无 VS Build Tools 仍会失败。
    # 探测: 先查 vswhere(最快最准), 再回退有限深度扫描。
    $linkExe = $null
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ($vsPath) {
            $linkExe = Get-ChildItem (Join-Path $vsPath "VC\Tools\MSVC") -Directory -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "bin\Hostx64\x64\link.exe" } |
                Where-Object { Test-Path $_ } | Select-Object -First 1
        }
    }
    if (-not $linkExe) {
        $linkExe = Get-ChildItem "C:\Program Files*\Microsoft Visual Studio" -Depth 6 -Filter link.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if ($linkExe) { $TARGET = "x86_64-pc-windows-msvc" }
}
if (-not $TARGET -and (Test-Path $gnuDir)) { $TARGET = "x86_64-pc-windows-gnu" }
if (-not $TARGET) {
    Write-Error "未找到可用工具链(需 msvc 工具链+link.exe, 或 gnu 工具链)。请先 rustup toolchain install stable"
}

# ---- 2) 定位该 target 的 cargo ----
# 直连 toolchain 内的真实 cargo.exe, 绕开 CARGO_HOME\bin 下的 rustup shim
# (shim 是 rustup.exe 的硬链接, 转发时可能裁剪子进程 PATH)。
$toolchainDir = Join-Path $toolchainsDir "stable-$TARGET"
$cargoExe = Join-Path $toolchainDir "bin\cargo.exe"
if (-not (Test-Path $cargoExe)) {
    $cargoExe = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if ($cargoExe) { Write-Host "[!] 未找到 toolchain 内 cargo, 回退: $cargoExe" -ForegroundColor Yellow }
}
if (-not $cargoExe) { Write-Error "未找到 cargo, 请先安装 Rust (rustup)" }

# ---- 3) gnu target: 注入完整 MinGW binutils ----
if ($TARGET -eq "x86_64-pc-windows-gnu") {
    $binutilsCandidates = @()
    if ($env:MINGW_BINUTILS_BIN) { $binutilsCandidates += $env:MINGW_BINUTILS_BIN }
    $binutilsCandidates += "D:\Kiln\mingw64\bin"         # 本机当前实际位置
    $binutilsCandidates += "D:\Kiln\mingw-binutils\bin"  # 旧位置, 保留兼容
    $binutils = $binutilsCandidates | Where-Object { $_ -and (Test-Path (Join-Path $_ "as.exe")) } | Select-Object -First 1
    if (-not $binutils) {
        Write-Error @"
[gnu] 未找到含 as.exe 的完整 MinGW binutils (dlltool 链接期需要 as)。
已探测:
$(($binutilsCandidates | ForEach-Object { "  - $_" }) -join "`n")
请任选其一:
  A) 下载完整 binutils 到 D:\Kiln\mingw64\bin
     清华镜像: https://mirrors.tuna.tsinghua.edu.cn/msys2/mingw/mingw64/mingw-w64-x86_64-binutils-2.43.1-1-any.pkg.tar.zst
  B) 安装 Visual Studio Build Tools 后改用 msvc target(CI 即此路径)
  C) 设置环境变量 MINGW_BINUTILS_BIN 指向已有的 binutils\bin
"@
    }
    # 顺序: binutils 在 toolchain bin **之前**, 确保同名工具(dlltool/as/ld)用完整版。
    $env:PATH = "$binutils;$toolchainDir\bin;$env:PATH"
    Write-Host "[*] MinGW binutils: $binutils" -ForegroundColor Cyan

    # 必须验证可达: 只塞进 PATH 不够, 否则会一路编译到某依赖链接期才报
    # "error calling dlltool: program not found", 极难定位。
    foreach ($tool in @("as.exe", "dlltool.exe", "ld.exe")) {
        $found = Get-Command $tool -ErrorAction SilentlyContinue
        if (-not $found) { Write-Error "[gnu] PATH 中找不到 $tool —— gnu 链接期必需(binutils: $binutils)" }
    }
}

# ---- 4) 构建 ----
$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $repoRoot

# 产物目录: 环境变量 CARGO_TARGET_DIR 优先(cargo 的约定), 否则用仓库内 target\。
# CI 会设置该变量到 runner 临时目录, 故此处必须同样遵守, 否则报错路径与实际不符。
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repoRoot "target" }

Write-Host "[*] 构建工具链: $cargoExe" -ForegroundColor Cyan
Write-Host "[*] 目标平台  : $TARGET" -ForegroundColor Cyan
Write-Host "[*] 产物目录  : $targetDir" -ForegroundColor Cyan

& $cargoExe build --release --target $TARGET
$buildExit = $LASTEXITCODE
$ErrorActionPreference = $prevEAP
if ($buildExit -ne 0) { Write-Error "构建失败 (exit=$buildExit)" }

$exe = Join-Path $targetDir "$TARGET\release\AI-Root.exe"
if (-not (Test-Path $exe)) { Write-Error "构建未产出 exe: $exe" }
Write-Host "[OK] 产物: $exe" -ForegroundColor Green
Write-Host ("     SHA256: " + (Get-FileHash $exe -Algorithm SHA256).Hash) -ForegroundColor Green
