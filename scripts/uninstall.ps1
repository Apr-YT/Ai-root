# scripts/uninstall.ps1 —— 可预测、可确认、可恢复的卸载
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\uninstall.ps1 -DryRun
#   ... -File scripts\uninstall.ps1                 # 备份后卸载（默认）
#   ... -File scripts\uninstall.ps1 -NoBackup       # 不备份（不推荐）
#   ... -File scripts\uninstall.ps1 -Purge          # 卸载并彻底删除配置/日志（先备份）
#   ... -File scripts\uninstall.ps1 -Force          # 跳过交互确认
#
# 边界（与 src/service.rs::uninstall 的实现一致）:
#   删除: 服务注册、AI-Root.exe(.old/.new)、state\tmp、state\pending、
#         state\enabled.flag、secrets\token.txt、state\sid.txt、C:\Windows\AI-Root.cmd
#   保留: policy.toml、rules.d\、logs\、state\policy.initialized、
#         %LOCALAPPDATA%\AI-Root\user.toml
#   -Purge 才会连同上述保留项一起删除（且默认先备份）。
#
# 为什么默认备份: 卸载最容易出的事故不是「删不掉」，而是「本想重装却丢了
# 自己调了很久的授权策略和审计日志」。备份目录不含 token（凭据不该被复制）。

