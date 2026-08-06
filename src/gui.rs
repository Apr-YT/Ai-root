//! 原生 GUI 控制面板（egui 即时模式）
//!
//! 功能：服务状态查看、启停/安装/卸载（仅必须管理员的走 UAC）、启用开关、
//! 令牌指纹与轮换、执行日志查看，以及通过管道直接下发命令（非提权即可）。
//!
//! 设计红线：不制造新的授权摩擦点 ——
//!   - start / stop：install/grant 后当前用户 SID 已持 GRGX（START/STOP/QUERY_STATUS），
//!     直接非提权调用服务控制接口，绝不弹 UAC；
//!   - install / uninstall / rotate-token：必须管理员权限，走 UAC 触发 + 后台轮询确认；
//!   - 管道命令执行：异步线程 + channel，避免同步 IO 阻塞 egui 事件循环。

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::common;
use crate::protocol;

pub fn run() {
    let options = eframe::NativeOptions {
        ..Default::default()
    };
    let _ = eframe::run_native(
        "AI-root 控制台",
        options,
        Box::new(|cc| {
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(WbRelayApp::new()))
        }),
    );
}

/// 加载系统中文字体。egui 内置字体不含 CJK 字形，不加载则中文渲染为方块。
/// 依次尝试微软雅黑（TTC 集合取 index 0）/ 黑体 / 宋体，找到即用并停止。
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

struct WbRelayApp {
    status: String,
    enabled: bool,
    log: String,
    run_cmd: String,
    output: String,
    /// 令牌指纹（前 8 位）与文件修改时间
    token_fp: String,
    token_mtime: String,
    /// 管道命令执行（后台线程）结果通道
    cmd_rx: Option<Receiver<String>>,
    cmd_busy: bool,
    /// 令牌轮换（UAC 后台进程）确认通道
    rotate_rx: Option<Receiver<bool>>,
    /// 危险操作二次确认状态
    confirm_rotate: bool,
    confirm_uninstall: bool,
    /// 操作结果提示（含 UAC 触发反馈）
    hint: String,
}

impl WbRelayApp {
    fn new() -> Self {
        let enabled = common::is_enabled();
        let status = query_status_string();
        let log = common::read_log_tail(200);
        let (fp, mt) = token_info();
        Self {
            status,
            enabled,
            log,
            run_cmd: String::new(),
            output: String::new(),
            token_fp: fp,
            token_mtime: mt,
            cmd_rx: None,
            cmd_busy: false,
            rotate_rx: None,
            confirm_rotate: false,
            confirm_uninstall: false,
            hint: String::new(),
        }
    }

    fn refresh(&mut self) {
        self.status = query_status_string();
        self.enabled = common::is_enabled();
        self.log = common::read_log_tail(200);
        let (fp, mt) = token_info();
        self.token_fp = fp;
        self.token_mtime = mt;
    }

    /// 以提权方式执行 ai-root 子命令（仅限必须管理员的操作：install/uninstall/rotate-token）。
    /// 通过 PowerShell Start-Process -Verb RunAs 触发 UAC；返回 PowerShell 是否成功启动
    /// （子命令的实际结果由后台轮询 / 状态刷新确认，UAC 分离进程的输出不可见）。
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

