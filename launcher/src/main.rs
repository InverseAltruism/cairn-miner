//! cairn-miner-launcher — native desktop control panel for cairn-miner.
//!
//! A single, self-contained Windows app (the miner is embedded). Tabbed layout:
//! a persistent top bar (brand + Start/Stop + status), then Dashboard (live
//! aggregated performance across every worker), Settings (mode / GPU picker /
//! CPU intensity / identity), and Logs (level-filtered). One process is spawned
//! per selected GPU plus an optional CPU worker; their stats are summed.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

mod autostart;
mod config;
mod devices;
mod embed;
mod engine;
mod stats;
mod theme;

use config::{CpuIntensity, LauncherConfig, Mode};
use devices::Devices;
use engine::{Aggregate, Engine, GpuSpec, StartSpec, WorkerRow};

const HISTORY_LEN: usize = 120;
const POLL_EVERY: Duration = Duration::from_millis(1000);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 720.0])
            .with_min_inner_size([620.0, 560.0])
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

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Dashboard,
    Settings,
    Logs,
}

#[derive(PartialEq, Clone, Copy)]
enum LogFilter {
    All,
    Warn,
    Error,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Level {
    Info,
    Warn,
    Error,
}

enum Action {
    Start,
    Stop,
    Save,
    RefreshDevices,
    SetAutostart(bool),
}

struct LauncherApp {
    cfg: LauncherConfig,
    config_path: PathBuf,
    log_dir: PathBuf,
    miner_exe: Option<PathBuf>,
    miner_err: Option<String>,
    devices: Devices,

    engine: Option<Engine>,
    agg: Aggregate,
    rows: Vec<WorkerRow>,
    hashrate_history: VecDeque<f32>,
    log_lines: Vec<String>,

