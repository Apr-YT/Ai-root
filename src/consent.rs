//! 用户确认通道（文件型，无 GUI 依赖）
//!
//! ## 为什么是「文件 + 管理员命令」而不是「弹窗」
//!
//! 服务以 LocalSystem 运行在 session 0，**没有交互桌面** —— 它无法弹 UAC、
//! 无法显示 GUI 窗口。任何号称「服务端弹窗确认」的实现，要么需要在用户会话里
//! 常驻一个接收端（新增攻击面），要么其实是在骗自己。
//!
//! ## 无状态重放模型（关键设计）
//!
//! 「服务端阻塞等人点确认」在本协议下会死锁：客户端是一问一答，
//! 它必须**先拿到请求 ID** 才能去批准，而服务端若阻塞等待就拿不到把 ID 发出去的机会。
//!
//! 因此改成无状态：
//!
//! ```text
//! 第 1 次请求 → 判定 confirm → 生成 <rid>.req.json（含参数哈希）→ 立刻返回
//!              { ok:false, decision:"confirm", confirmation_id:"<rid>" }
//! 用户        → AI-Root approve <rid>   （管理员终端；写 <rid>.approved，内容=哈希）
//! 第 2 次请求 → 同样的命令 → 重算出同样的哈希 → 扫到匹配的 .approved
//!              → 一次性消费（删文件）→ 执行
//! ```
//!
//! ## 安全属性
//!
//! 1. **不可伪造**：`pending\` 继承数据目录 ACL（SYSTEM:F / Administrators:F / 安装用户:R），
//!    普通用户（含 Agent 进程）**无法写入** `.approved`。
//! 2. **与参数绑定**：哈希 = `sha256(op | 完整参数摘要)`。
//!    **不含请求 ID** —— 保证「同一条命令重放」能匹配到批准；
//!    而参数改一个字符就是另一个哈希，旧批准作废 → 必须重新确认。
//!    这正是任务书要求的「确认后参数变化必须重新确认」。
//! 3. **一次性**：批准被消费即删除，不能用于第二次执行。
//! 4. **不接受客户端声明**：协议里没有 `confirmed` 字段（`deny_unknown_fields`
//!    下带它会直接解析失败）。确认只来自文件系统，且只有管理员能写。
//! 5. **过期即废**：批准文件与请求文件都带 TTL，`approve` 会拒绝过期请求；
//!    服务端消费前也会比对。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::policy::Action;

/// 确认请求子目录名
pub const PENDING_DIR: &str = "pending";

/// 一条待确认请求（写入 `<rid>.req.json`）
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PendingRequest {
    pub id: String,
    /// 人类可读创建时间
    pub created_at: String,
    /// 过期时刻（UNIX 秒）
    pub expires_at_unix: u64,
    /// 发起方身份（服务端从管道对端解析，不来自客户端声明）
    pub caller: String,
    pub caller_pid: u32,
    /// 操作类型（policy::OpKind::as_str）
    pub op: String,
    /// 操作目标（若有）
    pub target: Option<String>,
    /// 完整参数摘要（命令原文 / argv 数组 + cwd）
    pub params: String,
    /// 参数绑定哈希：sha256(op|params)，**不含 id**
    pub request_hash: String,
    /// 风险说明（给确认者看）
    pub risk: String,
}

