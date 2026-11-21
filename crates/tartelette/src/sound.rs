//! The Synth tab: test oscillators and drums patched into the board's outputs,
//! played through the sound card (see [`crate::synth`]).
use std::sync::{Arc, Mutex};

use cpal::{
    FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use egui::{Color32, RichText};

use crate::{
    session::Session,
    synth::{self, Kind, Live, Patch, Synth},
};

pub struct Sound {
    /// The oscillators; `None` until there is a board to take defaults from.
    patches: Option<Vec<Patch>>,
    /// What was asked for, made into `patches` once there is a board.
    asked: (Vec<Patch>, bool),
    out: Option<Output>,
    error: Option<String>,
    volume: f32,
}

struct Output {
    live: Arc<Mutex<Live>>,
    _stream: Stream,
}

impl Sound {
    /// With the oscillators (`--osc`) and drum kit (`--drums`) asked for, if
    /// any; without either, the board's default voice.
    pub fn new(osc: Vec<Patch>, drums: bool) -> Self {
        Sound {
            patches: None,
            asked: (osc, drums),
            out: None,
            error: None,
            volume: 0.8,
        }
    }

    /// Patch the oscillators into a new run (a reset, a new board).
    pub fn attach(&mut self, s: &mut Session) {
        let patches = self.patches(s);
        let Some(out) = &self.out else { return };
        let mut live = out.live.lock().unwrap_or_else(|e| e.into_inner());
        let rate = live.rate();
        match Synth::new(&s.sys.board, &patches, rate) {
            Ok(synth) => {
                live.replace(synth);
                s.synth = Some(out.live.clone());
                self.error = None;
            }
            Err(e) => {
                live.replace(Synth::new(&s.sys.board, &[], rate).unwrap());
                self.error = Some(e);
            }
        }
    }

    /// The patches, completed with `s`'s board defaults.
    fn patches(&mut self, s: &Session) -> Vec<Patch> {
        let board = &s.sys.board;
        let (osc, drums) = &self.asked;
        let patches = self
            .patches
            .get_or_insert_with(|| synth::patches(osc, *drums, board));
        for p in patches.iter_mut() {
            p.complete(board);
        }
        patches.clone()
    }

    fn start(&mut self, s: &mut Session) {
        match open(&s.sys.board, &self.patches(s)) {
            Ok(out) => {
                out.live.lock().unwrap().volume = self.volume;
                s.synth = Some(out.live.clone());
                self.out = Some(out);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn stop(&mut self, s: &mut Session) {
        s.synth = None;
        self.out = None;
    }

    /// The Synth tab's contents.
    pub fn ui(&mut self, ui: &mut egui::Ui, s: &mut Session) {
        let mut patches = self.patches(s);
        let before = patches.clone();
        ui.horizontal(|ui| {
            let mut on = self.out.is_some();
            if ui.checkbox(&mut on, "Sound").changed() {
                if on {
                    self.start(s);
                } else {
                    self.stop(s);
                }
            }
            ui.label("Volume");
            if ui
                .add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false))
                .changed()
                && let Some(out) = &self.out
            {
                out.live.lock().unwrap().volume = self.volume;
            }
        });
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        ui.label(
            RichText::new(
                "Oscillators: 1 V/octave, 0 V = C4. Drums: a hit per rising gate edge.\n\
                 Recorded headless with --osc, --drums and --wav.",
            )
            .weak(),
        );
        ui.separator();
        let board = &s.sys.board;
        let (cvs, gates) = (synth::cv_outputs(board), synth::gate_outputs(board));
        let mut remove = None;
        egui::Grid::new("oscillators")
            .num_columns(6)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for (i, p) in patches.iter_mut().enumerate() {
                    egui::ComboBox::from_id_salt(("kind", i))
                        .selected_text(p.kind.name())
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            for k in Kind::ALL {
                                if k == Kind::Kick {
                                    ui.separator();
                                }
                                ui.selectable_value(&mut p.kind, k, k.name());
                            }
                        });
                    p.complete(board);
                    if p.kind.is_drum() {
                        ui.label("");
                    } else {
                        pick(ui, ("cv", i), "CV", &mut p.cv, &cvs);
                    }
                    pick(ui, ("gate", i), "Gate", &mut p.gate, &gates);
                    ui.add(egui::Slider::new(&mut p.level, 0.0..=1.0).show_value(false))
                        .on_hover_text("Level");
                    readout(ui, synth::probe(board, p));
                    if ui.small_button("✖").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        if let Some(i) = remove {
            patches.remove(i);
        }
        ui.horizontal(|ui| {
            if ui.button("Add oscillator").clicked() {
                let mut p = Patch::default();
                p.complete(board);
                patches.push(p);
            }
            if ui
                .button("Drum kit")
                .on_hover_text("Kick, snare, closed and open hat on the first four gates, in place of the voices above")
                .clicked()
            {
                patches = synth::drum_kit(board);
            }
        });
        if patches != before {
            self.patches = Some(patches);
            self.attach(s);
        }
    }
}

/// A labelled choice of output names.
fn pick(ui: &mut egui::Ui, id: (&str, usize), label: &str, value: &mut String, names: &[String]) {
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(id)
            .selected_text(value.as_str())
            .width(100.0)
            .show_ui(ui, |ui| {
                for n in names {
                    ui.selectable_value(value, n.clone(), n);
                }
            });
    });
}