    tab: Tab,
    log_filter: LogFilter,
    pools_text: String,
    autostart: bool,
    status: String,
    last_poll: Instant,
    pending: Option<Action>,
}

impl LauncherApp {
    fn new() -> Self {
        let config_path = config::config_path();
        let cfg = LauncherConfig::load(&config_path);
        let log_dir = config::app_dir().join("logs");
        let pools_text = cfg.pools.join("\n");

        let (miner_exe, miner_err) = match embed::ensure_miner() {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let devices = match &miner_exe {
            Some(m) => devices::probe(m, &log_dir),
            None => Devices::default(),
        };

        let mut app = Self {
            cfg,
            config_path,
            log_dir,
            miner_exe,
            miner_err,
            devices,
            engine: None,
            agg: Aggregate::default(),
            rows: Vec::new(),
            hashrate_history: VecDeque::with_capacity(HISTORY_LEN),
            log_lines: Vec::new(),
            tab: Tab::Dashboard,
            log_filter: LogFilter::All,
            pools_text,
            autostart: autostart::is_enabled(),
            status: "idle — set up in Settings, then Start".into(),
            last_poll: Instant::now() - POLL_EVERY,
            pending: None,
        };
        app.preselect_gpus_if_empty();
        // Nudge first-run users with no address straight to Settings.
        if app.cfg.address.trim().is_empty() {
            app.tab = Tab::Settings;
        }
        app
    }

    fn is_mining(&self) -> bool {
        self.engine.is_some()
    }

    fn preselect_gpus_if_empty(&mut self) {
        if self.cfg.selected_gpus.is_empty() && !self.devices.gpus.is_empty() {
            self.cfg.selected_gpus = self.devices.gpus.iter().map(|g| g.key()).collect();
        }
    }

    fn sync_pools_from_text(&mut self) {
        self.cfg.pools = self
            .pools_text
            .split(['\n', ','])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    fn refresh_devices(&mut self) {
        if let Some(m) = &self.miner_exe {
            self.devices = devices::probe(m, &self.log_dir);
            let present: Vec<String> = self.devices.gpus.iter().map(|g| g.key()).collect();
            self.cfg.selected_gpus.retain(|k| present.contains(k));
            self.preselect_gpus_if_empty();
            self.status = format!(
                "detected {} GPU(s), {} CPU cores",
                self.devices.gpus.len(),
                self.devices.cpu.logical_cores
            );
        }
    }

    fn save_config(&mut self) {
        self.sync_pools_from_text();
        self.cfg.address = normalize_address(&self.cfg.address);
        match self.cfg.save(&self.config_path) {
            Ok(()) => self.status = "settings saved".into(),
            Err(e) => self.status = format!("could not save settings: {e}"),
        }
    }

    fn build_spec(&self) -> Result<StartSpec, String> {
        let miner_exe = self
            .miner_exe
            .clone()
            .ok_or_else(|| self.miner_err.clone().unwrap_or_else(|| "miner unavailable".into()))?;

        let address = normalize_address(&self.cfg.address);
        if !valid_address(&address) {
            return Err("address must be 40 hex characters (your addr20 payout address)".into());
        }

        let gpus: Vec<GpuSpec> = if self.cfg.mode.uses_gpu() {
            let chosen: Vec<GpuSpec> = self
                .devices
                .gpus
                .iter()
                .filter(|g| self.cfg.selected_gpus.contains(&g.key()))
                .map(|g| GpuSpec { backend: g.backend.clone(), index: g.index, name: g.name.clone() })
                .collect();
            if chosen.is_empty() {
                if self.devices.gpus.is_empty() {
                    return Err("no GPUs detected — switch mode to \"CPU only\"".into());
                }
                return Err("select at least one GPU (or switch to \"CPU only\")".into());
            }
            chosen
        } else {
            Vec::new()
        };

        let cpu_threads = if self.cfg.mode.uses_cpu() {
            Some(self.cfg.cpu_intensity.threads(self.devices.cpu.logical_cores))
        } else {
            None
        };

        let worker_base = {
            let w = self.cfg.worker.trim();
            if w.is_empty() { "rig".to_string() } else { w.to_string() }
        };

        Ok(StartSpec {
            miner_exe,
            address,
            worker_base,
            pools: self.cfg.pools.clone(),
            gpus,
            cpu_threads,
            log_dir: self.log_dir.clone(),
        })
    }

    fn start(&mut self) {
        self.sync_pools_from_text();
        self.cfg.address = normalize_address(&self.cfg.address);
        let spec = match self.build_spec() {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("✗ {e}");
                self.tab = Tab::Settings;
                return;
            }
        };
        let _ = self.cfg.save(&self.config_path);
        match Engine::start(&spec) {
            Ok(engine) => {
                let n = engine.worker_count();
                self.engine = Some(engine);
                self.hashrate_history.clear();
                self.log_lines.clear();
                self.agg = Aggregate::default();
                self.rows.clear();
                self.last_poll = Instant::now() - POLL_EVERY;
                self.status = format!("mining started — {n} worker(s)");
                self.tab = Tab::Dashboard;
            }
            Err(e) => {
                self.status = format!("✗ {e}");
                self.tab = Tab::Settings;
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut e) = self.engine.take() {
            e.stop();
        }
        self.rows.clear();
        self.agg = Aggregate::default();
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

    fn poll(&mut self) {
        if self.last_poll.elapsed() < POLL_EVERY {
            return;
        }
        self.last_poll = Instant::now();
        if let Some(engine) = &mut self.engine {
            engine.poll();
            self.agg = engine.aggregate();
            self.rows = engine.rows();
            self.log_lines = engine.tail_logs(60);
            self.hashrate_history.push_back(self.agg.hashrate_total_hps as f32);
            while self.hashrate_history.len() > HISTORY_LEN {
                self.hashrate_history.pop_front();
            }
            if self.agg.workers_alive == 0 {
                self.status = "⚠ all workers exited — see Logs".into();
            }
        }
    }
}

impl eframe::App for LauncherApp {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        let c = theme::BG;
        [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, 1.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.is_mining() {
            self.poll();
            ui.ctx().request_repaint_after(POLL_EVERY);
        }

        self.top_bar(ui);
        ui.add_space(8.0);
        self.tab_bar(ui);
        ui.add_space(10.0);
        match self.tab {
            Tab::Dashboard => self.dashboard(ui),
            Tab::Settings => self.settings(ui),
            Tab::Logs => self.logs(ui),
        }

        match self.pending.take() {
            Some(Action::Start) => self.start(),
            Some(Action::Stop) => self.stop(),
            Some(Action::Save) => self.save_config(),
            Some(Action::RefreshDevices) => self.refresh_devices(),
            Some(Action::SetAutostart(v)) => self.set_autostart(v),
            None => {}
        }
    }
}

// --- top bar + tabs ---------------------------------------------------------

impl LauncherApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, label) = if self.is_mining() && self.agg.connected {
                    (theme::GREEN, "LIVE")
                } else if self.is_mining() {
                    (theme::AMBER, "CONNECTING")
                } else {
                    (theme::DIM, "OFFLINE")
                };
                ui.label(RichText::new("●").color(dot).size(15.0));
                ui.label(RichText::new("cairn").color(theme::GREEN).size(19.0).strong());
                ui.label(RichText::new("// miner").color(theme::AMBER).size(19.0));
                ui.label(RichText::new(label).color(dot).size(11.0).strong());

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_mining() {
                        if big_button(ui, "■  STOP", theme::RED).clicked() {
                            self.pending = Some(Action::Stop);
                        }
                    } else {
                        let enabled = self.miner_exe.is_some();
                        if ui.add_enabled(enabled, big("▶  START".into(), theme::GREEN)).clicked() {
                            self.pending = Some(Action::Start);
                        }
                    }
                });
            });
            ui.add_space(2.0);
            ui.label(RichText::new(&self.status).color(theme::DIM2).size(11.0));
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Dashboard, "Dashboard"),
                (Tab::Settings, "Settings"),
                (Tab::Logs, "Logs"),
            ] {
                let selected = self.tab == tab;
                let color = if selected { theme::GREEN } else { theme::DIM2 };
                if ui
                    .selectable_label(selected, RichText::new(label).color(color).size(13.0))
                    .clicked()
                {
                    self.tab = tab;
                }
            }
        });
    }

    // --- Dashboard tab ------------------------------------------------------

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            let (num, unit) = theme::split_hashrate(self.agg.hashrate_total_hps);
            ui.horizontal(|ui| {
                ui.label(RichText::new(num).color(theme::GREEN).size(38.0).strong());
                ui.label(RichText::new(unit).color(theme::DIM2).size(15.0));
                if self.is_mining() && self.agg.workers_total > 1 {
                    ui.label(
                        RichText::new(format!("across {} workers", self.agg.workers_total))
                            .color(theme::DIM)
                            .size(11.0),
                    );
                }
            });
            draw_sparkline(ui, &self.hashrate_history);
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                stat_tile(ui, "ACCEPTED", &self.agg.shares_accepted.to_string(), theme::GREEN);
                let rej = format!("{} ({:.1}%)", self.agg.shares_rejected, self.agg.reject_pct());
                let rc = if self.agg.shares_rejected > 0 { theme::RED } else { theme::DIM2 };
                stat_tile(ui, "REJECTED", &rej, rc);
                stat_tile(ui, "DIFFICULTY", &format!("{:.0}", self.agg.difficulty), theme::AMBER);
                stat_tile(ui, "UPTIME", &theme::format_duration(self.agg.uptime_secs), theme::FG);
                stat_tile(
                    ui,
                    "WORKERS",
                    &format!("{}/{}", self.agg.workers_alive, self.agg.workers_total),
                    theme::FG,
                );
            });
        });

        if !self.rows.is_empty() {
            ui.add_space(10.0);
            panel(ui, |ui| {
                heading(ui, "WORKERS");
                for r in &self.rows {
                    ui.horizontal(|ui| {
                        let dot = if r.alive && r.connected {
                            theme::GREEN
                        } else if r.alive {
                            theme::AMBER
                        } else {
                            theme::RED
                        };
                        ui.label(RichText::new("●").color(dot).size(10.0));
                        ui.label(RichText::new(&r.label).color(theme::FG).size(12.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(theme::format_hashrate(r.hashrate_hps))
                                    .color(theme::GREEN)
                                    .size(12.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new(format!("acc {} · rej {}", r.accepted, r.rejected))
                                    .color(theme::DIM)
                                    .size(11.0),
                            );
                        });
                    });
                }
            });
        } else if !self.is_mining() {
            ui.add_space(10.0);
            ui.label(
                RichText::new("Not mining. Configure in Settings, then press START.")
                    .color(theme::DIM)
                    .size(12.0),
            );
        }
    }

    // --- Settings tab -------------------------------------------------------

    fn settings(&mut self, ui: &mut egui::Ui) {
        let mining = self.is_mining();
        panel(ui, |ui| {
            if let Some(err) = &self.miner_err {
                ui.label(RichText::new(format!("✗ miner unavailable: {err}")).color(theme::RED).size(11.0));
            }
            ui.add_enabled_ui(!mining, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Mine with").color(theme::DIM2));
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(self.cfg.mode.label())
                        .show_ui(ui, |ui| {
                            for m in Mode::ALL {
                                ui.selectable_value(&mut self.cfg.mode, m, m.label());
                            }
                        });
                });

                if self.cfg.mode.uses_gpu() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("GPUs").color(theme::AMBER).size(11.0).strong());
                        if ui.small_button("↻ refresh").clicked() {
                            self.pending = Some(Action::RefreshDevices);
                        }
                    });
                    if self.devices.gpus.is_empty() {
                        ui.label(
                            RichText::new("No GPUs detected. Use \"CPU only\", or check drivers and refresh.")
                                .color(theme::RED)
                                .size(11.0),
                        );
                        for note in &self.devices.notes {
                            ui.label(RichText::new(format!("• {note}")).color(theme::DIM).size(10.0));
                        }
                    } else {
                        for g in self.devices.gpus.clone() {
                            let key = g.key();
                            let mut on = self.cfg.selected_gpus.contains(&key);
                            if ui.checkbox(&mut on, g.display()).changed() {
                                if on {
                                    if !self.cfg.selected_gpus.contains(&key) {
                                        self.cfg.selected_gpus.push(key.clone());
                                    }
                                } else {
                                    self.cfg.selected_gpus.retain(|k| k != &key);
                                }
                            }
                        }
                    }
                }

                if self.cfg.mode.uses_cpu() {
                    ui.add_space(8.0);
                    let cores = self.devices.cpu.logical_cores;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("CPU intensity").color(theme::DIM2));
                        egui::ComboBox::from_id_salt("cpu_intensity")
                            .selected_text(self.cfg.cpu_intensity.label())
                            .show_ui(ui, |ui| {
                                for c in CpuIntensity::ALL {
                                    ui.selectable_value(&mut self.cfg.cpu_intensity, c, c.label());
                                }
                            });
                        ui.label(
                            RichText::new(format!("({} of {} cores)", self.cfg.cpu_intensity.threads(cores), cores))
                                .color(theme::DIM)
                                .size(11.0),
                        );
                    });
                }

                ui.add_space(10.0);
                egui::Grid::new("identity")
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
                                .hint_text("rig name (per-GPU suffixes added automatically)")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        ui.label(RichText::new("Pool(s)").color(theme::DIM2));
                        ui.add(
                            egui::TextEdit::multiline(&mut self.pools_text)
                                .hint_text("blank = default cairn pool; extra lines = failover")
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if mining {
                    ui.label(RichText::new("stop mining to change settings").color(theme::DIM).size(11.0));
                } else if ui.button("Save settings").clicked() {
                    self.pending = Some(Action::Save);
                }
                let mut a = self.autostart;
                if ui.checkbox(&mut a, "Start on Windows login").changed() {
                    self.pending = Some(Action::SetAutostart(a));
                }
            });
        });
    }

    // --- Logs tab -----------------------------------------------------------

    fn logs(&mut self, ui: &mut egui::Ui) {
        panel(ui, |ui| {
            ui.horizontal(|ui| {
                heading_inline(ui, "LOG");
                for (f, label) in [
                    (LogFilter::All, "All"),
                    (LogFilter::Warn, "Warnings"),
                    (LogFilter::Error, "Errors"),
                ] {
                    let sel = self.log_filter == f;
                    let c = if sel { theme::GREEN } else { theme::DIM2 };
                    if ui.selectable_label(sel, RichText::new(label).color(c).size(11.0)).clicked() {
                        self.log_filter = f;
                    }
                }
            });
            ui.add_space(4.0);
            let avail = ui.available_height().max(120.0);
            egui::ScrollArea::vertical()
                .max_height(avail)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut shown = 0;
                    for line in &self.log_lines {
                        let lvl = line_level(line);
                        if !passes(self.log_filter, lvl) {
                            continue;
                        }
                        shown += 1;
                        let color = match lvl {
                            Level::Error => theme::RED,
                            Level::Warn => theme::AMBER,
                            Level::Info => theme::DIM2,
                        };
                        ui.label(RichText::new(line).color(color).size(11.0).family(egui::FontFamily::Monospace));
                    }
                    if shown == 0 {
                        let msg = match self.log_filter {
                            LogFilter::All => "(no log yet — start mining)",
                            LogFilter::Warn => "(no warnings)",
                            LogFilter::Error => "(no errors)",
                        };
                        ui.label(RichText::new(msg).color(theme::DIM).size(11.0));
                    }
                });
        });
    }
}

