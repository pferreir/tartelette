//! The window: a toolbar, the front panel, and a UART console.
use crate::brand::Logo;
use crate::files::{self, Inbox, Slot};
use crate::midi::Midi;
use crate::panel::{self, Layout, PanelState, Request};
use crate::session::{self, Network, Run, Session, Shared, Source, lock};
use egui::{Color32, RichText};

const SPEEDS: &[(f64, &str)] = &[(0.01, "0.01×"), (0.1, "0.1×"), (0.25, "0.25×"), (0.5, "0.5×"), (1.0, "1×"), (2.0, "2×"), (4.0, "4×"), (f64::INFINITY, "max")];
/// The UART whose receiver is the firmware's MIDI in (GPIO1 on the BTL-01).
/// Tall enough to read the lettering.
const LOGO_HEIGHT: f32 = 96.0;
const MIDI_UART: usize = 0;
const LOG_TAB: usize = 2;
const LINE_ENDINGS: &[(&str, &str)] = &[("\n", "LF"), ("\r", "CR"), ("\r\n", "CRLF"), ("", "none")];

pub struct App {
    logo: Option<Logo>,
    firmware: Option<Source>,
    board: Option<Source>,
    net: Network,
    run: Option<Shared>,
    layout: Option<Layout>,
    panel: PanelState,
    /// Problems to show: a run that could not start, ignored panel options.
    errors: Vec<String>,
    inbox: Inbox,
    midi: Midi,
    /// 0 and 1: a UART; `LOG_TAB`: the firmware's log.
    console_tab: usize,
    log_min: rtt::Level,
    uart_line: String,
    line_ending: usize,
    #[cfg(not(target_arch = "wasm32"))]
    watch: crate::watch::Watch,
}

impl App {
    pub fn new(firmware: Option<Source>, board: Option<Source>, net: Network) -> Self {
        let mut app = Self {
            logo: None,
            firmware,
            board,
            net,
            run: None,
            layout: None,
            panel: PanelState::default(),
            errors: Vec::new(),
            inbox: Inbox::default(),
            midi: Midi::new(),
            console_tab: 0,
            log_min: rtt::Level::Trace,
            uart_line: String::new(),
            line_ending: 0,
            #[cfg(not(target_arch = "wasm32"))]
            watch: Default::default(),
        };
        app.restart();
        app
    }

