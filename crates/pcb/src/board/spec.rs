//! What each device kind accepts in a board file, declared once per kind.
//! The board validates part tables against it, and hosts turn it into help
//! text ([`describe`]) and a JSON Schema for editors ([`json_schema`]).
//!
//! ```ignore
//! pub const KIND: Kind = Kind {
//!     name: "led",
//!     summary: "LED on a pin",
//!     bus: None,
//!     params: &[
//!         Param::shared_pin("gpio", "the anode's pin"),
//!         Param::choice("active", &["high", "low"], "level that lights it").default_str("high"),
//!     ],
//!     build: |p| Ok(Box::new(Led { pin: p.pin("gpio"), active_high: p.str("active") == "high", lit: false })),
//! };
//! ```
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::fmt::Write;

use super::{BUS_KINDS, Registry, params::Params};
use crate::{NUM_PINS, device::Device};

/// Builds a device from its validated parameters.
pub type Build = fn(&mut Params) -> Result<Box<dyn Device>, String>;

/// A device kind: the `kind = "..."` of a part.
#[derive(Clone, Copy)]
pub struct Kind {
    pub name: &'static str,
    /// One line, for listings.
    pub summary: &'static str,
    /// The bus the device sits on, if any: its part then takes `bus = "..."`
    /// (and, on SPI, an optional chip select pin `cs`).
    pub bus: Option<BusType>,
    pub params: &'static [Param],
    pub build: Build,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusType {
    I2c,
    Spi,
}

impl BusType {
    pub fn prefix(self) -> &'static str {
        match self {
            BusType::I2c => "i2c",
            BusType::Spi => "spi",
        }
    }
}

/// One key a part's table may have.
#[derive(Clone, Copy)]
pub struct Param {
    pub key: &'static str,
    pub ty: Ty,
    pub need: Need,
    pub doc: &'static str,
}

#[derive(Clone, Copy)]
pub enum Ty {
    /// A GPIO wired to this part alone: no other part may use it.
    Pin,
    /// A GPIO other parts may use too (an LED watching a signal, an
    /// open-drain interrupt line).
    SharedPin,
    Int {
        min: i64,
        max: i64,
        hex: bool,
    },
    Choice(&'static [&'static str]),
    Str,
    /// An array of tables, each with these keys.
    Tables(&'static [Param]),
}

#[derive(Clone, Copy)]
pub enum Need {
    Required,
    Optional,
    DefaultInt(i64),
    DefaultStr(&'static str),
}

impl Param {
    const fn new(key: &'static str, ty: Ty, doc: &'static str) -> Self {
        Self {
            key,
            ty,
            need: Need::Required,
            doc,
        }
    }
    pub const fn pin(key: &'static str, doc: &'static str) -> Self {
        Self::new(key, Ty::Pin, doc)
    }
    pub const fn shared_pin(key: &'static str, doc: &'static str) -> Self {
        Self::new(key, Ty::SharedPin, doc)
    }
    pub const fn int(key: &'static str, min: i64, max: i64, doc: &'static str) -> Self {
        Self::new(
            key,
            Ty::Int {
                min,
                max,
                hex: false,
            },
            doc,
        )
    }
    /// An integer usually written in hex (an address).
    pub const fn hex(key: &'static str, min: i64, max: i64, doc: &'static str) -> Self {
        Self::new(
            key,
            Ty::Int {
                min,
                max,
                hex: true,
            },
            doc,
        )
    }
    pub const fn choice(
        key: &'static str,
        options: &'static [&'static str],
        doc: &'static str,
    ) -> Self {
        Self::new(key, Ty::Choice(options), doc)
    }
    pub const fn str(key: &'static str, doc: &'static str) -> Self {
        Self::new(key, Ty::Str, doc)
    }
    pub const fn tables(key: &'static str, params: &'static [Param], doc: &'static str) -> Self {
        Self::new(key, Ty::Tables(params), doc)
    }
    pub const fn optional(self) -> Self {
        Self {
            need: Need::Optional,
            ..self
        }
    }
    pub const fn default_int(self, v: i64) -> Self {
        Self {
            need: Need::DefaultInt(v),
            ..self
        }
    }
    pub const fn default_str(self, v: &'static str) -> Self {
        Self {
            need: Need::DefaultStr(v),
            ..self
        }
    }

