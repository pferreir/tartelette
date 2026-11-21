//! The window: a top bar (logo, controls, status), the front panel in the
//! middle, and a dock of tabs at the bottom or on the right: the serial
//! consoles, the firmware's log, USB, the synth, SD card browsers and the
//! problems found.
use egui::{Color32, RichText};

use crate::{
    brand::Logo,
    files::{self, Inbox, Slot},
    midi::Midi,
    panel::{self, Layout, PanelState, Request},
    sdbrowse::{SdBrowser, Want},
    session::{self, Network, Run, Session, Shared, Source, lock},
    sound::Sound,
};

const SPEEDS: &[(f64, &str)] = &[
    (0.01, "0.01×"),
    (0.1, "0.1×"),
    (0.25, "0.25×"),
    (0.5, "0.5×"),
    (1.0, "1×"),
    (2.0, "2×"),
    (4.0, "4×"),
    (f64::INFINITY, "max"),
];
/// Tall enough to read the lettering, as tall as the top bar's two rows.
const LOGO_HEIGHT: f32 = 64.0;
/// The UART whose receiver is the firmware's MIDI in (GPIO1 on the BTL-01).
const MIDI_UART: usize = 0;
const LINE_ENDINGS: &[(&str, &str)] = &[("\n", "LF"), ("\r", "CR"), ("\r\n", "CRLF"), ("", "none")];

/// A tab of the dock.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tab {
    Uart(usize),
    UsbSerial,
    /// USB devices and traffic.
    Usb,
    Log,
    Synth,
    /// The browser of the SD card part with this name.
    Sd(String),
    Problems,
}

/// Where the dock sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockSide {
    Bottom,
    Right,
}

pub struct App {
    logo: Option<Logo>,
    firmware: Option<Source>,
    board: Option<Source>,
    /// Add-on boards on top of `board`, from the command line.
    shields: Vec<Source>,
    net: Network,
    run: Option<Shared>,
    layout: Option<Layout>,
    panel: PanelState,
    /// Problems to show: a run that could not start, ignored panel options,
    /// warnings from the run.
    errors: Vec<String>,
    /// How many of `errors` the Problems tab has shown.
    errors_seen: usize,
    inbox: Inbox,
    midi: Midi,
    tab: Tab,
    dock: DockSide,
    log_min: rtt::Level,
    uart_line: String,
    line_ending: usize,
    #[cfg(not(target_arch = "wasm32"))]
    watch: crate::watch::Watch,
    /// The open SD card browsers, a tab each.
    sd_browsers: Vec<SdBrowser>,
    /// Test oscillators on the board's outputs.
    sound: Sound,
    /// SD cards written through to a file (`--sd-live`): part name, file.
    #[cfg(not(target_arch = "wasm32"))]
    sd_files: Vec<(String, std::path::PathBuf)>,
}

/// The default proportional font lacks arrows and shapes (→, ●, ↻) that the
/// monospace one has; without the emoji fonts they'd render as tofu.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(p) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        p.insert(1, "Hack".to_owned());
    }
    ctx.set_fonts(fonts);
}

impl App {
    pub fn new(
        firmware: Option<Source>,
        board: Option<Source>,
        shields: Vec<Source>,
        net: Network,
    ) -> Self {
        let mut app = Self {
            logo: None,
            firmware,
            board,
            shields,
            net,
            run: None,
            layout: None,
            panel: PanelState::default(),
            errors: Vec::new(),
            errors_seen: 0,
            inbox: Inbox::default(),
            midi: Midi::new(),
            tab: Tab::Uart(0),
            dock: DockSide::Bottom,
            log_min: rtt::Level::Trace,
            uart_line: String::new(),
            line_ending: 0,
            #[cfg(not(target_arch = "wasm32"))]
            watch: Default::default(),
            sd_browsers: Vec::new(),
            sound: Sound::new(Vec::new()),
            #[cfg(not(target_arch = "wasm32"))]
            sd_files: Vec::new(),
        };
        app.restart();
        app
    }