    /// Power-cycle: a fresh run from the current firmware and board. SD
    /// cards keep their contents.
    fn restart(&mut self) {
        let old = self.run.take();
        let cards = old.as_ref().map(|r| lock(r).session.take_sd_cards()).unwrap_or_default();
        let (running, speed) = old.as_ref().map_or((true, 1.0), |r| {
            let g = lock(r);
            (g.clock.running, g.clock.speed)
        });
        if let Some(old) = old {
            session::stop(old);
        }
        self.layout = None;
        self.panel = PanelState::default();
        self.errors.clear();
        #[cfg(not(target_arch = "wasm32"))]
        self.watch.track(self.firmware.as_ref(), self.board.as_ref());
        let Some(fw) = &self.firmware else { return };
        match Session::new(fw, self.board.as_ref(), &self.net) {
            Ok(mut s) => {
                for (name, image, _) in cards {
                    s.load_sd_card(&name, image);
                }
                let (layout, warnings) = Layout::new(&s.sys.board);
                self.errors = warnings;
                self.layout = Some(layout);
                self.run = Some(session::start(s, running, speed));
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Load a disk image into the SD card part `name` of the running board.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_sd_card(&mut self, name: &str, image: Vec<u8>) -> bool {
        self.run.as_ref().is_some_and(|r| lock(r).session.load_sd_card(name, image))
    }

    fn take_files(&mut self) {
        let mut restart = false;
        for (slot, src) in self.inbox.take() {
            let slot = if slot == Slot::Guess { files::guess(&src.name) } else { slot };
            match slot {
                Slot::Firmware => {
                    self.firmware = Some(src);
                    restart = true;
                }
                Slot::Board => {
                    self.board = Some(src);
                    restart = true;
                }
                Slot::Sd(name) => {
                    let Some(run) = self.run.clone() else { continue };
                    let mut g = lock(&run);
                    let s = &mut g.session;
                    // A dropped image goes to the first card.
                    let name = if name.is_empty() {
                        match s.sys.board.devices_mut::<pcb_devices::sdcard::SdCard>().next() {
                            Some((name, _)) => name.to_string(),
                            None => {
                                self.errors.push(format!("{}: the board has no SD card", src.name));
                                continue;
                            }
                        }
                    } else {
                        name
                    };
                    s.load_sd_card(&name, src.bytes);
                }
                Slot::Guess => unreachable!(),
            }
        }
        if restart {
            self.restart();
        }
    }

    /// Returns whether Reset was pressed.
    fn toolbar(&mut self, ui: &mut egui::Ui, mut run: Option<&mut Run>) -> bool {
        let mut reset = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open firmware…").clicked() {
                files::pick(Slot::Firmware, &self.inbox, ui.ctx());
            }
            if ui.button("Open board…").clicked() {
                files::pick(Slot::Board, &self.inbox, ui.ctx());
            }
            ui.separator();
            let label = if run.as_deref().is_some_and(|r| r.clock.running) { "Pause" } else { "Run" };
            if ui.add_enabled(run.is_some(), egui::Button::new(label)).clicked()
                && let Some(r) = run.as_deref_mut()
            {
                r.clock.running = !r.clock.running;
            }
            if ui.add_enabled(self.firmware.is_some(), egui::Button::new("Reset")).clicked() {
                reset = true;
            }
            if let Some(r) = run.as_deref_mut() {
                let current = SPEEDS.iter().find(|s| s.0 == r.clock.speed).map_or("?", |s| s.1);
                egui::ComboBox::from_id_salt("speed").selected_text(current).width(60.0).show_ui(ui, |ui| {
                    for &(v, name) in SPEEDS {
                        ui.selectable_value(&mut r.clock.speed, v, name);
                    }
                });
            }
            ui.separator();
            self.midi.ui(ui);
            ui.separator();
            let names: Vec<&str> = [&self.firmware, &self.board].into_iter().flatten().map(|s| s.name.as_str()).collect();
            ui.label(RichText::new(names.join(" + ")).weak());
            if let Some(Run { session: s, clock }) = run.as_deref() {
                ui.separator();
                ui.monospace(format!("{:.6} s", s.seconds()));
                if let Some(r) = &s.halted {
                    ui.label(RichText::new(format!("halted: {r:?}")).color(ui.visuals().warn_fg_color));
                } else if clock.running {
                    ui.monospace(format!("{:5.2}× real time", clock.measured()));
                } else {
                    ui.label("paused");
                }
            }
        });
        reset
    }