    pub fn required(&self) -> bool {
        matches!(self.need, Need::Required)
    }

    /// The type, as help text shows it.
    pub fn type_text(&self) -> String {
        match self.ty {
            Ty::Pin => "GPIO".into(),
            Ty::SharedPin => "GPIO (shareable)".into(),
            Ty::Int {
                min,
                max,
                hex: true,
            } => format!("{min:#x}..{max:#x}"),
            Ty::Int { min, max, .. } => format!("{min}..{max}"),
            Ty::Choice(options) => options.join(" | "),
            Ty::Str => "string".into(),
            Ty::Tables(_) => "array of tables".into(),
        }
    }

    pub fn need_text(&self) -> String {
        match self.need {
            Need::Required => "required".into(),
            Need::Optional => "optional".into(),
            Need::DefaultInt(v) => match self.ty {
                Ty::Int { hex: true, .. } => format!("= {v:#x}"),
                _ => format!("= {v}"),
            },
            Need::DefaultStr(v) => format!("= {v:?}"),
        }
    }
}

/// The closest of `options` to a misspelt `word`, if one is close enough.
pub fn suggest<'a>(word: &str, options: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.len() / 3).clamp(1, 3);
    options
        .into_iter()
        .map(|o| (distance(word, o), o))
        .filter(|&(d, _)| d <= limit)
        .min_by_key(|&(d, _)| d)
        .map(|(_, o)| o)
}

/// Edit distance, counting a swap of neighbours as one edit (optimal
/// string alignment).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let w = b.len() + 1;
    let mut d = vec![0; (a.len() + 1) * w];
    for i in 0..=a.len() {
        for j in 0..=b.len() {
            d[i * w + j] = if i == 0 || j == 0 {
                i + j
            } else {
                let mut best = (d[(i - 1) * w + j - 1] + (a[i - 1] != b[j - 1]) as usize)
                    .min(d[(i - 1) * w + j] + 1)
                    .min(d[i * w + j - 1] + 1);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    best = best.min(d[(i - 2) * w + j - 2] + 1);
                }
                best
            };
        }
    }
    d[a.len() * w + b.len()]
}

/// Help text for a kind: its summary and a table of its keys.
pub fn describe(kind: &Kind) -> String {
    let mut out = format!("{}: {}\n", kind.name, kind.summary);
    let mut rows: Vec<(String, String, String, &str)> = Vec::new();
    if let Some(bus) = kind.bus {
        rows.push((
            "bus".into(),
            format!("{}0 | {}1", bus.prefix(), bus.prefix()),
            "optional".into(),
            "the bus it sits on (declared under [buses])",
        ));
        if bus == BusType::Spi {
            rows.push((
                "cs".into(),
                "GPIO".into(),
                "optional".into(),
                "chip select, active low (none: always selected)",
            ));
        }
    }
    fn add(
        rows: &mut Vec<(String, String, String, &'static str)>,
        prefix: &str,
        params: &'static [Param],
    ) {
        for p in params {
            rows.push((
                format!("{prefix}{}", p.key),
                p.type_text(),
                p.need_text(),
                p.doc,
            ));
            if let Ty::Tables(inner) = p.ty {
                add(rows, &format!("{prefix}{}[].", p.key), inner);
            }
        }
    }
    add(&mut rows, "", kind.params);
    let w0 = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let w1 = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.2.len()).max().unwrap_or(0);
    for (key, ty, need, doc) in rows {
        let _ = writeln!(out, "  {key:w0$}  {ty:w1$}  {need:w2$}  {doc}");
    }
    out
}

