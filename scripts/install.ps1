# scripts/install.ps1 —— 安全安装 / 更新 AI-Root 服务
#
# 用法:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1
#   ... -File scripts\install.ps1 -PreflightOnly     # 只检查环境与签名，不安装（不需要管理员）
#   ... -File scripts\install.ps1 -Update            # 已安装时走签名更新（保留现有配置）
#   ... -File scripts\install.ps1 -Force             # 已安装时卸载后重装（配置与日志保留）
#   ... -File scripts\install.ps1 -Exe D:\x\AI-Root.exe
#
# 幂等性:
#   服务已存在时，默认**不**重装，只打印当前状态并退出 0。
#   要变更版本必须显式选择 -Update（官方更新通道）或 -Force（重装）。
#   这样重复执行本脚本不会破坏正在使用的安装。
#
# 安全:
#   - install 本身不做签名校验（本地自编译场景必须允许），但会**展示**签名状态并告警。
#   - 需要签名强制的场景（分发给他人）加 -StrictSignature，未签名即拒绝。
#   - 不关闭 Defender、不修改安全策略、不绕过 UAC（由系统弹窗，用户可拒绝）。

[CmdletBinding()]
param(
    [string]$Exe = "",

    # 已安装时执行签名更新（走 AI-Root update，失败自动回滚）
    [switch]$Update,

    # 已安装时卸载后重装（先 uninstall 再 install；策略/规则/日志会保留）
    [switch]$Force,

    # 只做检查（环境、二进制、签名、服务状态），不产生任何变更，不需要管理员
    [switch]$PreflightOnly,

    # 未签名即拒绝安装（分发给他人时使用）
    [switch]$StrictSignature,

    # 跳过交互确认
    [switch]$Yes
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "_common.ps1")

if ($Update -and $Force) {
    Write-Bad "-Update 与 -Force 互斥：更新走签名通道，重装走卸载再安装，请选其一。"
    exit 2
}

$repoRoot = Get-RepoRoot

# ------------------------------------------------------------
# 1. 定位二进制
# ------------------------------------------------------------
$found = Find-AiRootExe -RepoRoot $repoRoot -Explicit $Exe
if (-not $found.Path) {
    Write-Bad "找不到 AI-Root.exe: $($found.Source)"
    Write-Info "先编译: scripts\build.ps1"
    exit 1
}
$exePath = $found.Path

Write-Host ""
Write-Host "=== AI-Root 安装检查 ===" -ForegroundColor Cyan
Write-Host "  二进制: $exePath" -ForegroundColor Gray
Write-Host "  来源  : $($found.Source)" -ForegroundColor DarkGray

# ------------------------------------------------------------
# 2. 二进制基本校验
# ------------------------------------------------------------
$item = Get-Item $exePath
$sha = (Get-FileHash $exePath -Algorithm SHA256).Hash
Write-Host ("  大小  : {0:N1} MB" -f ($item.Length / 1MB)) -ForegroundColor Gray
Write-Host "  SHA256: $sha" -ForegroundColor Gray

$ver = Invoke-AiRoot -Exe $exePath -Arguments @("--version")
if ($ver.ExitCode -eq 0) {
    Write-Ok "可执行文件可运行: $(($ver.Output -split "`r?`n")[0])"
} else {
    Write-Bad "无法运行该二进制（架构不匹配或文件损坏）: $($ver.Output)"
    exit 1
}

# 架构一致性：服务与安装程序必须同为 x64（或同为 ARM64）。
# 这里只做冒烟提示，真正的强校验在 AI-Root update 内部（PE Machine 比对）。
$vi = (Get-Item $exePath).VersionInfo
if ($vi.FileVersion -or $vi.ProductName) {
    Write-Info "文件版本: $($vi.FileVersion)  产品: $($vi.ProductName)"
} else {
    Write-Info "未嵌入版本资源（Rust 默认行为，不影响安装）"
}

# ------------------------------------------------------------
# 3. 签名状态（展示 + 可选强制）
# ------------------------------------------------------------
$isSigned = $false
$sigText = ""
$sig = Invoke-AiRoot -Exe $exePath -Arguments @("signature", $exePath)
if ($sig.Output) {
    $sigText = $sig.Output
    # 判据只用「明确通过」的措辞，避免把「未通过」误判成通过。
    if ($sigText -match "信任链\s*[:：]\s*通过" -or $sigText -match "签名验证通过") {
        $isSigned = $true
    }
}

