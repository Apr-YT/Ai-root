# scripts/_common.ps1 —— 共享工具库
#
# 被其它脚本以点源方式引用:
#   . (Join-Path $PSScriptRoot "_common.ps1")
#
# ⚠ 本文件（以及 scripts\ 下所有 .ps1）必须保存为 **UTF-8 with BOM**。
#   Windows PowerShell 5.1 读取 .ps1 时按系统 ANSI 代码页解码（中文机器为 GBK/936），
#   无 BOM 的 UTF-8 中文会变成乱码，甚至因乱码产生不配对的引号而报语法错误。
#   仓库根目录的 install-service.ps1 早就记录过这个坑，此处沿用同一约定。

$ErrorActionPreference = "Continue"

# ============================================================
# 输出
# ============================================================
$script:AiRootWarnCount = 0
$script:AiRootFailCount = 0

function Write-Head([string]$Text) {
    Write-Host ""
    Write-Host "=== $Text ===" -ForegroundColor Cyan
}

function Write-Ok([string]$Text) {
    Write-Host "  [OK]   $Text" -ForegroundColor Green
}

function Write-Warn([string]$Text) {
    $script:AiRootWarnCount++
    Write-Host "  [WARN] $Text" -ForegroundColor Yellow
}

function Write-Bad([string]$Text) {
    $script:AiRootFailCount++
    Write-Host "  [FAIL] $Text" -ForegroundColor Red
}

function Write-Info([string]$Text) {
    Write-Host "         $Text" -ForegroundColor DarkGray
}

function Write-Note([string]$Text) {
    Write-Host ""
    Write-Host "  $Text" -ForegroundColor Gray
}

function Get-AiRootWarnCount { return $script:AiRootWarnCount }
function Get-AiRootFailCount { return $script:AiRootFailCount }

function Reset-AiRootCounters {
    $script:AiRootWarnCount = 0
    $script:AiRootFailCount = 0
}

# ============================================================
# 仓库与路径
# ============================================================
function Get-RepoRoot {
    # _common.ps1 位于 <repo>\scripts\，故父目录即仓库根。
    return (Split-Path -Parent $PSScriptRoot)
}

function Test-AsciiPath([string]$Path) {
    # .NET 的字符串是 UTF-16，逐字符判断是否超出 ASCII 范围即可。
    foreach ($c in $Path.ToCharArray()) {
        if ([int]$c -gt 127) { return $false }
    }
    return $true
}

function Get-BuildTargetDir([string]$RepoRoot) {
    # cargo 产物目录。
    #
    # 非 ASCII 仓库路径下必须改用 ASCII 目录：MinGW 的 ld/dlltool 按 ANSI 代码页
    # 解释路径，中文目录会报
    #   cannot find ...\工具\...\libsyn-xxx.rlib
    # （文件明明存在却找不到）。CI 与纯 ASCII 路径的机器不受影响，仍用仓库内 target\。
    if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
    if (-not (Test-AsciiPath $RepoRoot)) {
        $base = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { $env:TEMP }
        return (Join-Path $base "AI-Root\build-target")
    }
    return (Join-Path $RepoRoot "target")
}

