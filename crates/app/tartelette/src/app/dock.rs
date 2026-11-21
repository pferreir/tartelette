//! The dock: a strip of tabs (the serial consoles, the firmware's log, USB,
//! the synth, SD card browsers and the problems found), then the chosen one.
use egui::{Color32, RichText};
use tartelette_runtime::{LogEntry, Session};

use super::{App, DockSide, Tab, usb_tab};
use crate::sdbrowse::Want;

const LINE_ENDINGS: &[(&str, &str)] = &[("\n", "LF"), ("\r", "CR"), ("\r\n", "CRLF"), ("", "none")];

impl App {
    /// The dock: a strip of tabs, then the chosen one.
    pub(super) fn dock(&mut self, ui: &mut egui::Ui, s: Option<&mut Session>) {
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
            (Tab::Usb, Some(s)) => usb_tab::show(ui, s, &self.history),
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
        let bridge = match uart {
            None => s.usb.serial_bridge(),
            Some(n) => s.uarts.bridge(n),
        };
        if let Some(name) = bridge {
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
                self.history.console(uart).clear();
            }
            if send.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let line = std::mem::take(&mut self.uart_line) + LINE_ENDINGS[self.line_ending].0;
                match uart {
                    None => s.usb.input.extend(line.as_bytes()),
                    Some(n) => s.uarts.input[n].extend(line.as_bytes()),
                }
                send.request_focus();
            }
        });
        let text = self.history.console(uart).as_str();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
            });
    }

    /// The firmware's defmt log: one truncated line per message (the whole
    /// message on hover), at least as severe as `log_min`.
    fn log_view(&mut self, ui: &mut egui::Ui, s: &Session) {
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
                self.history.log.clear();
            }
        });
        let shown: Vec<&LogEntry> = self
            .history
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
}

/// `time  LEVEL  message`, the level in its colour.
fn log_job(ui: &egui::Ui, e: &LogEntry) -> egui::text::LayoutJob {
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
