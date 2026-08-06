//! 原生 GUI 控制面板（egui 即时模式）
//!
//! 功能：服务状态查看、启停/安装/卸载（自动提权 runas）、
//! 启用开关、执行日志查看，以及通过管道直接下发命令（非提权即可，因仅连接管道）。

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
        Box::new(|_cc| Ok(Box::new(WbRelayApp::new()))),
    );
}

struct WbRelayApp {
    status: String,
    enabled: bool,
    log: String,
    run_cmd: String,
    output: String,
}

impl WbRelayApp {
    fn new() -> Self {
        let enabled = common::is_enabled();
        let status = query_status_string();
        let log = common::read_log_tail(200);
        Self {
            status,
            enabled,
            log,
            run_cmd: String::new(),
            output: String::new(),
        }
    }

    fn refresh(&mut self) {
        self.status = query_status_string();
        self.enabled = common::is_enabled();
        self.log = common::read_log_tail(200);
    }

    /// 以提权方式执行 ai-root 子命令（通过 PowerShell Start-Process -Verb RunAs 触发 UAC）
    fn elevate(&self, sub: &str) -> bool {
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_str = exe.to_string_lossy().to_string();
        // PowerShell: Start-Process -FilePath <exe> -ArgumentList <sub> -Verb RunAs
        let ps_cmd = format!(
            "Start-Process -FilePath '{}' -ArgumentList '{}' -Verb RunAs",
            exe_str.replace('\'', "''"),
            sub.replace('\'', "''")
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .status();
        matches!(status, Ok(s) if s.success())
    }

    /// 通过管道直接执行命令（非提权，连接即可）
    fn execute(&mut self, command: &str) {
        let token = match common::read_token() {
            Some(t) => t,
            None => {
                self.output = "[错误] 未找到令牌，请先以管理员运行 install".to_string();
                return;
            }
        };
        let mut file = match protocol::connect_client() {
            Ok(f) => f,
            Err(e) => {
                self.output = format!("[错误] 连接失败: {e}（服务是否运行？）");
                return;
            }
        };
        let req = protocol::Request {
            token,
            command: command.to_string(),
            cwd: None,
            timeout_ms: Some(60000),
        };
        let bytes = match serde_json::to_vec(&req) {
            Ok(b) => b,
            Err(e) => {
                self.output = format!("[错误] 序列化失败: {e}");
                return;
            }
        };
        if protocol::write_frame(&mut file, &bytes).is_err() {
            self.output = "[错误] 发送失败".to_string();
            return;
        }
        let resp_bytes = match protocol::read_frame(&mut file) {
            Ok(b) => b,
            Err(e) => {
                self.output = format!("[错误] 读取失败: {e}");
                return;
            }
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
                self.output = s;
            }
            Err(e) => {
                self.output = format!("[错误] 响应解析失败: {e}");
            }
        }
    }
}

impl eframe::App for WbRelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                if ui.button("安装服务 (管理员)").clicked() {
                    self.elevate("install");
                    self.refresh();
                }
                if ui.button("启动 (管理员)").clicked() {
                    self.elevate("start");
                    self.refresh();
                }
                if ui.button("停止 (管理员)").clicked() {
                    self.elevate("stop");
                    self.refresh();
                }
                if ui.button("卸载 (管理员)").clicked() {
                    self.elevate("uninstall");
                }
            });

            ui.separator();
            let mut en = self.enabled;
            if ui.checkbox(&mut en, "中继启用（关闭后拒绝执行）").clicked() {
                common::set_enabled(en);
                self.enabled = en;
            }

            ui.separator();
            ui.heading("直接下发命令（连管道即可，无需提权）");
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.run_cmd);
                if ui.button("执行").clicked() && !self.run_cmd.trim().is_empty() {
                    let cmd = self.run_cmd.clone();
                    self.execute(&cmd);
                    self.log = common::read_log_tail(200);
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
