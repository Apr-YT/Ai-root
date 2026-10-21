# scripts/doctor.ps1 —— 环境体检
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\doctor.ps1
#   powershell ... -File scripts\doctor.ps1 -Verbose
#
# 只读脚本: 不安装、不改配置、不请求 UAC。
# 退出码: 0 = 无致命问题（可能有警告）；1 = 存在必须解决的缺失项。

[CmdletBinding()]
param(
    # 只输出 JSON（供自动化消费）
    [switch]$Json
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

$repoRoot = Get-RepoRoot
$results = New-Object System.Collections.ArrayList

function Add-Result {
    param(
        [string]$Name,
        [ValidateSet("OK", "WARN", "FAIL", "INFO")][string]$Level,
        [string]$Detail,
        [string]$Fix
    )
    [void]$results.Add([pscustomobject]@{
        Name   = $Name
        Level  = $Level
        Detail = $Detail
        Fix    = $Fix
    })
}

# ============================================================
# 1. PowerShell 与架构
# ============================================================
$psv = $PSVersionTable.PSVersion
if ($psv.Major -ge 5) {
    Add-Result "PowerShell" "OK" "$($psv.ToString())（$($PSVersionTable.PSEdition)）" ""
} else {
    Add-Result "PowerShell" "FAIL" "$($psv.ToString()) 过低" "安装 PowerShell 5.1+（Windows 10/11 自带）或 PowerShell 7"
}

if ([Environment]::Is64BitProcess) {
    Add-Result "进程架构" "OK" "64 位" ""
} else {
    Add-Result "进程架构" "WARN" "32 位 PowerShell" "用 64 位 PowerShell 运行，否则可能编不出 x64 目标"
}

# ============================================================
# 2. 仓库结构
# ============================================================
$cargoToml = Join-Path $repoRoot "Cargo.toml"
if (Test-Path $cargoToml) {
    Add-Result "Cargo.toml" "OK" $cargoToml ""
} else {
    Add-Result "Cargo.toml" "FAIL" "仓库根未找到 Cargo.toml" "确认从仓库根目录运行，或脚本未被复制到别处"
}

$cargoCfg = Join-Path $repoRoot ".cargo\config.toml"
if (Test-Path $cargoCfg) {
    Add-Result ".cargo/config.toml" "OK" "存在" ""
} else {
    Add-Result ".cargo/config.toml" "WARN" "缺失（构建仍可进行，但产物目录等约定可能不一致）" "git checkout .cargo/config.toml"
}

if (Test-AsciiPath $repoRoot) {
    Add-Result "仓库路径编码" "OK" "纯 ASCII" ""
} else {
    $buildDir = Get-BuildTargetDir $repoRoot
    Add-Result "仓库路径编码" "WARN" "含非 ASCII 字符: $repoRoot" "脚本会自动把产物目录切到 $buildDir；MinGW 的 ld/dlltool 无法处理中文路径"
}

# ============================================================
# 3. Rust 工具链
# ============================================================
$rustup = Get-ToolPath "rustup.exe"
$cargo = Get-ToolPath "cargo.exe"
$rustc = Get-ToolPath "rustc.exe"

if ($rustc) {
    $v = Invoke-External $rustc @("--version")
    if ($v.ExitCode -eq 0) {
        Add-Result "rustc" "OK" $v.Output $rustc
    } else {
        Add-Result "rustc" "FAIL" $v.Output "重装 Rust 工具链"
    }
} else {
    Add-Result "rustc" "FAIL" "未找到" "安装 Rust: winget install Rustlang.Rustup  或 https://rustup.rs"
}

if ($cargo) {
    $v = Invoke-External $cargo @("--version")
    if ($v.ExitCode -eq 0) { Add-Result "cargo" "OK" $v.Output $cargo }
    else { Add-Result "cargo" "FAIL" $v.Output "重装 Rust 工具链" }
} else {
    Add-Result "cargo" "FAIL" "未找到" "安装 Rust: https://rustup.rs"
}

$installedTargets = @()
if ($rustup) {
    $t = Invoke-External $rustup @("target", "list", "--installed")
    if ($t.ExitCode -eq 0) {
        $installedTargets = @($t.Output -split "`r?`n" | Where-Object { $_.Trim() -ne "" })
        Add-Result "已安装 target" "OK" ($(if ($installedTargets.Count) { $installedTargets -join ", " } else { "（无附加 target，仅 host）" })) ""
    } else {
        Add-Result "已安装 target" "WARN" "无法读取: $($t.Output)" "rustup target list --installed"
    }
} else {
    Add-Result "rustup" "WARN" "未找到（无法管理 target）" "安装 rustup: https://rustup.rs"
}

# ============================================================
# 4. 链接工具（msvc 或 gnu，至少一条可用）
# ============================================================
$hasMsvcTarget = $installedTargets -contains "x86_64-pc-windows-msvc"
$hasGnuTarget = $installedTargets -contains "x86_64-pc-windows-gnu"

$linkExe = Get-ToolPath "link.exe"
if (-not $linkExe) {
    # VS 的 link.exe 通常不在 PATH，按目录探测（与根 build.ps1 的判定保持一致）
    $vsLink = Get-ChildItem "C:\Program Files*\Microsoft Visual Studio" -Depth 6 -Filter "link.exe" -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($vsLink) { $linkExe = $vsLink.FullName }
}
if ($linkExe) {
    Add-Result "MSVC 链接器 (link.exe)" "OK" $linkExe ""
} else {
    if ($hasMsvcTarget) {
        Add-Result "MSVC 链接器 (link.exe)" "FAIL" "已装 msvc target 但找不到 link.exe" "安装 VS Build Tools（含「使用 C++ 的桌面开发」）: https://aka.ms/vs/17/release/vs_BuildTools.exe"
    } else {
        Add-Result "MSVC 链接器 (link.exe)" "INFO" "未找到（未安装 msvc target，非必需）" ""
    }
}

$asExe = Get-ToolPath "as.exe"
if ($asExe) {
    Add-Result "MinGW binutils (as.exe)" "OK" $asExe ""
} else {
    if ($hasGnuTarget) {
        Add-Result "MinGW binutils (as.exe)" "FAIL" "已装 gnu target 但找不到 as.exe（rustup self-contained 缺 as.exe）" "MSYS2: pacman -S mingw-w64-x86_64-binutils，或用 MSVC 目标"
    } else {
        Add-Result "MinGW binutils (as.exe)" "INFO" "未找到（未安装 gnu target，非必需）" ""
    }
}

$targetNames = @()
if ($hasMsvcTarget) { $targetNames += "msvc" }
if ($hasGnuTarget) { $targetNames += "gnu" }
if (-not ($hasMsvcTarget -or $hasGnuTarget)) {
    Add-Result "可构建目标" "FAIL" "没有任何 Windows target" "rustup target add x86_64-pc-windows-gnu（或 -msvc）"
} else {
    Add-Result "可构建目标" "OK" ($targetNames -join " + ") ""
}

# Windows SDK（msvc 链接期需要）
$sdkRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\Include"
if (Test-Path $sdkRoot) {
    $sdkVer = Get-ChildItem $sdkRoot -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($sdkVer) {
        Add-Result "Windows SDK" "OK" "$sdkRoot\$($sdkVer.Name)" ""
    } else {
        Add-Result "Windows SDK" "WARN" "目录存在但没有版本子目录" "通过 VS Installer 勾选 Windows SDK"
    }
} else {
    if ($hasMsvcTarget) {
        Add-Result "Windows SDK" "WARN" "未找到 $sdkRoot" "VS Installer 勾选 Windows 10/11 SDK；msvc 链接可能失败"
    } else {
        Add-Result "Windows SDK" "INFO" "未找到（gnu 目标不需要）" ""
    }
}

# ============================================================
# 5. 权限
# ============================================================
if (Test-IsAdmin) {
    Add-Result "当前权限" "INFO" "管理员（install/uninstall 可直接执行）" ""
} else {
    Add-Result "当前权限" "INFO" "普通用户（build/test 够用；install 会自动请求 UAC）" ""
}

# ============================================================
# 6. 服务状态
# ============================================================
$svc = Get-AiRootServiceState
if ($svc.Exists) {
    Add-Result "AI-Root 服务" "OK" "已安装，状态: $($svc.State)" ""
} else {
    Add-Result "AI-Root 服务" "INFO" "未安装（首次使用执行 scripts\install.ps1）" ""
}

# ============================================================
# 7. 二进制与配置
# ============================================================
$exe = Find-AiRootExe -RepoRoot $repoRoot
if ($exe.Path) {
    Add-Result "AI-Root.exe" "OK" "$($exe.Path)（来源: $($exe.Source)）" ""
    $ver = Invoke-AiRoot -Exe $exe.Path -Arguments @("--version")
    if ($ver.ExitCode -eq 0) {
        Add-Result "版本" "OK" $ver.Output ""
    }

    # config check 是只读的（load_readonly / load_existing），不会创建目录
    $machinePolicy = Join-Path $env:ProgramData "AI-Root\policy.toml"
    $chk = Invoke-AiRoot -Exe $exe.Path -Arguments @("config", "check")
    if (-not (Test-Path $machinePolicy)) {
        # 「策略缺失」在 config check 里有意不算失败（首次使用不该报错），
        # 但直接显示「通过」会让人误以为授权策略已就绪。此处如实区分。
        Add-Result "配置校验" "INFO" "策略尚未初始化（首次 install 或 AI-Root config init 后生成）" ""
    } elseif ($chk.ExitCode -eq 0) {
        Add-Result "配置校验" "OK" "全部配置层通过" ""
    } else {
        Add-Result "配置校验" "FAIL" (($chk.Output -split "`r?`n" | Select-Object -First 6) -join " / ") "按提示修正 policy.toml / user.toml，或执行 AI-Root config check 查看完整输出"
    }
} else {
    Add-Result "AI-Root.exe" "INFO" $exe.Source "编译: scripts\build.ps1"
}

# ============================================================
# 输出
# ============================================================
$failCount = @($results | Where-Object { $_.Level -eq "FAIL" }).Count
$warnCount = @($results | Where-Object { $_.Level -eq "WARN" }).Count

if ($Json) {
    [pscustomobject]@{
        RepoRoot = $repoRoot
        Fails    = $failCount
        Warnings = $warnCount
        Checks   = $results
    } | ConvertTo-Json -Depth 5
    if ($failCount -gt 0) { exit 1 } else { exit 0 }
}

Write-Host ""
Write-Host "AI-Root 环境体检" -ForegroundColor White
Write-Host "仓库: $repoRoot" -ForegroundColor DarkGray

foreach ($r in $results) {
    $tag = switch ($r.Level) {
        "OK"   { "[OK]  " }
        "WARN" { "[WARN]" }
        "FAIL" { "[FAIL]" }
        default { "[--]  " }
    }
    $color = switch ($r.Level) {
        "OK"   { "Green" }
        "WARN" { "Yellow" }
        "FAIL" { "Red" }
        default { "DarkGray" }
    }
    Write-Host ("  {0} {1,-24} {2}" -f $tag, $r.Name, $r.Detail) -ForegroundColor $color
    if ($r.Fix -and $r.Level -ne "OK") {
        Write-Host ("         修复: {0}" -f $r.Fix) -ForegroundColor DarkGray
    }
}

Write-Host ""
if ($failCount -gt 0) {
    Write-Host "结论: 有 $failCount 个必须解决的问题，$warnCount 个警告。" -ForegroundColor Red
    Write-Host "修好后重新运行本脚本确认。" -ForegroundColor Gray
    exit 1
} elseif ($warnCount -gt 0) {
    Write-Host "结论: 可以构建，但有 $warnCount 个警告。" -ForegroundColor Yellow
    exit 0
} else {
    Write-Host "结论: 环境就绪。" -ForegroundColor Green
    exit 0
}
