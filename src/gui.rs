//! 原生 GUI 控制面板（egui 即时模式）+ 系统托盘常驻（tray-icon）
//!
//! 功能：服务状态查看、启停/安装/卸载（仅必须管理员的走 UAC）、启用开关、
//! 令牌指纹与轮换、执行日志查看，以及通过管道直接下发命令（非提权即可）。
//! 托盘：状态角标（绿=运行且启用 / 红=运行但禁用 / 灰=停止或未安装）+ 右键菜单；
//! 关闭窗口 = 最小化到托盘（不退出），托盘菜单「退出」才结束 GUI 进程（不影响服务）。
//!
//! 界面结构（2026-08-06 重构 v2 优化 + v3 极夜青配色）：
//! - 顶部：标题 + 状态徽章 + 启用开关 + 操作提示（固定）
//! - 左栏：服务控制 / 令牌 / 管理员操作（折叠，默认收起）
//! - 主区：三 Tab 共享 —— [命令台] [实时信息流] [审计日志]（Ctrl+1/2/3 切换）
//! - 底部：状态栏（最近退出码 / 耗时 / 事件数 / 日志行数）
//!
//! v3 配色「极夜青」（2026-08-06）：
//! 深空蓝灰中性阶（#0B0F14→#2E3A4A）+ GitHub-Dark 同源语义色
//! （success #3FB950 / warning #D29922 / danger #F85149 / info #58A6FF）
//! + brand #3D8BFF + 活动指示 accent #2DD4BF。
//! Tab 选中态 = brand 描边 + accent 底部 2px 指示条（区别于 hover 的 border 变蓝）。
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

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::common;
use crate::protocol;

// ── 视觉 token「极夜青」方案（v3，2026-08-06）──
// 深空蓝灰中性阶 + GitHub-Dark 同源语义色 + 冷青活动指示。
// 设计依据：对比度 ≥7:1（WCAG AAA）、语义不打架（绿=成功/红=危险/蓝=操作/青=活动）。
// 注：窗口底/面板底色不再用常量（install_theme 玻璃拟态直接用 from_rgba_unmultiplied 构造）。
const C_BG_INPUT: egui::Color32 = egui::Color32::from_rgb(26, 32, 42); // 卡片/输入底 #1A202A
const C_BG_CARD: egui::Color32 = egui::Color32::from_rgb(26, 32, 42); // 卡片 #1A202A
const C_BG_HOVER: egui::Color32 = egui::Color32::from_rgb(35, 43, 55); // 悬浮 #232B37
const C_BORDER: egui::Color32 = egui::Color32::from_rgb(46, 58, 74); // 描边 #2E3A4A
const C_TEXT: egui::Color32 = egui::Color32::from_rgb(230, 237, 243); // 主文本 #E6EDF3
const C_TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(154, 167, 180); // 次要 #9AA7B4
// 状态语义色（规范：不改变字号，仅着色）：
//   成功绿 rgba(0.25,0.85,0.45,0.95) / 警告黄 rgba(0.92,0.72,0.22,0.95) / 错误红 rgba(0.90,0.32,0.32,0.95)
// 注：Color32::from_rgba_unmultiplied 非 const、字段私有 → 用运行时求值函数
fn c_ok() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(64, 217, 115, 242)
}
fn c_warn() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(235, 184, 56, 242)
}
fn c_err() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(230, 82, 82, 242)
}
const C_INFO: egui::Color32 = egui::Color32::from_rgb(88, 166, 255); // info #58A6FF
const C_NEUTRAL: egui::Color32 = egui::Color32::from_rgb(139, 148, 158); // 中性灰 #8B949E
const C_BRAND: egui::Color32 = egui::Color32::from_rgb(61, 139, 255); // brand #3D8BFF
const C_ACCENT: egui::Color32 = egui::Color32::from_rgb(45, 212, 191); // 活动指示 #2DD4BF

// ── 5 级文字层级（暗色终端控制台，RGBA 0-1 浮点换算 0-255）──
// L1 主标题 16px 粗体 rgba(0.95,0.95,0.97,1.0) —— 窗口大标题
// L2 模块标题/Tab 14px medium rgba(0.85,0.87,0.90,0.95) —— 面板标题、Tab 标签
// L3 主体正文 13px 常规 rgba(0.80,0.82,0.85,0.90) —— 按钮/输入框/表格主列/输出日志
// L4 次要辅助 12px 常规 rgba(0.60,0.63,0.68,0.75) —— 说明文本/描述列/状态栏次要
// L5 弱占位 11px rgba(0.45,0.48,0.52,0.55) —— 输入框占位符
const FS_L1: f32 = 16.0;
const FS_L2: f32 = 14.0;
const FS_L3: f32 = 13.0;
const FS_L4: f32 = 12.0;
const FS_L5: f32 = 11.0;
// 文字层级颜色（运行时求值，见上：Color32::from_rgba_unmultiplied 非 const）
fn txt_l1() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(242, 242, 247, 255)
}
fn txt_l2() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(217, 222, 230, 242)
}
fn txt_l3() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(204, 209, 217, 230)
}
fn txt_l4() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(153, 161, 173, 191)
}
fn txt_l5() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(115, 122, 133, 140)
}

/// 输出历史环形缓冲容量
const OUTPUT_HISTORY: usize = 20;
/// 事件流环形缓冲容量
const EVENT_HISTORY: usize = 500;

pub fn run() {
    let options = eframe::NativeOptions {
        ..Default::default()
    };
    let _ = eframe::run_native(
        "AI-root 控制台",
        options,
        Box::new(|cc| {
            install_cjk_fonts(&cc.egui_ctx);
            install_theme(&cc.egui_ctx);
            Ok(Box::new(WbRelayApp::new()))
        }),
    );
}