    /// 非提权服务控制：install/grant 后用户 SID 已持 GRGX（含 START/STOP/QUERY_STATUS），
    /// 直接调服务接口即可，不再需要 UAC 弹窗。
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
                "✗ {what}失败: {es}\n提示：请以管理员终端执行一次 `ai-root grant`，此后 start/stop/update 均无需再提权"
            )
        } else {
            format!("✗ {what}失败: {es}")
        };
    }

    /// 触发 UAC 轮换令牌。rotate 输出包含明文令牌，不向 GUI 展示；
    /// 后台轮询 token.txt mtime 变化，确认轮换已生效（服务端自动热加载）。
    fn rotate_trigger(&mut self) {
        let before = file_mtime();
        if !self.elevate("rotate-token") {
            self.hint = "✗ UAC 触发失败（可能被取消）".to_string();
            self.confirm_rotate = false;
            return;
        }
        self.hint = "已触发 UAC，等待令牌轮换完成…\n新令牌已写入文件，AI 侧自动热加载；请勿从 GUI 获取明文令牌".to_string();
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

    /// 异步执行管道命令：后台线程完成同步管道 IO（最长 60s），结果经 channel 回传，
    /// egui 事件循环不被阻塞。
    fn execute_async(&mut self, command: &str) {
        let token = match common::read_token() {
            Some(t) => t,
            None => {
                self.output = "[错误] 未找到令牌，请先以管理员运行 install".to_string();
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
        self.output = "[执行中…]".to_string();
    }

    /// 每帧轮询后台任务结果（命令执行 / 令牌轮换确认）
    fn poll_tasks(&mut self) {
        if let Some(rx) = self.cmd_rx.take() {
            match rx.try_recv() {
                Ok(out) => {
                    self.output = out;
                    self.log = common::read_log_tail(200);
                    self.cmd_busy = false;
                }
                Err(mpsc::TryRecvError::Empty) => self.cmd_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.output = "[错误] 后台任务异常退出".to_string();
                    self.cmd_busy = false;
                }
            }
        }
        if let Some(rx) = self.rotate_rx.take() {
            match rx.try_recv() {
                Ok(true) => {
                    self.hint = "✓ 令牌已轮换（文件 mtime 已更新，服务端已热加载新令牌）".to_string();
                    self.refresh();
                }
                Ok(false) => {
                    self.hint = "⚠ 轮换确认超时（30s）。若 UAC 已确认，请改在管理员终端执行 ai-root rotate-token".to_string();
                }
                Err(mpsc::TryRecvError::Empty) => self.rotate_rx = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
    }
}

impl eframe::App for WbRelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_tasks();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("AI-root 控制台");
            ui.label("以 LocalSystem 运行的提权命令中继（命名管道 IPC）");

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("服务状态:");
                ui.colored_label(egui::Color32::from_rgb(80, 160, 80), &self.status);
                if ui.button("刷新").clicked() {
                    self.refresh();
                }
            });

            ui.horizontal_wrapped(|ui| {
                if ui.button("安装服务 (管理员, 首次)").clicked() {
                    if self.elevate("install") {
                        self.hint = "已触发 UAC，请在弹窗中确认安装。\n首次安装会自动 grant，此后 start/stop/update 零提权".to_string();
                    } else {
                        self.hint = "✗ UAC 触发失败（可能被取消）".to_string();
                    }
                }
                if ui.button("启动").clicked() {
                    self.svc_start();
                }
                if ui.button("停止").clicked() {
                    self.svc_stop();
                }
                if ui.button("卸载 (管理员)").clicked() {
                    self.confirm_uninstall = true;
                }
            });

            // 卸载二次确认
            if self.confirm_uninstall {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(163, 45, 45), "确认卸载服务？");
                    if ui.button("确认卸载").clicked() {
                        if self.elevate("uninstall") {
                            self.hint = "已触发 UAC，请在弹窗中确认卸载".to_string();
                        } else {
                            self.hint = "✗ UAC 触发失败（可能被取消）".to_string();
                        }
                        self.confirm_uninstall = false;
                    }
                    if ui.button("取消").clicked() {
                        self.confirm_uninstall = false;
                    }
                });
            }

            ui.separator();
            let mut en = self.enabled;
            if ui.checkbox(&mut en, "中继启用（关闭后拒绝执行）").clicked() {
                common::set_enabled(en);
                self.enabled = en;
            }

            ui.separator();
            ui.heading("令牌");
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("指纹: {}", self.token_fp));
                ui.label(format!("写入时间: {}", self.token_mtime));
                if ui.button("轮换令牌 (管理员)").clicked() {
                    self.confirm_rotate = true;
                }
            });
            // 轮换二次确认：旧令牌立即失效，AI 侧需同步
            if self.confirm_rotate {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(163, 45, 45),
                        "确认轮换？旧令牌立即失效，持有旧令牌的 AI 客户端将无法执行",
                    );
                    if ui.button("确认轮换").clicked() {
                        self.rotate_trigger();
                    }
                    if ui.button("取消").clicked() {
                        self.confirm_rotate = false;
                    }
                });
            }
            ui.label("令牌等同于 SYSTEM 凭据，仅显示前 8 位指纹；完整令牌在管理员终端执行 ai-root rotate-token 查看");

            ui.separator();
            ui.heading("直接下发命令（连管道即可，无需提权）");
            ui.horizontal(|ui| {
                ui.add_enabled(
                    !self.cmd_busy,
                    egui::TextEdit::singleline(&mut self.run_cmd).desired_width(480.0),
                );
                if ui
                    .add_enabled(!self.cmd_busy, egui::Button::new("执行"))
                    .clicked()
                    && !self.run_cmd.trim().is_empty()
                {
                    let cmd = self.run_cmd.clone();
                    self.execute_async(&cmd);
                }
            });
            ui.label("示例: pnputil /remove-device \"HID\\VID_373B&PID_1216&MI_00&COL01\\7&5091028&0&0000\" /force");
            ui.add(
                egui::TextEdit::multiline(&mut self.output)
                    .desired_rows(10)
                    .desired_width(f32::INFINITY),
            );

            ui.separator();
            ui.heading("执行日志");
            ui.add_enabled(
                false,
                egui::TextEdit::multiline(&mut self.log)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );

            if !self.hint.is_empty() {
                ui.separator();
                let color = if self.hint.starts_with('✓') {
                    egui::Color32::from_rgb(59, 109, 17)
                } else if self.hint.starts_with('⚠') {
                    egui::Color32::from_rgb(133, 79, 11)
                } else {
                    egui::Color32::from_rgb(163, 45, 45)
                };
                ui.colored_label(color, &self.hint);
            }
        });
    }
}