if ($isSigned) {
    Write-Ok "Authenticode 签名校验通过"
} else {
    if ($StrictSignature) {
        Write-Bad "未通过 Authenticode 签名校验，-StrictSignature 已启用，拒绝安装。"
        # 逐行输出，避免整段签名报告被 [string] 参数用空格拼成一行难以阅读
        foreach ($line in ($sigText -split "`r?`n" | Select-Object -First 8)) {
            if ($line.Trim()) { Write-Info $line }
        }
        exit 1
    }
    Write-Warn "该二进制没有可验证的 Authenticode 嵌入签名（本地自编译属正常）"
    Write-Info "install 不校验签名（否则本地开发无法安装）；但 update 通道会强制校验。"
    Write-Info "分发给他人时请用 signtool 签名，并加 -StrictSignature。"
}

# ------------------------------------------------------------
# 4. 服务状态（幂等判断）
# ------------------------------------------------------------
$dataDir = Join-Path $env:ProgramData "AI-Root"
$installedExe = Join-Path $dataDir "AI-Root.exe"
$svc = Get-AiRootServiceState
if ($svc.Exists) {
    Write-Ok "服务已存在，当前状态: $($svc.State)"
} else {
    Write-Info "服务未安装"
}

# ------------------------------------------------------------
# 5. PreflightOnly 到此为止
# ------------------------------------------------------------
if ($PreflightOnly) {
    Write-Host ""
    Write-Host "=== 预检结论 ===" -ForegroundColor Cyan
    Write-Ok "二进制可运行"
    if ($isSigned) { Write-Ok "签名有效" } else { Write-Warn "未签名（本地开发可接受）" }
    if ($svc.Exists) {
        Write-Info "已安装（状态 $($svc.State)）。变更版本请用 -Update 或 -Force。"
    } else {
        Write-Info "未安装。去掉 -PreflightOnly 即可执行安装（会请求 UAC）。"
    }
    # 提前拦截「注定失败」的组合：update 通道强制要求签名 + 信任根。
    # 让用户在提权前就知道这条路走不通，而不是等 UAC 之后再报错。
    if ($Update -and -not $isSigned) {
        Write-Bad "-Update 要求 Authenticode 嵌入签名，当前二进制未通过校验，update 一定会被拒绝。"
        Write-Info "改用: scripts\install.ps1 -Force（重装，保留策略与日志）"
        Write-Info "或先签名并用 AI-Root trust-publisher 建立信任根。"
        exit 1
    }
    Write-Host ""
    Write-Host "预检完成，未做任何变更。" -ForegroundColor Green
    exit 0
}

# ------------------------------------------------------------
# 5.5 变更前确认（放在提权**之前**）
# ------------------------------------------------------------
# 为什么必须在提权前：UAC 会新开一个窗口，用户在那里看不到本脚本刚打印的
# 服务状态、二进制版本与签名信息，却要对着一个孤零零的 y/N 做决定。
# 在原来的窗口里确认，上下文完整。
if ($svc.Exists -and $Force -and -not $Yes) {
    Write-Host ""
    Write-Warn "将卸载现有服务（保留 $dataDir 下的配置与日志）并重新安装。"
    $answer = Read-Host "  继续? [y/N]"
    if ($answer -notmatch "^(y|yes)$") {
        Write-Info "已取消，未做变更。"
        exit 0
    }
}

# ------------------------------------------------------------
# 6. 提权（UAC）
# ------------------------------------------------------------
if (-not (Test-IsAdmin)) {
    Write-Host ""
    Write-Warn "安装/更新服务需要管理员权限，正在请求 UAC..."
    Write-Info "系统会弹出授权窗口；拒绝即可取消，不会产生任何变更。"

    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`"")
    if ($Exe) { $argList += @("-Exe", "`"$Exe`"") }
    if ($Update) { $argList += "-Update" }
    if ($Force) { $argList += "-Force" }
    if ($StrictSignature) { $argList += "-StrictSignature" }
    if ($Yes) { $argList += "-Yes" }

    try {
        $p = Start-Process -FilePath "powershell.exe" -ArgumentList $argList -Verb RunAs -Wait -PassThru
        exit $p.ExitCode
    } catch {
        Write-Bad "提权被取消或失败: $($_.Exception.Message)"
        Write-Info "也可以在「以管理员身份运行」的 PowerShell 中重跑本脚本。"
        exit 1
    }
}