/// 深色「玻璃拟态运维台」主题（Glassmorphism）：
/// 半透明低饱和深蓝灰面板（露出底层渐变背景，视觉等效毛玻璃）+ 1px 细描边 + 大圆角。
/// 语义色仅用于状态（徽章/结果），标题/说明用中性色；拒绝厚重纯色硬边框。
fn install_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    // 面板：低饱和深蓝灰半透明（alpha 混合露出背景渐变 → 玻璃感）
    v.panel_fill = egui::Color32::from_rgba_unmultiplied(16, 20, 28, 218);
    v.window_fill = egui::Color32::from_rgba_unmultiplied(20, 25, 34, 224);
    v.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 7); // 玻璃高光
    v.extreme_bg_color = egui::Color32::from_rgba_unmultiplied(8, 11, 15, 208); // 输入框底
    v.code_bg_color = egui::Color32::from_rgba_unmultiplied(10, 13, 18, 205);
    v.override_text_color = Some(txt_l3()); // L3 正文为全局默认；输入框占位符由 override gamma 弱化（≈L5）

    // 5 级字号层级：Heading16/Body13/Button13/Small12/Monospace13（px）
    ctx.style_mut(|s| {
        use egui::{FontId, TextStyle};
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(FS_L1));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(FS_L3));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(FS_L3));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(FS_L4));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(FS_L3));
    });

    // 选区 / 链接：品牌蓝（低饱和细边）
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(61, 139, 255, 150);
    v.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(45, 212, 191, 160));
    v.hyperlink_color = C_INFO;

    // 控件四态：非交互 < 常态 < 悬浮 < 按下；统一 8px 圆角 + 1px 半透明细描边
    let base_radius = egui::Rounding::same(8.0);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = base_radius;
        w.expansion = 0.0;
    }
    let hairline = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14); // 细描边 white 5.5%
    let hairline_brand = egui::Color32::from_rgba_unmultiplied(61, 139, 255, 95); // 悬浮 brand 细边
    v.widgets.noninteractive.bg_fill = egui::Color32::from_rgba_unmultiplied(23, 28, 37, 205);
    v.widgets.noninteractive.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(16, 20, 28, 200);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    v.widgets.inactive.bg_fill = egui::Color32::from_rgba_unmultiplied(23, 28, 37, 205);
    v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(16, 20, 28, 200);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    v.widgets.hovered.bg_fill = egui::Color32::from_rgba_unmultiplied(34, 42, 54, 212);
    v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(28, 35, 46, 210);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, hairline_brand);
    v.widgets.active.bg_fill = egui::Color32::from_rgba_unmultiplied(61, 139, 255, 150);
    v.widgets.active.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(61, 139, 255, 120);
    v.widgets.open.bg_fill = egui::Color32::from_rgba_unmultiplied(34, 42, 54, 212);
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, hairline_brand);

    // 间距节奏：全局 8px 网格（边距/padding/margin 一律 8px 整数倍）
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0); // 控件间距 8
        s.spacing.button_padding = egui::vec2(16.0, 8.0); // 按钮内 16 水平 × 8 垂直
        s.spacing.window_margin = egui::Margin::same(8.0); // 面板内边距 8
    });
    v.window_rounding = egui::Rounding::same(0.0);

    ctx.set_visuals(v);
}

/// 加载系统中文字体。egui 内置字体不含 CJK 字形，不加载则中文渲染为方块。
fn install_cjk_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push("cjk".to_owned());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

// ── 主区 Tab 定义（任务导向：命令 / 事件 / 日志 三视图）──
#[derive(PartialEq, Clone, Copy)]
enum MainTab {
    Cmd,
    Events,
    Log,
}

impl MainTab {
    fn label(self) -> &'static str {
        match self {
            MainTab::Cmd => "命令台",
            MainTab::Events => "实时信息流",
            MainTab::Log => "审计日志",
        }
    }
    fn hotkey(self) -> &'static str {
        match self {
            MainTab::Cmd => "Ctrl+1",
            MainTab::Events => "Ctrl+2",
            MainTab::Log => "Ctrl+3",
        }
    }
}

/// 实时信息流等级筛选（与 ParsedEvent.level 对应，枚举比较零字符串操作）
#[derive(PartialEq, Clone, Copy)]
enum EventLevel {
    All,
    Start,
    DoneOk,
    DoneErr,
    Service,
}

impl EventLevel {
    fn label(self) -> &'static str {
        match self {
            EventLevel::All => "全部",
            EventLevel::Start => "▶ 开始",
            EventLevel::DoneOk => "成功",
            EventLevel::DoneErr => "失败",
            EventLevel::Service => "◇ 服务",
        }
    }
}

/// 一次命令执行的结构化结果（含命令与时间戳，支撑输出历史）
struct CmdResult {
    command: String,
    ts: String,
    stdout: String,
    stderr: String,
    exit_code: i32,
    relay_err: Option<String>,
    dur_ms: u64,
}