// --- log helpers ------------------------------------------------------------

fn line_level(line: &str) -> Level {
    if line.contains("ERROR") {
        Level::Error
    } else if line.contains("WARN") {
        Level::Warn
    } else {
        Level::Info
    }
}

fn passes(filter: LogFilter, lvl: Level) -> bool {
    match filter {
        LogFilter::All => true,
        LogFilter::Warn => matches!(lvl, Level::Warn | Level::Error),
        LogFilter::Error => matches!(lvl, Level::Error),
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
    ui.label(RichText::new(text).color(theme::AMBER).size(12.0).strong());
    ui.add_space(6.0);
}

fn heading_inline(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::AMBER).size(12.0).strong());
    ui.add_space(8.0);
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

fn big(text: String, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(color).size(15.0).strong())
        .min_size(egui::vec2(140.0, 36.0))
        .stroke(egui::Stroke::new(1.0, color))
        .fill(theme::PANEL2)
}

fn big_button(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    ui.add(big(text.to_string(), color))
}

fn draw_sparkline(ui: &mut egui::Ui, history: &VecDeque<f32>) {
    let height = 100.0;
    let (rect, _r) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme::PANEL2);
    for i in 1..4 {
        let y = rect.top() + rect.height() * (i as f32 / 4.0);
        painter.hline(rect.left()..=rect.right(), y, egui::Stroke::new(1.0, theme::LINE));
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
    let x_at = |i: usize| rect.left() + pad + (rect.width() - 2.0 * pad) * (i as f32 / (n - 1) as f32);
    let y_at = |v: f32| rect.bottom() - pad - (rect.height() - 2.0 * pad) * (v / max).clamp(0.0, 1.0);
    let points: Vec<egui::Pos2> =
        history.iter().enumerate().map(|(i, &v)| egui::pos2(x_at(i), y_at(v))).collect();
    let mut poly = points.clone();
    poly.push(egui::pos2(x_at(n - 1), rect.bottom() - pad));
    poly.push(egui::pos2(x_at(0), rect.bottom() - pad));
    painter.add(egui::Shape::convex_polygon(poly, theme::GREEN.linear_multiply(0.10), egui::Stroke::NONE));
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.6, theme::GREEN)));
}

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
        assert!(valid_address(&normalize_address("0xABCDEF0123456789abcdef0123456789ABCDEF01")));
        assert!(!valid_address("tooshort"));
    }

    #[test]
    fn log_level_detection_and_filter() {
        assert_eq!(line_level("[gpu0] 2026 ERROR boom"), Level::Error);
        assert_eq!(line_level("[cpu] 2026  WARN careful"), Level::Warn);
        assert_eq!(line_level("[gpu0] 2026  INFO ok"), Level::Info);
        assert!(passes(LogFilter::All, Level::Info));
        assert!(!passes(LogFilter::Warn, Level::Info));
        assert!(passes(LogFilter::Warn, Level::Error));
        assert!(passes(LogFilter::Error, Level::Error));
        assert!(!passes(LogFilter::Error, Level::Warn));
    }
}