# ------------------------------------------------------------
# 7. 安装动作（「全新安装」与「-Force 重装」共用）
# ------------------------------------------------------------
# 为什么抽成函数：-Force 分支原先只调用 uninstall，然后指望外层 else 去安装。
# 但 $svc 是**卸载之前**取的快照，Exists 仍为 true，外层 else 永远不会执行 ——
# 实测结果就是「只卸载、没安装」：留下一个 delete-pending 的服务（SCM 显示
# DISABLED）、没有令牌文件，exec 直接报「未找到令牌」。
function Install-Service {
    Write-Host ""
    Write-Host "=== 注册 LocalSystem 服务 ===" -ForegroundColor Cyan

    $result = Invoke-AiRoot -Exe $exePath -Arguments @("install")

    # 卸载是异步的：SCM 标记删除后进程仍在退出，同名服务可能还处于 delete-pending，
    # 此时 create_service 会报「已存在」。等 2s 重试一次即可。
    if ($result.ExitCode -ne 0 -and $result.Output -match "已存在") {
        Write-Info "同名服务可能仍在 delete-pending，等待 2s 后重试..."
        Start-Sleep -Seconds 2
        $result = Invoke-AiRoot -Exe $exePath -Arguments @("install")
    }

    Write-Host $result.Output
    if ($result.ExitCode -ne 0) {
        Write-Bad "安装失败（exit=$($result.ExitCode)）"
        Write-Info "常见原因:"
        Write-Info "  - 数据目录 ACL 设置失败（需管理员，且不能被杀软锁定）"
        Write-Info "  - 同名服务仍处于 delete-pending（卸载后需等几秒）"
        Write-Info "排查: AI-Root status / 事件查看器 / scripts\doctor.ps1"
        exit 1
    }
}

# ------------------------------------------------------------
# 7. 已安装分支：默认不动，显式选择才变更
# ------------------------------------------------------------
if ($svc.Exists) {
    if ($Update) {
        Write-Host ""
        Write-Host "=== 通过 update 通道更新 ===" -ForegroundColor Cyan
        if (-not $isSigned) {
            Write-Bad "update 要求「Authenticode 嵌入签名 + 发布者证书在 trusted_publishers 中」。"
            Write-Info "当前二进制未通过签名校验，update 一定会被拒绝。"
            Write-Info "可选方案:"
            Write-Info "  1) 用 scripts\install.ps1 -Force 重装（保留策略与日志）"
            Write-Info "  2) 先给二进制签名，再 AI-Root trust-publisher <exe> 建立信任根"
            exit 1
        }
        # 用**已安装的副本**执行 update，而不是刚编译出来的源文件。
        #
        # 原因：update 内部用 std::env::current_exe() 探测「当前版本」做降级判断。
        # 若拿源文件执行，current 与 target 是同一个文件，版本比较变成 1.3.0 -> 1.3.0，
        # 「拒绝降级」这道检查就形同虚设。用部署副本执行，current 才是真实在用版本。
        $runner = $exePath
        if (Test-Path $installedExe) {
            $runner = $installedExe
            Write-Info "执行者: $runner（已安装副本，用于识别当前版本）"
        } else {
            Write-Warn "未找到已安装副本，退回用源文件执行；版本降级检查可能不准确"
        }

        $u = Invoke-AiRoot -Exe $runner -Arguments @("update", $exePath)
        Write-Host $u.Output
        if ($u.ExitCode -ne 0) {
            Write-Bad "更新失败（exit=$($u.ExitCode)）。AI-Root update 失败时会自动回滚到上一版本。"
            Write-Info "确认服务状态: AI-Root status"
            exit 1
        }
        Write-Ok "更新完成"
    }
    elseif ($Force) {
        Write-Host ""
        Write-Host "=== 重装（卸载 -> 安装；策略/规则/日志保留）===" -ForegroundColor Cyan
        # 确认已在提权前完成（步骤 5.5），此处不再询问。
        $u = Invoke-AiRoot -Exe $exePath -Arguments @("uninstall")
        Write-Host $u.Output
        if ($u.ExitCode -ne 0) {
            Write-Bad "卸载失败，已中止（未执行安装）。"
            exit 1
        }
        # 卸载完成后**必须显式安装**：$svc 是卸载前的快照，外层 else 不可达。
        Install-Service
    }
    else {
        Write-Host ""
        Write-Host "=== 已安装，未做变更 ===" -ForegroundColor Yellow
        Write-Info "当前状态: $($svc.State)"
        Write-Info "更新版本: scripts\install.ps1 -Update（签名通道，失败自动回滚）"
        Write-Info "重装    : scripts\install.ps1 -Force（保留配置与日志）"
        Write-Info "卸载    : scripts\uninstall.ps1"
        exit 0
    }
}
else {
    # --------------------------------------------------------
    # 8. 全新安装
    # --------------------------------------------------------
    Install-Service
}

