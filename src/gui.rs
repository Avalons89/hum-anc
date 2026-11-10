use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::audio::{self, Engine, MASK_CEILING_DB, Masking, StatusSlot, Tuning};
use crate::autostart;
use crate::dsp::NoiseColor;
use crate::tray::{Command, Controls, HOTKEY_LABEL};

const MIN_OFFSET_DB: f32 = -40.0;
const MAX_OFFSET_DB: f32 = 80.0;
const MIN_GAIN_DB: f32 = -40.0;
const MAX_GAIN_DB: f32 = 40.0;
const QUIETEST_MAX_DB: f32 = -60.0;
const BAR_FLOOR_DB: f32 = -100.0;
const BAR_TOP_DB: f32 = -20.0;
const BAR_WIDTH: f32 = 240.0;
/// Leaves room for the value box so sliders end where the level bars do.
const SLIDER_WIDTH: f32 = 170.0;
const COMBO_WIDTH: f32 = 280.0;
const LIVE_REFRESH: Duration = Duration::from_millis(100);
/// 30 s of history at one point per LIVE_REFRESH.
const GRAPH_POINTS: usize = 300;
const GRAPH_HEIGHT: f32 = 80.0;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Mode {
    Masking,
    Hum,
}

/// Everything the window remembers between launches.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    mic: String,
    output: String,
    mode: Mode,
    offset_db: f32,
    max_db: f32,
    color: NoiseColor,
    hum_hz: f32,
    phase_deg: f32,
    gain_db: f32,
    start_audio_on_open: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: String::new(),
            output: String::new(),
            mode: Mode::Masking,
            offset_db: 0.0,
            max_db: MASK_CEILING_DB,
            color: NoiseColor::Brown,
            hum_hz: 100.0,
            phase_deg: 180.0,
            gain_db: 0.0,
            start_audio_on_open: false,
        }
    }
}

type ScanResult = Result<Vec<(f32, f32)>, String>;