pub fn pending_dir(dir: &Path) -> PathBuf {
    dir.join(PENDING_DIR)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id() -> String {
    use rand::Rng;
    let mut b = [0u8; 8];
    rand::thread_rng().fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 参数绑定哈希：`sha256(op | describe)`，**刻意不含请求 ID**。
///
/// 不含 ID 是为了让「同一条命令重放」能匹配到批准；含 ID 会让每次重放都产生
/// 新哈希，批准永远对不上。参数级绑定由 `describe()` 的完整内容保证。
pub fn request_hash_for(action: &Action<'_>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(action.op_kind().as_str().as_bytes());
    h.update(b"|");
    h.update(action.describe().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02X}")).collect()
}

/// 创建（或复用）确认请求并落盘。
///
/// 去重：同一参数哈希的待确认请求只保留一条，重复请求返回同一个 rid ——
/// 避免 AI 反复重试把 `pending\` 刷满。
pub fn create(
    dir: &Path,
    caller: &str,
    caller_pid: u32,
    action: &Action<'_>,
    target: Option<&str>,
    ttl_secs: u64,
) -> Result<PendingRequest, String> {
    let pd = pending_dir(dir);
    std::fs::create_dir_all(&pd).map_err(|e| format!("创建 pending 目录失败: {e}"))?;

    let hash = request_hash_for(action);
    // 复用未过期的同哈希请求
    for existing in list(dir) {
        if existing.request_hash == hash && existing.expires_at_unix >= now_unix() {
            return Ok(existing);
        }
    }

    let id = new_id();
    let req = PendingRequest {
        id: id.clone(),
        created_at: crate::common::now_str(),
        expires_at_unix: now_unix() + ttl_secs,
        caller: caller.to_string(),
        caller_pid,
        op: action.op_kind().as_str().to_string(),
        target: target.map(|s| s.to_string()),
        params: action.describe(),
        request_hash: hash,
        risk: risk_note(action),
    };

    let path = pd.join(format!("{id}.req.json"));
    let json = serde_json::to_vec_pretty(&req).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("写确认请求失败 {}: {e}", path.display()))?;
    f.write_all(&json)
        .map_err(|e| format!("写确认请求失败: {e}"))?;
    f.flush().map_err(|e| format!("落盘确认请求失败: {e}"))?;
    Ok(req)
}

/// 依据动作给出风险提示（给执行 `approve` 的人看）
fn risk_note(action: &Action<'_>) -> String {
    match action {
        Action::Shell { .. } => {
            "传统 shell：命令由 cmd.exe 解释，程序/参数/重定向无法结构化校验，风险最高".into()
        }
        Action::Argv { program, .. } => {
            format!("结构化执行 `{program}`：程序与参数已固定，实际影响取决于该程序自身能力")
        }
        Action::Op { op, .. } => format!("结构化操作 {}：按目标与参数范围执行", op.as_str()),
    }
}

/// 查找与参数哈希匹配、且仍有效的批准文件。返回对应 rid。
fn find_approved(dir: &Path, hash: &str) -> Option<String> {
    let pd = pending_dir(dir);
    let rd = std::fs::read_dir(&pd).ok()?;
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rid) = name.strip_suffix(".approved") else {
            continue;
        };
        // 批准内容必须与哈希逐字相等
        let Ok(content) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        if content.trim() != hash {
            continue;
        }
        // 对应的请求必须存在且未过期（防止残留批准被复用）
        let Ok(req) = load(dir, rid) else {
            continue;
        };
        if req.expires_at_unix < now_unix() {
            cleanup(dir, rid);
            continue;
        }
        return Some(rid.to_string());
    }
    None
}

/// 服务端消费一次批准：返回 `Some(rid)` 表示「这条参数确实被批准过」。
///
/// 消费即删除（一次性），避免同一份批准被重复使用。
pub fn take_approval(dir: &Path, action: &Action<'_>) -> Option<String> {
    let hash = request_hash_for(action);
    let rid = find_approved(dir, &hash)?;
    cleanup(dir, &rid);
    Some(rid)
}

/// 删除一次性文件（req / approved / denied）
pub fn cleanup(dir: &Path, id: &str) {
    let pd = pending_dir(dir);
    for suffix in [".req.json", ".approved", ".denied"] {
        let _ = std::fs::remove_file(pd.join(format!("{id}{suffix}")));
    }
}

/// 读取单条待确认请求
pub fn load(dir: &Path, id: &str) -> Result<PendingRequest, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("请求 ID 格式无效（应为 hex）".into());
    }
    let path = pending_dir(dir).join(format!("{id}.req.json"));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("找不到待确认请求 {id}（可能已过期或已处理）: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("解析确认请求失败: {e}"))
}

/// 列出当前所有待确认请求（按创建时间升序）
pub fn list(dir: &Path) -> Vec<PendingRequest> {
    let pd = pending_dir(dir);
    let Ok(rd) = std::fs::read_dir(&pd) else {
        return vec![];
    };
    let mut out: Vec<PendingRequest> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".req.json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<PendingRequest>(&s).ok())
        .collect();
    out.sort_by_key(|r| r.created_at.clone());
    out
}

/// 批准一条请求。返回被批准的请求（调用方应打印参数让用户复核）。
///
/// ⚠ 能否写入 `pending\` 目录本身由 ACL 决定（只有 SYSTEM/Administrators 可写），
/// CLI 层还会额外要求管理员已提权，双重保证。
pub fn approve(dir: &Path, id: &str) -> Result<PendingRequest, String> {
    let req = load(dir, id)?;
    if now_unix() > req.expires_at_unix {
        cleanup(dir, id);
        return Err(format!("请求 {id} 已过期，未批准"));
    }
    let path = pending_dir(dir).join(format!("{id}.approved"));
    std::fs::write(&path, &req.request_hash)
        .map_err(|e| format!("写入批准失败（需要管理员权限）: {e}"))?;
    Ok(req)
}

