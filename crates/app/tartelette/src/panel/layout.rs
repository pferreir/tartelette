//! Where each item goes on the grid: from the board file's `panel` lines,
//! or packed automatically, with the widget and options for each part.
use egui::{Color32, Key};
use pcb::Board;
use pcb_devices::st7789::St7789;

#[cfg(test)]
use super::test_board;

pub struct Layout {
    pub items: Vec<Item>,
    pub cols: u16,
    pub rows: u16,
}

pub struct Item {
    pub part: usize,
    pub name: String,
    pub label: String,
    pub cell: (u16, u16),
    pub span: (u16, u16),
    pub widget: Widget,
}

#[derive(Clone)]
pub enum Widget {
    Screen {
        rotate: u16,
    },
    Led {
        color: Color32,
    },
    Leds,
    Button {
        key: Option<Key>,
    },
    Encoder {
        keys: Option<(Key, Key)>,
    },
    Expander(Expander),
    Dac,
    SdCard,
    Wifi,
    /// A kind this app has no widget for.
    Other,
}

/// An `mcp23008` item's pins and how they respond.
#[derive(Clone, Debug, PartialEq)]
pub struct Expander {
    pub pins: Vec<u8>,
    /// `None`: each pin as its wiring says.
    pub mode: Option<PinMode>,
    pub active_high: bool,
    /// Empty, or one per entry of `pins`.
    pub keys: Vec<Key>,
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinMode {
    Hold,
    Toggle,
    View,
}

impl Default for Expander {
    fn default() -> Self {
        Expander {
            pins: (0..8).collect(),
            mode: None,
            active_high: false,
            keys: Vec::new(),
            labels: Vec::new(),
        }
    }
}

impl Layout {
    /// The board's panel layout, or an automatic one if it has no `[panel]`
    /// section. Returns messages for widget options that were ignored.
    pub fn new(board: &Board) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut items = Vec::new();
        if board.panel.is_empty() {
            let mut placed: Vec<(u16, u16, u16, u16)> = Vec::new();
            let width = board
                .parts
                .iter()
                .map(|p| auto_span(board, p).0)
                .max()
                .unwrap_or(1)
                .max(6);
            for (i, p) in board.parts.iter().enumerate() {
                let span = auto_span(board, p);
                let span = (span.0.min(width), span.1);
                let cell = first_fit(&placed, span, width);
                placed.push((cell.0, cell.1, span.0, span.1));
                let widget = widget_for(&p.kind, &mut std::iter::empty()).unwrap_or(Widget::Other);
                items.push(Item {
                    part: i,
                    name: p.name.clone(),
                    label: p.name.clone(),
                    cell,
                    span,
                    widget,
                });
            }
        } else {
            for it in &board.panel {
                let p = &board.parts[it.part];
                let mut opts = it.options.iter().map(|(k, v)| (k.as_str(), v.as_str()));
                let widget = widget_for(&p.kind, &mut opts).unwrap_or_else(|e| {
                    warnings.push(format!("panel.{}: {e} (ignored)", p.name));
                    widget_for(&p.kind, &mut std::iter::empty()).unwrap_or(Widget::Other)
                });
                let label = it.label.clone().unwrap_or_else(|| p.name.clone());
                items.push(Item {
                    part: it.part,
                    name: p.name.clone(),
                    label,
                    cell: (it.col, it.row),
                    span: (it.cols, it.rows),
                    widget,
                });
            }
        }
        let cols = items.iter().map(|i| i.cell.0 + i.span.0).max().unwrap_or(1);
        let rows = items.iter().map(|i| i.cell.1 + i.span.1).max().unwrap_or(1);
        (Self { items, cols, rows }, warnings)
    }
}

/// Grid size a part gets in an automatic layout.
fn auto_span(board: &Board, part: &pcb::Part) -> (u16, u16) {
    match part.kind.as_str() {
        "st7789" => {
            let (w, h) = board
                .device::<St7789>(&part.name)
                .map_or((240, 320), St7789::size);
            if w > h { (4, 3) } else { (3, 4) }
        }
        "ws2812" | "mcp23008" => (4, 1),
        "dac8165" => (2, 2),
        "sdcard" => (2, 1),
        "cyw43439" => (3, 2),
        _ => (1, 1),
    }
}