impl CmdResult {
    /// 中继层失败（连接失败 / 超时 / spawn 失败等）：无 stdout/stderr，仅 error
    fn relay_failure(command: &str, msg: String) -> Self {
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
    fn to_text(&self) -> String {
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
    fn summary(&self) -> String {
        let ec = if self.exit_code == 0 {
            "✓".to_string()
        } else {
            format!("✘{}", self.exit_code)
        };
        format!("[{}] {}  {}  {}ms", self.ts, ec, self.command, self.dur_ms)
    }
}

/// 解析一次并缓存的事件行（避免每帧重复 JSON 解析）
struct ParsedEvent {
    level: EventLevel,
    color: egui::Color32,
    ts: String,
    sym: String,
    cmd: String,
    meta: String,
}

/// 快捷命令分组（真分组：组标签 + 组内命令，组间分隔）
const CMD_GROUPS: &[(&str, &[&str])] = &[
    ("身份", &["whoami", "whoami /priv", "whoami /groups"]),
    ("服务", &["sc query ai-root", "sc queryex ai-root"]),
    ("系统", &["tasklist /svc", "systeminfo"]),
    ("硬件", &["pnputil /enum-devices", "driverquery /v"]),
];

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

/// 托盘图标状态与菜单项句柄
struct TrayState {
    tray: TrayIcon,
    status_item: MenuItem,
    open_item: MenuItem,
    start_item: MenuItem,
    stop_item: MenuItem,
    enabled_item: CheckMenuItem,
    rotate_item: MenuItem,
    quit_item: MenuItem,
}

/// 托盘图标颜色：绿=运行且启用，红=运行但禁用，灰=停止/未安装
const ICON_GREEN: [u8; 3] = [59, 158, 95];
const ICON_RED: [u8; 3] = [214, 69, 69];
const ICON_GRAY: [u8; 3] = [138, 138, 138];

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

    /// 创建系统托盘图标与右键菜单；失败则静默降级为纯窗口模式
    fn init_tray(&mut self) {
        let menu = Menu::new();
        let status_item = MenuItem::new("状态: —", false, None);
        let open_item = MenuItem::new("打开控制台", true, None);
        let start_item = MenuItem::new("启动服务", true, None);
        let stop_item = MenuItem::new("停止服务", true, None);
        let enabled_item = CheckMenuItem::new("中继启用", self.enabled, true, None);
        let rotate_item = MenuItem::new("轮换令牌…", true, None);
        let quit_item = MenuItem::new("退出控制台（不影响服务）", true, None);

        menu.append(&status_item).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&open_item).ok();
        menu.append(&start_item).ok();
        menu.append(&stop_item).ok();
        menu.append(&enabled_item).ok();
        menu.append(&rotate_item).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&quit_item).ok();

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("ai-root 提权中继")
            .with_icon(make_icon(ICON_GRAY))
            .build()
            .ok();
        if let Some(tray) = tray {
            self.tray = Some(TrayState {
                tray,
                status_item,
                open_item,
                start_item,
                stop_item,
                enabled_item,
                rotate_item,
                quit_item,
            });
            self.sync_tray(true);
        }
    }

    /// 同步托盘角标颜色与菜单状态（2s 节流）
    fn sync_tray(&mut self, force: bool) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        if !force && self.tray_last_sync.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.tray_last_sync = Instant::now();

        let (color, text) = tray_status(&self.status, self.enabled);
        let _ = tray.tray.set_icon(Some(make_icon(color)));
        tray.status_item.set_text(text);
        let running = self.status.starts_with("Running");
        tray.start_item.set_enabled(!running);
        tray.stop_item.set_enabled(running);
        tray.enabled_item.set_checked(self.enabled);
    }

    /// 处理托盘右键菜单与左键点击事件（每帧轮询一次）
    fn poll_tray_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let Some(tray) = self.tray.as_ref() else {
                continue;
            };
            if ev.id == tray.open_item.id() {
                show_window(ctx);
            } else if ev.id == tray.start_item.id() {
                self.svc_start();
            } else if ev.id == tray.stop_item.id() {
                self.svc_stop();
            } else if ev.id == tray.enabled_item.id() {
                let new_en = !self.enabled;
                common::set_enabled(new_en);
                self.enabled = new_en;
                self.hint = if new_en {
                    "✓ 中继已启用".to_string()
                } else {
                    "⚠ 中继已禁用（AI 无法执行命令）".to_string()
                };
                self.sync_tray(true);
            } else if ev.id == tray.rotate_item.id() {
                self.confirm_rotate = true;
                show_window(ctx);
            } else if ev.id == tray.quit_item.id() {
                // 仅结束 GUI 进程；服务（LocalSystem）不受影响
                std::process::exit(0);
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                show_window(ctx);
            }
        }
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

    /// 状态徽章（颜色 + 文本），与托盘角标语义一致
    fn status_badge(&self) -> (egui::Color32, String) {
        let (rgb, text) = tray_status(&self.status, self.enabled);
        (egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]), text)
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

    /// ── 主区渲染：Tab 切换 + 三视图 ──
    fn render_main(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        // Tab 栏（自绘分段条；支持 Ctrl+1/2/3 快捷键切换）
        // 选中态：brand 描边 + accent 底部 2px 指示条（区别于 hover 的 border 变蓝）
        ui.horizontal(|ui| {
            let tabs = [MainTab::Cmd, MainTab::Events, MainTab::Log];
            for t in tabs {
                let selected = self.tab == t;
                let label = format!("{}  {}", t.label(), t.hotkey());
                let btn = egui::Button::new(
                    egui::RichText::new(label)
                        .strong()
                        .size(FS_L2) // L2 模块标题/Tab 14px
                        .color(if selected { txt_l1() } else { txt_l2() }),
                )
                .fill(if selected { C_BG_CARD } else { C_BG_CARD })
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if selected { C_BRAND } else { C_BORDER },
                ))
                // 圆角统一 8px；min_size 高 32px = 8px 网格整数倍，保证三 Tab 绝对等高、文字基线一致
                .rounding(egui::Rounding::same(8.0))
                .min_size(egui::vec2(0.0, 32.0));
                let resp = ui.add(btn);
                if selected {
                    // 底部 accent 指示条（2px，宽度收缩 8px 内边距，避免顶到圆角）
                    let r = resp.rect;
                    let bar = egui::Rect::from_min_max(
                        egui::pos2(r.left() + 4.0, r.bottom() - 2.0),
                        egui::pos2(r.right() - 4.0, r.bottom()),
                    );
                    ui.painter().rect_filled(bar, egui::Rounding::same(1.0), C_ACCENT);
                }
                if resp.clicked() {
                    self.tab = t;
                }
            }
            // Ctrl+1/2/3 快捷键（在 Tab 栏内统一处理）
            let (k1, k2, k3, ctrl) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Num1),
                    i.key_pressed(egui::Key::Num2),
                    i.key_pressed(egui::Key::Num3),
                    i.modifiers.command,
                )
            });
            if ctrl {
                if k1 {
                    self.tab = MainTab::Cmd;
                } else if k2 {
                    self.tab = MainTab::Events;
                } else if k3 {
                    self.tab = MainTab::Log;
                }
            }
        });
        ui.add_space(6.0);
        match self.tab {
            MainTab::Cmd => self.render_cmd_tab(ui),
            MainTab::Events => self.render_events_tab(ui),
            MainTab::Log => self.render_log_tab(ui),
        }
    }

    /// Tab 1 · 命令台：快捷命令真分组 + 输入行 + 历史 + 输出历史（可翻看）
    fn render_cmd_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("命令以 SYSTEM 身份执行，无需提权；多行内容自动作为脚本运行");
        ui.add_space(4.0);

        // 快捷命令：真分组（彩色组标签 + 组间分隔），命中当前输入高亮
        for (gi, (group, cmds)) in CMD_GROUPS.iter().enumerate() {
            if gi > 0 {
                ui.add_space(4.0);
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(*group)
                        .strong()
                        .color(C_BRAND)
                        .size(12.0),
                );
                for c in *cmds {
                    let active = self.run_cmd == *c;
                    let mut text = (*c).to_string();
                    if active {
                        text.push_str(" ✓");
                    }
                    let btn = egui::Button::new(
                        egui::RichText::new(text).monospace().size(12.0),
                    )
                    .fill(if active { C_BRAND } else { C_BG_CARD })
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if active { C_BRAND } else { C_BORDER },
                    ));
                    if ui.add(btn).clicked() {
                        self.run_cmd = (*c).to_string();
                        self.hist_cursor = None;
                        self.run_cmd_now();
                    }
                }
            });
        }
        ui.add_space(6.0);

        // 输入行：Enter 执行 / ↑↓ 历史翻页 / Esc 清空
        // 输入框与执行按钮等高 32px（8px 网格整数倍），同 horizontal 自动基线对齐
        ui.horizontal(|ui| {
            let input = ui.add_enabled(
                !self.cmd_busy,
                egui::TextEdit::singleline(&mut self.run_cmd)
                    .hint_text("输入命令（Enter 执行 · ↑↓ 翻历史 · Esc 清输入）…")
                    .desired_width((ui.available_width() - 136.0).max(160.0))
                    .desired_rows(1),
            );
            let enter_btn = ui.add_enabled(
                !self.cmd_busy,
                egui::Button::new("执行")
                    .min_size(egui::vec2(64.0, 32.0)),
            );
            // SDF 辉光：执行按钮 hover 时外扩光晕（品牌蓝）
            if enter_btn.hovered() && !self.cmd_busy {
                sdf_glow(ui, enter_btn.rect, C_BRAND, 9.0);
            }
            if enter_btn.clicked() {
                self.run_cmd_now();
            }
            if input.has_focus() {
                let (enter, up, down, esc) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::Escape),
                    )
                });
                if enter {
                    self.run_cmd_now();
                } else if up {
                    // 首次按 ↑ 回填最新一条；再按向上翻更旧的
                    let next = match self.hist_cursor {
                        None => 0,
                        Some(c) => c + 1,
                    };
                    self.hist_cursor = Some(next.min(self.cmd_history.len().max(1) - 1));
                    if let Some(c) = self.cmd_history.get(
                        self.cmd_history.len().saturating_sub(1 + self.hist_cursor.unwrap_or(0)),
                    ) {
                        self.run_cmd = c.to_string();
                    }
                } else if down {
                    if let Some(cur) = self.hist_cursor {
                        if cur > 0 {
                            self.hist_cursor = Some(cur - 1);
                            if let Some(c) = self.cmd_history.get(
                                self.cmd_history.len().saturating_sub(1 + self.hist_cursor.unwrap_or(0)),
                            ) {
                                self.run_cmd = c.to_string();
                            }
                        } else {
                            self.hist_cursor = None;
                            self.run_cmd.clear();
                        }
                    }
                } else if esc {
                    self.run_cmd.clear();
                    self.hist_cursor = None;
                }
            }
            if enter_btn.clicked() {
                self.run_cmd_now();
            }
            if self.cmd_busy {
                ui.spinner();
                ui.label("执行中…");
                // 取消等待
                if ui.button("取消").on_hover_text("放弃等待结果（服务端可能仍在执行）").clicked() {
                    self.cancel_wait();
                }
            }
        });
        ui.add_space(4.0);

        // 历史命令下拉（点击回放）
        if !self.cmd_history.is_empty() {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("命令历史:").strong().size(FS_L2).color(txt_l2()));
                egui::ComboBox::from_id_salt("hist_combo")
                    .selected_text("点击回放…")
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for c in self.cmd_history.iter().rev().take(20) {
                            if ui.button(c).clicked() {
                                self.run_cmd = c.clone();
                                self.hist_cursor = None;
                            }
                        }
                    });
            });
            ui.add_space(4.0);
        }

        // 输出区：徽章行 + 输出历史选择 + 只读输出
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("输出:").strong().size(FS_L2).color(txt_l2()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut as_ = self.output_autoscroll;
                let as_btn = egui::Button::new(if as_ { "⏸ 暂停滚动" } else { "▶ 恢复滚动" })
                    .selected(as_)
                    .fill(if as_ { C_BG_CARD } else { C_BRAND });
                if ui.add(as_btn).clicked() {
                    as_ = !as_;
                }
                self.output_autoscroll = as_;
                if ui
                    .button("复制输出")
                    .on_hover_text("将当前输出复制到剪贴板（含敏感令牌检测）")
                    .clicked()
                {
                    if let Some(r) = self.outputs.get(self.output_idx) {
                        let text = r.to_text();
                        if contains_token_like(&text) {
                            self.hint = "⚠ 已拦截：输出疑似包含 64 位十六进制令牌/密钥，未复制。确认无敏感信息后再手动选择复制".to_string();
                        } else {
                            ui.ctx().copy_text(text);
                            self.hint = "✓ 已复制到剪贴板".to_string();
                        }
                    }
                }
            });
        });

        // 输出历史选择（下拉切换查看）
        if self.outputs.len() > 1 {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("查看输出:").strong().size(FS_L2).color(txt_l2()));
                let cur = &self.outputs[self.output_idx];
                egui::ComboBox::from_id_salt("output_hist")
                    .selected_text(cur.summary())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for (i, r) in self.outputs.iter().enumerate() {
                            if ui
                                .selectable_label(i == self.output_idx, r.summary())
                                .clicked()
                            {
                                self.output_idx = i;
                            }
                        }
                    });
            });
            ui.add_space(2.0);
        }

        // 徽章行：exit code + 耗时 + stderr/relay 标记
        if let Some(r) = self.outputs.get(self.output_idx) {
            ui.horizontal(|ui| {
                let (ec_color, ec_text) = if r.exit_code == 0 {
                    (c_ok(), "exit=0".to_string())
                } else {
                    (c_err(), format!("exit={}", r.exit_code))
                };
                pill(ui, ec_color, &ec_text);
                pill(ui, C_TEXT_DIM, &format!("{}ms", r.dur_ms));
                if !r.stderr.is_empty() {
                    pill(ui, c_warn(), "含错误输出");
                }
                if let Some(e) = &r.relay_err {
                    pill(ui, c_err(), "中继错误");
                    ui.label(egui::RichText::new(e).color(c_err()).small().monospace());
                }
            });
            ui.add_space(2.0);
        }

        // 只读输出（可选中复制）：高度自适应；统一卡片容器（与其他两个面板一致）
        let avail_h = (ui.available_height() - 8.0).max(100.0);
        panel_frame(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(self.output_autoscroll)
                .max_height(avail_h)
                .show(ui, |ui| {
                        ui.style_mut().spacing.item_spacing.y = 4.0;
                        if let Some(r) = self.outputs.get(self.output_idx) {
                            if r.stdout.is_empty() && r.stderr.is_empty() && r.relay_err.is_none() {
                                ui.label(
                                    egui::RichText::new("命令无输出")
                                        .color(C_TEXT_DIM)
                                        .italics(),
                                );
                            }
                            if !r.stdout.is_empty() {
                                ui.label(
                                    egui::RichText::new(&r.stdout)
                                        .monospace()
                                        .color(C_TEXT)
                                        .size(13.0),
                                );
                            }
                            if !r.stderr.is_empty() {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(format!("[stderr]\n{}", r.stderr))
                                        .monospace()
                                        .color(c_warn())
                                        .size(13.0),
                                );
                            }
                            if let Some(e) = &r.relay_err {
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new(format!("[中继错误]\n{e}"))
                                        .monospace()
                                        .color(c_err())
                                        .size(13.0),
                                );
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("暂无输出 — 输入命令后回车执行")
                                    .color(C_TEXT_DIM)
                                    .italics(),
                            );
                        }
                    });
            });
        ui.add_space(4.0);
    }

    /// Tab 2 · 实时信息流：等级筛选 + 增量事件（开始/完成/轮换/服务启动）
    fn render_events_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("AI 执行动态").strong().size(FS_L2).color(txt_l2()));
            ui.label(format!("{} 条", self.events.len()));
            egui::ComboBox::from_id_salt("event_level")
                .selected_text(self.event_level.label())
                .show_ui(ui, |ui| {
                    for lv in [
                        EventLevel::All,
                        EventLevel::Start,
                        EventLevel::DoneOk,
                        EventLevel::DoneErr,
                        EventLevel::Service,
                    ] {
                        ui.selectable_value(&mut self.event_level, lv, lv.label());
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 暂停滚动开关（与输出区一致）
                let mut as_ = self.events_autoscroll;
                let as_btn = egui::Button::new(if as_ { "⏸ 暂停滚动" } else { "▶ 恢复滚动" })
                    .selected(as_)
                    .fill(if as_ { C_BG_CARD } else { C_BRAND });
                if ui.add(as_btn).clicked() {
                    as_ = !as_;
                }
                self.events_autoscroll = as_;
                if ui.button("重新加载").clicked() {
                    self.events.clear();
                    self.events_offset = 0;
                    self.poll_events();
                }
                if ui.button("清空显示").clicked() {
                    self.events.clear();
                }
            });
        });
        ui.add_space(4.0);
        let avail_h = (ui.available_height() - 8.0).max(100.0);
        // 统一卡片容器：与命令输出/审计日志外观一致
        panel_frame(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(self.events_autoscroll)
                .max_height(avail_h)
                .show(ui, |ui| {
                    ui.style_mut().spacing.item_spacing.y = 2.0;
                    let mut shown = 0;
                    for ev in &self.events {
                        if !self.event_matches(ev) {
                            continue;
                        }
                        shown += 1;
                        ui.add_space(2.0);
                        // 固定列宽表格行：时间戳(172)｜符号(56)｜命令(flex, 省略号)｜元数据(右对齐)
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 20.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.add_sized(
                                    [172.0, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new(&ev.ts)
                                            .monospace()
                                            .color(txt_l4())
                                            .size(12.5),
                                    ),
                                );
                                ui.add_sized(
                                    [56.0, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new(&ev.sym)
                                            .strong()
                                            .color(ev.color)
                                            .size(13.0),
                                    ),
                                );
                                let cmd_w = (ui.available_width() - 120.0).max(40.0);
                                ui.add_sized(
                                    [cmd_w, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new(&ev.cmd).strong().size(13.0),
                                    )
                                    .truncate(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_sized(
                                            [110.0, 18.0],
                                            egui::Label::new(
                                                egui::RichText::new(&ev.meta)
                                                    .color(txt_l4())
                                                    .small(),
                                            )
                                            .truncate(),
                                        );
                                    },
                                );
                            },
                        );
                    }
                    if shown == 0 {
                        ui.label(
                            egui::RichText::new(if self.events.is_empty() {
                                "暂无动态 — AI 执行命令后将在此实时展示"
                            } else {
                                "当前筛选无匹配事件"
                            })
                            .color(txt_l4())
                            .italics(),
                        );
                    }
                });
        });
        ui.add_space(4.0);
    }

    /// Tab 3 · 审计日志：关键字 + PID 精确过滤 + 着色条
    fn render_log_tab(&mut self, ui: &mut egui::Ui) {
        // 显式 id：清空过滤时据此重置 TextEditState（有焦点时外部字符串变更不会自动同步）
        let filter_id = egui::Id::new("log_filter_input");
        let pid_input_id = egui::Id::new("log_pid_input");
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("命令日志").strong().size(FS_L2).color(txt_l2()));
            ui.add(
                egui::TextEdit::singleline(&mut self.log_filter)
                    .id(filter_id)
                    .hint_text("过滤关键字…")
                    .desired_width(160.0),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.log_pid)
                    .id(pid_input_id)
                    .hint_text("PID 精确匹配…")
                    .desired_width(110.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新日志").clicked() {
                    self.refresh();
                }
                if ui
                    .button("清空过滤")
                    .on_hover_text("清空关键字与 PID 过滤")
                    .clicked()
                {
                    self.log_filter.clear();
                    self.log_pid.clear();
                    // 手动清掉两个输入框的 TextEditState：否则聚焦中的输入框
                    // 不会用外部空字符串刷新显示（egui 已知行为），看起来像「没清掉」
                    let ctx = ui.ctx().clone();
                    ctx.data_mut(|d| {
                        d.remove::<egui::text_edit::TextEditState>(filter_id);
                        d.remove::<egui::text_edit::TextEditState>(pid_input_id);
                    });
                    ctx.request_repaint();
                }
            });
        });
        ui.add_space(4.0);
        let avail_h = (ui.available_height() - 8.0).max(100.0);
        let mut shown = 0usize;
        // 统一卡片容器：与命令输出/实时信息流外观一致
        panel_frame(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .max_height(avail_h)
                .show(ui, |ui| {
                    ui.style_mut().spacing.item_spacing.y = 2.0;
                    for line in self.log.lines() {
                        if !self.log_matches(line) {
                            continue;
                        }
                        shown += 1;
                        let (ts, id, exit, cmd) = parse_log_row(line);
                        let stripe = match exit {
                            Some(0) => C_BORDER,
                            Some(_) => c_err(),
                            None => C_BG_HOVER,
                        };
                        let exit_color = match exit {
                            Some(0) => c_ok(),
                            Some(_) => c_err(),
                            None => txt_l4(),
                        };
                        // 固定列宽表格行：色条(3)｜时间戳(172)｜身份(240)｜退出码(64)｜命令(flex, 省略号)
                        ui.horizontal(|ui| {
                            let (stripe_rect, _) =
                                ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                            ui.painter()
                                .rect_filled(stripe_rect, egui::Rounding::same(1.0), stripe);
                            ui.add_space(4.0);
                            ui.add_sized(
                                [172.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(ts).monospace().color(txt_l4()).size(12.5),
                                ),
                            );
                            ui.add_sized(
                                [240.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(id).monospace().color(txt_l4()).size(12.5),
                                )
                                .truncate(),
                            );
                            let exit_text = match exit {
                                Some(c) => format!("exit={c}"),
                                None => "---".to_string(),
                            };
                            ui.add_sized(
                                [64.0, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(exit_text)
                                        .monospace()
                                        .strong()
                                        .color(exit_color)
                                        .size(12.5),
                                ),
                            );
                            let cmd_w = (ui.available_width() - 12.0).max(40.0);
                            ui.add_sized(
                                [cmd_w, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(cmd).monospace().color(txt_l3()).size(13.0),
                                )
                                .truncate(),
                            );
                        });
                    }
                    if shown == 0 {
                        ui.label(
                            egui::RichText::new(if self.log.is_empty() {
                                "暂无记录 — 每次命令执行后自动生成"
                            } else {
                                "当前过滤无匹配行"
                            })
                            .color(txt_l4())
                            .italics(),
                        );
                    }
                });
        });
        // 显示「过滤后行数 / 总行数」（替换原总行数——清空过滤后总行数不变会造成「没清掉」错觉）
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("显示 {shown} / 共 {} 行", self.log.lines().count()))
                    .small()
                    .color(txt_l4()),
            );
        });
        ui.add_space(4.0);
    }
}

