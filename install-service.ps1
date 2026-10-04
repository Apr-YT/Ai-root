# AI-Root 服务安装（需管理员 PowerShell）
#
# 用途：以管理员身份注册 AI-Root 为 LocalSystem 自启服务，并验证安装结果。
# 说明：本脚本按顺序排好命令并打印每一步结果，不含隐藏动作。
#
# 用法：右键「以管理员身份运行 PowerShell」，然后执行本文件。
#
# 注意：本文件必须以 UTF-8 **带 BOM** 保存。
#   Windows PowerShell 5.1 默认按 ANSI(本机为 GBK/936) 读取 .ps1；
#   无 BOM 的 UTF-8 文件里的中文会被解码成乱码，甚至因乱码产生 ? 与
#   不配对的引号而导致语法错误。（本机实测踩过，见记忆）

$ErrorActionPreference = "Continue"

$Exe = "D:\AI-Root-target\x86_64-pc-windows-gnu\release\AI-Root.exe"
$ExpectedSha256 = "F0AC9A45C908EE059A8C31CE430F54E1A7A8E0AD421C97FE5ECAA31B76D5431C"

function Step($n, $t) { Write-Host ""; Write-Host "=== [$n] $t ===" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "  [OK] $m" -ForegroundColor Green }
function Bad($m)  { Write-Host "  [X]  $m" -ForegroundColor Red }

# ---------- 0) 提权自检 ----------
Step 0 "提权自检"
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Bad "当前不是管理员。请以管理员身份重开 PowerShell 再执行本脚本。"
    exit 1
}
Ok "已提权"

# ---------- 1) 二进制指纹核对 ----------
Step 1 "二进制指纹核对"
if (-not (Test-Path $Exe)) { Bad "找不到 $Exe"; exit 1 }
$sha = (Get-FileHash $Exe -Algorithm SHA256).Hash
Write-Host "  实际: $sha"
Write-Host "  期望: $ExpectedSha256"
if ($sha -ne $ExpectedSha256) {
    Bad "指纹不匹配 —— 二进制与已验证版本不同，请先确认再继续。"
    exit 1
}
Ok "指纹一致（确认是已修复 ACL 缺陷的版本）"

# ---------- 2) 安装前状态快照 ----------
Step 2 "安装前状态快照"
$svcBefore = Get-Service -Name "AI-Root" -ErrorAction SilentlyContinue
if ($svcBefore) { Write-Host "  服务已存在: $($svcBefore.Status)" } else { Write-Host "  服务不存在（预期）" }
Write-Host "  数据目录 ACL:"
if (Test-Path "C:\ProgramData\AI-Root") { icacls "C:\ProgramData\AI-Root" | ForEach-Object { Write-Host "    $_" } }
Write-Host ("  D:\Kiln\AI-Root 存在? " + (Test-Path "D:\Kiln\AI-Root"))
Write-Host ("  C:\Windows\AI-Root.cmd 存在? " + (Test-Path "C:\Windows\AI-Root.cmd"))

# ---------- 3) 安装 ----------
Step 3 "执行 install（注册 LocalSystem 自启服务）"
& $Exe install
Write-Host "  install 退出码: $LASTEXITCODE"

# ---------- 4) 安装后核对 ----------
Step 4 "安装后核对"

$svc = Get-Service -Name "AI-Root" -ErrorAction SilentlyContinue
if ($svc) {
    Ok "服务已注册，状态: $($svc.Status)"
    Write-Host "  启动类型: $($svc.StartType)"
    $qv = & sc.exe qc AI-Root
    $qv | Where-Object { $_ -match "SERVICE_START_NAME|BINARY_PATH_NAME" } | ForEach-Object { Write-Host "  $_" }
} else {
    Bad "服务未注册"
}

if (Test-Path "D:\Kiln\AI-Root\AI-Root.exe") { Ok "已部署 D:\Kiln\AI-Root\AI-Root.exe" } else { Bad "部署副本缺失" }
if (Test-Path "C:\Windows\AI-Root.cmd")      { Ok "已写全局包装器 C:\Windows\AI-Root.cmd" } else { Bad "全局包装器缺失" }

Write-Host "  数据目录 ACL:"
icacls "C:\ProgramData\AI-Root" | ForEach-Object { Write-Host "    $_" }
$acl = icacls "C:\ProgramData\AI-Root" | Out-String
if ($acl -match 'Users.*(WD|AD|WEA|WA)') {
    Bad "Users 仍持有写权限 —— ACL 修复未生效，请立即停下并反馈！"
} else {
    Ok "Users 无写权限（隔离生效）"
}
if ($acl -match 'SYSTEM.*\(F\)') { Ok "SYSTEM 保留完全控制" } else { Bad "SYSTEM 失去完全控制" }

if (Test-Path "C:\ProgramData\AI-Root\sid.txt") {
    Ok ("sid.txt = " + (Get-Content "C:\ProgramData\AI-Root\sid.txt" -Raw).Trim())
} else { Write-Host "  sid.txt 不存在" }

# ---------- 5) 启动服务 ----------
Step 5 "启动服务"
& $Exe start
Start-Sleep -Seconds 2
$svc2 = Get-Service -Name "AI-Root" -ErrorAction SilentlyContinue
if ($svc2) { Write-Host "  当前状态: $($svc2.Status)" }

# ---------- 6) 失败恢复策略核对 ----------
Step 6 "失败恢复策略（应为 60s/300s/600s 重启）"
& sc.exe qfailure AI-Root

Write-Host ""
Write-Host "==================== 完成 ====================" -ForegroundColor Cyan
Write-Host "下一步："
Write-Host ("  1) 非提权验证身份：" + $Exe + " run whoami    （应返回 nt authority\system）")
Write-Host ("  2) 查看概览：      " + $Exe + " status")
Write-Host "  3) 若 ACL 检查项报 [X]，请勿继续使用并反馈。"