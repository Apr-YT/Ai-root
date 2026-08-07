//! 视图层：三 Tab 主区（命令台 / 实时信息流 / 审计日志）+ 顶/底/侧/中央面板装配，
//! 以及 `eframe::App::update` 主循环（每帧轮询后台任务、锁定主题、渲染布局）。
//!
//! 界面结构（2026-08-06 重构 v2 优化 + v4 Linear flat 纯色）：
//! - 顶部：标题 + 状态徽章 + 启用开关 + 操作提示（固定）
//! - 左栏：服务控制 / 令牌 / 管理员操作（折叠，默认收起）
//! - 主区：三 Tab 共享 —— [命令台] [实时信息流] [审计日志]（Ctrl+1/2/3 切换）
//! - 底部：状态栏（最近退出码 / 耗时 / 事件数 / 日志行数）

use eframe::egui;

use crate::common;

use super::parse::{contains_token_like, parse_log_row, EventLevel};
use super::theme::{
    c_border, c_err, c_ok, c_warn, lock_dark_theme, txt_l1, txt_l2, txt_l3, txt_l4, C_BG_APP,
    C_BG_CARD, C_BG_HOVER, C_BG_PANEL, C_BRAND, C_TEXT, C_TEXT_DIM, FS_L1, FS_L2, FS_L4,
};
use super::tray::tray_status;
use super::ui_widgets::{badge, hint_color, panel_frame, pill, section, toggle_switch};
use super::WbRelayApp;

// ── 主区 Tab 定义（任务导向：命令 / 事件 / 日志 三视图）──
#[derive(PartialEq, Clone, Copy)]
pub(super) enum MainTab {
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

/// 快捷命令分组（真分组：组标签 + 组内命令，组间分隔）
const CMD_GROUPS: &[(&str, &[&str])] = &[
    ("身份", &["whoami", "whoami /priv", "whoami /groups"]),
    ("服务", &["sc query ai-root", "sc queryex ai-root"]),
    ("系统", &["tasklist /svc", "systeminfo"]),
    ("硬件", &["pnputil /enum-devices", "driverquery /v"]),
];

impl WbRelayApp {
    /// 状态徽章（颜色 + 文本），与托盘角标语义一致
    pub(super) fn status_badge(&self) -> (egui::Color32, String) {
        let (rgb, text) = tray_status(&self.status, self.enabled);
        (egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]), text)
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
                .fill(if selected { C_BG_CARD } else { C_BG_PANEL })
                .stroke(egui::Stroke::new(
                    1.0_f32,
                    if selected { C_BRAND } else { c_border() },
                ))
                // 圆角统一 8px；min_size 高 32px = 8px 网格整数倍，保证三 Tab 绝对等高、文字基线一致
                .rounding(egui::Rounding::same(8.0))
                .min_size(egui::vec2(0.0, 32.0));
                let resp = ui.add(btn);
                if selected {
                    // 底部品牌色指示条（2px，宽度收缩 8px 内边距，避免顶到圆角）
                    let r = resp.rect;
                    let bar = egui::Rect::from_min_max(
                        egui::pos2(r.left() + 4.0, r.bottom() - 2.0),
                        egui::pos2(r.right() - 4.0, r.bottom()),
                    );
                    ui.painter().rect_filled(bar, egui::Rounding::same(1.0), C_BRAND);
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
                        if active { C_BRAND } else { c_border() },
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
                    .fill(C_BRAND)
                    .min_size(egui::vec2(64.0, 32.0)),
            );
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
                        let tok = common::read_token().unwrap_or_default();
                        if !tok.is_empty() && contains_token_like(&text, &tok) {
                            self.hint = "⚠ 已拦截：输出包含本机当前 SYSTEM 令牌，未复制。确认无敏感信息后再手动选择复制".to_string();
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
                    // 绕过 poll_events 的 1s 节流：把上次轮询时间置为 2s 前，
                    // 使本次清空后立即重新载入（否则要等下一帧 ≤1s，表现为「点了没反应」）(G5 修复)
                    self.events_last_poll = std::time::Instant::now() - std::time::Duration::from_secs(2);
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
                    .button("清空显示")
                    .on_hover_text("清空当前显示的日志内容（后续新执行记录仍会追加）")
                    .clicked()
                {
                    self.log.clear();
                    // 同步把增量读偏移置到文件末尾：否则 poll_logs 会从旧偏移
                    // 把已读内容重新拉回，表现为「清空后立即恢复」（与事件流 Tab 一致）
                    self.log_offset = std::fs::metadata(common::log_path())
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let ctx = ui.ctx().clone();
                    ctx.request_repaint();
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
                        d.remove::<egui::widgets::text_edit::TextEditState>(filter_id);
                        d.remove::<egui::widgets::text_edit::TextEditState>(pid_input_id);
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
                            Some(0) => c_ok(),
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

impl eframe::App for WbRelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        lock_dark_theme(ctx);
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
            .min_width(240.0)
            .max_width(480.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                section(ui, "服务控制", |ui| {
                    // 状态感知：与托盘菜单一致，按服务状态禁用不适用按钮
                    let running = self.status.starts_with("Running");
                    // 并排按钮：各占半宽（减去中间 8px 间距），等高 32px
                    let half = (ui.available_width() - 8.0) / 2.0;
                    ui.horizontal(|ui| {
                        let start = ui.add_enabled(
                            !running,
                            egui::Button::new("启动").min_size(egui::vec2(half, 32.0)),
                        );
                        if start.on_hover_text(if running {
                            "服务已在运行"
                        } else {
                            "启动服务（已授权，无需管理员）"
                        }).clicked()
                        {
                            self.svc_start();
                        }
                        let stop = ui.add_enabled(
                            running,
                            egui::Button::new("停止").min_size(egui::vec2(half, 32.0)),
                        );
                        if stop.on_hover_text(if running {
                            "停止服务（已授权，无需管理员）"
                        } else {
                            "服务未在运行"
                        }).clicked()
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
                    // 安装仅限未安装状态；卸载仅限已安装状态（与启停按钮同源的状态感知）
                    let installed = self.status.starts_with("Running")
                        || self.status.starts_with("Stopped");
                    let install_btn = ui.add_enabled(
                        !installed,
                        egui::Button::new("安装服务 (首次)").min_size(egui::vec2(w, 32.0)),
                    );
                    if install_btn
                        .on_hover_text(if installed {
                            "服务已安装；如需重装请先卸载"
                        } else {
                            "以管理员权限安装并完成首次授权（仅此一次）"
                        })
                        .clicked()
                    {
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
                    } else {
                        let uninstall_btn = ui.add_enabled(
                            installed,
                            egui::Button::new("卸载服务").min_size(egui::vec2(w, 32.0)),
                        );
                        if uninstall_btn
                            .on_hover_text(if installed {
                                "以管理员权限卸载服务"
                            } else {
                                "服务未安装"
                            })
                            .clicked()
                        {
                            self.confirm_uninstall = true;
                        }
                    }
                    ui.small("这些操作需管理员确认；日常使用无需进入");
                });
                ui.add_space(4.0);
            });

        // ── 主区：Tab 化（中央区域用最底层的 app 色 #0D1117,与面板 #161B22 区分）──
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(C_BG_APP))
            .show(ctx, |ui| {
                self.render_main(ui);
            });
    }
}