    /// Power-cycle: a fresh run from the current firmware and board. SD
    /// cards keep their contents.
    fn restart(&mut self) {
        let old = self.run.take();
        let cards = old
            .as_ref()
            .map(|r| lock(r).session.take_sd_cards())
            .unwrap_or_default();
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
        self.errors_seen = 0;
        #[cfg(not(target_arch = "wasm32"))]
        self.watch
            .track(self.firmware.iter().chain(&self.board).chain(&self.shields));
        let Some(fw) = &self.firmware else { return };
        match Session::new(fw, self.board.as_ref(), &self.shields, &self.net) {
            Ok(mut s) => {
                for (name, image, _) in cards {
                    s.load_sd_card(&name, image);
                }
                #[cfg(not(target_arch = "wasm32"))]
                self.sd_files
                    .retain(|(name, path)| match s.write_sd_card_to(name, path) {
                        Ok(()) => true,
                        Err(e) => {
                            self.errors.push(format!("--sd-live: {e}"));
                            false
                        }
                    });
                self.sound.attach(&mut s);
                let (layout, warnings) = Layout::new(&s.sys.board);
                self.errors = warnings;
                self.layout = Some(layout);
                self.run = Some(session::start(s, running, speed));
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Start with the voices `--osc` and `--drums` ask for, on the Synth
    /// tab (without either, the board's default voice).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn set_voices(&mut self, args: &crate::cli::RunArgs) {
        if args.osc.is_empty() && !args.drums {
            return;
        }
        self.tab = Tab::Synth;
        let Some(r) = &self.run else {
            self.sound = Sound::new(args.osc.clone());
            return;
        };
        let mut g = lock(r);
        self.sound = Sound::new(args.patches(&g.session.sys.board));
        self.sound.attach(&mut g.session);
    }

    /// Load a disk image into the SD card part `name` of the running board.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load_sd_card(&mut self, name: &str, image: Vec<u8>) -> bool {
        self.run
            .as_ref()
            .is_some_and(|r| lock(r).session.load_sd_card(name, image))
    }

    /// Write what the firmware writes to the SD card part `name` to `path`
    /// as well, from now on and across restarts.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn write_sd_card_to(&mut self, name: &str, path: &std::path::Path) -> Result<(), String> {
        if let Some(r) = &self.run {
            lock(r).session.write_sd_card_to(name, path)?;
        }
        self.sd_files.push((name.into(), path.into()));
        Ok(())
    }