/// 查询服务状态字符串（出错区分「未安装」与「已停止」）
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

/// 同步执行一次管道请求并格式化输出（在后台线程运行）
fn run_pipe(token: &str, command: &str) -> String {
    let mut file = match protocol::connect_client() {
        Ok(f) => f,
        Err(e) => return format!("[错误] 连接失败: {e}（服务是否运行？）"),
    };
    let req = protocol::Request {
        token: token.to_string(),
        command: command.to_string(),
        cwd: None,
        timeout_ms: Some(60000),
    };
    let bytes = match serde_json::to_vec(&req) {
        Ok(b) => b,
        Err(e) => return format!("[错误] 序列化失败: {e}"),
    };
    if protocol::write_frame(&mut file, &bytes).is_err() {
        return "[错误] 发送失败".to_string();
    }
    let resp_bytes = match protocol::read_frame(&mut file) {
        Ok(b) => b,
        Err(e) => return format!("[错误] 读取失败: {e}"),
    };
    match serde_json::from_slice::<protocol::Response>(&resp_bytes) {
        Ok(resp) => {
            let mut s = String::new();
            if !resp.stdout.is_empty() {
                s.push_str(&resp.stdout);
                s.push('\n');
            }
            if !resp.stderr.is_empty() {
                s.push_str("[stderr]\n");
                s.push_str(&resp.stderr);
                s.push('\n');
            }
            if let Some(e) = resp.error {
                s.push_str(&format!("[relay error] {e}\n"));
            }
            s.push_str(&format!("exit_code={}", resp.exit_code));
            s
        }
        Err(e) => format!("[错误] 响应解析失败: {e}"),
    }
}