/// 每帧强制锁定深色主题
fn lock_dark_theme(ctx: &egui::Context) {
    let current = ctx.style().visuals.dark_mode;
    if !current {
        install_theme(ctx);
    }
}

/// 全屏低饱和垂直渐变背景（背景层）：玻璃面板的半透明 fill 透出此渐变，视觉等效毛玻璃。
/// 直接用 `ctx.layer_painter(LayerId::background())` 在最底层画 mesh，避开 Area 的 auto-size 坑。
fn draw_bg_gradient(ctx: &egui::Context) {
    let painter = ctx.layer_painter(egui::LayerId::background());
    let rect = ctx.screen_rect();
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    let top = egui::Color32::from_rgb(32, 40, 56); // #202838 低饱和深蓝
    let bottom = egui::Color32::from_rgb(10, 14, 20); // #0A0E14
    let mesh = egui::Mesh {
        texture_id: egui::TextureId::default(),
        vertices: vec![
            egui::epaint::Vertex { pos: rect.left_top(), uv: egui::epaint::WHITE_UV, color: top },
            egui::epaint::Vertex { pos: rect.right_top(), uv: egui::epaint::WHITE_UV, color: top },
            egui::epaint::Vertex { pos: rect.left_bottom(), uv: egui::epaint::WHITE_UV, color: bottom },
            egui::epaint::Vertex { pos: rect.right_bottom(), uv: egui::epaint::WHITE_UV, color: bottom },
        ],
        indices: vec![0, 1, 2, 1, 3, 2],
    };
    painter.add(mesh);
}

