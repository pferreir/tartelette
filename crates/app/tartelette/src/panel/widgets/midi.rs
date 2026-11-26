//! `midi`: a MIDI in jack, with where it goes and a light that flashes as
//! bytes go down its pin; with `keyboard`, piano keys below that play into
//! it (note on while a key is held, note off on release).
use pcb_devices::midi::Route;

use super::*;

/// How long the light stays on after a byte, in seconds.
const LIGHT: f64 = 0.12;
/// Semitones above C of the white keys, and which white keys have a black
/// one after them.
const WHITE: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const BLACK_AFTER: [bool; 7] = [true, true, false, true, true, true, false];
const VELOCITY: i64 = 100;

pub(in crate::panel) fn midi(c: Ctx, keyboard: Option<u8>, channel: u8, out: &mut Vec<Request>) {
    let Some(m) = c.device::<Midi>() else { return };
    let to = match m.route {
        Route::Uart(n) => format!("UART{n}"),
        Route::Pin(p) => format!("GPIO{p}"),
    };
    // Bytes sent as of the last frame, and when that count last moved.
    let id = c.id("midi");
    let now = c.ui.input(|i| i.time);
    let (seen, at) =
        c.ui.data(|d| d.get_temp::<(u64, f64)>(id))
            .unwrap_or((m.sent, -LIGHT));
    let at = if m.sent != seen { now } else { at };
    c.ui.data_mut(|d| d.insert_temp(id, (m.sent, at)));
    let lit = now - at < LIGHT;
    if lit {
        crate::app::next_frame(c.ui.ctx());
    }
    let line_h = if keyboard.is_some() {
        18.0
    } else {
        c.body.height()
    };
    let line = Rect::from_min_size(c.body.min, vec2(c.body.width(), line_h));
    let r = (line.height() * 0.18).clamp(4.0, 6.0);
    let center = pos2(line.left() + r * 2.5, line.center().y);
    lamp(c.ui, center, r, Color32::from_rgb(40, 230, 70), lit);
    let v = c.ui.visuals();
    c.ui.painter().text(
        pos2(center.x + r * 2.0, center.y),
        Align2::LEFT_CENTER,
        format!("MIDI in → {to}"),
        FontId::monospace(12.0),
        v.text_color(),
    );
    let tip = match m.route {
        Route::Uart(_) => "fed by the toolbar's MIDI inputs",
        Route::Pin(_) => "31250-baud serial, fed by the toolbar's MIDI inputs",
    };
    c.ui.interact(line, id.with("line"), Sense::hover())
        .on_hover_text(tip);
    if let Some(low) = keyboard {
        let keys = Rect::from_min_max(
            pos2(c.body.left() + 4.0, line.bottom()),
            c.body.max - vec2(4.0, 2.0),
        );
        piano(&c, keys, low, channel, out);
    }
}

/// The keys from note `low` (or the white key above it), as many white
/// ones as fit, playing on `channel`.
fn piano(c: &Ctx, rect: Rect, low: u8, channel: u8, out: &mut Vec<Request>) {
    let n = ((rect.width() / 14.0).floor() as usize).max(1);
    let white_w = rect.width() / n as f32;
    let first = WHITE.iter().position(|&s| s >= low % 12).unwrap_or(0);
    let white_note = |k: usize| {
        let i = first + k;
        (low / 12) as usize * 12 + (i / 7) * 12 + WHITE[i % 7] as usize
    };
    // (where, note, black)
    let mut keys: Vec<(Rect, u8, bool)> = Vec::new();
    for k in 0..n {
        let note = white_note(k);
        if note > 127 {
            break;
        }
        let x = rect.left() + k as f32 * white_w;
        keys.push((
            Rect::from_min_size(pos2(x, rect.top()), vec2(white_w, rect.height())),
            note as u8,
            false,
        ));
        if BLACK_AFTER[(first + k) % 7] && k + 1 < n && note < 127 {
            let bw = white_w * 0.6;
            keys.push((
                Rect::from_min_size(
                    pos2(x + white_w - bw / 2.0, rect.top()),
                    vec2(bw, rect.height() * 0.6),
                ),
                note as u8 + 1,
                true,
            ));
        }
    }
    // Black keys sit on top of the white ones.
    let on_top = || {
        keys.iter()
            .filter(|k| k.2)
            .chain(keys.iter().filter(|k| !k.2))
    };
    let id = c.id("piano");
    let resp = c.ui.interact(rect, id, Sense::drag());
    let held = resp
        .interact_pointer_pos()
        .filter(|_| resp.is_pointer_button_down_on())
        .and_then(|p| on_top().find(|k| k.0.contains(p)).map(|k| k.1));
    let was: Option<u8> = c.ui.data(|d| d.get_temp(id)).flatten();
    if held != was {
        let mut send = |bytes: [i64; 3]| {
            for b in bytes {
                out.push(Request::Input(c.it.name.clone(), "byte".into(), b));
            }
        };
        if let Some(n) = was {
            send([0x80 | channel as i64, n as i64, 0]);
        }
        if let Some(n) = held {
            send([0x90 | channel as i64, n as i64, VELOCITY]);
        }
        c.ui.data_mut(|d| d.insert_temp(id, held));
    }
    let p = c.ui.painter_at(rect);
    let pressed_fill = c.ui.visuals().selection.bg_fill;
    // White keys first, so the black ones are drawn over them.
    for &(r, note, black) in keys
        .iter()
        .filter(|k| !k.2)
        .chain(keys.iter().filter(|k| k.2))
    {
        let fill = match (black, held == Some(note)) {
            (_, true) => pressed_fill,
            (false, false) => Color32::from_gray(225),
            (true, false) => Color32::from_gray(30),
        };
        p.rect(
            r.shrink(0.5),
            2.0,
            fill,
            Stroke::new(1.0, Color32::from_gray(60)),
            StrokeKind::Inside,
        );
        if !black && note.is_multiple_of(12) {
            p.text(
                pos2(r.center().x, r.bottom() - 2.0),
                Align2::CENTER_BOTTOM,
                format!("C{}", note as i32 / 12 - 1),
                FontId::proportional(9.0),
                Color32::from_gray(110),
            );
        }
    }
    resp.on_hover_text("click a key to play it into the jack");
}
