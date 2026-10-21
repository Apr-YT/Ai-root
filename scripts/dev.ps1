# scripts/dev.ps1 —— 统一入口
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev.ps1 -Action doctor
#   ... -Action build [-Target msvc|gnu|auto] [-Clean]
#   ... -Action test  [-Filter <名字>]
#   ... -Action install [-PreflightOnly|-Update|-Force] [-Exe <路径>]
#   ... -Action uninstall [-Purge|-DryRun] [-Force]
#   ... -Action all      # doctor -> build -> test（不自动安装，避免意外提权）
#
# 为什么用子进程调用各脚本而不是点源:
#   每个子脚本都会 `exit <code>`，点源会直接终止 dev.ps1，无法逐步汇总结果。
#   用 powershell -File 各自独立进程，能拿到真实退出码，且互不污染状态。

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet("doctor", "build", "test", "install", "uninstall", "all")]
    [string]$Action = "doctor",

    [string]$Exe = "",

    [ValidateSet("auto", "msvc", "gnu")]
    [string]$Target = "auto",

    [string]$Filter = "",

    [switch]$Clean,
    [switch]$PreflightOnly,
    [switch]$Update,
    [switch]$Force,
    [switch]$Purge,
    [switch]$DryRun,
    [switch]$Yes,
    # 透传给 install.ps1：未签名即拒绝（分发给他人时用）
    [switch]$StrictSignature
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

$repoRoot = Get-RepoRoot

function Invoke-Step {
    param(
        [string]$Name,
        [string]$ScriptName,
        [string[]]$Arguments = @()
    )

    $path = Join-Path $PSScriptRoot $ScriptName
    if (-not (Test-Path $path)) {
        Write-Bad "找不到脚本: $path"
        return 1
    }

    Write-Host ""
    Write-Host ("### {0}" -f $Name) -ForegroundColor White
    Write-Host ("    脚本: scripts\{0}" -f $ScriptName) -ForegroundColor DarkGray

    $psArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $path) + $Arguments
    # 关键：子进程的输出必须送到**主机**而不是留在管道里。
    # PowerShell 函数的返回值 = 所有未捕获输出 + return 值；若直接 `& powershell.exe`
    # 不接管输出，子脚本的每一行 stdout 都会混进返回值，使 `$c` 变成 Object[]
    # 而不是退出码（实测报 "无法将 System.Object[] 转换为 System.Int32"）。
    #
    # `2>&1` 也必须加：cargo 把编译进度写到 stderr，不重定向的话 PowerShell 会把它
    # 包装成 NativeCommandError 打到错误流，看起来像失败（实际 exit=0），非常误导。
    & powershell.exe @psArgs 2>&1 | ForEach-Object { Write-Host $_ }
    $code = $LASTEXITCODE
    return $code
}

$results = New-Object System.Collections.ArrayList

function Add-StepResult([string]$Name, [int]$Code) {
    [void]$results.Add([pscustomobject]@{ Name = $Name; Code = $Code })
}

switch ($Action) {
    "doctor" {
        $c = Invoke-Step "环境体检" "doctor.ps1"
        Add-StepResult "doctor" $c
    }

    "build" {
        $a = @("-Target", $Target)
        if ($Clean) { $a += "-Clean" }
        $c = Invoke-Step "Release 编译" "build.ps1" $a
        Add-StepResult "build" $c
    }

    "test" {
        $a = @("-Target", $Target)
        if ($Filter) { $a += @("-Filter", $Filter) }
        $c = Invoke-Step "运行测试" "test.ps1" $a
        Add-StepResult "test" $c
    }

    "install" {
        $a = @()
        if ($Exe) { $a += @("-Exe", $Exe) }
        if ($PreflightOnly) { $a += "-PreflightOnly" }
        if ($Update) { $a += "-Update" }
        if ($Force) { $a += "-Force" }
        if ($Yes) { $a += "-Yes" }
        if ($StrictSignature) { $a += "-StrictSignature" }
        $c = Invoke-Step "安装 / 更新" "install.ps1" $a
        Add-StepResult "install" $c
    }

    "uninstall" {
        $a = @()
        if ($Purge) { $a += "-Purge" }
        if ($DryRun) { $a += "-DryRun" }
        if ($Force) { $a += "-Force" }
        $c = Invoke-Step "卸载" "uninstall.ps1" $a
        Add-StepResult "uninstall" $c
    }

    "all" {
        # doctor 不致命也继续（例如只缺 MSVC 但本机用 gnu），但 doctor 返回 1 时给出提示。
        $c1 = Invoke-Step "环境体检" "doctor.ps1"
        Add-StepResult "doctor" $c1

        $b = @("-Target", $Target)
        if ($Clean) { $b += "-Clean" }
        $c2 = Invoke-Step "Release 编译" "build.ps1" $b
        Add-StepResult "build" $c2

        if ($c2 -eq 0) {
            $t = @("-Target", $Target)
            if ($Filter) { $t += @("-Filter", $Filter) }
            $c3 = Invoke-Step "运行测试" "test.ps1" $t
            Add-StepResult "test" $c3
        } else {
            Write-Host ""
            Write-Warn "构建失败，跳过测试。"
            Add-StepResult "test" -1
        }

        Write-Host ""
        Write-Host "说明: -Action all 不包含 install —— 安装会请求 UAC 并改变系统状态，" -ForegroundColor DarkGray
        Write-Host "      请显式执行 scripts\dev.ps1 -Action install" -ForegroundColor DarkGray
    }
}

# ------------------------------------------------------------
# 汇总
# ------------------------------------------------------------
Write-Host ""
Write-Host "=== 汇总 ($Action) ===" -ForegroundColor Cyan
$worst = 0
foreach ($r in $results) {
    if ($r.Code -eq 0) {
        Write-Host ("  [OK]   {0}" -f $r.Name) -ForegroundColor Green
    } elseif ($r.Code -eq -1) {
        Write-Host ("  [SKIP] {0}" -f $r.Name) -ForegroundColor DarkGray
        if ($worst -eq 0) { $worst = 1 }
    } else {
        Write-Host ("  [FAIL] {0} (exit={1})" -f $r.Name, $r.Code) -ForegroundColor Red
        $worst = 1
    }
}

if ($worst -ne 0) { exit 1 }
exit 0
