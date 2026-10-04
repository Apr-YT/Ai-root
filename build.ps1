# AI-Root 一键构建脚本(本机与 CI 通用)
#
# 用法:  powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
# 产物:  $CARGO_TARGET_DIR\<target>\release\AI-Root.exe
#
# 目标自动选择逻辑(按优先级):
#   1) 若已安装 x86_64-pc-windows-msvc 工具链且能找到 link.exe(MSVC / VS Build Tools)
#      → 用 msvc。CI(GitHub Actions windows-latest)自带 MSVC, 走此路径。
#   2) 否则若已安装 x86_64-pc-windows-gnu 工具链
#      → 用 gnu, 并自动把 MinGW binutils(as/dlltool/ld/windres)注入 PATH。
#        gnu 链接期必需 as.exe; rustup 的 self-contained 目录实测只带
#        dlltool.exe/ld.exe/x86_64-w64-mingw32-gcc.exe, **缺 as.exe**,
#        故该目录不可作为候选(2026-10-04 实测确认)。探测顺序:
#          a) $env:MINGW_BINUTILS_BIN (显式指定)
#          b) D:\Kiln\mingw64\bin (本机当前实际位置, 2026-10-04 变更)
#          c) D:\Kiln\mingw-binutils\bin (旧位置, 保留兼容)
#        找不到完整 binutils 时明确报错并给出获取方式, 不再以晦涩的
#        "dlltool: CreateProcess" 报错收场。
#
# ⚠ 重要(2026-10-04 实测): 本机 `bash build.sh` 曾失败于
#   "error calling dlltool 'dlltool.exe': program not found"。
#   根因是 Git Bash 的 PATH 查找只认 POSIX 形式(/d/Kiln/...), 不认 Windows
#   形式(D:/Kiln/...)。PowerShell 侧无此问题(原生 Windows 路径语义), 但若在
#   Git Bash 中调用本脚本需注意。另: 加入 PATH 后必须验证 dlltool 可达,
#   否则会到编译中途才报错 —— 见步骤 3b 的验证。
#
# 为什么优先使用 rustup 真实 toolchain:
#   本机 PATH 中可能存在代理二进制导致 cargo spawn rustc 报
#   "os error 193: 不是有效的 Win32 应用程序", 故优先直连 rustup toolchain 的 cargo.exe。

$ErrorActionPreference = "Stop"
# cargo 会把编译进度写到 stderr; PowerShell 在 $ErrorActionPreference="Stop" 下
# 会把原生命令的 stderr 首行包装成 NativeCommandError 并中断脚本(CI 会因此误判失败)。
# 临时放宽为 Continue, 靠 $LASTEXITCODE 判断真实成败。
$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"

# ---- 0) 定位 rustup 根目录(CARGO_HOME/RUSTUP_HOME 可能被重定位) ----
function Resolve-RustupRoot {
    $candidates = @()
    if ($env:RUSTUP_HOME) { $candidates += $env:RUSTUP_HOME }
    $candidates += (Join-Path $env:USERPROFILE ".rustup")
    $candidates += "D:\software\rust\rustup"
    foreach ($c in $candidates) {
        if ($c -and (Test-Path (Join-Path $c "toolchains"))) { return $c }
    }
    return $null
}

$rustupRoot = Resolve-RustupRoot
$toolchainsDir = if ($rustupRoot) { Join-Path $rustupRoot "toolchains" } else { $null }

# ---- 1) 选择 target ----
$msvcDir = if ($toolchainsDir) { Join-Path $toolchainsDir "stable-x86_64-pc-windows-msvc" } else { $null }
$gnuDir = if ($toolchainsDir) { Join-Path $toolchainsDir "stable-x86_64-pc-windows-gnu" } else { $null }

$TARGET = $null
if ($msvcDir -and (Test-Path $msvcDir)) {
    # msvc 需 link.exe 才能真正链接; 无 VS Build Tools 时仅本工具链存在也会失败。
    # 探测策略: 先查 vswhere(最快最准), 再回退有限深度扫描 VS 安装目录。
    # 不用 -Recurse 全盘扫(在装了 VS 的机器上会跑数十秒)。
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
        # 回退: 限制深度扫描, 避免全盘递归
        $linkExe = Get-ChildItem "C:\Program Files*\Microsoft Visual Studio" -Depth 6 -Filter link.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if ($linkExe) {
        $TARGET = "x86_64-pc-windows-msvc"
    } else {
        Write-Host "[!] 发现 msvc 工具链但未找到 link.exe(缺 VS Build Tools), 尝试 gnu" -ForegroundColor Yellow
    }
}
if (-not $TARGET -and $gnuDir -and (Test-Path $gnuDir)) {
    $TARGET = "x86_64-pc-windows-gnu"
}
if (-not $TARGET) {
    # 无 rustup 布局: 回退 PATH, 默认 msvc
    $TARGET = "x86_64-pc-windows-msvc"
    Write-Host "[!] 未发现 rustup toolchain 布局, 回退 PATH 中的 cargo ($TARGET)" -ForegroundColor Yellow
}

# ---- 2) 定位该 target 的 cargo ----
# 优先直连 toolchain 内的真实 cargo.exe, 绕开 CARGO_HOME\bin 下的 rustup shim
# (shim 是 rustup.exe 的硬链接, 转发时可能裁剪子进程 PATH)。
$toolchainDir = if ($rustupRoot) { Join-Path $toolchainsDir "stable-$TARGET" } else { $null }
$cargoExe = if ($toolchainDir) { Join-Path $toolchainDir "bin\cargo.exe" } else { $null }
if (-not $cargoExe -or -not (Test-Path $cargoExe)) {
    $cargoExe = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if ($cargoExe) {
        Write-Host "[!] 未找到 toolchain 内 cargo, 回退: $cargoExe" -ForegroundColor Yellow
    }
}
if (-not $cargoExe) { Write-Error "未找到 cargo, 请先安装 Rust (rustup)" }

