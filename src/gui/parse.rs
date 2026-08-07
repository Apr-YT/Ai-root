//! 数据类型与解析：命令执行结果（CmdResult）、事件流缓存条目（ParsedEvent）、
//! 事件等级筛选枚举（EventLevel），以及 JSONL 事件行 / 审计日志行的解析函数。
//!
//! 解析集中在此模块：事件行「单次解析、缓存复用」（渲染零重复解析），
//! 审计日志行按固定列宽拆分以便对齐渲染。

use eframe::egui;

use crate::common;

use super::theme::{c_err, c_ok, c_warn, C_INFO, C_NEUTRAL, C_TEXT_DIM};

/// 实时信息流等级筛选（与 ParsedEvent.level 对应，枚举比较零字符串操作）
#[derive(PartialEq, Clone, Copy)]
pub(super) enum EventLevel {
    All,
    Start,
    DoneOk,
    DoneErr,
    Service,
}

impl EventLevel {
    pub(super) fn label(self) -> &'static str {
        match self {
            EventLevel::All => "全部",
            EventLevel::Start => "开始",
            EventLevel::DoneOk => "成功",
            EventLevel::DoneErr => "失败",
            EventLevel::Service => "服务",
        }
    }
}

/// 一次命令执行的结构化结果（含命令与时间戳，支撑输出历史）
pub(super) struct CmdResult {
    pub(super) command: String,
    pub(super) ts: String,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) exit_code: i32,
    pub(super) relay_err: Option<String>,
    pub(super) dur_ms: u64,
}

