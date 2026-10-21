# scripts/test.ps1 —— 运行测试并区分「通过 / 失败 / 无法在当前环境执行」
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test.ps1
#   ... -File scripts\test.ps1 -Filter policy      # 只跑名字含 policy 的测试
#   ... -File scripts\test.ps1 -Target msvc
#
# 说明:
#   - 本项目部分测试需要真实 Windows 服务 / SYSTEM 上下文 / 有效签名样本。
#     缺少条件时测试自身会打印 [skip] 并返回（不谎报通过），本脚本把这些单独统计。
#   - 另有一批行为在当前环境「根本无法触发」（如 install 的真实 SCM 注册），
#     本脚本在结尾明确列出，避免把「测试全绿」误读成「全部已验证」。

[CmdletBinding()]
param(
    [string]$Filter = "",

    [ValidateSet("auto", "msvc", "gnu")]
    [string]$Target = "auto",

    # 结构化输出（便于 CI/自动化消费）
    [switch]$Json
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

$repoRoot = Get-RepoRoot
$cargo = Get-ToolPath "cargo.exe"
if (-not $cargo) {
    Write-Bad "找不到 cargo.exe，请先安装 Rust（https://rustup.rs）或运行 scripts\doctor.ps1"
    exit 1
}

# 与 build.ps1 相同的路径适配
$buildDir = Get-BuildTargetDir $repoRoot
if (-not $env:CARGO_TARGET_DIR -and -not (Test-AsciiPath $repoRoot)) {
    $env:CARGO_TARGET_DIR = $buildDir
}

$cargoArgs = @("test", "--release")
if ($Target -ne "auto") {
    $cargoArgs += @("--target", "x86_64-pc-windows-$Target")
}
if ($Filter) {
    $cargoArgs += @($Filter)
}

if (-not $Json) {
    Write-Host ""
    Write-Host "=== 运行测试 ===" -ForegroundColor Cyan
    Write-Host "  命令: cargo $($cargoArgs -join ' ')" -ForegroundColor DarkGray
    if ($env:CARGO_TARGET_DIR) {
        Write-Host "  产物: $($env:CARGO_TARGET_DIR)" -ForegroundColor DarkGray
    }
    Write-Host ""
}

# ------------------------------------------------------------
# 执行并实时转发输出
# ------------------------------------------------------------
$collected = New-Object System.Collections.ArrayList
# 必须切到仓库根再调用 cargo：cargo 按**进程当前目录**查找 Cargo.toml，
# 而用户可能在任意目录执行本脚本（例如从仓库父目录运行 scripts\test.ps1）。
Push-Location $repoRoot
try {
    & $cargo @cargoArgs 2>&1 | ForEach-Object {
        $line = $_.ToString()
        [void]$collected.Add($line)
        if (-not $Json) { Write-Host $line }
    }
    $exitCode = $LASTEXITCODE
} finally {
    Pop-Location
}

# ------------------------------------------------------------
# 解析
# ------------------------------------------------------------
$passed = 0
$failed = 0
$ignored = 0
$failNames = New-Object System.Collections.ArrayList
$skipNotes = New-Object System.Collections.ArrayList

foreach ($l in $collected) {
    if ($l -match "test result:\s+\w+\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored") {
        $passed += [int]$Matches[1]
        $failed += [int]$Matches[2]
        $ignored += [int]$Matches[3]
    }
    if ($l -match "^----\s+(.+?)\s+stdout\s+----") {
        [void]$failNames.Add($Matches[1])
    }
    if ($l -match "\[skip\]") {
        [void]$skipNotes.Add(($l -replace "^\s+", "").Trim())
    }
}

# 环境能力（用于解释 skip 与「无法覆盖」）
$isAdmin = Test-IsAdmin
$svc = Get-AiRootServiceState
$exe = Find-AiRootExe -RepoRoot $repoRoot

if ($Json) {
    [pscustomobject]@{
        Passed     = $passed
        Failed     = $failed
        Ignored    = $ignored
        ExitCode   = $exitCode
        Failures   = $failNames
        Skips      = $skipNotes
        Environment = [pscustomobject]@{
            Admin        = $isAdmin
            ServiceState = $svc.State
            Exe          = $exe.Path
            TargetDir    = $env:CARGO_TARGET_DIR
        }
    } | ConvertTo-Json -Depth 5
    if ($exitCode -ne 0) { exit 1 } else { exit 0 }
}

# ------------------------------------------------------------
# 报告
# ------------------------------------------------------------
Write-Host ""
Write-Host "=== 测试结果 ===" -ForegroundColor Cyan
Write-Host ("  通过      : {0}" -f $passed) -ForegroundColor Green
if ($failed -gt 0) {
    Write-Host ("  失败      : {0}" -f $failed) -ForegroundColor Red
} else {
    Write-Host ("  失败      : {0}" -f $failed) -ForegroundColor Green
}
Write-Host ("  忽略      : {0}" -f $ignored) -ForegroundColor DarkGray
Write-Host ("  环境跳过  : {0}" -f $skipNotes.Count) -ForegroundColor Yellow

if ($failNames.Count -gt 0) {
    Write-Host ""
    Write-Host "失败用例:" -ForegroundColor Red
    foreach ($n in $failNames) { Write-Host "  - $n" -ForegroundColor Red }
}

if ($skipNotes.Count -gt 0) {
    Write-Host ""
    Write-Host "因当前环境缺少条件而跳过的断言（测试如实声明，不算通过）:" -ForegroundColor Yellow
    foreach ($n in $skipNotes) { Write-Host "  - $n" -ForegroundColor Yellow }
}

# ------------------------------------------------------------
# 当前环境无法覆盖的部分（基于项目实际验证边界，非猜测）
# ------------------------------------------------------------
Write-Host ""
Write-Host "当前环境无法覆盖、需在可恢复虚拟机验证的项:" -ForegroundColor DarkGray
$notCovered = @(
    "install / start / stop / uninstall 的真实 SCM 生命周期"
    "服务以 LocalSystem 运行时的目录 ACL 行为"
    "update 端到端：签名包替换、健康检查失败后自动回滚"
    "域环境下的管道 SDDL 与非交互登录连接"
)
if ($svc.Exists) {
    Write-Host "  （检测到本机已安装 AI-Root 服务，但仍未执行破坏性用例）" -ForegroundColor DarkGray
    # 已装服务并不改变上面前三项：测试套件本身不做真实安装/卸载。
}
foreach ($n in $notCovered) { Write-Host "  - $n" -ForegroundColor DarkGray }

Write-Host ""
if ($exitCode -eq 0) {
    Write-Host "结论: 测试通过（失败 $failed，忽略 $ignored，环境跳过 $($skipNotes.Count)）。" -ForegroundColor Green
    exit 0
} else {
    Write-Host "结论: 测试失败（exit=$exitCode）。" -ForegroundColor Red
    exit 1
}
