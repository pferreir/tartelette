//! The window: a toolbar, the front panel, and a UART console.
use crate::files::{self, Inbox, Slot};
use crate::panel::{self, Layout, PanelState, Request};
use crate::session::{Clock, Session, Source};
use egui::{Color32, RichText};

const SPEEDS: &[(f64, &str)] = &[(0.01, "0.01×"), (0.1, "0.1×"), (0.25, "0.25×"), (0.5, "0.5×"), (1.0, "1×"), (2.0, "2×"), (4.0, "4×"), (f64::INFINITY, "max")];
const LINE_ENDINGS: &[(&str, &str)] = &[("\n", "LF"), ("\r", "CR"), ("\r\n", "CRLF"), ("", "none")];

pub struct App {
    firmware: Option<Source>,
    board: Option<Source>,
    session: Option<Session>,
    layout: Option<Layout>,
    panel: PanelState,
    clock: Clock,
    /// Problems to show: a run that could not start, ignored panel options.
    errors: Vec<String>,
    inbox: Inbox,
    uart_tab: usize,
    uart_line: String,
    line_ending: usize,
    #[cfg(not(target_arch = "wasm32"))]
    watch: crate::watch::Watch,
}

impl App {
    pub fn new(firmware: Option<Source>, board: Option<Source>) -> Self {
        let mut app = Self {
            firmware,
            board,
            session: None,
            layout: None,
            panel: PanelState::default(),
            clock: Clock::new(),
            errors: Vec::new(),
            inbox: Inbox::default(),
            uart_tab: 0,
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
        let cards = self.session.as_mut().map(Session::take_sd_cards).unwrap_or_default();
        self.session = None;
        self.layout = None;
        self.panel = PanelState::default();
        self.errors.clear();
        #[cfg(not(target_arch = "wasm32"))]
        self.watch.track(self.firmware.as_ref(), self.board.as_ref());
        let Some(fw) = &self.firmware else { return };
        match Session::new(fw, self.board.as_ref()) {
            Ok(mut s) => {
                for (name, image, _) in cards {
                    s.load_sd_card(&name, image);
                }
                let (layout, warnings) = Layout::new(&s.sys.board);
                self.errors = warnings;
                self.layout = Some(layout);
                self.session = Some(s);
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Load a disk image into the SD card part `name` of the running board.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_sd_card(&mut self, name: &str, image: Vec<u8>) -> bool {
        self.session.as_mut().is_some_and(|s| s.load_sd_card(name, image))
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
                    let Some(s) = &mut self.session else { continue };
                    // A dropped image goes to the first card.
                    let name = if name.is_empty() {
                        match s.sys.board.parts.iter().find(|p| p.kind == "sdcard") {
                            Some(p) => p.name.clone(),
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

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open firmware…").clicked() {
                files::pick(Slot::Firmware, &self.inbox, ui.ctx());
            }
            if ui.button("Open board…").clicked() {
                files::pick(Slot::Board, &self.inbox, ui.ctx());
            }
            ui.separator();
            let label = if self.clock.running { "Pause" } else { "Run" };
            if ui.add_enabled(self.session.is_some(), egui::Button::new(label)).clicked() {
                self.clock.running = !self.clock.running;
            }
            if ui.add_enabled(self.firmware.is_some(), egui::Button::new("Reset")).clicked() {
                self.restart();
            }
            let current = SPEEDS.iter().find(|s| s.0 == self.clock.speed).map_or("?", |s| s.1);
            egui::ComboBox::from_id_salt("speed").selected_text(current).width(60.0).show_ui(ui, |ui| {
                for &(v, name) in SPEEDS {
                    ui.selectable_value(&mut self.clock.speed, v, name);
                }
            });
            ui.separator();
            let names: Vec<&str> = [&self.firmware, &self.board].into_iter().flatten().map(|s| s.name.as_str()).collect();
            ui.label(RichText::new(names.join(" + ")).weak());
            if let Some(s) = &self.session {
                ui.separator();
                ui.monospace(format!("{:.6} s", s.seconds()));
                if let Some(r) = &s.halted {
                    ui.label(RichText::new(format!("halted: {r:?}")).color(ui.visuals().warn_fg_color));
                } else if self.clock.running {
                    ui.monospace(format!("{:5.2}× real time", self.clock.measured()));
                } else {
                    ui.label("paused");
                }
            }
        });
    }

    fn console(&mut self, ui: &mut egui::Ui) {
        let Some(s) = &mut self.session else { return };
        ui.horizontal(|ui| {
            for n in 0..2 {
                ui.selectable_value(&mut self.uart_tab, n, format!("UART{n}"));
            }
            if ui.small_button("Clear").clicked() {
                s.uart[self.uart_tab].clear();
            }
        });
        let n = self.uart_tab;
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

    fn apply(&mut self, requests: Vec<Request>, ctx: &egui::Context) {
        let Some(s) = &mut self.session else { return };
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

        if let Some(s) = &mut self.session {
            self.clock.tick(s);
            self.panel.absorb(&s.events);
            s.events.clear();
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            for e in &self.errors {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(2.0);
        });
        if self.session.is_some() {
            egui::Panel::bottom("console").resizable(true).default_size(160.0).show(ui, |ui| self.console(ui));
        }
        let requests = egui::CentralPanel::default()
            .show(ui, |ui| match (&self.session, &self.layout) {
                (Some(s), Some(layout)) => panel::show(ui, layout, &mut self.panel, s),
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("Drop a firmware ELF and a .board file here,\nor use the Open buttons above.").size(18.0).color(Color32::GRAY));
                    });
                    Vec::new()
                }
            })
            .inner;
        self.apply(requests, &ctx);

        let busy = self.session.as_ref().is_some_and(|s| s.halted.is_none()) && self.clock.running;
        if busy {
            ctx.request_repaint();
        } else {
            // Native: keep polling the files for changes.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}