/// A JSON value, just enough to write a schema.
enum J {
    Str(String),
    Int(i64),
    Bool(bool),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
    /// An object with keys only known at run time.
    Map(Vec<(String, J)>),
}

fn s(text: &str) -> J {
    J::Str(text.into())
}

impl J {
    fn write(&self, out: &mut String, indent: usize) {
        let pad = |out: &mut String, n: usize| out.extend(core::iter::repeat_n(' ', n));
        match self {
            J::Str(t) => {
                out.push('"');
                for c in t.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        c if (c as u32) < 0x20 => {
                            let _ = write!(out, "\\u{:04x}", c as u32);
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            J::Int(n) => {
                let _ = write!(out, "{n}");
            }
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Arr(items) if items.iter().all(|i| matches!(i, J::Str(_) | J::Int(_))) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, indent);
                }
                out.push(']');
            }
            J::Arr(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, indent + 2);
                    item.write(out, indent + 2);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push(']');
            }
            J::Obj(_) | J::Map(_) => {
                let entries: Vec<(&str, &J)> = match self {
                    J::Obj(e) => e.iter().map(|(k, v)| (*k, v)).collect(),
                    J::Map(e) => e.iter().map(|(k, v)| (k.as_str(), v)).collect(),
                    _ => unreachable!(),
                };
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (k, v)) in entries.iter().enumerate() {
                    pad(out, indent + 2);
                    s(k).write(out, indent + 2);
                    out.push_str(": ");
                    v.write(out, indent + 2);
                    out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

fn pin_schema(doc: &str) -> J {
    J::Obj(vec![
        ("type", s("integer")),
        ("minimum", J::Int(0)),
        ("maximum", J::Int(NUM_PINS as i64 - 1)),
        ("description", s(doc)),
    ])
}

fn param_schema(p: &Param) -> J {
    let doc = match p.need {
        Need::Required | Need::Optional => String::from(p.doc),
        _ => format!("{} (default {})", p.doc, &p.need_text()[2..]),
    };
    match p.ty {
        Ty::Pin => pin_schema(&doc),
        Ty::SharedPin => pin_schema(&format!("{doc} (other parts may use this GPIO too)")),
        Ty::Int { min, max, .. } => J::Obj(vec![
            ("type", s("integer")),
            ("minimum", J::Int(min)),
            ("maximum", J::Int(max)),
            ("description", J::Str(doc)),
        ]),
        Ty::Choice(options) => J::Obj(vec![
            ("enum", J::Arr(options.iter().map(|o| s(o)).collect())),
            ("description", J::Str(doc)),
        ]),
        Ty::Str => J::Obj(vec![("type", s("string")), ("description", J::Str(doc))]),
        Ty::Tables(inner) => J::Obj(vec![
            ("type", s("array")),
            ("items", table_schema(inner, Vec::new())),
            ("description", J::Str(doc)),
        ]),
    }
}

/// A closed object with `params` (plus `extra` keys).
fn table_schema(params: &[Param], extra: Vec<(String, J)>) -> J {
    let mut props = extra;
    props.extend(
        params
            .iter()
            .map(|p| (String::from(p.key), param_schema(p))),
    );
    let required: Vec<J> = params
        .iter()
        .filter(|p| p.required())
        .map(|p| s(p.key))
        .collect();
    let mut o = vec![("type", s("object")), ("properties", J::Map(props))];
    if !required.is_empty() {
        o.push(("required", J::Arr(required)));
    }
    o.push(("additionalProperties", J::Bool(false)));
    J::Obj(o)
}

/// A JSON Schema (draft 7) for board files using the kinds in `registry`,
/// for editors such as VS Code's Even Better TOML (taplo): completion,
/// hover docs and checking as you type.
pub fn json_schema(registry: &Registry) -> String {
    let buses = BUS_KINDS
        .iter()
        .map(|(name, keys)| {
            let props = keys
                .iter()
                .map(|k| (String::from(*k), pin_schema("GPIO")))
                .collect();
            (
                String::from(*name),
                J::Obj(vec![
                    ("type", s("object")),
                    ("properties", J::Map(props)),
                    ("additionalProperties", J::Bool(false)),
                ]),
            )
        })
        .collect();
    let kinds: Vec<&Kind> = registry.kinds().collect();
    let per_kind: Vec<J> = kinds
        .iter()
        .map(|k| {
            let mut extra = vec![(
                String::from("kind"),
                J::Obj(vec![("const", s(k.name)), ("description", s(k.summary))]),
            )];
            if let Some(bus) = k.bus {
                let names = [0, 1].map(|n| J::Str(format!("{}{n}", bus.prefix())));
                extra.push((
                    "bus".into(),
                    J::Obj(vec![
                        ("enum", J::Arr(names.into())),
                        (
                            "description",
                            s("the bus it sits on (declared under [buses])"),
                        ),
                    ]),
                ));
                if bus == BusType::Spi {
                    extra.push((
                        "cs".into(),
                        pin_schema("chip select, active low (none: always selected)"),
                    ));
                }
            }
            let cond = J::Obj(vec![(
                "properties",
                J::Obj(vec![("kind", J::Obj(vec![("const", s(k.name))]))]),
            )]);
            J::Obj(vec![("if", cond), ("then", table_schema(k.params, extra))])
        })
        .collect();
    let kind_names = J::Arr(kinds.iter().map(|k| s(k.name)).collect());
    let kind_docs: String = kinds
        .iter()
        .map(|k| format!("\n- {}: {}", k.name, k.summary))
        .collect();
    let part = J::Obj(vec![
        ("type", s("object")),
        ("required", J::Arr(vec![s("kind")])),
        (
            "properties",
            J::Obj(vec![(
                "kind",
                J::Obj(vec![
                    ("enum", kind_names),
                    (
                        "description",
                        J::Str(format!("what the part is:{kind_docs}")),
                    ),
                ]),
            )]),
        ),
        ("allOf", J::Arr(per_kind)),
    ]);
    let cell = |doc: &str, min: i64| {
        J::Obj(vec![
            ("type", s("array")),
            (
                "items",
                J::Obj(vec![("type", s("integer")), ("minimum", J::Int(min))]),
            ),
            ("minItems", J::Int(2)),
            ("maxItems", J::Int(2)),
            ("description", s(doc)),
        ])
    };
    let item = J::Obj(vec![
        ("type", s("object")),
        ("required", J::Arr(vec![s("at")])),
        (
            "properties",
            J::Obj(vec![
                ("at", cell("top-left grid cell [column, row], from 0", 0)),
                (
                    "span",
                    cell("cells covered [columns, rows] (default [1, 1])", 1),
                ),
                (
                    "label",
                    J::Obj(vec![
                        ("type", s("string")),
                        ("description", s("caption (default: the part's name)")),
                    ]),
                ),
            ]),
        ),
        (
            "additionalProperties",
            J::Obj(vec![
                (
                    "type",
                    J::Arr(vec![s("string"), s("integer"), s("boolean"), s("array")]),
                ),
                (
                    "description",
                    s("a widget option for the host (rotate, key, color, ...)"),
                ),
            ]),
        ),
    ]);
    let values = |bits: u32| {
        J::Obj(vec![
            ("type", s("array")),
            ("minItems", J::Int(1)),
            (
                "items",
                J::Obj(vec![
                    ("type", s("integer")),
                    ("minimum", J::Int(0)),
                    ("maximum", J::Int((1i64 << bits) - 1)),
                ]),
            ),
            (
                "description",
                J::Str(format!("the values, {bits}-bit little endian (one of u8, u16, u32)")),
            ),
        ])
    };
    let flash = J::Obj(vec![
        ("type", s("object")),
        (
            "properties",
            J::Obj(vec![
                (
                    "addr",
                    J::Obj(vec![
                        ("type", s("integer")),
                        ("minimum", J::Int(0x1000_0000)),
                        ("maximum", J::Int(0x10ff_ffff)),
                        ("description", s("where the values go, in the XIP window")),
                    ]),
                ),
                ("u8", values(8)),
                ("u16", values(16)),
                ("u32", values(32)),
            ]),
        ),
        ("required", J::Arr(vec![s("addr")])),
        ("minProperties", J::Int(2)),
        ("maxProperties", J::Int(2)),
        ("additionalProperties", J::Bool(false)),
    ]);
    let schema = J::Obj(vec![
        ("$schema", s("http://json-schema.org/draft-07/schema#")),
        ("title", s("Tartelette board")),
        (
            "description",
            s(
                "A PCB around the chip: buses, the parts on them and a front panel layout. Generated by `tartelette schema`.",
            ),
        ),
        ("type", s("object")),
        (
            "properties",
            J::Obj(vec![
                (
                    "name",
                    J::Obj(vec![
                        ("type", s("string")),
                        ("description", s("the board's (or shield's) name")),
                    ]),
                ),
                (
                    "buses",
                    J::Obj(vec![
                        ("type", s("object")),
                        (
                            "description",
                            s("the chip's bus controllers wired on this board, and their pins"),
                        ),
                        ("properties", J::Map(buses)),
                        ("additionalProperties", J::Bool(false)),
                    ]),
                ),
                (
                    "parts",
                    J::Obj(vec![
                        ("type", s("object")),
                        ("description", s("the devices on the board, by name")),
                        ("additionalProperties", part),
                    ]),
                ),
                (
                    "panel",
                    J::Obj(vec![
                        ("type", s("object")),
                        (
                            "description",
                            s(
                                "where a front panel puts each part's controls, on a grid, by part name",
                            ),
                        ),
                        (
                            "additionalProperties",
                            J::Obj(vec![(
                                "anyOf",
                                J::Arr(vec![
                                    item,
                                    J::Obj(vec![
                                        ("type", s("array")),
                                        (
                                            "items",
                                            J::Obj(vec![(
                                                "$ref",
                                                s(
                                                    "#/properties/panel/additionalProperties/anyOf/0",
                                                ),
                                            )]),
                                        ),
                                    ]),
                                ]),
                            )]),
                        ),
                    ]),
                ),
                (
                    "flash",
                    J::Obj(vec![
                        ("type", s("array")),
                        (
                            "description",
                            s(
                                "what the flash holds before the firmware goes on (factory settings, calibration), as [[flash]] entries",
                            ),
                        ),
                        ("items", flash),
                    ]),
                ),
            ]),
        ),
        ("additionalProperties", J::Bool(false)),
    ]);
    let mut out = String::new();
    schema.write(&mut out, 0);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_catch_typos_only() {
        assert_eq!(suggest("colour", ["color", "gpio"]), Some("color"));
        assert_eq!(suggest("gpi", ["color", "gpio"]), Some("gpio"));
        assert_eq!(suggest("width", ["color", "gpio"]), None);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("gpoi", "gpio"), 1);
    }

    #[test]
    fn describe_lists_keys_with_types_and_defaults() {
        const K: Kind = Kind {
            name: "thing",
            summary: "a thing",
            bus: Some(BusType::I2c),
            params: &[
                Param::pin("gpio", "its pin"),
                Param::hex("addr", 0x20, 0x27, "address").default_int(0x20),
                Param::choice("mode", &["a", "b"], "mode").optional(),
            ],
            build: |_| Err("no".into()),
        };
        let text = describe(&K);
        assert!(text.starts_with("thing: a thing\n"), "{text}");
        assert!(
            text.contains("\n  gpio  GPIO         required  its pin\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  addr  0x20..0x27   = 0x20    address\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  mode  a | b        optional  mode\n"),
            "{text}"
        );
    }
}
