//! cairn-miner-launcher — native desktop control panel for cairn-miner.
//!
//! Open it to configure your address/worker/pool/backend, Start/Stop mining,
//! toggle start-on-login, and watch live performance (hashrate graph, shares,
//! difficulty, uptime) — all in the cairn CRT-phosphor theme. It drives the
//! `cairn-miner` binary that ships alongside it and reads that miner's loopback
//! stats endpoint for the live numbers.

// Release builds are a GUI app: no console window should flash on launch.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

mod autostart;
mod config;
mod miner;
mod stats;
mod theme;

use config::LauncherConfig;
use miner::MinerHandle;
use stats::StatsSnapshot;

const HISTORY_LEN: usize = 120; // ~2 min of 1 Hz samples
const POLL_EVERY: Duration = Duration::from_millis(1000);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 760.0])
            .with_min_inner_size([560.0, 560.0])
            .with_title("cairn // miner"),
        ..Default::default()
    };
    eframe::run_native(
        "cairn-miner-launcher",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(LauncherApp::new()))
        }),
    )
}

/// A one-shot user intent captured while rendering, applied after the frame so
/// it doesn't fight the UI's borrow of `self`.
enum Action {
    Start,
    Stop,
    Save,
    SetAutostart(bool),
}

struct LauncherApp {
    cfg: LauncherConfig,
    config_path: PathBuf,
    log_dir: PathBuf,
    miner_exe: PathBuf,

    pools_text: String,
    autostart: bool,

    miner: Option<MinerHandle>,
    stats: Option<StatsSnapshot>,
    hashrate_history: VecDeque<f32>,
    log_lines: Vec<String>,

    status: String,
    last_poll: Instant,
    pending: Option<Action>,
}

impl LauncherApp {
    fn new() -> Self {
        let config_path = config::config_path();
        let cfg = LauncherConfig::load(&config_path);
        let pools_text = cfg.pools.join("\n");
        Self {
            log_dir: miner::log_dir(&config_path),
            miner_exe: miner::miner_exe_path(),
            pools_text,
            autostart: autostart::is_enabled(),
            cfg,
            config_path,
            miner: None,
            stats: None,
            hashrate_history: VecDeque::with_capacity(HISTORY_LEN),
            log_lines: Vec::new(),
            status: "idle — configure and press Start".to_string(),
            last_poll: Instant::now() - POLL_EVERY,
            pending: None,
        }
    }

    fn is_mining(&self) -> bool {
        self.miner.is_some()
    }

    /// Fold the editable pool textbox back into the typed config.
    fn sync_pools_from_text(&mut self) {
        self.cfg.pools = self
            .pools_text
            .split(['\n', ','])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    fn save_config(&mut self) {
        self.sync_pools_from_text();
        self.cfg.address = normalize_address(&self.cfg.address);
        match self.cfg.save(&self.config_path) {
            Ok(()) => self.status = format!("saved settings → {}", self.config_path.display()),
            Err(e) => self.status = format!("could not save settings: {e}"),
        }
    }

    fn start(&mut self) {
        self.sync_pools_from_text();
        self.cfg.address = normalize_address(&self.cfg.address);
        if !valid_address(&self.cfg.address) {
            self.status = "✗ address must be 40 hex characters (your addr20 payout address)".into();
            return;
        }
        if !self.miner_exe.exists() {
            self.status = format!(
                "✗ cairn-miner not found next to the launcher (looked for {})",
                self.miner_exe.display()
            );
            return;
        }
        if let Err(e) = self.cfg.save(&self.config_path) {
            self.status = format!("✗ could not write config: {e}");
            return;
        }
        match miner::spawn(&self.miner_exe, &self.config_path, &self.log_dir) {
            Ok(handle) => {
                self.status = "mining started".into();
                self.stats = None;
                self.hashrate_history.clear();
                self.log_lines.clear();
                self.miner = Some(handle);
                self.last_poll = Instant::now() - POLL_EVERY;
            }
            Err(e) => self.status = format!("✗ failed to start miner: {e}"),
        }
    }

    fn stop(&mut self) {
        if let Some(mut h) = self.miner.take() {
            h.stop();
        }
        self.stats = None;
        self.status = "mining stopped".into();
    }

    fn set_autostart(&mut self, enabled: bool) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cairn-miner-launcher"));
        match autostart::set(enabled, &exe) {
            Ok(()) => {
                self.autostart = enabled;
                self.status = if enabled {
                    "start-on-login enabled".into()
                } else {
                    "start-on-login disabled".into()
                };
            }
            Err(e) => self.status = format!("could not change autostart: {e}"),
        }
    }