/// 模拟 SDF 辉光：hover 时绘制 2~3 层半透明圆角描边逐级外扩（视觉近似 signed-distance 光晕）。
fn sdf_glow(ui: &mut egui::Ui, rect: egui::Rect, color: egui::Color32, radius: f32) {
    let p = ui.painter();
    for (i, a) in [0.30_f32, 0.15, 0.07].iter().enumerate() {
        let c = egui::Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            (255.0 * a) as u8,
        );
        let expand = 1.0 + i as f32 * 3.0;
        p.rect_stroke(
            rect.expand(expand),
            egui::Rounding::same(radius + expand),
            egui::Stroke::new(1.0_f32, c),
        );
    }
}

impl eframe::App for WbRelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        lock_dark_theme(ctx);
        // 玻璃背景渐变（最底层，半透明面板透出）
        draw_bg_gradient(ctx);
        self.poll_tray_events(ctx);
        self.sync_tray(false);
        self.poll_tasks();
        self.poll_events();
        self.poll_logs();

        // 关闭窗口 = 最小化到托盘；托盘菜单「退出」才真正结束
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // ── 顶部：标题 + 状态徽章 + 启用开关 + 操作提示 ──
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // L1 主标题 16px 粗体；L4 副标题 12px 次要
                ui.label(
                    egui::RichText::new("AI-root 控制台")
                        .size(FS_L1)
                        .strong()
                        .color(txt_l1()),
                );
                ui.label(
                    egui::RichText::new("SYSTEM 提权通道 · 已授权 AI 远程执行命令")
                        .color(txt_l4())
                        .size(FS_L4),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("刷新").clicked() {
                        self.refresh();
                    }
                    let (c, t) = self.status_badge();
                    badge(ui, c, &t);
                });
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let mut en = self.enabled;
                let resp = toggle_switch(ui, &mut en);
                if resp.clicked() {
                    common::set_enabled(en);
                    self.enabled = en;
                    self.sync_tray(true);
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if en {
                        "中继已启用 · AI 可经管道执行命令"
                    } else {
                        "中继已禁用 · AI 无法执行命令"
                    })
                    .strong()
                    .color(if en { c_ok() } else { c_err() }),
                );
                if !self.hint.is_empty() {
                    ui.colored_label(hint_color(&self.hint), &self.hint);
                }
            });
            ui.add_space(6.0);
        });

        // ── 底部状态栏：最近退出码 / 耗时 / 计数 ──
        egui::TopBottomPanel::bottom("statusbar").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if let Some(r) = self.outputs.get(self.output_idx) {
                    let (c, t) = if r.exit_code == 0 {
                        (c_ok(), format!("exit={}", r.exit_code))
                    } else {
                        (c_err(), format!("exit={}", r.exit_code))
                    };
                    pill(ui, c, &t);
                    pill(ui, C_TEXT_DIM, &format!("{}ms", r.dur_ms));
                    if r.relay_err.is_some() {
                        pill(ui, c_err(), "中继错误");
                    }
                } else {
                    pill(ui, C_TEXT_DIM, "就绪 — 等待命令");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "事件 {} · 日志 {} 条",
                            self.events.len(),
                            self.log.lines().count()
                        ))
                        .color(C_TEXT_DIM)
                        .small(),
                    );
                    if self.cmd_busy {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(2.0);
        });

        // ── 左侧：服务控制 / 令牌 / 管理员操作（折叠）──
        egui::SidePanel::left("left")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                section(ui, "服务控制", |ui| {
                    // 并排按钮：各占半宽（减去中间 8px 间距），等高 32px
                    let half = (ui.available_width() - 8.0) / 2.0;
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized([half, 32.0], egui::Button::new("启动"))
                            .clicked()
                        {
                            self.svc_start();
                        }
                        if ui
                            .add_sized([half, 32.0], egui::Button::new("停止"))
                            .clicked()
                        {
                            self.svc_stop();
                        }
                    });
                    ui.small("首次安装后即已授权，启停无需管理员");
                });

                section(ui, "令牌", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("指纹:");
                        ui.monospace(&self.token_fp);
                    });
                    ui.label(format!("写入时间: {}", self.token_mtime));
                    ui.add_space(8.0);
                    let w = ui.available_width();
                    if ui.add_sized([w, 32.0], egui::Button::new("轮换令牌")).clicked() {
                        self.confirm_rotate = true;
                    }
                    if self.confirm_rotate {
                        ui.add_space(8.0);
                        ui.colored_label(
                            c_err(),
                            "确认轮换？旧令牌立即失效，持有旧令牌的 AI 客户端将无法执行",
                        );
                        let half = (ui.available_width() - 8.0) / 2.0;
                        ui.horizontal(|ui| {
                            if ui.add_sized([half, 32.0], egui::Button::new("确认轮换")).clicked() {
                                self.rotate_trigger();
                            }
                            if ui.add_sized([half, 32.0], egui::Button::new("取消")).clicked() {
                                self.confirm_rotate = false;
                            }
                        });
                    }
                    ui.small("令牌是最高权限凭据，仅显示前 8 位指纹");
                });

                // 管理员操作：折叠卡片（默认收起，红色仅用于警示与确认文案）
                egui::CollapsingHeader::new(
                    egui::RichText::new("管理员操作（仅首次 / 应急）")
                        .strong()
                        .size(FS_L2)
                        .color(c_warn()),
                )
                .default_open(false)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    let w = ui.available_width();
                    if ui.add_sized([w, 32.0], egui::Button::new("安装服务 (首次)")).clicked() {
                        if self.elevate("install") {
                            self.hint =
                                "已请求系统确认，请在弹窗中批准安装。\n首次安装会自动完成授权，此后启停/更新无需再确认"
                                    .to_string();
                        } else {
                            self.hint = "✗ 未获系统确认（可能被取消）".to_string();
                        }
                    }
                    ui.add_space(8.0);
                    if self.confirm_uninstall {
                        ui.colored_label(c_err(), "确认卸载服务？");
                        let half = (ui.available_width() - 8.0) / 2.0;
                        ui.horizontal(|ui| {
                            if ui.add_sized([half, 32.0], egui::Button::new("确认卸载")).clicked() {
                                if self.elevate("uninstall") {
                                    self.hint = "已请求系统确认，请在弹窗中批准卸载".to_string();
                                } else {
                                    self.hint = "✗ 未获系统确认（可能被取消）".to_string();
                                }
                                self.confirm_uninstall = false;
                            }
                            if ui.add_sized([half, 32.0], egui::Button::new("取消")).clicked() {
                                self.confirm_uninstall = false;
                            }
                        });
                    } else if ui.add_sized([w, 32.0], egui::Button::new("卸载服务")).clicked() {
                        self.confirm_uninstall = true;
                    }
                    ui.small("这些操作需管理员确认；日常使用无需进入");
                });
                ui.add_space(4.0);
            });

        // ── 主区：Tab 化 ──
        egui::CentralPanel::default().show(ctx, |ui| {
            self.render_main(ui);
        });
    }
}