[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "High")]
param(
    # 彻底删除配置、规则、日志与用户偏好（默认只卸载服务与程序文件）
    [switch]$Purge,

    # 不备份（默认会备份配置与日志）
    [switch]$NoBackup,

    # 备份目录；默认 %ProgramData%\AI-Root-backup-<时间戳>
    [string]$BackupDir = "",

    # 只显示将发生什么，不执行任何变更（不需要管理员）
    [switch]$DryRun,

    # 跳过交互确认（仍然会打印清单）
    [switch]$Force
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

# -WhatIf 必须真的生效。
#
# 本脚本声明了 SupportsShouldProcess，但写操作分散在「备份 / 调 AI-Root uninstall /
# -Purge 删除」三处，逐处调用 $PSCmdlet.ShouldProcess 很容易漏掉一处 ——
# 而漏掉一处就意味着：用户以为加了 -WhatIf 是演练，实际却真的删了东西。
# 因此统一在入口把 -WhatIf 折叠成 -DryRun，保证「只要带上就绝不落盘」。
if ($WhatIfPreference) { $DryRun = $true }

$repoRoot = Get-RepoRoot
$dataDir = Join-Path $env:ProgramData "AI-Root"
$userDir = Join-Path $env:LOCALAPPDATA "AI-Root"

# ------------------------------------------------------------
# 1. 清点
# ------------------------------------------------------------
$svc = Get-AiRootServiceState
$found = Find-AiRootExe -RepoRoot $repoRoot

$deleteTargets = @()
$keepTargets = @()

if ($svc.Exists) { $deleteTargets += "服务注册: AI-Root（当前 $($svc.State)）" }
# 只有**部署目录内**的 exe 才属于安装内容。
# 构建产物（target\ 或 %LOCALAPPDATA%\AI-Root\build-target）是用户的编译结果，
# 卸载绝不能删 —— 删了下次还得从头编译，且完全超出「卸载服务」的语义。
if ($found.Path -and ($found.Path -like "$dataDir\*")) {
    $deleteTargets += "程序文件: $($found.Path)"
}
foreach ($n in @("AI-Root.exe", "AI-Root.exe.old", "AI-Root.exe.new")) {
    $p = Join-Path $dataDir $n
    if (Test-Path $p) { $deleteTargets += "程序文件: $p" }
}
foreach ($n in @("secrets\token.txt", "state\sid.txt", "state\enabled.flag")) {
    $p = Join-Path $dataDir $n
    if (Test-Path $p) { $deleteTargets += "凭据/状态: $p" }
}
foreach ($n in @("state\tmp", "state\pending")) {
    $p = Join-Path $dataDir $n
    if (Test-Path $p) { $deleteTargets += "运行期残留: $p" }
}
if (Test-Path "C:\Windows\AI-Root.cmd") { $deleteTargets += "全局包装器: C:\Windows\AI-Root.cmd" }

foreach ($p in @(
    (Join-Path $dataDir "policy.toml"),
    (Join-Path $dataDir "rules.d"),
    (Join-Path $dataDir "logs"),
    (Join-Path $dataDir "state\policy.initialized"),
    (Join-Path $userDir "user.toml")
)) {
    if (Test-Path $p) {
        if ($Purge) { $deleteTargets += "用户数据(-Purge): $p" }
        else { $keepTargets += $p }
    }
}

Write-Host ""
Write-Host "=== AI-Root 卸载清单 ===" -ForegroundColor Cyan

if ($deleteTargets.Count -eq 0 -and $keepTargets.Count -eq 0) {
    Write-Host "  未发现已安装的服务或数据，无需卸载。" -ForegroundColor Green
    exit 0
}

Write-Host ""
Write-Host "  将删除:" -ForegroundColor Red
foreach ($t in $deleteTargets) { Write-Host "    - $t" -ForegroundColor Red }

if ($keepTargets.Count -gt 0) {
    Write-Host ""
    Write-Host "  将保留（重新安装后继续生效）:" -ForegroundColor Green
    foreach ($t in $keepTargets) { Write-Host "    - $t" -ForegroundColor Green }
}

# ------------------------------------------------------------
# 2. DryRun
# ------------------------------------------------------------
if ($DryRun) {
    Write-Host ""
    Write-Host "=== DryRun：未做任何变更 ===" -ForegroundColor Yellow
    # -Purge 也会先备份（见下方备份段），这里不要错误地只在非 Purge 时提示。
    if (-not $NoBackup) {
        Write-Host "  实际执行时会先备份配置与日志到 %ProgramData%\AI-Root-backup-<时间戳>" -ForegroundColor DarkGray
    }
    Write-Host "  实际执行: scripts\uninstall.ps1 $($(if ($Purge) { '-Purge ' } else { '' }))$($(if ($Force) { '-Force' } else { '' }))" -ForegroundColor DarkGray
    exit 0
}

# ------------------------------------------------------------
# 3. 提权
# ------------------------------------------------------------
if (-not (Test-IsAdmin)) {
    Write-Host ""
    Write-Warn "卸载服务需要管理员权限，正在请求 UAC..."
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($Purge) { $argList += "-Purge" }
    if ($NoBackup) { $argList += "-NoBackup" }
    if ($BackupDir) { $argList += @("-BackupDir", "`"$BackupDir`"") }
    if ($Force) { $argList += "-Force" }

    try {
        $p = Start-Process -FilePath "powershell.exe" -ArgumentList $argList -Verb RunAs -Wait -PassThru
        exit $p.ExitCode
    } catch {
        Write-Bad "提权被取消或失败: $($_.Exception.Message)"
        exit 1
    }
}

# ------------------------------------------------------------
# 4. 确认
# ------------------------------------------------------------
if (-not $Force) {
    $prompt = if ($Purge) {
        "将卸载服务并**彻底删除**配置、规则、日志与用户偏好（会先备份）。继续? [y/N]"
    } else {
        "将卸载服务并删除程序文件与凭据，配置与日志保留。继续? [y/N]"
    }
    $answer = Read-Host "  $prompt"
    if ($answer -notmatch "^(y|yes)$") {
        Write-Info "已取消，未做变更。"
        exit 0
    }
}

# ------------------------------------------------------------
# 5. 备份（默认执行）
# ------------------------------------------------------------
if (-not $NoBackup) {
    if (-not $BackupDir) {
        $BackupDir = Join-Path $env:ProgramData ("AI-Root-backup-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
    }
    Write-Host ""
    Write-Host "=== 备份配置与日志 ===" -ForegroundColor Cyan
    try {
        New-Item -ItemType Directory -Force -Path $BackupDir | Out-Null
        $backedUp = 0
        foreach ($p in @(
            (Join-Path $dataDir "policy.toml"),
            (Join-Path $dataDir "rules.d"),
            (Join-Path $dataDir "logs"),
            (Join-Path $dataDir "state\policy.initialized"),
            (Join-Path $userDir "user.toml")
        )) {
            if (-not (Test-Path $p)) { continue }
            $isDir = (Get-Item $p).PSIsContainer
            if ($isDir) {
                Copy-Item $p -Destination $BackupDir -Recurse -Force -ErrorAction Stop
            } else {
                Copy-Item $p -Destination (Join-Path $BackupDir (Split-Path $p -Leaf)) -Force -ErrorAction Stop
            }
            $backedUp++
        }
        if ($backedUp -eq 0) {
            Write-Info "没有需要备份的配置或日志。"
        } else {
            Write-Ok "已备份 $backedUp 项到: $BackupDir"
            Write-Info "（备份不含令牌等凭据）"
        }
    } catch {
        Write-Bad "备份失败: $($_.Exception.Message)"
        Write-Info "为避免丢数据，已中止卸载。可加 -NoBackup 显式跳过（不推荐）。"
        exit 1
    }
}

# ------------------------------------------------------------
# 6. 执行卸载
# ------------------------------------------------------------
Write-Host ""
Write-Host "=== 卸载 ===" -ForegroundColor Cyan

if ($svc.Exists) {
    if (-not $found.Path) {
        Write-Bad "服务已安装但找不到 AI-Root.exe，无法调用官方卸载流程。"
        Write-Info "可手工执行: sc.exe delete AI-Root，然后清理 $dataDir"
        exit 1
    }
    $u = Invoke-AiRoot -Exe $found.Path -Arguments @("uninstall")
    Write-Host $u.Output
    if ($u.ExitCode -ne 0) {
        Write-Bad "AI-Root uninstall 失败（exit=$($u.ExitCode)）"
        Write-Info "服务可能仍存在，请检查: sc.exe query AI-Root"
        exit 1
    }
} else {
    Write-Info "服务未安装，仅清理残留文件。"
}

# ------------------------------------------------------------
# 7. -Purge：彻底删除保留项
# ------------------------------------------------------------
if ($Purge) {
    Write-Host ""
    Write-Host "=== -Purge：彻底删除用户数据 ===" -ForegroundColor Cyan
    foreach ($d in @($dataDir, $userDir)) {
        if (Test-Path $d) {
            try {
                Remove-Item $d -Recurse -Force -ErrorAction Stop
                Write-Ok "已删除: $d"
            } catch {
                Write-Warn "删除失败 $d : $($_.Exception.Message)"
            }
        }
    }
}

# ------------------------------------------------------------
# 8. 校验
# ------------------------------------------------------------
Write-Host ""
Write-Host "=== 卸载后校验 ===" -ForegroundColor Cyan
$svc2 = Get-AiRootServiceState
if ($svc2.Exists) {
    Write-Bad "服务仍存在（状态 $($svc2.State)），卸载未生效。"
    exit 1
}
Write-Ok "服务已不存在"

if ($Purge) {
    if (-not (Test-Path $dataDir)) { Write-Ok "数据目录已删除: $dataDir" }
    else { Write-Warn "数据目录仍存在: $dataDir" }
} else {
    Write-Ok "配置与日志已保留在: $dataDir"
}

Write-Host ""
Write-Host "=== 完成 ===" -ForegroundColor Green
if (-not $NoBackup -and $BackupDir -and (Test-Path $BackupDir)) {
    Write-Host "  备份位置: $BackupDir"
    Write-Host "  恢复方法: 把备份中的 policy.toml / rules.d / logs 复制回 $dataDir，再执行 scripts\install.ps1"
}
exit 0
