//! The window: a top bar (logo, controls, status), the front panel in the
//! middle, and a dock of tabs at the bottom or on the right: the serial
//! consoles, the firmware's log, USB, the synth, SD card browsers and the
//! problems found.
use egui::{Color32, RichText};
use tartelette_runtime::{
    Links, Session, Source,
    run::{self, Shared, lock},
    synth::Patch,
};

use crate::{
    brand::Logo,
    files::{self, Inbox, Slot},
    midi_in::MidiInPicker,
    panel::{self, Layout, PanelState, Request},
    sdbrowse::SdBrowser,
    sound::Sound,
};

mod dock;
mod history;
mod toolbar;
mod usb_tab;

use history::History;

/// Tall enough to read the lettering, as tall as the top bar's two rows.
const LOGO_HEIGHT: f32 = 64.0;

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
    links: Links,
    run: Option<Shared>,
    /// The consoles' text and the logs of the current run.
    history: History,
    layout: Option<Layout>,
    panel: PanelState,
    /// Problems to show: a run that could not start, ignored panel options,
    /// warnings from the run.
    errors: Vec<String>,
    /// How many of `errors` the Problems tab has shown.
    errors_seen: usize,
    inbox: Inbox,
    /// The two MIDI inputs.
    midi: [MidiInPicker; 2],
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
    /// WAV files loaded from the panel, by player: they outlast a restart.
    wav_files: Vec<(String, Source)>,
    /// What the flash chip held when the last run ended. A restart, or new
    /// firmware, keeps the rest of the flash, as on the real chip.
    flash: Option<Vec<u8>>,
    /// `--flash`: the file the flash chip is kept in.
    #[cfg(not(target_arch = "wasm32"))]
    flash_file: Option<FlashFile>,
}

