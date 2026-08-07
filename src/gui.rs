//! 原生 GUI 控制面板（egui 即时模式）+ 系统托盘常驻（tray-icon）
//!
//! 功能：服务状态查看、启停/安装/卸载（仅必须管理员的走 UAC）、启用开关、
//! 令牌指纹与轮换、执行日志查看，以及通过管道直接下发命令（非提权即可）。
//! 托盘：状态角标（绿=运行且启用 / 红=运行但禁用 / 灰=停止或未安装）+ 右键菜单；
//! 关闭窗口 = 最小化到托盘（不退出），托盘菜单「退出」才结束 GUI 进程（不影响服务）。
//!
//! 界面结构（2026-08-06 重构 v2 优化 + v4 Linear flat 纯色）：
//! - 顶部：标题 + 状态徽章 + 启用开关 + 操作提示（固定）
//! - 左栏：服务控制 / 令牌 / 管理员操作（折叠，默认收起）
//! - 主区：三 Tab 共享 —— [命令台] [实时信息流] [审计日志]（Ctrl+1/2/3 切换）
//! - 底部：状态栏（最近退出码 / 耗时 / 事件数 / 日志行数）
//!
//! v2 优化点（对照锐评 13 项）：
//! 1. 事件流单次解析（ParsedEvent 缓存，筛选/渲染零重复解析）—— P0
//! 2. 日志增量读（read_log_since + offset，不再整文件 read）—— P0
//! 3. 输出历史（VecDeque 环形缓冲 20 条，可翻看历史命令输出）—— P0
//! 4. 取消等待（运行中命令可放弃等待，服务端可能仍在执行）—— P1
//! 5. PID 精确过滤（pid 字段精确比较，非子串包含）—— P1
//! 6. 事件流暂停滚动开关（与输出区一致）—— P1
//! 7. 快捷命令真分组（组标签彩色小字 + 组间分隔）—— P2
//! 8. 滚动区高度自适应（available_height，不再写死）—— P1
//! 9. Tab 快捷键 Ctrl+1/2/3 —— P2
//! 10. 删除死代码（event_display / log_line_rich）—— P2
//! 11. 令牌复制警告（输出含 64-hex 时拦截复制并警告）—— P1
//!
//! 设计红线（不变）：不制造新的授权摩擦点 ——
//!   - start / stop：install/grant 后用户 SID 已持 GRGX，直接非提权调用服务控制接口；
//!   - install / uninstall / rotate-token：必须管理员权限，走 UAC + 后台轮询确认；
//!   - 管道命令执行：异步线程 + channel，避免同步 IO 阻塞 egui 事件循环。
//!
//! 模块拆分（2026-08-07）：原单文件 ~1960 行职责过重，按职责下沉子模块：
//!   - theme.rs      配色/字号 token、字体加载、主题装配与锁定
//!   - tray.rs       系统托盘常驻、角标同步、菜单事件（init_tray/sync_tray/poll_tray_events）
//!   - parse.rs      数据类型（CmdResult/ParsedEvent/EventLevel）与解析（事件/日志行）
//!   - views.rs      三 Tab 视图渲染 + eframe::App::update 主循环
//!   - ui_widgets.rs 无状态 UI 辅助控件
//! 本文件（gui 模块根）仅保留：WbRelayApp 结构体、控制器核心逻辑、入口 run()，
//! 以及少量解析/IO 辅助函数；子模块符号经 `use` 引入（均为 pub(super)，对父模块可见）。

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::common;
use crate::protocol;

// 子模块：按职责下沉（详见文件头说明）。headless 变体（`--no-default-features`）不引用本模块。
mod theme;
mod tray;
mod views;
mod parse;
mod ui_widgets;

// 从子模块引入本文件直接引用到的符号（均为 pub(super)，对父模块 gui 可见）
use parse::{parse_event, CmdResult, EventLevel, ParsedEvent};
use theme::{install_cjk_fonts, install_theme};
use tray::TrayState;
use views::MainTab;

/// 输出历史环形缓冲容量
const OUTPUT_HISTORY: usize = 20;
/// 事件流环形缓冲容量
const EVENT_HISTORY: usize = 500;

pub fn run() {
    let options = eframe::NativeOptions {
        // 窗口自适应：初始 1280×800，最小 960×600，防止拖拽到布局崩坏
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "AI-root 控制台",
        options,
        Box::new(|cc| {
            install_cjk_fonts(&cc.egui_ctx);
            install_theme(&cc.egui_ctx);
            // 托盘事件经 handler 转发 + request_repaint 唤醒主循环，
            // 否则窗口隐藏到托盘后 eframe 主循环 Wait 停转、菜单点击（含退出）不响应
            tray::install_tray_handlers(&cc.egui_ctx);
            Ok(Box::new(WbRelayApp::new()))
        }),
    );
}