    fn console(&mut self, ui: &mut egui::Ui, s: &mut Session) {
        ui.horizontal(|ui| {
            for n in 0..2 {
                ui.selectable_value(&mut self.console_tab, n, format!("UART{n}"));
            }
            ui.selectable_value(&mut self.console_tab, LOG_TAB, "Log");
            if ui.small_button("Clear").clicked() {
                match self.console_tab {
                    LOG_TAB => s.log.clear(),
                    n => s.uart[n].clear(),
                }
            }
            if self.console_tab == LOG_TAB {
                egui::ComboBox::from_id_salt("log-level").selected_text(format!("≥ {}", self.log_min.name())).width(80.0).show_ui(ui, |ui| {
                    for l in [rtt::Level::Trace, rtt::Level::Debug, rtt::Level::Info, rtt::Level::Warn, rtt::Level::Error] {
                        ui.selectable_value(&mut self.log_min, l, l.name());
                    }
                });
            }
        });
        if self.console_tab == LOG_TAB {
            self.log_view(ui, s);
            return;
        }
        let n = self.console_tab;
        ui.horizontal(|ui| {
            let send = ui.add(egui::TextEdit::singleline(&mut self.uart_line).hint_text("send to the firmware…").desired_width(ui.available_width() - 90.0));
            egui::ComboBox::from_id_salt("eol").selected_text(LINE_ENDINGS[self.line_ending].1).width(60.0).show_ui(ui, |ui| {
                for (i, e) in LINE_ENDINGS.iter().enumerate() {
                    ui.selectable_value(&mut self.line_ending, i, e.1);
                }
            });
            if send.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let line = std::mem::take(&mut self.uart_line) + LINE_ENDINGS[self.line_ending].0;
                s.send(n, line.as_bytes());
                send.request_focus();
            }
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(&s.uart[n]).monospace()).wrap());
        });
    }

    /// The firmware's defmt log: one truncated line per message (the whole
    /// message on hover), at least as severe as `log_min`.
    fn log_view(&self, ui: &mut egui::Ui, s: &Session) {
        let shown: Vec<&session::LogEntry> = s.log.iter().filter(|e| e.line.level.is_none_or(|l| l >= self.log_min)).collect();
        if shown.is_empty() {
            let note = s.log_note.as_deref().unwrap_or("nothing logged yet");
            ui.label(RichText::new(note).weak());
            return;
        }
        let row = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show_rows(ui, row, shown.len(), |ui, rows| {
            for e in &shown[rows] {
                ui.add(egui::Label::new(log_job(ui, e)).truncate()).on_hover_text(&e.line.text);
            }
        });
    }

    fn apply(&mut self, requests: Vec<Request>, ctx: &egui::Context, s: Option<&mut Session>) {
        let Some(s) = s else { return };
        for r in requests {
            match r {
                Request::Input(part, action, value) => {
                    s.input(&part, &action, value);
                }
                Request::LoadSd(name) => files::pick(Slot::Sd(name), &self.inbox, ctx),
                Request::SaveSd(name) => {
                    if let Some((_, image, _)) = s.take_sd_cards().into_iter().find(|c| c.0 == name) {
                        files::save(&format!("{name}.img"), image);
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        files::take_dropped(&ctx, &self.inbox);
        #[cfg(not(target_arch = "wasm32"))]
        if self.watch.changed(&mut self.firmware, &mut self.board) {
            self.restart();
        }
        self.take_files();

        let run = self.run.clone();
        let mut guard = run.as_ref().map(lock);
        #[cfg(target_arch = "wasm32")]
        if let Some(Run { session, clock }) = guard.as_deref_mut() {
            clock.tick(session, session::FRAME_BUDGET);
        }
        // Real MIDI goes to the firmware only while it runs; otherwise it is dropped.
        let midi = self.midi.take();
        if let Some(r) = guard.as_deref_mut().filter(|r| r.clock.running && !midi.is_empty()) {
            r.session.send(MIDI_UART, &midi);
        }
        if let Some(r) = guard.as_deref_mut() {
            self.panel.absorb(&r.session.events, ctx.input(|i| i.time));
            r.session.events.clear();
        }

        let mut reset = false;
        let logo = self.logo.get_or_insert_with(|| Logo::new(&ctx));
        egui::Panel::left("logo").resizable(false).show_separator_line(false).show(ui, |ui| {
            ui.add(logo.image(LOGO_HEIGHT));
        });
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            reset = self.toolbar(ui, guard.as_deref_mut());
            for e in &self.errors {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(2.0);
        });
        if let Some(r) = guard.as_deref_mut() {
            egui::Panel::bottom("console").resizable(true).default_size(160.0).show(ui, |ui| self.console(ui, &mut r.session));
        }
        let requests = egui::CentralPanel::default()
            .show(ui, |ui| match (guard.as_deref(), &self.layout) {
                (Some(r), Some(layout)) => panel::show(ui, layout, &mut self.panel, &r.session),
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Drop a firmware ELF and a .board file here,\nor use the Open buttons above.").size(18.0).color(Color32::GRAY));
                    });
                    Vec::new()
                }
            })
            .inner;
        self.apply(requests, &ctx, guard.as_deref_mut().map(|r| &mut r.session));

        let busy = guard.as_deref().is_some_and(|r| r.session.halted.is_none() && r.clock.running);
        drop(guard);
        if reset {
            self.restart();
        }
        if busy {
            ctx.request_repaint();
        } else {
            // Native: keep polling the files for changes.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

/// `time  LEVEL  message`, the level in its colour.
fn log_job(ui: &egui::Ui, e: &session::LogEntry) -> egui::text::LayoutJob {
    use egui::{FontId, TextFormat, text::LayoutJob};
    let v = ui.visuals();
    let font = FontId::monospace(12.0);
    let fmt = |color| TextFormat { font_id: font.clone(), color, ..Default::default() };
    let (name, color) = match e.line.level {
        Some(rtt::Level::Error) => ("ERROR", v.error_fg_color),
        Some(rtt::Level::Warn) => ("WARN ", v.warn_fg_color),
        Some(rtt::Level::Info) => ("INFO ", Color32::from_rgb(40, 200, 90)),
        Some(rtt::Level::Debug) => ("DEBUG", Color32::from_rgb(90, 160, 255)),
        Some(rtt::Level::Trace) => ("TRACE", v.weak_text_color()),
        None => ("     ", v.text_color()),
    };
    let mut job = LayoutJob::default();
    job.append(&format!("{:9.3} ", e.at), 0.0, fmt(v.weak_text_color()));
    job.append(&format!("{name} "), 0.0, fmt(color));
    job.append(&e.line.text, 0.0, fmt(v.text_color()));
    job
}
