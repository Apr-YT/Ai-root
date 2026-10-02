# AI-Root 一键构建脚本(本机与 CI 通用)
#
# 用法:  powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
# 产物:  $CARGO_TARGET_DIR\x86_64-pc-windows-msvc\release\AI-Root.exe
#
# 为什么固定使用 msvc target (x86_64-pc-windows-msvc):
#   本机早期尝试 gnu target 时, 链接阶段会依赖 dlltool/binutils, 出现
#   "error calling dlltool 'dlltool.exe'" / "could not create import library" 等失败;
#   msvc target 使用 link.exe, 完全绕开 binutils 链路, 且与 build.rs 的 rc.exe 资源分支、
#   以及 CI(GitHub Actions windows-latest 自带 MSVC + Windows SDK) 完全一致。
#   因此本项目统一 msvc target, 不再提供 gnu 分支。
#
# 为什么优先使用 rustup 真实 toolchain:
#   本机(开发环境)的 PATH 中 D:\Kiln\rust\cargo\bin 下所有工具是同一 12.8MB 代理二进制(全同 MD5),
#   cargo spawn rustc 时会报 "os error 193: 不是有效的 Win32 应用程序"。
#   CI(GitHub Actions windows-latest)下该问题不存在, 但为统一路径逻辑, 同样优先真实 toolchain:
#   $env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe
#   不存在时回退到 PATH 中的 cargo。

$ErrorActionPreference = "Stop"

$TARGET = "x86_64-pc-windows-msvc"

# 1) 定位真实 toolchain 的 cargo(本机 AprYT / CI runneradmin 通用)
$rustupCargo = Join-Path $env:USERPROFILE ".rustup\toolchains\stable-$TARGET\bin\cargo.exe"
if (-not (Test-Path $rustupCargo)) {
    Write-Host "[!] 未找到 rustup 真实 toolchain, 回退到 PATH 中的 cargo" -ForegroundColor Yellow
    $rustupCargo = (Get-Command cargo -ErrorAction Stop).Source
}

# 2) 确保 msvc target 已安装(rustup 环境; PATH 回退的独立 cargo 跳过此步)
$rustupExe = Join-Path $env:USERPROFILE ".rustup\toolchains\stable-$TARGET\bin\rustup.exe"
if (Test-Path $rustupExe) {
    $installed = & $rustupExe target list --installed 2>$null
    if ($installed -notmatch [regex]::Escape($TARGET)) {
        Write-Host "[*] 安装缺失的 target: $TARGET" -ForegroundColor Cyan
        & $rustupExe target add $TARGET
    }
}

# 3) 目标目录: 环境变量优先(CI 可注入), 否则本机惯例 D:\AI-Root-target, 否则仓库内 target
$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ($env:CARGO_TARGET_DIR) {
    $targetDir = $env:CARGO_TARGET_DIR
} elseif (Test-Path "D:\") {
    $targetDir = "D:\AI-Root-target"
} else {
    $targetDir = Join-Path $repoRoot "target"
}
$env:CARGO_TARGET_DIR = $targetDir

Set-Location $repoRoot

Write-Host "[*] 构建工具链: $rustupCargo" -ForegroundColor Cyan
Write-Host "[*] 目标平台  : $TARGET" -ForegroundColor Cyan
Write-Host "[*] CARGO_TARGET_DIR: $targetDir" -ForegroundColor Cyan
& $rustupCargo build --release --target $TARGET
if ($LASTEXITCODE -ne 0) {
    Write-Error "构建失败"
}
$exe = Join-Path $targetDir "$TARGET\release\AI-Root.exe"
Write-Host "[✓] 产物: $exe" -ForegroundColor Green
