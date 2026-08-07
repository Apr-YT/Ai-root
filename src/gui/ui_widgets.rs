//! 无状态 UI 辅助控件：胶囊徽章、状态圆点、分组卡片、内容面板容器、
//! 自绘 toggle 开关、操作提示着色。均为纯渲染函数，不持有应用状态。

use eframe::egui;

use super::theme::{c_border, c_err, c_ok, c_warn, txt_l2, C_BG_CARD, C_TEXT, FS_L2};

/// 胶囊形徽章（圆角矩形 + 语义色文字）
pub(super) fn pill(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
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
pub(super) fn badge(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.5, color);
        ui.label(egui::RichText::new(text).color(color).strong());
    });
}

/// 普通分组卡片（左侧面板分区）
pub(super) fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
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
/// 圆角 8px + 1px 低 alpha 细描边（white 8%）+ 卡片底色 #21262D + 内边距 12×10。
/// 保证三个只读面板外观完全一致。
pub(super) fn panel_frame(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(C_BG_CARD)
        .stroke(egui::Stroke::new(1.0_f32, c_border()))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin {
            left: 12.0,
            right: 12.0,
            top: 10.0,
            bottom: 10.0,
        })
        .show(ui, add_contents);
}

/// 操作提示着色：✓绿 / ⚠琥珀 / 其余错误红
pub(super) fn hint_color(hint: &str) -> egui::Color32 {
    if hint.starts_with('✓') {
        c_ok()
    } else if hint.starts_with('⚠') {
        c_warn()
    } else {
        c_err()
    }
}

/// 紧凑型 toggle switch：自定义绘制（矩形背景 + 圆形拨钮）
pub(super) fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(40.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let track_color = if *on {
        c_ok()
    } else {
        c_border()
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