/// 拒绝一条请求
pub fn deny(dir: &Path, id: &str) -> Result<(), String> {
    let req = load(dir, id)?;
    let path = pending_dir(dir).join(format!("{id}.denied"));
    std::fs::write(&path, &req.request_hash)
        .map_err(|e| format!("写入拒绝失败（需要管理员权限）: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::OpKind;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("airoot-consent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn act(target: &'static str) -> Action<'static> {
        Action::Op {
            op: OpKind::RestartService,
            target: Some(target),
        }
    }

    fn shell(cmd: &str) -> Action<'_> {
        Action::Shell { command: cmd, cwd: None }
    }

    #[test]
    fn create_writes_request_with_hash_binding() {
        let d = tmp("create");
        let a = act("Spooler");
        let req = create(&d, "TEST\\user", 1234, &a, Some("Spooler"), 60).unwrap();
        assert_eq!(req.request_hash.len(), 64);
        let back = load(&d, &req.id).unwrap();
        assert_eq!(back.request_hash, req.request_hash);
        assert_eq!(back.op, "restart_service");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn approve_then_replay_is_allowed_once() {
        let d = tmp("approve");
        let a = act("Spooler");
        let req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        approve(&d, &req.id).unwrap();

        // 第一次重放：应拿到批准
        assert_eq!(take_approval(&d, &a).as_deref(), Some(req.id.as_str()));
        // 第二次重放：批准已被消费 → 不得再次放行
        assert_eq!(take_approval(&d, &a), None, "批准必须一次性");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn approval_does_not_carry_over_to_changed_params() {
        // 关键断言（任务书 §7）：确认后参数变化必须重新确认
        let d = tmp("changed");
        let a = act("Spooler");
        let req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        approve(&d, &req.id).unwrap();

        // 目标变了 → 哈希不同 → 不能用同一份批准
        let b = act("WinDefend");
        assert_eq!(take_approval(&d, &b), None, "参数变化后旧批准必须失效");
        // 原参数仍可用（未被误删）
        assert!(take_approval(&d, &a).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shell_command_change_requires_new_approval() {
        let d = tmp("shellchg");
        let a = shell("whoami");
        let req = create(&d, "u", 1, &a, None, 60).unwrap();
        approve(&d, &req.id).unwrap();
        // 多一个空格也是另一条命令
        assert_eq!(take_approval(&d, &shell("whoami ")), None);
        assert!(take_approval(&d, &shell("whoami")).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn forged_approval_content_is_ignored() {
        let d = tmp("forge");
        let a = act("Spooler");
        let req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        std::fs::write(pending_dir(&d).join(format!("{}.approved", req.id)), "deadbeef").unwrap();
        assert_eq!(take_approval(&d, &a), None, "内容与哈希不符的批准必须被忽略");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn approval_for_one_request_does_not_unlock_another() {
        let d = tmp("bind");
        let a = act("Spooler");
        let b = act("WinDefend");
        let ra = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        let _rb = create(&d, "u", 1, &b, Some("WinDefend"), 60).unwrap();
        approve(&d, &ra.id).unwrap();
        // 把 A 的批准文件复制成 B 的批准文件
        let pd = pending_dir(&d);
        std::fs::copy(
            pd.join(format!("{}.approved", ra.id)),
            pd.join(format!("{}.approved", _rb.id)),
        )
        .unwrap();
        assert_eq!(take_approval(&d, &b), None, "复制的批准文件不能解锁别的参数");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn duplicate_request_reuses_same_rid() {
        let d = tmp("dedup");
        let a = act("Spooler");
        let r1 = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        let r2 = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        assert_eq!(r1.id, r2.id, "同参数重复请求应复用同一条待确认项");
        assert_eq!(list(&d).len(), 1, "不应刷出多条");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn expired_request_cannot_be_approved() {
        let d = tmp("expired");
        let a = act("Spooler");
        let mut req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        req.expires_at_unix = now_unix() - 1;
        let p = pending_dir(&d).join(format!("{}.req.json", req.id));
        std::fs::write(&p, serde_json::to_vec(&req).unwrap()).unwrap();
        assert!(approve(&d, &req.id).unwrap_err().contains("过期"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_approval_of_expired_request_is_not_used() {
        let d = tmp("stale");
        let a = act("Spooler");
        let mut req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        approve(&d, &req.id).unwrap();
        // 事后把请求改成已过期
        req.expires_at_unix = now_unix() - 1;
        let p = pending_dir(&d).join(format!("{}.req.json", req.id));
        std::fs::write(&p, serde_json::to_vec(&req).unwrap()).unwrap();
        assert_eq!(take_approval(&d, &a), None, "过期请求的批准不得被消费");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_rejects_path_traversal_id() {
        let d = tmp("badid");
        assert!(load(&d, "../etc/passwd").is_err(), "路径穿越式 id 必须被拒");
        assert!(load(&d, "").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deny_marks_request() {
        let d = tmp("deny");
        let a = act("Spooler");
        let req = create(&d, "u", 1, &a, Some("Spooler"), 60).unwrap();
        deny(&d, &req.id).unwrap();
        assert!(pending_dir(&d).join(format!("{}.denied", req.id)).exists());
        // 被拒绝的请求不得被批准使用
        assert_eq!(take_approval(&d, &a), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
