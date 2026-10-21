# scripts/build.ps1 —— 一键 Release 编译
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1
#   ... -File scripts\build.ps1 -Target msvc          # 强制 MSVC
#   ... -File scripts\build.ps1 -Target gnu -Clean    # 清理后重建
#
# 设计:
#   - 真正干活的是仓库根的 build.ps1（CI 与开发者都用它），本脚本只负责
#     「环境适配 + 结果报告」，避免出现第二套构建逻辑各自漂移。
#   - 仓库路径含中文时自动把 CARGO_TARGET_DIR 切到 ASCII 目录，用户不必手工设置。
#   - 输出 EXE 绝对路径、大小、SHA256，方便 install.ps1 或交付时核对。

[CmdletBinding()]
param(
    [ValidateSet("auto", "msvc", "gnu")]
    [string]$Target = "auto",

    # 先清理该目标的产物再编译
    [switch]$Clean,

    # 只输出结果，不打印额外说明
    [switch]$Quiet
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

$repoRoot = Get-RepoRoot
$rootBuild = Join-Path $repoRoot "build.ps1"

if (-not (Test-Path $rootBuild)) {
    Write-Bad "找不到根构建脚本: $rootBuild"
    exit 1
}

# ------------------------------------------------------------
# 1. 产物目录：非 ASCII 仓库路径必须切到 ASCII 目录
# ------------------------------------------------------------
$buildDir = Get-BuildTargetDir $repoRoot
if (-not $env:CARGO_TARGET_DIR -and -not (Test-AsciiPath $repoRoot)) {
    $env:CARGO_TARGET_DIR = $buildDir
    if (-not $Quiet) {
        Write-Warn "仓库路径含非 ASCII 字符，产物目录改用: $buildDir"
        Write-Info "原因: MinGW 的 ld/dlltool 按 ANSI 代码页解释路径，中文目录会导致链接失败"
    }
}

# ------------------------------------------------------------
# 2. 目标选择
# ------------------------------------------------------------
if ($Target -ne "auto") {
    $env:AIROOT_TARGET = "x86_64-pc-windows-$Target"
    if (-not $Quiet) { Write-Note "强制目标: $($env:AIROOT_TARGET)" }
}

# ------------------------------------------------------------
# 3. 可选清理
# ------------------------------------------------------------
if ($Clean) {
    $cargo = Get-ToolPath "cargo.exe"
    if (-not $cargo) {
        Write-Bad "找不到 cargo.exe，无法清理"
        exit 1
    }
    $cleanArgs = @("clean")
    if ($env:AIROOT_TARGET) { $cleanArgs += @("--target", $env:AIROOT_TARGET) }
    if (-not $Quiet) { Write-Note "[*] 清理产物: cargo $($cleanArgs -join ' ')" }
    # cargo 按进程当前目录查找 Cargo.toml，必须显式切到仓库根。
    Push-Location $repoRoot
    try {
        & $cargo @cleanArgs | Out-Null
    } finally {
        Pop-Location
    }
}

# ------------------------------------------------------------
# 4. 调用根构建脚本（真实构建路径）
# ------------------------------------------------------------
if (-not $Quiet) {
    Write-Host ""
    Write-Host "=== 编译 (release) ===" -ForegroundColor Cyan
}

$buildOk = $true
$buildError = ""
try {
    & $rootBuild
    if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
        $buildOk = $false
        $buildError = "根构建脚本返回 $LASTEXITCODE"
    }
} catch {
    $buildOk = $false
    $buildError = $_.Exception.Message
}

if (-not $buildOk) {
    Write-Host ""
    Write-Bad "构建失败: $buildError"
    Write-Info "常见原因与处理:"
    Write-Info "  - 找不到 as.exe / link.exe  → 运行 scripts\doctor.ps1 查看缺失项"
    Write-Info "  - 中文路径导致 cannot find ...rlib → 本脚本已自动切 ASCII 产物目录，确认未被 CARGO_TARGET_DIR 覆盖"
    Write-Info "  - 编译错误 → 看上方 rustc 输出"
    exit 1
}

# ------------------------------------------------------------
# 5. 定位并报告产物
# ------------------------------------------------------------
$exe = Find-AiRootExe -RepoRoot $repoRoot
if (-not $exe.Path) {
    Write-Bad "构建完成但找不到产物: $($exe.Source)"
    exit 1
}

$item = Get-Item $exe.Path
$sha = (Get-FileHash $exe.Path -Algorithm SHA256).Hash
$ver = ""
$v = Invoke-AiRoot -Exe $exe.Path -Arguments @("--version")
if ($v.ExitCode -eq 0 -and $v.Output) { $ver = ($v.Output -split "`r?`n")[0] }

Write-Host ""
Write-Host "=== 构建成功 ===" -ForegroundColor Green
Write-Host "  EXE     : $($exe.Path)"
if ($ver) {
    Write-Host "  版本    : $ver"
}
Write-Host ("  大小    : {0:N1} MB" -f ($item.Length / 1MB))
Write-Host "  SHA256  : $sha"
Write-Host "  构建于  : $($item.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))"
Write-Host ""
Write-Host "下一步:" -ForegroundColor Gray
Write-Host "  测试     : scripts\test.ps1"
Write-Host "  安装     : scripts\install.ps1" -ForegroundColor Gray
Write-Host "  换版本  : scripts\install.ps1 -Force   (本地构建无签名，-Update 会被签名门控拒绝)" -ForegroundColor Gray

exit 0