    fn take_files(&mut self) {
        let mut restart = false;
        for (slot, src) in self.inbox.take() {
            let slot = if slot == Slot::Guess {
                files::guess(&src.name)
            } else {
                slot
            };
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
                    let Some(run) = self.run.clone() else {
                        continue;
                    };
                    let mut g = lock(&run);
                    let s = &mut g.session;
                    // A dropped image goes to the first card.
                    let name = if name.is_empty() {
                        match s
                            .sys
                            .board
                            .devices_mut::<pcb_devices::sdcard::SdCard>()
                            .next()
                        {
                            Some((name, _)) => name.to_string(),
                            None => {
                                self.errors
                                    .push(format!("{}: the board has no SD card", src.name));
                                continue;
                            }
                        }
                    } else {
                        name
                    };
                    // The card now holds another disk than its file.
                    #[cfg(not(target_arch = "wasm32"))]
                    self.sd_files.retain(|(n, _)| *n != name);
                    s.load_sd_card(&name, src.bytes);
                }
                Slot::Guess => unreachable!(),
            }
        }
        if restart {
            self.restart();
        }
    }

    /// The controls and the status line beside the logo. Returns whether
    /// Reset was pressed.
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
            let running = run.as_deref().is_some_and(|r| r.clock.running);
            let label = if running { "⏸ Pause" } else { "▶ Run" };
            if ui
                .add_enabled(run.is_some(), egui::Button::new(label))
                .on_hover_text("Pause or resume the emulation")
                .clicked()
                && let Some(r) = run.as_deref_mut()
            {
                r.clock.running = !r.clock.running;
            }
            if ui
                .add_enabled(self.firmware.is_some(), egui::Button::new("↻ Reset"))
                .on_hover_text("Power-cycle the board (SD cards keep their contents)")
                .clicked()
            {
                reset = true;
            }
            if let Some(r) = run.as_deref_mut() {
                let current = SPEEDS
                    .iter()
                    .find(|s| s.0 == r.clock.speed)
                    .map_or("?", |s| s.1);
                egui::ComboBox::from_id_salt("speed")
                    .selected_text(current)
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for &(v, name) in SPEEDS {
                            ui.selectable_value(&mut r.clock.speed, v, name);
                        }
                    })
                    .response
                    .on_hover_text("Emulation speed");
            }
            ui.separator();
            self.midi.ui(ui);
        });
        ui.horizontal_wrapped(|ui| {
            let names: Vec<&str> = self
                .firmware
                .iter()
                .chain(&self.board)
                .chain(&self.shields)
                .map(|s| s.name.as_str())
                .collect();
            if names.is_empty() {
                ui.label(RichText::new("no firmware loaded").weak());
            } else {
                ui.label(RichText::new(names.join(" + ")).weak());
            }
            if let Some(Run { session: s, clock }) = run.as_deref() {
                ui.separator();
                ui.monospace(format!("{:.6} s", s.seconds()));
                if let Some(r) = &s.halted {
                    ui.label(
                        RichText::new(format!("halted: {r}")).color(ui.visuals().warn_fg_color),
                    );
                } else if clock.running {
                    ui.monospace(format!("{:5.2}× real time", clock.measured()));
                } else {
                    ui.label("paused");
                }
            }
            let fresh = self.errors.len() - self.errors_seen;
            if fresh > 0 {
                ui.separator();
                let badge = RichText::new(format!("⚠ {fresh} new problem{}", plural(fresh)))
                    .color(ui.visuals().warn_fg_color);
                if ui
                    .add(egui::Button::new(badge).frame(false))
                    .on_hover_text("Show them")
                    .clicked()
                {
                    self.tab = Tab::Problems;
                }
            }
        });
        reset
    }

    /// The dock: a strip of tabs, then the chosen one.
    fn dock(&mut self, ui: &mut egui::Ui, s: Option<&mut Session>) {
        let mut tabs = vec![
            (Tab::Uart(0), "UART0".to_string()),
            (Tab::Uart(1), "UART1".to_string()),
            (Tab::UsbSerial, "USB serial".to_string()),
            (Tab::Usb, "USB".to_string()),
            (Tab::Log, "Log".to_string()),
            (Tab::Synth, "Synth".to_string()),
        ];
        tabs.extend(
            self.sd_browsers
                .iter()
                .map(|b| (Tab::Sd(b.part.clone()), format!("SD {}", b.part))),
        );
        let problems = match self.errors.len() {
            0 => "Problems".to_string(),
            n => format!("Problems ({n})"),
        };
        tabs.push((Tab::Problems, problems));
        if !tabs.iter().any(|t| t.0 == self.tab) {
            self.tab = Tab::Uart(0);
        }
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            for (tab, name) in tabs {
                let selected = self.tab == tab;
                let mut text = RichText::new(name);
                if tab == Tab::Problems && self.errors.len() > self.errors_seen {
                    text = text.color(ui.visuals().warn_fg_color);
                }
                if ui.selectable_label(selected, text).clicked() {
                    self.tab = tab.clone();
                }
                if let Tab::Sd(part) = &tab
                    && ui
                        .small_button("×")
                        .on_hover_text("Close this browser")
                        .clicked()
                {
                    close = Some(part.clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (icon, tip) = match self.dock {
                    DockSide::Bottom => ("→ Dock right", "Move the tabs to the right"),
                    DockSide::Right => ("↓ Dock below", "Move the tabs to the bottom"),
                };
                if ui.small_button(icon).on_hover_text(tip).clicked() {
                    self.dock = match self.dock {
                        DockSide::Bottom => DockSide::Right,
                        DockSide::Right => DockSide::Bottom,
                    };
                }
            });
        });
        if let Some(part) = close {
            self.sd_browsers.retain(|b| b.part != part);
        }
        ui.separator();
        match (self.tab.clone(), s) {
            (Tab::Problems, _) => self.problems(ui),
            (_, None) => {
                ui.label(RichText::new("No firmware running.").weak());
            }
            (Tab::Synth, Some(s)) => {
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .show(ui, |ui| self.sound.ui(ui, s));
            }
            (Tab::Sd(part), Some(s)) => {
                let now = s.sd_card_writes(&part);
                let Some(b) = self.sd_browsers.iter_mut().find(|b| b.part == part) else {
                    return;
                };
                if let Want::Refresh = b.ui(ui, now)
                    && let Some((image, written)) = s.sd_card_copy(&part)
                {
                    b.refresh(image, s.seconds(), written);
                }
            }
            (Tab::Log, Some(s)) => self.log_view(ui, s),
            (Tab::Usb, Some(s)) => usb_log_view(ui, s),
            (Tab::Uart(n), Some(s)) => self.console(ui, s, Some(n)),
            (Tab::UsbSerial, Some(s)) => self.console(ui, s, None),
        }
    }

    /// Everything that went wrong or looks suspicious, newest last.
    fn problems(&mut self, ui: &mut egui::Ui) {
        self.errors_seen = self.errors.len();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.errors.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                self.errors.clear();
                self.errors_seen = 0;
            }
            ui.label(
                RichText::new(
                    "Errors starting the run, ignored panel options and warnings from the board",
                )
                .weak(),
            );
        });
        if self.errors.is_empty() {
            ui.label(RichText::new("No problems.").weak());
            return;
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                for e in &self.errors {
                    let color = if e.starts_with("warning:") {
                        ui.visuals().warn_fg_color
                    } else {
                        ui.visuals().error_fg_color
                    };
                    ui.add(egui::Label::new(RichText::new(e).color(color)).wrap());
                }
            });
    }

    /// A serial console: UART `n`, or the USB serial port for `None`.
    fn console(&mut self, ui: &mut egui::Ui, s: &mut Session, uart: Option<usize>) {
        if uart.is_none()
            && let Some(name) = s.usb.serial_bridge()
        {
            ui.label(
                RichText::new(format!(
                    "bridged to {name}: open it with a terminal program"
                ))
                .weak(),
            );
        }
        ui.horizontal(|ui| {
            let send = ui.add(
                egui::TextEdit::singleline(&mut self.uart_line)
                    .hint_text("send to the firmware…")
                    .desired_width(ui.available_width() - 140.0),
            );
            egui::ComboBox::from_id_salt("eol")
                .selected_text(LINE_ENDINGS[self.line_ending].1)
                .width(60.0)
                .show_ui(ui, |ui| {
                    for (i, e) in LINE_ENDINGS.iter().enumerate() {
                        ui.selectable_value(&mut self.line_ending, i, e.1);
                    }
                });
            if ui.small_button("Clear").clicked() {
                match uart {
                    None => s.usb.console.clear(),
                    Some(n) => s.uart[n].clear(),
                }
            }
            if send.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let line = std::mem::take(&mut self.uart_line) + LINE_ENDINGS[self.line_ending].0;
                match uart {
                    None => s.usb.input.extend(line.as_bytes()),
                    Some(n) => s.send(n, line.as_bytes()),
                }
                send.request_focus();
            }
        });
        let text = match uart {
            None => &s.usb.console,
            Some(n) => &s.uart[n],
        };
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
            });
    }

    /// The firmware's defmt log: one truncated line per message (the whole
    /// message on hover), at least as severe as `log_min`.
    fn log_view(&mut self, ui: &mut egui::Ui, s: &mut Session) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("log-level")
                .selected_text(format!("≥ {}", self.log_min.name()))
                .width(80.0)
                .show_ui(ui, |ui| {
                    for l in [
                        rtt::Level::Trace,
                        rtt::Level::Debug,
                        rtt::Level::Info,
                        rtt::Level::Warn,
                        rtt::Level::Error,
                    ] {
                        ui.selectable_value(&mut self.log_min, l, l.name());
                    }
                });
            if ui.small_button("Clear").clicked() {
                s.log.clear();
            }
        });
        let shown: Vec<&session::LogEntry> = s
            .log
            .iter()
            .filter(|e| e.line.level.is_none_or(|l| l >= self.log_min))
            .collect();
        if shown.is_empty() {
            let note = s.log_note.as_deref().unwrap_or("nothing logged yet");
            ui.label(RichText::new(note).weak());
            return;
        }
        let row = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show_rows(ui, row, shown.len(), |ui, rows| {
                for e in &shown[rows] {
                    ui.add(egui::Label::new(log_job(ui, e)).truncate())
                        .on_hover_text(&e.line.text);
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
                    if let Some((_, image, _)) = s.take_sd_cards().into_iter().find(|c| c.0 == name)
                    {
                        files::save(&format!("{name}.img"), image);
                    }
                }
                Request::BrowseSd(name) => {
                    self.tab = Tab::Sd(name.clone());
                    if self.sd_browsers.iter().any(|b| b.part == name) {
                        continue;
                    }
                    if let Some((image, written)) = s.sd_card_copy(&name) {
                        let at = s.seconds();
                        self.sd_browsers
                            .push(SdBrowser::new(name, image, at, written));
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
        if self.watch.changed(
            self.firmware
                .iter_mut()
                .chain(&mut self.board)
                .chain(&mut self.shields),
        ) {
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
        if let Some(r) = guard
            .as_deref_mut()
            .filter(|r| r.clock.running && !midi.is_empty())
        {
            r.session.send(MIDI_UART, &midi);
        }
        if let Some(r) = guard.as_deref_mut() {
            self.panel.absorb(&r.session.events, ctx.input(|i| i.time));
            r.session.events.clear();
            self.errors.extend(
                r.session
                    .warnings
                    .drain(..)
                    .map(|w| format!("warning: {w}")),
            );
        }

        let mut reset = false;
        let logo = self.logo.get_or_insert_with(|| Logo::new(&ctx)).clone();
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(logo.image(LOGO_HEIGHT));
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    reset = self.toolbar(ui, guard.as_deref_mut());
                });
            });
            ui.add_space(2.0);
        });
        let session = guard.as_deref_mut().map(|r| &mut r.session);
        // The board comes first: the dock may only grow into what the board
        // doesn't need at its smallest cells. In a window too small for both,
        // the dock keeps its minimum and the board scrolls.
        let room = ui.available_size();
        let board_min = self
            .layout
            .as_ref()
            .map_or(egui::Vec2::splat(3.0 * panel::MIN_CELL), |l| l.min_size());
        match self.dock {
            DockSide::Bottom => egui::Panel::bottom("dock")
                .resizable(true)
                .default_size(220.0)
                .min_size(80.0)
                .max_size((room.y - board_min.y).max(80.0))
                .show(ui, |ui| self.dock(ui, session)),
            DockSide::Right => egui::Panel::right("dock")
                .resizable(true)
                .default_size(360.0)
                .min_size(240.0)
                .max_size((room.x - board_min.x).max(240.0))
                .show(ui, |ui| self.dock(ui, session)),
        };
        let requests = egui::CentralPanel::default()
            .show(ui, |ui| match (guard.as_deref(), &self.layout) {
                (Some(r), Some(layout)) => panel::show(ui, layout, &mut self.panel, &r.session),
                _ => {
                    ui.centered_and_justified(|ui| {
                        let (text, color) = if self.firmware.is_some() && !self.errors.is_empty() {
                            (format!("The board could not start:\n\n{}", self.errors.join("\n")), ui.visuals().error_fg_color)
                        } else {
                            ("Drop a firmware ELF or UF2 and a board .toml file here,\nor use the Open buttons above.".to_string(), Color32::GRAY)
                        };
                        ui.label(RichText::new(text).size(18.0).color(color));
                    });
                    Vec::new()
                }
            })
            .inner;
        self.apply(requests, &ctx, guard.as_deref_mut().map(|r| &mut r.session));

        let busy = guard
            .as_deref()
            .is_some_and(|r| r.session.halted.is_none() && r.clock.running);
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

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// What is on the USB port, the MIDI bridge's ports, then the traffic: one
/// line per transfer or packet.
fn usb_log_view(ui: &mut egui::Ui, s: &mut Session) {
    ui.horizontal(|ui| {
        match &s.usb.summary {
            Some(d) => ui.label(d),
            None => ui.label(RichText::new("no USB device configured").weak()),
        };
        if crate::usb::UsbSide::has_midi(s.sys.usb_host()) {
            ui.separator();
            s.usb.midi.ui(ui);
            if let Some(e) = &s.usb.midi.error {
                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
            }
        }
    });
    let row = ui.text_style_height(&egui::TextStyle::Monospace);
    let entries = &s.usb.log;
    if entries.is_empty() {
        ui.label(RichText::new("no USB traffic yet").weak());
        return;
    }
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink(false)
        .show_rows(ui, row, entries.len(), |ui, rows| {
            for e in entries.range(rows) {
                let text = format!("{:9.6} {}", e.at, e.event.describe());
                ui.add(egui::Label::new(RichText::new(text).monospace()).truncate());
            }
        });
}

/// `time  LEVEL  message`, the level in its colour.
fn log_job(ui: &egui::Ui, e: &session::LogEntry) -> egui::text::LayoutJob {
    use egui::{FontId, TextFormat, text::LayoutJob};
    let v = ui.visuals();
    let font = FontId::monospace(12.0);
    let fmt = |color| TextFormat {
        font_id: font.clone(),
        color,
        ..Default::default()
    };
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