/// Gate lamp, and an oscillator's note and pitch.
fn readout(ui: &mut egui::Ui, now: Option<(Option<f32>, bool)>) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        let lit = now.is_some_and(|n| n.1);
        let color = if lit {
            Color32::from_rgb(80, 220, 90)
        } else {
            Color32::from_gray(60)
        };
        ui.painter().circle_filled(rect.center(), 5.0, color);
        let text = match now {
            Some((None, _)) => String::new(),
            Some((Some(v), _)) => format!(
                "{:<8} {:>8.1} Hz {:+.3} V",
                synth::note_name(v),
                synth::hz(v),
                v
            ),
            None => "—".into(),
        };
        ui.monospace(text);
    });
}

/// Start the sound card playing the voices.
fn open(board: &pcb::Board, patches: &[Patch]) -> Result<Output, String> {
    let host = cpal::default_host();
    let dev = host
        .default_output_device()
        .ok_or("no sound output device")?;
    let cfg = dev
        .default_output_config()
        .map_err(|e| format!("sound output: {e}"))?;
    let synth = Synth::new(board, patches, cfg.sample_rate())?;
    let live = Arc::new(Mutex::new(Live::new(synth)));
    let channels = cfg.channels() as usize;
    let config: StreamConfig = cfg.config();
    let l = live.clone();
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => stream::<f32>(&dev, config, channels, l),
        SampleFormat::I16 => stream::<i16>(&dev, config, channels, l),
        SampleFormat::U16 => stream::<u16>(&dev, config, channels, l),
        SampleFormat::I32 => stream::<i32>(&dev, config, channels, l),
        f => return Err(format!("sound output: unsupported sample format {f}")),
    }?;
    stream.play().map_err(|e| format!("sound output: {e}"))?;
    Ok(Output {
        live,
        _stream: stream,
    })
}

fn stream<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    config: StreamConfig,
    channels: usize,
    live: Arc<Mutex<Live>>,
) -> Result<Stream, String> {
    let mut mono = Vec::new();
    dev.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            mono.resize(data.len() / channels, 0.0);
            live.lock()
                .unwrap_or_else(|e| e.into_inner())
                .render(&mut mono);
            for (frame, v) in data.chunks_mut(channels).zip(&mono) {
                frame.fill(T::from_sample(*v));
            }
        },
        |e| eprintln!("sound output: {e}"),
        None,
    )
    .map_err(|e| format!("sound output: {e}"))
}