pub struct App {
    settings: Settings,
    mics: Vec<String>,
    outputs: Vec<String>,
    masking: Arc<Mutex<Masking>>,
    tuning: Arc<Mutex<Tuning>>,
    status: StatusSlot,
    engine: Option<Engine>,
    scan: Option<Receiver<ScanResult>>,
    tones: Vec<(f32, f32)>,
    controls: Option<Controls>,
    window_visible: bool,
    /// eframe shows the window after its first frame regardless, so a tray start hides it again.
    hide_after_first_frame: bool,
    quitting: bool,
    options_open: bool,
    start_with_windows: bool,
    /// (background, masking) in dBFS, oldest first.
    history: VecDeque<(f32, f32)>,
    last_history_point: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start_hidden: bool) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, eframe::APP_KEY))
            .unwrap_or_default();
        let mics = audio::mic_names();
        let outputs = audio::output_names();
        if !mics.contains(&settings.mic) {
            settings.mic = audio::default_mic_name().unwrap_or_default();
        }
        if !outputs.contains(&settings.output) {
            settings.output = audio::default_output_name().unwrap_or_default();
        }
        let status = StatusSlot::default();
        let controls = match Controls::new(&cc.egui_ctx) {
            Ok(controls) => {
                *status.lock().unwrap() = controls.hotkey_problem.clone();
                Some(controls)
            }
            Err(err) => {
                *status.lock().unwrap() = Some(format!("tray icon unavailable: {err}"));
                None
            }
        };
        // Without a tray icon a hidden window could never be reopened.
        let window_visible = !start_hidden || controls.is_none();
        if window_visible && start_hidden {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        let mut app = Self {
            settings,
            mics,
            outputs,
            masking: Arc::default(),
            tuning: Arc::default(),
            status,
            engine: None,
            scan: None,
            tones: Vec::new(),
            controls,
            window_visible,
            hide_after_first_frame: !window_visible,
            quitting: false,
            options_open: false,
            start_with_windows: autostart::is_enabled(),
            history: VecDeque::with_capacity(GRAPH_POINTS),
            last_history_point: Instant::now(),
        };
        if app.settings.start_audio_on_open {
            app.toggle_audio();
        }
        app
    }

    fn handle_commands(&mut self, ctx: &egui::Context) {
        let Some(controls) = &self.controls else {
            return;
        };
        let commands: Vec<Command> = controls.commands.try_iter().collect();
        for command in commands {
            match command {
                Command::ToggleAudio => self.toggle_audio(),
                Command::ToggleWindow => self.set_window_visible(ctx, !self.window_visible),
                Command::ShowWindow => self.set_window_visible(ctx, true),
                Command::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn finish_hidden_start(&mut self, ctx: &egui::Context) {
        if !self.hide_after_first_frame {
            return;
        }
        if ctx.cumulative_frame_nr() == 0 {
            ctx.request_repaint();
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.hide_after_first_frame = false;
    }

    fn set_window_visible(&mut self, ctx: &egui::Context, visible: bool) {
        self.window_visible = visible;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// The close button hides to the tray; only the tray's Quit exits. Without a tray, close as usual.
    fn hide_to_tray_on_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        if close_requested && self.controls.is_some() && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.set_window_visible(ctx, false);
        }
    }

    fn push_controls_to_audio(&self) {
        {
            let mut masking = self.masking.lock().unwrap();
            masking.offset_db = self.settings.offset_db;
            masking.max_db = self.settings.max_db;
            masking.color = self.settings.color;
        }
        let mut tuning = self.tuning.lock().unwrap();
        tuning.phase_deg = self.settings.phase_deg;
        tuning.gain_db = self.settings.gain_db;
    }

    fn record_history(&mut self) {
        let masking_runs = self.engine.is_some() && self.settings.mode == Mode::Masking;
        if !masking_runs || self.last_history_point.elapsed() < LIVE_REFRESH {
            return;
        }
        self.last_history_point = Instant::now();
        let point = {
            let masking = self.masking.lock().unwrap();
            (masking.room_db, masking.level_db())
        };
        if self.history.len() == GRAPH_POINTS {
            self.history.pop_front();
        }
        self.history.push_back(point);
    }

    fn toggle_audio(&mut self) {
        if self.engine.take().is_none() {
            self.start_audio();
        }
        if let Some(controls) = &self.controls {
            controls.show_running(self.engine.is_some());
        }
    }

    fn start_audio(&mut self) {
        self.show_problem(None);
        self.push_controls_to_audio();
        self.history.clear();
        let settings = &self.settings;
        let started = match settings.mode {
            Mode::Masking => audio::start_masking(
                &settings.mic,
                &settings.output,
                Arc::clone(&self.masking),
                Arc::clone(&self.status),
            ),
            Mode::Hum => audio::start_hum(
                &settings.mic,
                &settings.output,
                settings.hum_hz,
                Arc::clone(&self.tuning),
                Arc::clone(&self.status),
            ),
        };
        match started {
            Ok(engine) => self.engine = Some(engine),
            Err(problem) => self.show_problem(Some(problem.to_string())),
        }
    }

    fn start_scan(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let mic = self.settings.mic.clone();
        thread::spawn(move || {
            let _ = sender.send(audio::scan_for_hum(&mic).map_err(|err| err.to_string()));
        });
        self.show_problem(None);
        self.tones.clear();
        self.scan = Some(receiver);
    }

    fn collect_scan(&mut self) {
        let Some(receiver) = &self.scan else {
            return;
        };
        match receiver.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(Ok(tones)) => {
                if let Some(&(clearest_hz, _)) = tones.first() {
                    self.settings.hum_hz = clearest_hz;
                }
                self.tones = tones;
            }
            Ok(Err(problem)) => self.show_problem(Some(problem)),
            Err(TryRecvError::Disconnected) => {}
        }
        self.scan = None;
    }

    fn show_problem(&self, problem: Option<String>) {
        *self.status.lock().unwrap() = problem;
    }

    fn device_pickers(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("devices").num_columns(2).show(ui, |ui| {
            ui.label("Mic");
            device_combo(ui, "mic", &mut self.settings.mic, &self.mics);
            ui.end_row();
            ui.label("Output");
            device_combo(ui, "output", &mut self.settings.output, &self.outputs);
            ui.end_row();
        });
    }

    /// Mode tabs lock while audio runs; Options stays reachable.
    fn tab_row(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!running, |ui| {
                for (mode, label) in [(Mode::Masking, "Masking"), (Mode::Hum, "Hum")] {
                    let selected = !self.options_open && self.settings.mode == mode;
                    if ui.selectable_label(selected, label).clicked() {
                        self.settings.mode = mode;
                        self.options_open = false;
                    }
                }
            });
            ui.toggle_value(&mut self.options_open, "Options");
        });
    }

    fn masking_tab(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = SLIDER_WIDTH;
        let live = self.engine.is_some();
        let (room_db, masking_db) = {
            let masking = self.masking.lock().unwrap();
            (masking.room_db, masking.level_db())
        };
        egui::Grid::new("masking").num_columns(2).show(ui, |ui| {
            ui.label("Background");
            level_bar(ui, room_db, live);
            ui.end_row();
            ui.label("Masking");
            level_bar(ui, masking_db, live);
            ui.end_row();
            ui.label("Offset");
            ui.add(
                egui::Slider::new(&mut self.settings.offset_db, MIN_OFFSET_DB..=MAX_OFFSET_DB)
                    .suffix(" dB")
                    .fixed_decimals(1),
            );
            ui.end_row();
            ui.label("Max volume");
            ui.add(
                egui::Slider::new(&mut self.settings.max_db, QUIETEST_MAX_DB..=MASK_CEILING_DB)
                    .suffix(" dBFS")
                    .fixed_decimals(0),
            );
            ui.end_row();
            ui.label("Noise");
            ui.horizontal(|ui| {
                let color = &mut self.settings.color;
                ui.selectable_value(color, NoiseColor::Brown, "Brown");
                ui.selectable_value(color, NoiseColor::Pink, "Pink");
                ui.selectable_value(color, NoiseColor::White, "White");
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        level_graph(ui, &self.history);
    }

    fn hum_tab(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().slider_width = SLIDER_WIDTH;
        let running = self.engine.is_some();
        let scanning = self.scan.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running && !scanning, egui::Button::new("Scan"))
                .clicked()
            {
                self.start_scan();
            }
            if scanning {
                ui.spinner();
                ui.label("listening for 3 s");
            } else if !self.tones.is_empty() {
                ui.weak("dB above the surrounding noise; click one to use it");
            }
        });
        ui.horizontal_wrapped(|ui| {
            for &(hz, db) in &self.tones {
                let tone = egui::Button::new(format!("{hz:.1} Hz (+{db:.0} dB)"));
                if ui.add_enabled(!running, tone).clicked() {
                    self.settings.hum_hz = hz;
                }
            }
        });
        egui::Grid::new("hum").num_columns(2).show(ui, |ui| {
            ui.label("Frequency");
            ui.add_enabled(
                !running,
                egui::DragValue::new(&mut self.settings.hum_hz)
                    .range(20.0..=2000.0)
                    .speed(0.1)
                    .suffix(" Hz"),
            );
            ui.end_row();
            ui.label("Phase");
            ui.add(
                egui::Slider::new(&mut self.settings.phase_deg, 0.0..=360.0)
                    .suffix(" deg")
                    .fixed_decimals(0),
            );
            ui.end_row();
            ui.label("Gain");
            ui.add(
                egui::Slider::new(&mut self.settings.gain_db, MIN_GAIN_DB..=MAX_GAIN_DB)
                    .suffix(" dB")
                    .fixed_decimals(1),
            );
            ui.end_row();
        });
    }

    fn options_tab(&mut self, ui: &mut egui::Ui) {
        let mut start_with_windows = self.start_with_windows;
        if ui
            .checkbox(
                &mut start_with_windows,
                "Start with Windows, hidden in the tray",
            )
            .changed()
        {
            match autostart::set(start_with_windows) {
                Ok(()) => self.start_with_windows = start_with_windows,
                Err(err) => {
                    self.show_problem(Some(format!("couldn't change Windows startup: {err}")))
                }
            }
        }
        ui.checkbox(
            &mut self.settings.start_audio_on_open,
            "Start audio when hum-anc opens",
        );
    }
}