struct WbRelayApp {
    status: String,
    enabled: bool,
    /// 审计日志（增量追加，保留最近 N 行）
    log: String,
    /// 日志增量读偏移
    log_offset: u64,
    /// 日志轮询节流
    log_last_poll: Instant,
    run_cmd: String,
    /// 输出历史环形缓冲（0=最新）
    outputs: VecDeque<CmdResult>,
    /// 当前查看的历史索引（0=最新）
    output_idx: usize,
    /// 命令执行中标记
    cmd_busy: bool,
    /// 输出区自动滚动
    output_autoscroll: bool,
    /// 令牌指纹（前 8 位）与文件修改时间
    token_fp: String,
    token_mtime: String,
    /// 管道命令执行（后台线程）结果通道
    cmd_rx: Option<Receiver<CmdResult>>,
    /// 令牌轮换（UAC 后台进程）确认通道
    rotate_rx: Option<Receiver<bool>>,
    /// 危险操作二次确认状态
    confirm_rotate: bool,
    confirm_uninstall: bool,
    /// 操作结果提示（含 UAC 触发反馈）
    hint: String,
    /// 命令历史（↑↓ 翻页 / 下拉重放），去重，上限 20
    cmd_history: Vec<String>,
    /// 历史翻页游标（None=回到输入框当前内容）
    hist_cursor: Option<usize>,
    /// 审计日志过滤关键字（空 = 显示全部）
    log_filter: String,
    /// 审计日志 PID 筛选（空 = 全部；精确匹配 pid 字段）
    log_pid: String,
    /// 实时信息流（解析缓存）
    events: VecDeque<ParsedEvent>,
    /// 上次已读的 events.jsonl 字节偏移
    events_offset: u64,
    /// 信息流轮询节流
    events_last_poll: Instant,
    /// 事件流自动滚动
    events_autoscroll: bool,
    /// 主区当前 Tab
    tab: MainTab,
    /// 信息流等级筛选
    event_level: EventLevel,
    /// 系统托盘（常驻角标 + 右键菜单）
    tray: Option<TrayState>,
    /// 托盘状态同步节流（每 2s 最多重建一次图标）
    tray_last_sync: Instant,
}

impl WbRelayApp {
    fn new() -> Self {
        let enabled = common::is_enabled();
        let status = query_status_string();
        // 日志初值：整读一次作为基线，offset 记到文件末尾
        let log = common::read_log_tail(200);
        let log_offset = std::fs::metadata(common::log_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let (fp, mt) = token_info();
        let mut app = Self {
            status,
            enabled,
            log,
            log_offset,
            log_last_poll: Instant::now(),
            run_cmd: String::new(),
            outputs: VecDeque::new(),
            output_idx: 0,
            cmd_busy: false,
            output_autoscroll: true,
            token_fp: fp,
            token_mtime: mt,
            cmd_rx: None,
            rotate_rx: None,
            confirm_rotate: false,
            confirm_uninstall: false,
            hint: String::new(),
            cmd_history: Vec::new(),
            hist_cursor: None,
            log_filter: String::new(),
            log_pid: String::new(),
            events: VecDeque::new(),
            events_offset: 0,
            events_last_poll: Instant::now(),
            events_autoscroll: true,
            tab: MainTab::Cmd,
            event_level: EventLevel::All,
            tray: None,
            tray_last_sync: Instant::now(),
        };
        app.init_tray();
        app
    }

    fn refresh(&mut self) {
        self.status = query_status_string();
        self.enabled = common::is_enabled();
        // 日志重置偏移全量重读（文件可能被截断）
        self.log = common::read_log_tail(200);
        self.log_offset = std::fs::metadata(common::log_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let (fp, mt) = token_info();
        self.token_fp = fp;
        self.token_mtime = mt;
        self.sync_tray(true);
    }

    /// 以提权方式执行 ai-root 子命令（仅限必须管理员的操作）
    fn elevate(&self, sub: &str) -> bool {
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_str = exe.to_string_lossy().to_string();
        let ps_cmd = format!(
            "Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs",
            exe_str.replace('\'', "''"),
            sub.replace('\'', "''")
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 非提权服务控制：install/grant 后用户 SID 已持 GRGX，直接调服务接口
    fn svc_start(&mut self) {
        match crate::service::start_svc() {
            Ok(()) => {
                self.hint = "✓ 服务已启动".to_string();
                self.refresh();
            }
            Err(e) => self.ctl_err("启动", e),
        }
    }

    fn svc_stop(&mut self) {
        match crate::service::stop_svc() {
            Ok(()) => {
                self.hint = "✓ 服务已停止".to_string();
                self.refresh();
            }
            Err(e) => self.ctl_err("停止", e),
        }
    }

    /// 服务控制失败提示：区分「未授权」（需管理员 grant）与其它错误
    fn ctl_err(&mut self, what: &str, e: Box<dyn std::error::Error>) {
        let es = e.to_string();
        let denied = (es.contains("5") && es.to_lowercase().contains("access"))
            || es.to_lowercase().contains("access is denied");
        self.hint = if denied {
            format!(
                "✗ {what}失败：权限不足。\n请先运行一次安装/授权（需管理员一次），此后启停/更新不再需要提权"
            )
        } else {
            format!("✗ {what}失败: {es}")
        };
    }

    /// 触发 UAC 轮换令牌。rotate 输出包含明文令牌，不向 GUI 展示；
    /// 后台轮询 token.txt mtime 变化确认轮换已生效。
    fn rotate_trigger(&mut self) {
        let before = file_mtime();
        if !self.elevate("rotate-token") {
            self.hint = "✗ 未获系统确认（可能被取消）".to_string();
            self.confirm_rotate = false;
            return;
        }
        self.hint = "已请求系统确认，等待令牌轮换完成…\n新令牌自动生效，不在界面显示明文".to_string();
        self.confirm_rotate = false;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..60 {
                std::thread::sleep(Duration::from_millis(500));
                if let Some(now) = file_mtime() {
                    if before.is_none() || Some(now) != before {
                        let _ = tx.send(true);
                        return;
                    }
                }
            }
            let _ = tx.send(false);
        });
        self.rotate_rx = Some(rx);
    }

    /// 异步执行管道命令：后台线程完成同步管道 IO，结果经 channel 回传
    fn execute_async(&mut self, command: &str) {
        let token = match common::read_token() {
            Some(t) => t,
            None => {
                self.push_output(CmdResult::relay_failure(
                    command,
                    "未找到令牌，请先以管理员运行 install".to_string(),
                ));
                return;
            }
        };
        let cmd = command.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = run_pipe(&token, &cmd);
            let _ = tx.send(out);
        });
        self.cmd_rx = Some(rx);
        self.cmd_busy = true;
    }