    /// Poll the miner's stats endpoint + tail its log (throttled to POLL_EVERY).
    fn poll(&mut self) {
        if self.last_poll.elapsed() < POLL_EVERY {
            return;
        }
        self.last_poll = Instant::now();

        // Detect a miner that exited on its own (bad config, lost pool, crash).
        let exited = self.miner.as_mut().and_then(MinerHandle::exit_code);
        if let Some(code) = exited {
            self.miner = None;
            self.stats = None;
            self.status = format!("⚠ miner exited (code {code}) — check the log below");
        }

        if let Some(h) = self.miner.as_ref() {
            let port = h.stats_port;
            let log_path = h.log_path.clone();
            if let Some(s) = stats::fetch(port) {
                self.hashrate_history
                    .push_back(s.hashrate_total_hps as f32);
                while self.hashrate_history.len() > HISTORY_LEN {
                    self.hashrate_history.pop_front();
                }
                self.stats = Some(s);
            }
            self.log_lines = miner::tail_log(&log_path, 200);
        }
    }
}

impl eframe::App for LauncherApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = theme::BG;
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            1.0,
        ]
    }

    // eframe 0.35 hands us the central `Ui` directly (no margin/background).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.is_mining() {
            self.poll();
            ui.ctx().request_repaint_after(POLL_EVERY);
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.header(ui);
                ui.add_space(10.0);
                self.performance_panel(ui);
                ui.add_space(10.0);
                self.settings_panel(ui);
                ui.add_space(10.0);
                self.controls_panel(ui);
                ui.add_space(10.0);
                self.log_panel(ui);
            });

        // Apply the captured intent now that the UI borrow is released.
        match self.pending.take() {
            Some(Action::Start) => self.start(),
            Some(Action::Stop) => self.stop(),
            Some(Action::Save) => self.save_config(),
            Some(Action::SetAutostart(v)) => self.set_autostart(v),
            None => {}
        }
    }
}

// --- UI sections ------------------------------------------------------------