# ------------------------------------------------------------
# 9. 安装后验证
# ------------------------------------------------------------
Write-Host ""
Write-Host "=== 安装后验证 ===" -ForegroundColor Cyan

$svc2 = Get-AiRootServiceState
if ($svc2.Exists) {
    Write-Ok "服务已注册，状态: $($svc2.State)"
} else {
    Write-Bad "服务注册后查询不到，安装可能未生效"
    exit 1
}

if ($svc2.State -ne "Running") {
    Write-Info "服务未运行，尝试启动..."
    $s = Invoke-AiRoot -Exe $exePath -Arguments @("start")
    Write-Host $s.Output
    Start-Sleep -Seconds 2
    $svc2 = Get-AiRootServiceState
    if ($svc2.State -eq "Running") {
        Write-Ok "服务已启动"
    } else {
        Write-Warn "服务仍未进入 Running（当前: $($svc2.State)）"
        Write-Info "查看: AI-Root status，或事件查看器 -> Windows 日志 -> 系统"
    }
}

# 配置层校验（只读）
$chk = Invoke-AiRoot -Exe $exePath -Arguments @("config", "check")
if ($chk.ExitCode -eq 0) {
    Write-Ok "配置校验通过"
} else {
    Write-Warn "配置校验未通过:"
    Write-Info (($chk.Output -split "`r?`n" | Select-Object -First 6) -join " / ")
}

# 数据目录 ACL 提示（真正的强制校验在服务启动时，验证失败会拒绝启动）
$dataDir = Join-Path $env:ProgramData "AI-Root"
if (Test-Path $dataDir) {
    $acl = (& icacls $dataDir 2>&1 | Out-String)
    if ($acl -match "BUILTIN\\Users:.*\((M|F|W)\)" -or $acl -match "Everyone:.*\((M|F|W)\)") {
        Write-Warn "数据目录 ACL 中检测到普通用户写权限，请人工确认:"
        Write-Info $dataDir
    } else {
        Write-Ok "数据目录 ACL 未发现普通用户写权限"
    }
}

# 冒烟：用受控的只读操作确认中继可用（whoami 在默认白名单内）
# 冒烟测试必须重试：服务进入 Running 与管道可连接不是同一时刻。
# 实测刚启动就发请求会拿到「命名管道不存在」，服务本身是好的。
# 重试 10 次、每次 1 秒，覆盖绝大多数启动耗时；仍失败才认为有问题。
$smoke = $null
$smokeOk = $false
for ($i = 1; $i -le 10; $i++) {
    $smoke = Invoke-AiRoot -Exe $exePath -Arguments @("exec", "whoami")
    if ($smoke.ExitCode -eq 0 -and $smoke.Output -match "SYSTEM") {
        $smokeOk = $true
        break
    }
    if ($i -lt 10) { Start-Sleep -Seconds 1 }
}

if ($smokeOk) {
    Write-Ok "中继可用（whoami 返回 SYSTEM 身份）"
} else {
    Write-Warn "中继在 10 秒内未就绪（服务状态为 $($svc2.State)）"
    Write-Info (($smoke.Output -split "`r?`n" | Select-Object -First 3) -join " / ")
    Write-Info "排查: AI-Root status / 事件查看器 / scripts\doctor.ps1"
}

Write-Host ""
Write-Host "=== 完成 ===" -ForegroundColor Green
Write-Host "  服务状态: $($svc2.State)"
Write-Host "  使用示例: AI-Root exec git --version"
Write-Host "  查看策略: AI-Root policy"
Write-Host "  查看待确认: AI-Root pending"
Write-Host "  卸载: scripts\uninstall.ps1"
exit 0