/// Top-most, then left-most free spot for `span` in a grid `width` wide.
fn first_fit(placed: &[(u16, u16, u16, u16)], span: (u16, u16), width: u16) -> (u16, u16) {
    let free = |c: u16, r: u16| {
        placed.iter().all(|&(pc, pr, pw, ph)| {
            c + span.0 <= pc || pc + pw <= c || r + span.1 <= pr || pr + ph <= r
        })
    };
    for r in 0.. {
        for c in 0..=width - span.0 {
            if free(c, r) {
                return (c, r);
            }
        }
    }
    unreachable!()
}

/// The widget for a part `kind` with `options` from its panel line.
fn widget_for<'a>(
    kind: &str,
    options: &mut dyn Iterator<Item = (&'a str, &'a str)>,
) -> Result<Widget, String> {
    let mut w = match kind {
        "st7789" => Widget::Screen { rotate: 0 },
        "led" => Widget::Led {
            color: Color32::from_rgb(255, 48, 32),
        },
        "ws2812" => Widget::Leds,
        "button" => Widget::Button { key: None },
        "encoder" => Widget::Encoder { keys: None },
        "mcp23008" => Widget::Expander(Expander::default()),
        "dac8165" => Widget::Dac,
        "sdcard" => Widget::SdCard,
        "cyw43439" => Widget::Wifi,
        _ => Widget::Other,
    };
    let key = |v: &str| Key::from_name(v).ok_or_else(|| format!("unknown key {v:?}"));
    for (k, v) in options {
        match (&mut w, k) {
            (Widget::Screen { rotate }, "rotate") => {
                *rotate = match v {
                    "0" | "90" | "180" | "270" => v.parse().unwrap(),
                    _ => return Err("rotate must be 0, 90, 180 or 270".into()),
                }
            }
            (Widget::Led { color }, "color") => {
                *color = parse_color(v).ok_or_else(|| format!("unknown color {v:?}"))?
            }
            (Widget::Button { key: k }, "key") => *k = Some(key(v)?),
            (Widget::Encoder { keys }, "keys") => {
                let (a, b) = v.split_once(',').ok_or("keys must be CCW,CW")?;
                *keys = Some((key(a)?, key(b)?));
            }
            (Widget::Expander(x), "pins") => {
                let mut pins = Vec::new();
                for n in v.split(',') {
                    match n.parse::<u8>() {
                        Ok(n) if n < 8 && !pins.contains(&n) => pins.push(n),
                        _ => return Err("pins must be different pins, 0..7".into()),
                    }
                }
                x.pins = pins;
            }
            (Widget::Expander(x), "mode") => {
                x.mode = Some(match v {
                    "hold" => PinMode::Hold,
                    "toggle" => PinMode::Toggle,
                    "view" => PinMode::View,
                    _ => return Err("mode must be hold, toggle or view".into()),
                })
            }
            (Widget::Expander(x), "keys") => {
                x.keys = v.split(',').map(key).collect::<Result<_, _>>()?
            }
            (Widget::Expander(x), "labels") => x.labels = v.split(',').map(String::from).collect(),
            (Widget::Expander(Expander { active_high, .. }), "active") => {
                *active_high = match v {
                    "low" => false,
                    "high" => true,
                    _ => return Err("active must be low or high".into()),
                }
            }
            _ => return Err(format!("{kind} has no panel option {k:?}")),
        }
    }
    if let Widget::Expander(x) = &w {
        let n = x.pins.len();
        if !x.keys.is_empty() && x.keys.len() != n {
            return Err(format!("keys must give one key per pin ({n})"));
        }
        if !x.labels.is_empty() && x.labels.len() != n {
            return Err(format!("labels must give one label per pin ({n})"));
        }
    }
    Ok(w)
}