    /// 放弃等待：丢弃接收端（服务端可能仍在执行，GUI 不再等待结果）
    fn cancel_wait(&mut self) {
        self.cmd_rx = None;
        self.cmd_busy = false;
        self.hint = "⚠ 已放弃等待（服务端可能仍在执行该命令）".to_string();
    }

    /// 输出入历史环形缓冲（0 索引 = 最新）
    fn push_output(&mut self, res: CmdResult) {
        self.output_idx = 0;
        self.outputs.push_front(res);
        while self.outputs.len() > OUTPUT_HISTORY {
            self.outputs.pop_back();
        }
    }

    /// 每帧轮询后台任务结果（命令执行 / 令牌轮换确认）
    fn poll_tasks(&mut self) {
        if let Some(rx) = self.cmd_rx.take() {
            match rx.try_recv() {
                Ok(res) => {
                    self.push_output(res);
                    self.output_autoscroll = true;
                    self.cmd_busy = false;
                }
                Err(mpsc::TryRecvError::Empty) => self.cmd_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let cmd = self
                        .outputs
                        .front()
                        .map(|o| o.command.clone())
                        .unwrap_or_default();
                    self.push_output(CmdResult::relay_failure(
                        &cmd,
                        "后台任务异常退出".to_string(),
                    ));
                    self.cmd_busy = false;
                }
            }
        }
        if let Some(rx) = self.rotate_rx.take() {
            match rx.try_recv() {
                Ok(true) => {
                    self.hint = "✓ 令牌已轮换（新令牌已生效，AI 侧自动使用新令牌）".to_string();
                    self.refresh();
                }
                Ok(false) => {
                    self.hint = "⚠ 轮换未在 30 秒内确认。若已确认仍失败，请重试或手动执行轮换命令".to_string();
                }
                Err(mpsc::TryRecvError::Empty) => self.rotate_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
    }

    /// 统一命令执行入口：非空校验 + 写入历史（去重，上限 20）+ 异步执行
    fn run_cmd_now(&mut self) {
        let cmd = self.run_cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        if self.cmd_history.last() != Some(&cmd) {
            self.cmd_history.push(cmd.clone());
            if self.cmd_history.len() > 20 {
                self.cmd_history.remove(0);
            }
        }
        self.hist_cursor = None;
        let c = self.run_cmd.clone();
        self.execute_async(&c);
    }

    /// 实时信息流：增量读取 events.jsonl（1s 节流），单次解析缓存
    fn poll_events(&mut self) {
        if self.events_last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.events_last_poll = Instant::now();
        let (lines, new_off) = common::read_events_since(self.events_offset);
        self.events_offset = new_off;
        if !lines.is_empty() {
            for line in lines {
                self.events.push_back(parse_event(&line));
            }
            while self.events.len() > EVENT_HISTORY {
                self.events.pop_front();
            }
        }
    }

    /// 审计日志增量刷新（1s 节流）：从 offset 追加新行，保留最近 200 行
    fn poll_logs(&mut self) {
        if self.log_last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.log_last_poll = Instant::now();
        let (lines, new_off) = common::read_log_since(self.log_offset);
        self.log_offset = new_off;
        if !lines.is_empty() {
            let mut all = self.log.lines().map(|s| s.to_string()).collect::<Vec<_>>();
            all.extend(lines);
            let keep = all.len().saturating_sub(200);
            self.log = all.split_off(keep).join("\n");
        }
    }

    /// 事件是否命中当前等级筛选（枚举比较，零解析）
    fn event_matches(&self, ev: &ParsedEvent) -> bool {
        match self.event_level {
            EventLevel::All => true,
            EventLevel::Start => ev.level == EventLevel::Start,
            EventLevel::DoneOk => ev.level == EventLevel::DoneOk,
            EventLevel::DoneErr => ev.level == EventLevel::DoneErr,
            EventLevel::Service => ev.level == EventLevel::Service,
        }
    }

    /// 日志行是否命中关键字 + PID 双重过滤（PID 精确匹配字段，非子串）
    fn log_matches(&self, line: &str) -> bool {
        let l = line.to_lowercase();
        if !self.log_filter.is_empty() && !l.contains(&self.log_filter.to_lowercase()) {
            return false;
        }
        if !self.log_pid.trim().is_empty() {
            let target = self.log_pid.trim();
            // 精确匹配 pid 字段：pid=123 后跟空格或行尾
            let matched = line.split(" pid=").nth(1).map_or(false, |rest| {
                rest.split_whitespace().next().map_or(false, |p| p == target)
            });
            if !matched {
                return false;
            }
        }
        true
    }
}

/// 查询服务状态字符串
fn query_status_string() -> String {
    match crate::service::query_status() {
        Ok(state) => format!("{:?}", state),
        Err(_) => "未安装 / 不可访问".to_string(),
    }
}

/// 令牌指纹（前 8 位）与文件写入时间；未安装时返回占位
fn token_info() -> (String, String) {
    match common::read_token() {
        Some(t) => {
            let fp = if t.len() >= 8 { t[..8].to_string() } else { t.clone() };
            let mt = std::fs::metadata(common::token_path())
                .and_then(|m| m.modified())
                .ok()
                .map(|st| {
                    let dt: chrono::DateTime<chrono::Utc> = st.into();
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_default();
            (fp, mt)
        }
        None => ("（未安装）".to_string(), String::new()),
    }
}

/// token.txt 的修改时间（轮换确认用）
fn file_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(common::token_path())
        .and_then(|m| m.modified())
        .ok()
}

/// 同步执行一次管道请求并返回结构化结果（在后台线程运行）
fn run_pipe(token: &str, command: &str) -> CmdResult {
    let start = Instant::now();
    let mut file = match protocol::connect_client() {
        Ok(f) => f,
        Err(e) => {
            return CmdResult::relay_failure(command, format!("连接失败: {e}（服务是否运行？）"))
        }
    };
    let req = protocol::Request {
        token: token.to_string(),
        command: command.to_string(),
        cwd: None,
        timeout_ms: Some(common::DEFAULT_COMMAND_TIMEOUT_MS),
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => return CmdResult::relay_failure(command, format!("序列化失败: {e}")),
    };
    if protocol::write_frame(&mut file, &bytes).is_err() {
        return CmdResult::relay_failure(command, "发送失败".to_string());
    }
    let resp_bytes = match protocol::read_frame(&mut file) {
        Ok(b) => b,
        Err(e) => return CmdResult::relay_failure(command, format!("读取失败: {e}")),
    };
    let dur_ms = start.elapsed().as_millis() as u64;
    match serde_json::from_slice::<protocol::Response>(&resp_bytes) {
        Ok(resp) => {
            // 输出被服务端截断时在 relay_err 位置给出提示（非错误，仅信息）
            let mut relay_err = resp.error;
            if resp.truncated {
                let msg = "[提示] 输出超过 1MB 已被服务端截断".to_string();
                relay_err = Some(match relay_err {
                    Some(e) => format!("{e}\n{msg}"),
                    None => msg,
                });
            }
            CmdResult {
                command: command.to_string(),
                ts: common::now_str(),
                stdout: resp.stdout,
                stderr: resp.stderr,
                exit_code: resp.exit_code,
                relay_err,
                dur_ms,
            }
        }
        Err(e) => CmdResult::relay_failure(command, format!("响应解析失败: {e}")),
    }
}