# ---- 3) gnu target: 注入完整 MinGW binutils ----
if ($TARGET -eq "x86_64-pc-windows-gnu") {
    $binutilsCandidates = @()
    if ($env:MINGW_BINUTILS_BIN) { $binutilsCandidates += $env:MINGW_BINUTILS_BIN }
    # 2026-10-04 本机把 D:\Kiln\mingw-binutils 改成了 D:\Kiln\mingw64, 故两处都探测
    $binutilsCandidates += "D:\Kiln\mingw64\bin"
    $binutilsCandidates += "D:\Kiln\mingw-binutils\bin"
    # 注意: rustup self-contained 目录实测缺 as.exe, 不作为候选(否则必然误判为"已找到")
    $binutils = $binutilsCandidates | Where-Object { $_ -and (Test-Path (Join-Path $_ "as.exe")) } | Select-Object -First 1
    if (-not $binutils) {
        Write-Error @"
[gnu] 未找到含 as.exe 的完整 MinGW binutils (dlltool 链接期需要 as)。
已探测:
$(($binutilsCandidates | ForEach-Object { "  - $_" }) -join "`n")
请任选其一:
  A) 下载完整 binutils 到 D:\Kiln\mingw64\bin
     清华镜像: https://mirrors.tuna.tsinghua.edu.cn/msys2/mingw/mingw64/mingw-w64-x86_64-binutils-2.43.1-1-any.pkg.tar.zst
     解压后将 mingw64\bin 内容放入该目录
  B) 安装 Visual Studio Build Tools 后改用 msvc target(推荐, CI 即此路径)
  C) 设置环境变量 MINGW_BINUTILS_BIN 指向已有的 binutils\bin
"@
    }
    $env:PATH = "$binutils;$env:PATH"
    Write-Host "[*] MinGW binutils: $binutils" -ForegroundColor Cyan
}

# ---- 3b) 验证 as/dlltool 在当前 PATH 下确实可达 ----
# 这是本次修复的核心: 只把目录塞进 PATH 不够, 必须确认能找到, 否则会一路编译到
# 某个依赖(如 chrono)链接期才报 "error calling dlltool: program not found", 极难定位。
if ($TARGET -eq "x86_64-pc-windows-gnu") {
    foreach ($tool in @("as.exe", "dlltool.exe", "ld.exe")) {
        $found = Get-Command $tool -ErrorAction SilentlyContinue
        if (-not $found) {
            Write-Error "[gnu] PATH 中找不到 $tool —— gnu 链接期必需。请检查 binutils 注入: $binutils"
        }
        Write-Host "    $tool -> $($found.Source)" -ForegroundColor DarkGray
    }
}

# ---- 3b) 把 toolchain 的 bin 注入 PATH (rustc 供依赖 build.rs 调用) ----
# 注意顺序: toolchain bin 放在 binutils **之后**, 确保同名工具(dlltool/as/ld)
# 优先使用步骤 3 注入的完整 binutils(含 as.exe), 而非 self-contained 里的残缺版本。
if ($toolchainDir -and (Test-Path (Join-Path $toolchainDir "bin"))) {
    $cargoBinDir = Split-Path -Parent $cargoExe
    if ($cargoBinDir) { $env:PATH = "$env:PATH;$cargoBinDir" }
}

# ---- 4) 目标目录 ----
$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ($env:CARGO_TARGET_DIR) {
    $targetDir = $env:CARGO_TARGET_DIR
} elseif (Test-Path "D:\") {
    $targetDir = "D:\AI-Root-target"
} else {
    $targetDir = Join-Path $repoRoot "target"
}

# gnu 链接期必须用纯 ASCII 目标目录: MinGW binutils(dlltool/ld)按 ANSI 代码页解释
# 路径, 遇非 ASCII(如中文 "工具")会毁成 "????", 报
#   dlltool.exe: Can't create .lib file: ...\????\...\kernel32.dll_imports.lib
# 错误信息本身已乱码, 极难定位。msvc 不受影响, 但此处统一拦住以免误判。
if ($TARGET -like "*windows-gnu*") {
    if ($targetDir -match '[^\x20-\x7E]') {
        Write-Error "CARGO_TARGET_DIR 含非 ASCII 字符: $targetDir`nMinGW binutils 无法处理, 链接期会因路径乱码失败。`n请改用纯 ASCII 路径, 例如 D:\AI-Root-target"
    }
}

$env:CARGO_TARGET_DIR = $targetDir

Set-Location $repoRoot

Write-Host "[*] 构建工具链: $cargoExe" -ForegroundColor Cyan
Write-Host "[*] 目标平台  : $TARGET" -ForegroundColor Cyan
Write-Host "[*] CARGO_TARGET_DIR: $targetDir" -ForegroundColor Cyan
# 显式传 --target-dir: 不依赖 env 是否被 cargo 正确继承(CI 下更稳)
& $cargoExe build --release --target $TARGET --target-dir $targetDir
$buildExit = $LASTEXITCODE
$ErrorActionPreference = $prevEAP
if ($buildExit -ne 0) {
    Write-Error "构建失败 (exit=$buildExit)"
}
$exe = Join-Path $targetDir "$TARGET\release\AI-Root.exe"
if (-not (Test-Path $exe)) { Write-Error "构建未产出 exe: $exe" }
Write-Host "[OK] 产物: $exe" -ForegroundColor Green
Write-Host ("     SHA256: " + (Get-FileHash $exe -Algorithm SHA256).Hash) -ForegroundColor Green