fn parse_color(v: &str) -> Option<Color32> {
    Some(match v {
        "red" => Color32::from_rgb(255, 48, 32),
        "green" => Color32::from_rgb(40, 230, 70),
        "blue" => Color32::from_rgb(40, 110, 255),
        "yellow" => Color32::from_rgb(255, 220, 40),
        "orange" => Color32::from_rgb(255, 140, 20),
        "white" => Color32::from_rgb(240, 240, 255),
        _ => {
            // Hex without a '#', which would start a board-file comment.
            let h = Some(v).filter(|h| h.len() == 6)?;
            let n = u32::from_str_radix(h, 16).ok()?;
            Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_layout_uses_panel_lines_and_reports_bad_options() {
        let b = test_board(
            "buses.spi0 = { sck = 18, mosi = 19 }\nparts.a = { kind = \"led\", gpio = 1 }\nparts.b = { kind = \"button\", gpio = 2 }\nparts.s = { kind = \"st7789\", bus = \"spi0\", cs = 9, dc = 13 }\npanel.s = { at = [0, 0], span = [3, 4], rotate = 90 }\npanel.a = { at = [3, 0], color = \"00ff00\" }\npanel.b = { at = [3, 1], key = \"Nope\" }",
        );
        let (l, warnings) = Layout::new(&b);
        assert_eq!((l.cols, l.rows), (4, 4));
        assert_eq!(l.items.len(), 3);
        assert!(matches!(l.items[0].widget, Widget::Screen { rotate: 90 }));
        assert!(
            matches!(l.items[1].widget, Widget::Led { color } if color == Color32::from_rgb(0, 255, 0))
        );
        assert!(matches!(l.items[2].widget, Widget::Button { key: None }));
        assert_eq!(warnings, ["panel.b: unknown key \"Nope\" (ignored)"]);
    }

    #[test]
    fn automatic_layout_packs_every_part_without_overlap() {
        let b = test_board(include_str!("../../../../../boards/btl-01.toml"));
        let (l, warnings) = Layout::new(&b);
        assert!(warnings.is_empty());
        for part in 0..b.parts.len() {
            assert!(
                l.items.iter().any(|i| i.part == part),
                "part {part} missing"
            );
        }
        for (i, a) in l.items.iter().enumerate() {
            assert!(a.cell.0 + a.span.0 <= l.cols);
            for o in &l.items[i + 1..] {
                let apart = a.cell.0 + a.span.0 <= o.cell.0
                    || o.cell.0 + o.span.0 <= a.cell.0
                    || a.cell.1 + a.span.1 <= o.cell.1
                    || o.cell.1 + o.span.1 <= a.cell.1;
                assert!(apart, "{} overlaps {}", a.name, o.name);
            }
        }
    }

    #[test]
    fn widget_options() {
        let w = |k, o: &[(&'static str, &'static str)]| widget_for(k, &mut o.iter().copied());
        assert!(matches!(
            w("encoder", &[("keys", "A,D")]),
            Ok(Widget::Encoder {
                keys: Some((Key::A, Key::D))
            })
        ));
        let Ok(Widget::Expander(x)) = w(
            "mcp23008",
            &[
                ("mode", "toggle"),
                ("active", "high"),
                ("keys", "Q,W"),
                ("pins", "7,2"),
                ("labels", "A,B"),
            ],
        ) else {
            panic!()
        };
        assert_eq!(
            x,
            Expander {
                pins: vec![7, 2],
                mode: Some(PinMode::Toggle),
                active_high: true,
                keys: vec![Key::Q, Key::W],
                labels: vec!["A".into(), "B".into()],
            }
        );
        assert!(matches!(w("mcp23008", &[]), Ok(Widget::Expander(x)) if x == Expander::default()));
        let err = |o| w("mcp23008", o).err().unwrap();
        assert_eq!(err(&[("pins", "1,8")]), "pins must be different pins, 0..7");
        assert_eq!(err(&[("pins", "1,1")]), "pins must be different pins, 0..7");
        assert_eq!(
            err(&[("mode", "push")]),
            "mode must be hold, toggle or view"
        );
        assert_eq!(
            err(&[("keys", "A,B")]),
            "keys must give one key per pin (8)"
        );
        assert_eq!(
            err(&[("pins", "0"), ("labels", "a,b")]),
            "labels must give one label per pin (1)"
        );
        assert_eq!(
            w("led", &[("rotate", "90")]).err().unwrap(),
            "led has no panel option \"rotate\""
        );
        assert!(w("st7789", &[("rotate", "45")]).is_err());
        assert!(w("led", &[("color", "12345")]).is_err());
        assert!(w("led", &[("color", "pink")]).is_err());
        assert!(matches!(w("custom-thing", &[]), Ok(Widget::Other)));
    }
}