/// 查询服务状态字符串
fn query_status_string() -> String {
    match crate::service::query_status() {
        Ok(state) => format!("{:?}", state),
        Err(_) => "未安装 / 不可访问".to_string(),
    }
}

/// 显示主窗口（托盘左键 / 「打开控制台」菜单触发）
fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// 由当前服务状态与启用开关推导托盘角标颜色与状态文案
fn tray_status(status: &str, enabled: bool) -> ([u8; 3], String) {
    if status.starts_with("Running") {
        if enabled {
            (ICON_GREEN, "运行中 · 中继启用".to_string())
        } else {
            (ICON_RED, "运行中 · 中继已禁用".to_string())
        }
    } else if status.starts_with("Stopped") {
        (ICON_GRAY, "服务状态: 已停止".to_string())
    } else {
        (ICON_GRAY, "未安装 / 不可访问".to_string())
    }
}

/// 程序化生成托盘图标：32×32 RGBA 实心圆（颜色编码状态），免外部图片资源
fn make_icon(color: [u8; 3]) -> Icon {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = 11.0_f32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius * radius {
                let i = (y * SIZE + x) * 4;
                rgba[i] = color[0];
                rgba[i + 1] = color[1];
                rgba[i + 2] = color[2];
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("托盘图标生成失败")
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
        timeout_ms: Some(60000),
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

// ── UI 辅助 ──

/// 胶囊形徽章（圆角矩形 + 语义色文字）
fn pill(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    let font_id = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_string(), font_id, color);
    let pad = egui::vec2(6.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let bg = color.linear_multiply(0.18);
    ui.painter().rect_filled(rect, egui::Rounding::same(9.0), bg);
    ui.painter().galley(rect.min + pad, galley, color);
}

/// 彩色圆点 + 文本徽章（服务状态可视化）
fn badge(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.5, color);
        ui.label(egui::RichText::new(text).color(color).strong());
    });
}