impl LauncherApp {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let connected = self.stats.as_ref().map(|s| s.connected).unwrap_or(false);
            let (dot, label) = if self.is_mining() && connected {
                (theme::GREEN, "LIVE")
            } else if self.is_mining() {
                (theme::AMBER, "CONNECTING")
            } else {
                (theme::DIM, "OFFLINE")
            };
            ui.label(RichText::new("●").color(dot).size(16.0));
            ui.label(
                RichText::new("cairn")
                    .color(theme::GREEN)
                    .size(20.0)
                    .strong(),
            );
            ui.label(RichText::new("// miner").color(theme::AMBER).size(20.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(label).color(dot).size(11.0).strong());
            });
        });
        let subtitle = match self.stats.as_ref().filter(|_| self.is_mining()) {
            Some(s) => {
                let worker = if s.worker.is_empty() { "—" } else { &s.worker };
                format!("{}  ·  worker {}  ·  miner v{}", s.pool, worker, s.version)
            }
            None => "stack CSD; mark what matters.".to_string(),
        };
        ui.label(RichText::new(subtitle).color(theme::DIM).size(11.0));
    }

    fn performance_panel(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            heading(ui, "PERFORMANCE");
            let s = self.stats.clone().unwrap_or_default();

            // Big auto-scaled hashrate.
            let (num, unit) = theme::split_hashrate(s.hashrate_total_hps);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(num)
                        .color(theme::GREEN)
                        .size(34.0)
                        .strong(),
                );
                ui.label(RichText::new(unit).color(theme::DIM2).size(14.0));
            });
            if self.is_mining() {
                ui.label(
                    RichText::new(format!(
                        "gpu {}  ·  cpu {}",
                        theme::format_hashrate(s.hashrate_gpu_hps),
                        theme::format_hashrate(s.hashrate_cpu_hps),
                    ))
                    .color(theme::DIM)
                    .size(11.0),
                );
            }

            draw_sparkline(ui, &self.hashrate_history);
            ui.add_space(8.0);

            // Stat tiles.
            ui.horizontal_wrapped(|ui| {
                stat_tile(ui, "ACCEPTED", &s.shares_accepted.to_string(), theme::GREEN);
                let rej = format!("{} ({:.1}%)", s.shares_rejected, s.reject_pct());
                let rej_color = if s.shares_rejected > 0 { theme::RED } else { theme::DIM2 };
                stat_tile(ui, "REJECTED", &rej, rej_color);
                stat_tile(ui, "FOUND", &s.shares_submitted.to_string(), theme::DIM2);
                stat_tile(ui, "DIFFICULTY", &format!("{:.0}", s.difficulty), theme::AMBER);
                stat_tile(ui, "UPTIME", &theme::format_duration(s.uptime_secs), theme::FG);
                let backend = if s.backend.is_empty() { "—".into() } else { s.backend.to_uppercase() };
                stat_tile(ui, "BACKEND", &backend, theme::FG);
                let last = s
                    .last_share_age_secs
                    .map(|a| theme::format_duration(a) + " ago")
                    .unwrap_or_else(|| "none yet".into());
                stat_tile(ui, "LAST SHARE", &last, theme::DIM2);
            });

            if !self.is_mining() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("not mining — press Start below")
                        .color(theme::DIM)
                        .size(11.0),
                );
            }
        });
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let mining = self.is_mining();
        panel(ui, |ui| {
            heading(ui, "SETTINGS");
            ui.add_enabled_ui(!mining, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Payout address").color(theme::DIM2));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.address)
                                .hint_text("your addr20 — 40 hex chars")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label(RichText::new("Worker name").color(theme::DIM2));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.worker)
                                .hint_text("defaults to this PC's hostname")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label(RichText::new("Pool(s)").color(theme::DIM2));
                        ui.add(
                            egui::TextEdit::multiline(&mut self.pools_text)
                                .hint_text("cairn-pool.com:3333\n(blank = default; extra lines = failover)")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label(RichText::new("Backend").color(theme::DIM2));
                        egui::ComboBox::from_id_salt("backend")
                            .selected_text(self.cfg.backend.to_uppercase())
                            .show_ui(ui, |ui| {
                                for opt in ["auto", "cuda", "opencl", "cpu"] {
                                    ui.selectable_value(&mut self.cfg.backend, opt.to_string(), opt);
                                }
                            });
                        ui.end_row();

                        ui.label(RichText::new("GPU device index").color(theme::DIM2));
                        ui.add(egui::DragValue::new(&mut self.cfg.device).range(0..=64));
                        ui.end_row();

                        ui.label(RichText::new("CPU threads (dual-mine)").color(theme::DIM2));
                        ui.add(egui::DragValue::new(&mut self.cfg.cpu_threads).range(0..=256));
                        ui.end_row();

                        ui.label(RichText::new("Reserve cores").color(theme::DIM2));
                        ui.add(egui::DragValue::new(&mut self.cfg.reserve).range(0..=64));
                        ui.end_row();
                    });
            });
            if mining {
                ui.label(
                    RichText::new("stop mining to change settings")
                        .color(theme::DIM)
                        .size(11.0),
                );
            } else if ui.button("Save settings").clicked() {
                self.pending = Some(Action::Save);
            }
        });
    }

    fn controls_panel(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            ui.horizontal(|ui| {
                if self.is_mining() {
                    if big_button(ui, "■  STOP", theme::RED).clicked() {
                        self.pending = Some(Action::Stop);
                    }
                } else if big_button(ui, "▶  START", theme::GREEN).clicked() {
                    self.pending = Some(Action::Start);
                }

                ui.add_space(16.0);
                let mut autostart = self.autostart;
                if ui
                    .checkbox(&mut autostart, "Start on Windows login")
                    .changed()
                {
                    self.pending = Some(Action::SetAutostart(autostart));
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(&self.status).color(theme::DIM2).size(12.0));
        });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            heading(ui, "LOG");
            egui::ScrollArea::vertical()
                .max_height(160.0)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.log_lines.is_empty() {
                        ui.label(RichText::new("(no log yet)").color(theme::DIM).size(11.0));
                    }
                    for line in &self.log_lines {
                        ui.label(
                            RichText::new(line)
                                .color(theme::DIM2)
                                .size(11.0)
                                .family(egui::FontFamily::Monospace),
                        );
                    }
                });
        });
    }
}

