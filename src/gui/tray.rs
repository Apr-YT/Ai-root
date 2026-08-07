//! 系统托盘常驻：状态角标（绿=运行且启用 / 红=运行但禁用 / 灰=停止或未安装）+ 右键菜单。
//!
//! 关闭窗口 = 最小化到托盘（不退出），托盘菜单「退出」才结束 GUI 进程（不影响服务）。
//! 托盘图标程序化生成（32×32 RGBA 实心圆，颜色编码状态），免外部图片资源。
//! init_tray / sync_tray / poll_tray_events 挂在 WbRelayApp 上（跨模块 inherent 方法）。

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::common;

use super::WbRelayApp;

/// 「退出控制台」菜单项 ID（双保险：handler 线程直接 exit，不依赖主循环唤醒）
static QUIT_ID: OnceLock<tray_icon::menu::MenuId> = OnceLock::new();

/// 托盘事件转发通道：tray-icon 事件线程 → GUI 主循环。
///
/// 背景（2026-08-07 修复「托盘右键不能实时退出控制」）：
/// 直接轮询 `MenuEvent::receiver()`/`TrayIconEvent::receiver()`（全局 channel）时，
/// 窗口隐藏到托盘后 eframe 主循环进入 `Wait` 停转，`update()` 不再执行，
/// 轮询点 `poll_tray_events` 不运行 → 托盘菜单事件（含「退出」）积压不响应。
/// tray-icon 官方文档要求 winit/tao 用户用 `set_event_handler` 把事件转发到事件循环；
/// 此处 handler 收到事件后 ① 转发到本通道 ② `ctx.request_repaint()` 唤醒主循环，
/// 下一帧 `update()` 内 `poll_tray_events` 从本通道消费并处理。
enum TrayCmd {
    Menu(MenuEvent),
    Icon(TrayIconEvent),
}

static TRAY_TX: OnceLock<Sender<TrayCmd>> = OnceLock::new();
/// Receiver 非 `Sync`，须经 Mutex 包装才能进 static（仅主循环消费，锁竞争可忽略）
static TRAY_RX: OnceLock<Mutex<Receiver<TrayCmd>>> = OnceLock::new();

/// 安装托盘事件处理器（全局一次性；须在创建托盘菜单前调用）。
/// 事件到达即转发 + 唤醒主循环，使隐藏窗口状态下菜单点击可实时响应。
pub(super) fn install_tray_handlers(ctx: &egui::Context) {
    let (tx, rx) = mpsc::channel::<TrayCmd>();
    let _ = TRAY_TX.set(tx.clone());
    let _ = TRAY_RX.set(Mutex::new(rx));

    let ctx_m = ctx.clone();
    let tx_m = tx.clone();
    MenuEvent::set_event_handler(Some(move |ev: tray_icon::menu::MenuEvent| {
        // 双保险：退出在 handler 线程直接执行，不依赖主循环唤醒
        if let Some(qid) = QUIT_ID.get() {
            if ev.id == *qid {
                // 仅结束 GUI 进程；服务（LocalSystem）不受影响
                std::process::exit(0);
            }
        }
        let _ = tx_m.send(TrayCmd::Menu(ev));
        ctx_m.request_repaint();
    }));
    let ctx_t = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |ev| {
        let _ = tx.send(TrayCmd::Icon(ev));
        ctx_t.request_repaint();
    }));
}

/// 托盘图标状态与菜单项句柄
pub(super) struct TrayState {
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
    /// 创建系统托盘图标与右键菜单；失败则静默降级为纯窗口模式
    pub(super) fn init_tray(&mut self) {
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
            // 记录「退出控制台」菜单项 ID，供 handler 双保险直接退出
            let _ = QUIT_ID.set(quit_item.id().clone());
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
    pub(super) fn sync_tray(&mut self, force: bool) {
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

    /// 处理托盘右键菜单与左键点击事件（每帧轮询一次；事件经转发通道消费）
    pub(super) fn poll_tray_events(&mut self, ctx: &egui::Context) {
        let Some(rx_guard) = TRAY_RX.get() else {
            return;
        };
        let rx = rx_guard.lock().unwrap();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                TrayCmd::Menu(ev) => {
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
                TrayCmd::Icon(ev) => {
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
        }
    }
}

/// 显示主窗口（托盘左键 / 「打开控制台」菜单触发）
fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// 由当前服务状态与启用开关推导托盘角标颜色与状态文案
pub(super) fn tray_status(status: &str, enabled: bool) -> ([u8; 3], String) {
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