#[cfg(not(target_arch = "wasm32"))]
struct FlashFile {
    path: std::path::PathBuf,
    /// The flash at the start of the current run, to tell whether the
    /// firmware wrote to it.
    at_start: Vec<u8>,
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
        links: Links,
    ) -> Self {
        let mut app = Self::stopped(firmware, board, shields, links);
        app.restart();
        app
    }

    /// Like `new`, with the flash chip kept in `path` (`--flash`): it starts
    /// from the file's contents, if it exists, and is saved back to it on
    /// each restart and on exit if the firmware wrote to it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_flash_file(
        firmware: Option<Source>,
        board: Option<Source>,
        shields: Vec<Source>,
        links: Links,
        path: std::path::PathBuf,
    ) -> Result<Self, String> {
        let mut app = Self::stopped(firmware, board, shields, links);
        if path.exists() {
            app.flash = Some(std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?);
        }
        app.flash_file = Some(FlashFile {
            path,
            at_start: app.flash.clone().unwrap_or_default(),
        });
        app.restart();
        Ok(app)
    }

    /// The app before its first run.
    fn stopped(
        firmware: Option<Source>,
        board: Option<Source>,
        shields: Vec<Source>,
        links: Links,
    ) -> Self {
        Self {
            logo: None,
            firmware,
            board,
            shields,
            links,
            run: None,
            history: History::default(),
            layout: None,
            panel: PanelState::default(),
            errors: Vec::new(),
            errors_seen: 0,
            inbox: Inbox::default(),
            midi: [MidiInPicker::new(0), MidiInPicker::new(1)],
            tab: Tab::Uart(0),
            dock: DockSide::Bottom,
            log_min: rtt::Level::Trace,
            uart_line: String::new(),
            line_ending: 0,
            #[cfg(not(target_arch = "wasm32"))]
            watch: Default::default(),
            sd_browsers: Vec::new(),
            sound: Sound::new(Vec::new(), false),
            #[cfg(not(target_arch = "wasm32"))]
            sd_files: Vec::new(),
            wav_files: Vec::new(),
            flash: None,
            #[cfg(not(target_arch = "wasm32"))]
            flash_file: None,
        }
    }

    /// Keep what the running firmware left in flash for the next run.
    fn keep_flash(&mut self) {
        if let Some(r) = &self.run {
            self.flash = Some(lock(r).session.sys.bus.flash.data.clone());
        }
    }

    /// Write the flash to the `--flash` file if the firmware wrote to it
    /// (or the file isn't there yet).
    #[cfg(not(target_arch = "wasm32"))]
    fn save_flash(&self) -> Result<(), String> {
        let (Some(f), Some(data)) = (&self.flash_file, &self.flash) else {
            return Ok(());
        };
        if *data != f.at_start || !f.path.exists() {
            std::fs::write(&f.path, data)
                .map_err(|e| format!("--flash: {}: {e}", f.path.display()))?;
        }
        Ok(())
    }

    /// Power-cycle: a fresh run from the current firmware and board. SD
    /// cards and the flash keep their contents.
    fn restart(&mut self) {
        self.keep_flash();
        self.start();
    }

    /// Stop the current run, if any, and start one with the flash holding
    /// `self.flash` (or else the board's `[[flash]]` entries).
    fn start(&mut self) {
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
            run::stop(old);
        }
        self.layout = None;
        self.panel = PanelState::default();
        self.history = History::default();
        self.errors.clear();
        self.errors_seen = 0;
        #[cfg(not(target_arch = "wasm32"))]
        if let Err(e) = self.save_flash() {
            self.errors.push(e);
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.watch
            .track(self.firmware.iter().chain(&self.board).chain(&self.shields));
        let Some(fw) = &self.firmware else { return };
        match Session::with_flash(
            fw,
            self.board.as_ref(),
            &self.shields,
            &self.links,
            self.flash.as_deref(),
        ) {
            Ok(mut s) => {
                #[cfg(not(target_arch = "wasm32"))]
                if let Some(f) = &mut self.flash_file {
                    f.at_start = s.sys.bus.flash.data.clone();
                }
                for (name, image, _) in cards {
                    s.load_sd_card(&name, image);
                }
                // A board without that player any more drops its file.
                self.wav_files
                    .retain(|(name, file)| s.load_wav(name, file).unwrap_or(false));
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
                let run = run::start(s, running, speed);
                #[cfg(target_arch = "wasm32")]
                crate::pump::start(&run);
                self.run = Some(run);
            }
            Err(e) => self.errors.push(e),
        }
    }

    /// Start with the voices `--osc` and `--drums` (or the page's `osc` and
    /// `drums`) ask for, on the Synth tab (without either, the board's
    /// default voice).
    pub fn set_voices(&mut self, osc: &[Patch], drums: bool) {
        if osc.is_empty() && !drums {
            return;
        }
        self.tab = Tab::Synth;
        self.sound = Sound::new(osc.to_vec(), drums);
        if let Some(r) = &self.run {
            self.sound.attach(&mut lock(r).session);
        }
    }

    /// Load `src` as if the user had opened it for `slot`, on the next frame.
    #[cfg(target_arch = "wasm32")]
    pub fn open(&self, slot: Slot, src: Source) {
        self.inbox.push(slot, src);
    }

    /// Show `msg` among the problems found.
    #[cfg(target_arch = "wasm32")]
    pub fn report(&mut self, msg: String) {
        self.errors.push(msg);
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
        let mut new_board = false;
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
                    new_board = true;
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
                Slot::Wav(name) => {
                    let Some(run) = self.run.clone() else {
                        continue;
                    };
                    let mut g = lock(&run);
                    let s = &mut g.session;
                    let name = match name.is_empty() {
                        false => name,
                        true => match s.sys.board.devices_mut::<pcb_devices::wav::Wav>().next() {
                            Some((name, _)) => name.to_string(),
                            None => {
                                self.errors
                                    .push(format!("{}: the board has no WAV player", src.name));
                                continue;
                            }
                        },
                    };
                    match s.load_wav(&name, &src) {
                        Ok(_) => {
                            self.wav_files.retain(|(n, _)| *n != name);
                            self.wav_files.push((name, src));
                        }
                        Err(e) => self.errors.push(e),
                    }
                }
                Slot::Guess => unreachable!(),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let in_file = self.flash_file.is_some();
        #[cfg(target_arch = "wasm32")]
        let in_file = false;
        if new_board && !in_file {
            // Another board has another flash chip, which starts from the
            // board's `[[flash]]` entries.
            self.flash = None;
            self.start();
        } else if restart {
            self.restart();
        }
    }

    fn apply(&mut self, requests: Vec<Request>, ctx: &egui::Context, s: Option<&mut Session>) {
        let Some(s) = s else { return };
        for r in requests {
            match r {
                Request::Input(part, action, value) => {
                    s.input(&part, &action, value);
                }
                Request::LoadSd(name) => files::pick(Slot::Sd(name), &self.inbox, ctx),
                Request::LoadWav(name) => files::pick(Slot::Wav(name), &self.inbox, ctx),
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
    #[cfg(not(target_arch = "wasm32"))]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.flash_file.is_some() {
            self.keep_flash();
            if let Err(e) = self.save_flash() {
                eprintln!("{e}");
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        #[cfg(target_arch = "wasm32")]
        crate::pump::frame_drawn();
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
        // Real MIDI goes to the firmware only while it runs; otherwise it is dropped.
        for m in &mut self.midi {
            let bytes = m.take();
            if let Some(r) = guard
                .as_deref_mut()
                .filter(|r| r.clock.running && !bytes.is_empty())
            {
                let dest = m.dest(&r.session.sys.board);
                r.session.midi(&dest, &bytes);
            }
        }
        if let Some(r) = guard.as_deref_mut() {
            let mut o = r.session.take_output();
            self.panel
                .absorb(o.events.make_contiguous(), ctx.input(|i| i.time));
            self.errors.extend(
                o.warnings
                    .drain(..)
                    .chain(o.notes.drain(..))
                    .map(|w| format!("warning: {w}")),
            );
            self.history.absorb(&mut o);
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
        panel::end_frame(&ctx);
        self.apply(requests, &ctx, guard.as_deref_mut().map(|r| &mut r.session));

        let busy = guard
            .as_deref()
            .is_some_and(|r| r.session.halted.is_none() && r.clock.running);
        drop(guard);
        // `restart` waits for every handle on the old run to go, this one too.
        drop(run);
        if reset {
            self.restart();
        }
        if busy {
            next_frame(&ctx);
        } else {
            // Native: keep polling the files for changes.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }
}

/// Ask for the next frame, as soon as the running firmware leaves room for it:
/// in a browser, frames take time from the run (see `pump`).
pub fn next_frame(ctx: &egui::Context) {
    #[cfg(not(target_arch = "wasm32"))]
    ctx.request_repaint();
    #[cfg(target_arch = "wasm32")]
    ctx.request_repaint_after(crate::pump::frame_interval());
}