impl CmdResult {
    /// 中继层失败（连接失败 / 超时 / spawn 失败等）：无 stdout/stderr，仅 error
    pub(super) fn relay_failure(command: &str, msg: String) -> Self {
        CmdResult {
            command: command.to_string(),
            ts: common::now_str(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            relay_err: Some(msg),
            dur_ms: 0,
        }
    }
    /// 合并可复制的纯文本
    pub(super) fn to_text(&self) -> String {
        let mut s = String::new();
        if !self.stdout.is_empty() {
            s.push_str(&self.stdout);
            s.push('\n');
        }
        if !self.stderr.is_empty() {
            s.push_str("[stderr]\n");
            s.push_str(&self.stderr);
            s.push('\n');
        }
        if let Some(e) = &self.relay_err {
            s.push_str(&format!("[relay error] {e}\n"));
        }
        s.push_str(&format!("exit_code={}", self.exit_code));
        s
    }
    /// 历史下拉显示的摘要（命令 + 退出码 + 耗时）
    pub(super) fn summary(&self) -> String {
        let ec = if self.exit_code == 0 {
            "✓".to_string()
        } else {
            format!("✘{}", self.exit_code)
        };
        format!("[{}] {}  {}  {}ms", self.ts, ec, self.command, self.dur_ms)
    }
}

/// 解析一次并缓存的事件行（避免每帧重复 JSON 解析）
pub(super) struct ParsedEvent {
    pub(super) level: EventLevel,
    pub(super) color: egui::Color32,
    pub(super) ts: String,
    pub(super) sym: String,
    pub(super) cmd: String,
    pub(super) meta: String,
}

/// 解析事件 JSONL 行为缓存结构（单次解析；失败降级为中性条目）
pub(super) fn parse_event(line: &str) -> ParsedEvent {
    let gray = C_TEXT_DIM;
    let fallback = ParsedEvent {
        level: EventLevel::Service,
        color: gray,
        ts: "--:--:--".to_string(),
        sym: "·".to_string(),
        cmd: line.to_string(),
        meta: String::new(),
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return fallback;
    };
    let ts = v["ts"].as_str().unwrap_or("--:--:--").to_string();
    let ty = v["type"].as_str().unwrap_or("");
    let cmd = v["cmd"].as_str().unwrap_or("").replace('\n', " ");
    let caller = v["caller"].as_str().unwrap_or("");
    let pid = v["pid"].as_i64().unwrap_or(0);
    let to_ms = v["timeout_ms"].as_i64().unwrap_or(0);
    let dur_ms = v["duration_ms"].as_i64().unwrap_or(0);
    let exit = v["exit"].as_i64().unwrap_or(-99);
    let preview = v["out_preview"].as_str().unwrap_or("").replace('\n', " ");

    match ty {
        "exec_start" => ParsedEvent {
            level: EventLevel::Start,
            color: C_INFO,
            ts,
            sym: "▶ 开始".to_string(),
            cmd: if cmd.is_empty() { "(空命令)".into() } else { cmd },
            meta: format!("pid={pid} · {to_ms}ms"),
        },
        "exec_done" => {
            let ok = exit == 0;
            ParsedEvent {
                level: if ok { EventLevel::DoneOk } else { EventLevel::DoneErr },
                color: if ok { c_ok() } else { c_err() },
                ts,
                sym: if ok { "✔ 完成".to_string() } else { "✘ 完成".to_string() },
                cmd,
                meta: if preview.is_empty() {
                    format!("exit={exit} · {dur_ms}ms · pid={pid}")
                } else {
                    format!("exit={exit} · {dur_ms}ms · pid={pid} · 「{}」", preview.chars().take(40).collect::<String>())
                },
            }
        }
        "token_rotate" => ParsedEvent {
            level: EventLevel::Service,
            color: c_warn(),
            ts,
            sym: "⚠ 轮换".to_string(),
            cmd: "令牌".to_string(),
            meta: "旧令牌立即失效".to_string(),
        },
        "service_start" => ParsedEvent {
            level: EventLevel::Service,
            color: C_NEUTRAL,
            ts,
            sym: "◇ 启动".to_string(),
            cmd: v["pipe"].as_str().unwrap_or("").to_string(),
            meta: format!("caller={caller}"),
        },
        _ => fallback,
    }
}

/// 解析审计日志单行 →（时间戳, 身份, 退出码, 命令），用于固定列宽渲染对齐。
/// 兼容新格式 `[ts] caller=DOMAIN\user pid=N exit=N cmd=...` 与旧格式 `[ts] user=XXX exit=N cmd=...`。
pub(super) fn parse_log_row(line: &str) -> (String, String, Option<i32>, String) {
    let ts = line
        .chars()
        .skip(1)
        .take(19)
        .collect::<String>();
    // 身份段：caller=DOMAIN\user（或旧式 user=XXX）。
    // 从 caller=/user= 截取并截到 pid= 之前，去掉行首 [ts] 时间戳前缀，
    // 避免与时间戳列（ts）内容重复、挤占身份列宽度（修复 G2）。
    let id_part = line
        .split("caller=")
        .nth(1)
        .or_else(|| line.split("user=").nth(1))
        .map(|s| s.split(" pid=").next().unwrap_or(s).trim().to_string())
        .unwrap_or_default();
    let exit = extract_exit(line);
    let cmd = line
        .split("cmd=")
        .nth(1)
        .unwrap_or("")
        .to_string();
    (ts, id_part, exit, cmd)
}

/// 从审计日志行提取退出码（`... exit=0 cmd=...`），无则 None
fn extract_exit(line: &str) -> Option<i32> {
    let idx = line.find("exit=")?;
    let rest = &line[idx + 5..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    num.parse().ok()
}

/// 检测文本是否包含当前 SYSTEM 中继令牌（精确比对，避免 SHA-256 等 64 位哈希误杀）。
///
/// 令牌由 `common::gen_token` 生成：32 随机字节 → 恰好 64 个十六进制字符。
/// 旧启发式「连续 ≥64 个 hex 字符即告警」会把 `certutil -hashfile`、证书指纹等
/// 正常 64-hex 串也判为令牌，造成复制被误拦截（G4 修复）。现改为与本机**当前真实令牌**
/// 做子串精确匹配：仅当文本确实包含该令牌时才拦截，既杜绝误报，又仍保护真实凭据。
/// `token` 为空（极端情况未读到令牌）时返回 false，不误拦。
pub(super) fn contains_token_like(s: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    s.contains(token)
}
