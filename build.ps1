# ai-root 一键构建脚本(本机与 CI 通用)
#
# 用法:  powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
# 产物:  $CARGO_TARGET_DIR\x86_64-pc-windows-msvc\release\ai-root.exe
#
# 为什么优先使用 rustup 真实 toolchain:
#   本机(开发环境)的 PATH 中 D:\Kiln\rust\cargo\bin 下所有工具是同一 12.8MB 代理二进制(全同 MD5),
#   cargo spawn rustc 时会报 "os error 193: 不是有效的 Win32 应用程序"。
#   CI(GitHub Actions windows-latest)下该问题不存在, 但为统一路径逻辑, 同样优先真实 toolchain:
#   $env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe
#   不存在时回退到 PATH 中的 cargo。

$ErrorActionPreference = "Stop"

# 1) 定位真实 toolchain 的 cargo(本机 AprYT / CI runneradmin 通用)
$rustupCargo = Join-Path $env:USERPROFILE ".rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe"
if (-not (Test-Path $rustupCargo)) {
    Write-Host "[!] 未找到 rustup 真实 toolchain, 回退到 PATH 中的 cargo" -ForegroundColor Yellow
    $rustupCargo = (Get-Command cargo -ErrorAction Stop).Source
}

# 2) 目标目录: 环境变量优先(CI 可注入), 否则本机惯例 D:\ai-root-target, 否则仓库内 target
$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ($env:CARGO_TARGET_DIR) {
    $targetDir = $env:CARGO_TARGET_DIR
} elseif (Test-Path "D:\") {
    $targetDir = "D:\ai-root-target"
} else {
    $targetDir = Join-Path $repoRoot "target"
}
$env:CARGO_TARGET_DIR = $targetDir

Set-Location $repoRoot

Write-Host "[*] 构建工具链: $rustupCargo" -ForegroundColor Cyan
Write-Host "[*] CARGO_TARGET_DIR: $targetDir" -ForegroundColor Cyan
& $rustupCargo build --release --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) {
    Write-Error "构建失败"
}
Write-Host "[✓] 产物: $targetDir\x86_64-pc-windows-msvc\release\ai-root.exe" -ForegroundColor Green