impl eframe::App for App {
    /// Runs every frame, and also while the window is hidden in the tray.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.finish_hidden_start(ctx);
        self.collect_scan();
        self.push_controls_to_audio();
        self.handle_commands(ctx);
        self.hide_to_tray_on_close(ctx);
        self.record_history();
        if self.window_visible && (self.engine.is_some() || self.scan.is_some()) {
            ctx.request_repaint_after(LIVE_REFRESH);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            let running = self.engine.is_some();
            ui.add_enabled_ui(!running, |ui| self.device_pickers(ui));
            self.tab_row(ui, running);
            ui.separator();
            if self.options_open {
                self.options_tab(ui);
            } else {
                match self.settings.mode {
                    Mode::Masking => self.masking_tab(ui),
                    Mode::Hum => self.hum_tab(ui),
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(if running { "Stop" } else { "Start" }).clicked() {
                    self.toggle_audio();
                }
                if self.controls.is_some() {
                    ui.weak(format!(
                        "{HOTKEY_LABEL} anywhere; closing keeps it in the tray"
                    ));
                }
            });
            let problem = self.status.lock().unwrap().clone();
            if let Some(problem) = problem {
                ui.colored_label(ui.visuals().warn_fg_color, problem);
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

fn device_combo(ui: &mut egui::Ui, id: &str, selected: &mut String, names: &[String]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected.clone())
        .width(COMBO_WIDTH)
        .show_ui(ui, |ui| {
            for name in names {
                ui.selectable_value(selected, name.clone(), name.as_str());
            }
        });
}

fn level_bar(ui: &mut egui::Ui, db: f32, live: bool) {
    if !live {
        ui.weak("stopped");
        return;
    }
    ui.add(
        egui::ProgressBar::new(bar_fraction(db))
            .desired_width(BAR_WIDTH)
            .text(format!("{db:.0} dBFS")),
    );
}

fn bar_fraction(db: f32) -> f32 {
    ((db - BAR_FLOOR_DB) / (BAR_TOP_DB - BAR_FLOOR_DB)).clamp(0.0, 1.0)
}

/// Last 30 s of background and masking levels, newest at the right edge.
fn level_graph(ui: &mut egui::Ui, history: &VecDeque<(f32, f32)>) {
    let background_color = ui.visuals().weak_text_color();
    let masking_color = ui.visuals().selection.bg_fill;
    let size = egui::vec2(ui.available_width(), GRAPH_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(2),
        ui.visuals().extreme_bg_color,
    );
    let step = rect.width() / (GRAPH_POINTS - 1) as f32;
    let newest = history.len().saturating_sub(1);
    let line = |level: fn(&(f32, f32)) -> f32| -> Vec<egui::Pos2> {
        history
            .iter()
            .enumerate()
            .map(|(i, point)| {
                let x = rect.right() - (newest - i) as f32 * step;
                egui::pos2(
                    x,
                    rect.bottom() - bar_fraction(level(point)) * rect.height(),
                )
            })
            .collect()
    };
    painter.line(line(|p| p.0), egui::Stroke::new(1.5, background_color));
    painter.line(line(|p| p.1), egui::Stroke::new(1.5, masking_color));
    ui.horizontal(|ui| {
        ui.colored_label(background_color, "background");
        ui.colored_label(masking_color, "masking");
        ui.weak("last 30 s");
    });
}