// --- small UI helpers -------------------------------------------------------

fn panel<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .color(theme::AMBER)
            .size(12.0)
            .strong(),
    );
    ui.add_space(6.0);
}

fn stat_tile(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    egui::Frame::new()
        .fill(theme::PANEL2)
        .stroke(egui::Stroke::new(1.0, theme::LINE))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).color(color).size(18.0).strong());
                ui.label(RichText::new(label).color(theme::DIM).size(10.0));
            });
        });
}

fn big_button(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).color(color).size(16.0).strong())
        .min_size(egui::vec2(150.0, 40.0))
        .stroke(egui::Stroke::new(1.0, color))
        .fill(theme::PANEL2);
    ui.add(btn)
}

/// A dependency-free hashrate chart: green line + faint area fill over a faint
/// gridline, drawn straight onto the egui painter (matches the cairn explorer).
fn draw_sparkline(ui: &mut egui::Ui, history: &VecDeque<f32>) {
    let height = 90.0;
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Backdrop + gridlines.
    painter.rect_filled(rect, 6.0, theme::PANEL2);
    for i in 1..4 {
        let y = rect.top() + rect.height() * (i as f32 / 4.0);
        painter.hline(
            rect.left()..=rect.right(),
            y,
            egui::Stroke::new(1.0, theme::LINE),
        );
    }

    if history.len() < 2 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "gathering samples…",
            egui::FontId::monospace(11.0),
            theme::DIM,
        );
        return;
    }

    let max = history.iter().cloned().fold(1.0_f32, f32::max);
    let n = history.len();
    let pad = 6.0;
    let x_at = |i: usize| {
        rect.left() + pad + (rect.width() - 2.0 * pad) * (i as f32 / (n - 1) as f32)
    };
    let y_at = |v: f32| {
        rect.bottom() - pad - (rect.height() - 2.0 * pad) * (v / max).clamp(0.0, 1.0)
    };

    let points: Vec<egui::Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| egui::pos2(x_at(i), y_at(v)))
        .collect();

    // Area fill under the line.
    let mut poly = points.clone();
    poly.push(egui::pos2(x_at(n - 1), rect.bottom() - pad));
    poly.push(egui::pos2(x_at(0), rect.bottom() - pad));
    painter.add(egui::Shape::convex_polygon(
        poly,
        theme::GREEN.linear_multiply(0.10),
        egui::Stroke::NONE,
    ));
    // The line itself.
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.6, theme::GREEN),
    ));
}

// --- address helpers --------------------------------------------------------

fn normalize_address(a: &str) -> String {
    let a = a.trim();
    let a = a.strip_prefix("0x").or_else(|| a.strip_prefix("0X")).unwrap_or(a);
    a.to_lowercase()
}

fn valid_address(a: &str) -> bool {
    a.len() == 40 && a.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_normalizes_and_validates() {
        assert_eq!(
            normalize_address("0xABCDEF0123456789abcdef0123456789ABCDEF01"),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert!(valid_address(&normalize_address(
            "0xABCDEF0123456789abcdef0123456789ABCDEF01"
        )));
        assert!(!valid_address("tooshort"));
        assert!(!valid_address("g23456789012345678901234567890123456789z"));
    }
}