function Find-AiRootExe {
    # 定位可用的 AI-Root.exe。
    # 返回 [pscustomobject]@{ Path = <string or $null>; Source = <说明> }
    param(
        [string]$RepoRoot,
        [string]$Explicit
    )

    if ($Explicit) {
        if (Test-Path $Explicit) {
            return [pscustomobject]@{ Path = (Resolve-Path $Explicit).Path; Source = "命令行 -Exe" }
        }
        return [pscustomobject]@{ Path = $null; Source = "命令行 -Exe 指向的文件不存在: $Explicit" }
    }

    if ($env:AIROOT_EXE -and (Test-Path $env:AIROOT_EXE)) {
        return [pscustomobject]@{ Path = (Resolve-Path $env:AIROOT_EXE).Path; Source = "环境变量 AIROOT_EXE" }
    }

    $buildDir = Get-BuildTargetDir $RepoRoot
    if (Test-Path $buildDir) {
        # 优先 <build>\<triple>\release\AI-Root.exe（显式 --target 的产物）
        $cands = @()
        $cands += Get-ChildItem -Path $buildDir -Filter "AI-Root.exe" -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "\\release\\" } |
            Sort-Object LastWriteTime -Descending
        foreach ($c in $cands) {
            return [pscustomobject]@{ Path = $c.FullName; Source = "构建产物" }
        }
    }

    $installed = Join-Path $env:ProgramData "AI-Root\AI-Root.exe"
    if (Test-Path $installed) {
        return [pscustomobject]@{ Path = $installed; Source = "已安装的部署副本" }
    }

    $onPath = Get-Command "AI-Root.exe" -ErrorAction SilentlyContinue
    if ($onPath) {
        return [pscustomobject]@{ Path = $onPath.Source; Source = "PATH" }
    }

    return [pscustomobject]@{ Path = $null; Source = "未找到（先执行 scripts\build.ps1）" }
}

# ============================================================
# 管理员 / 服务状态
# ============================================================
function Test-IsAdmin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($id)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-AiRootServiceState {
    # 用 sc.exe 而不是 Get-Service：不依赖服务控制管理器的 PowerShell 提供程序，
    # 且能区分「不存在」与「存在但查询失败」。
    $raw = & sc.exe query "AI-Root" 2>&1 | Out-String
    if ($raw -match "1060" -or $raw -match "does not exist" -or $raw -match "指定的服务未安装") {
        return [pscustomobject]@{ Exists = $false; State = "NotInstalled"; Raw = $raw }
    }
    if ($raw -match "RUNNING") { return [pscustomobject]@{ Exists = $true; State = "Running"; Raw = $raw } }
    if ($raw -match "STOPPED") { return [pscustomobject]@{ Exists = $true; State = "Stopped"; Raw = $raw } }
    if ($raw -match "START_PENDING") { return [pscustomobject]@{ Exists = $true; State = "StartPending"; Raw = $raw } }
    if ($raw -match "STOP_PENDING") { return [pscustomobject]@{ Exists = $true; State = "StopPending"; Raw = $raw } }
    return [pscustomobject]@{ Exists = $true; State = "Unknown"; Raw = $raw }
}

# ============================================================
# 调用 AI-Root
# ============================================================
function Invoke-AiRoot {
    # 调用 AI-Root 并捕获输出与退出码。
    #
    # 编码：AI-Root 的 stdout 是 **UTF-8**，而 Windows PowerShell 5.1 默认按控制台
    # 代码页（中文机器为 GBK/936）解码子进程输出 —— 不切换就会把中文提示读成
    # 「绛惧悕淇℃伅」这类乱码，用户根本看不懂安装结果与签名错误。
    #
    # 只在本调用期间切换、finally 立即恢复：icacls / sc.exe 等系统工具按 **OEM**
    # 代码页输出，若全局改成 UTF-8，它们反而会乱码。两者不能共用一个全局值。
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string[]]$Arguments = @()
    )

    $prevEnc = $null
    try { $prevEnc = [Console]::OutputEncoding } catch { }

    $out = $null
    $code = -1
    try {
        [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
        $out = & $Exe @Arguments 2>&1
        $code = $LASTEXITCODE
    } finally {
        if ($null -ne $prevEnc) { [Console]::OutputEncoding = $prevEnc }
    }

    return [pscustomobject]@{
        ExitCode = $code
        Output   = ($out | Out-String).Trim()
    }
}

# ============================================================
# 工具探测
# ============================================================
function Get-ToolPath([string]$Name) {
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    return $null
}

function Invoke-External {
    # 运行外部命令并按行返回输出（用于 rustc --version 之类短命令）
    param([string]$File, [string[]]$Arguments = @())
    try {
        $out = & $File @Arguments 2>&1
        return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($out | Out-String).Trim() }
    } catch {
        return [pscustomobject]@{ ExitCode = -1; Output = $_.Exception.Message }
    }
}