/// 普通分组卡片（左侧面板分区）
fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).strong().size(FS_L2).color(txt_l2()));
            ui.add_space(4.0);
            add_contents(ui);
        });
    ui.add_space(6.0);
}

/// 统一「内容显示面板」容器（命令输出 / 实时信息流 / 审计日志共用）：
/// 圆角 8px + 1px 细描边（white 6%）+ 深底 #1F242B + 内边距 12×10。
/// 保证三个只读面板外观完全一致。
fn panel_frame(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(31, 36, 43, 255)) // #1F242B
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 15),
        ))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin {
            left: 12.0,
            right: 12.0,
            top: 10.0,
            bottom: 10.0,
        })
        .show(ui, add_contents);
}

/// 解析审计日志单行 →（时间戳, 身份, 退出码, 命令），用于固定列宽渲染对齐。
/// 兼容新格式 `[ts] caller=DOMAIN\user pid=N exit=N cmd=...` 与旧格式 `[ts] user=XXX exit=N cmd=...`。
fn parse_log_row(line: &str) -> (String, String, Option<i32>, String) {
    let ts = line
        .chars()
        .skip(1)
        .take(19)
        .collect::<String>();
    // 身份段：caller=... pid=N（或旧式 user=...）
    let id_part = line
        .split(" pid=")
        .next()
        .unwrap_or("")
        .split(" exit=")
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
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

/// 操作提示着色：✓绿 / ⚠琥珀 / 其余错误红
fn hint_color(hint: &str) -> egui::Color32 {
    if hint.starts_with('✓') {
        c_ok()
    } else if hint.starts_with('⚠') {
        c_warn()
    } else {
        c_err()
    }
}

/// 解析事件 JSONL 行为缓存结构（单次解析；失败降级为中性条目）
fn parse_event(line: &str) -> ParsedEvent {
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

/// 紧凑型 toggle switch：自定义绘制（矩形背景 + 圆形拨钮）
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(40.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let track_color = if *on {
        c_ok()
    } else {
        C_BORDER
    };
    ui.painter()
        .rect_filled(rect.shrink2(egui::vec2(0.0, 2.0)), egui::Rounding::same(9.0), track_color);
    let knob_radius = rect.height() * 0.35;
    let knob_x = if *on {
        rect.right() - rect.height() * 0.5
    } else {
        rect.left() + rect.height() * 0.5
    };
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        knob_radius,
        C_TEXT,
    );
    resp
}

/// 检测文本是否包含疑似 64 位十六进制令牌/密钥（连续 ≥64 个 hex 字符）
fn contains_token_like(s: &str) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            if run >= 64 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}
