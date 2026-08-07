//! 视觉 token 与主题装配：配色四阶、语义色、5 级字号层级、字体加载与主题锁定。
//!
//! 配色「Linear flat」（v4，2026-08-06）：
//! GitHub-Dark 同源纯色四阶（#0D1117 / #161B22 / #21262D / #30363D）+ 1px 低 alpha 细描边
//! （白 8% / 16%）+ 4 级字号 + 语义色（绿=成功/黄=警告/红=错误/蓝=操作/蓝亮=品牌）。
//! 无半透明 / 渐变 / 玻璃 / 辉光；Tab 选中态 = 底部 2px 品牌色指示条。
//!
//! 说明：`Color32::from_rgba_unmultiplied` 非 const，含 alpha 的色值以运行时求值函数暴露。

use eframe::egui;

// ── 视觉 token「Linear flat」方案（v4，2026-08-06）──
// GitHub-Dark 同源纯色四阶 + 1px 低 alpha 细描边（flat：无半透明 / 渐变 / 玻璃 / 辉光）。
// 背景四阶：app #0D1117 / panel #161B22 / card #21262D / hover #30363D
pub(super) const C_BG_APP: egui::Color32 = egui::Color32::from_rgb(13, 17, 23); // 应用底层 #0D1117
pub(super) const C_BG_PANEL: egui::Color32 = egui::Color32::from_rgb(22, 27, 34); // 面板 #161B22
pub(super) const C_BG_CARD: egui::Color32 = egui::Color32::from_rgb(33, 38, 45); // 卡片 #21262D
pub(super) const C_BG_HOVER: egui::Color32 = egui::Color32::from_rgb(48, 54, 61); // 悬浮 #30363D

// 边框两档（1px 低 alpha 模拟细描边：弱 ~8% / 强 ~16%）
// 注：from_rgba_unmultiplied 非 const → 用运行时求值函数
pub(super) fn c_border() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20)
}
pub(super) fn c_border_strong() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 41)
}

// 文字三阶
pub(super) const C_TEXT: egui::Color32 = egui::Color32::from_rgb(230, 237, 243); // 主文本 #E6EDF3
pub(super) const C_TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(125, 133, 144); // 次要 #7D8590

// 状态语义色（规范：不改变字号，仅着色）：
//   成功绿 #3FB950 / 警告黄 #D29922 / 错误红 #F85149 / info #58A6FF / brand #3D8BFF
// 注：Color32::from_rgba_unmultiplied 非 const → 用运行时求值函数
pub(super) fn c_ok() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(63, 185, 80, 242)
}
pub(super) fn c_warn() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(210, 153, 34, 242)
}
pub(super) fn c_err() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(248, 81, 73, 242)
}
pub(super) const C_INFO: egui::Color32 = egui::Color32::from_rgb(88, 166, 255); // info #58A6FF
pub(super) const C_NEUTRAL: egui::Color32 = egui::Color32::from_rgb(139, 148, 158); // 中性灰 #8B949E
pub(super) const C_BRAND: egui::Color32 = egui::Color32::from_rgb(61, 139, 255); // brand #3D8BFF

// ── 5 级文字层级（暗色终端控制台，RGBA 0-1 浮点换算 0-255）──
// L1 主标题 16px 粗体 rgba(0.95,0.95,0.97,1.0) —— 窗口大标题
// L2 模块标题/Tab 14px medium rgba(0.85,0.87,0.90,0.95) —— 面板标题、Tab 标签
// L3 主体正文 13px 常规 rgba(0.80,0.82,0.85,0.90) —— 按钮/输入框/表格主列/输出日志
// L4 次要辅助 12px 常规 rgba(0.60,0.63,0.68,0.75) —— 说明文本/描述列/状态栏次要
// L5 弱占位 11px rgba(0.45,0.48,0.52,0.55) —— 输入框占位符
pub(super) const FS_L1: f32 = 16.0;
pub(super) const FS_L2: f32 = 14.0;
pub(super) const FS_L3: f32 = 13.0;
pub(super) const FS_L4: f32 = 12.0;

// 文字层级颜色（运行时求值，见上：Color32::from_rgba_unmultiplied 非 const）
pub(super) fn txt_l1() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(242, 242, 247, 255)
}
pub(super) fn txt_l2() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(217, 222, 230, 242)
}
pub(super) fn txt_l3() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(204, 209, 217, 230)
}
pub(super) fn txt_l4() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(153, 161, 173, 191)
}

/// 深色「Linear flat」主题（纯色平铺，无半透明 / 渐变 / 玻璃 / 辉光）：
/// GitHub-Dark 同源四阶背景 + 1px 低 alpha 细描边；语义色仅用于状态。
pub(super) fn install_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    // 背景：纯色平铺。panel_fill=面板 #161B22；window_fill=应用底层 #0D1117（中央主区）
    v.panel_fill = C_BG_PANEL;
    v.window_fill = C_BG_APP;
    v.faint_bg_color = C_BG_HOVER; // 极淡高光 = hover 色（flat 不用玻璃高光）
    v.extreme_bg_color = C_BG_APP; // 输入框最底层
    v.code_bg_color = C_BG_CARD; // 代码/输出底色 = 卡片
    v.override_text_color = Some(txt_l3()); // L3 正文为全局默认

    // 5 级字号层级：Heading16/Body13/Button13/Small12/Monospace13（px）
    ctx.style_mut(|s| {
        use egui::{FontId, TextStyle};
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(FS_L1));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(FS_L3));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(FS_L3));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(FS_L4));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(FS_L3));
    });

    // 选区 / 链接：品牌蓝
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(61, 139, 255, 150);
    v.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(61, 139, 255, 160));
    v.hyperlink_color = C_INFO;

    // 控件四态 flat：统一 8px 圆角 + 1px 低 alpha 细描边；hover/active 仅变底色
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
    let hairline = c_border(); // 弱 8%
    let hairline_strong = c_border_strong(); // 强 16%
    v.widgets.noninteractive.bg_fill = C_BG_CARD;
    v.widgets.noninteractive.weak_bg_fill = C_BG_PANEL;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    v.widgets.inactive.bg_fill = C_BG_CARD;
    v.widgets.inactive.weak_bg_fill = C_BG_PANEL;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, hairline);
    v.widgets.hovered.bg_fill = C_BG_HOVER;
    v.widgets.hovered.weak_bg_fill = C_BG_HOVER;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, hairline_strong);
    v.widgets.active.bg_fill = C_BG_HOVER;
    v.widgets.active.weak_bg_fill = C_BG_HOVER;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, C_BRAND);
    v.widgets.open.bg_fill = C_BG_HOVER;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, hairline_strong);

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
pub(super) fn install_cjk_fonts(ctx: &egui::Context) {
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

/// 每帧强制锁定深色主题（egui 可能因系统主题变更而回退，此处兜底重装）
pub(super) fn lock_dark_theme(ctx: &egui::Context) {
    let current = ctx.style().visuals.dark_mode;
    if !current {
        install_theme(ctx);
    }
}
