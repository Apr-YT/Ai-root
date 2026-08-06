# ai-root 一键构建脚本
#
# 用法:  powershell -NoProfile -ExecutionPolicy Bypass -File build.ps1
# 产物:  D:\wbrelay_target\x86_64-pc-windows-msvc\release\ai-root.exe
#
# 为什么必须指定真实 toolchain:
#   本机 PATH 中的 D:\Kiln\rust\cargo\bin 下所有工具是同一 12.8MB 代理二进制(全同 MD5),
#   cargo spawn rustc 时会报 "os error 193: 不是有效的 Win32 应用程序"。
#   必须使用 rustup 真实 toolchain 目录下的 cargo.exe。

$ErrorActionPreference = "Stop"

$rustupCargo = "C:\Users\AprYT\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe"
if (-not (Test-Path $rustupCargo)) {
    Write-Host "[!] 未找到 rustup 真实 toolchain, 回退到 PATH 中的 cargo" -ForegroundColor Yellow
    $rustupCargo = (Get-Command cargo -ErrorAction Stop).Source
}

$env:CARGO_TARGET_DIR = "D:\wbrelay_target"
$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $repoRoot

Write-Host "[*] 构建工具链: $rustupCargo" -ForegroundColor Cyan
& $rustupCargo build --release --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) {
    Write-Error "构建失败"
}
Write-Host "[✓] 产物: D:\wbrelay_target\x86_64-pc-windows-msvc\release\ai-root.exe" -ForegroundColor Green
