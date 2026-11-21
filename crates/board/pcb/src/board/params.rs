//! A part's parameters, checked against its kind's [`Param`] list.
//!
//! [`Params::check`] rejects unknown keys (suggesting the closest known one),
//! missing required keys, values of the wrong type or out of range; the
//! getters then can't fail. Asking for a key the kind did not declare is a
//! bug in the device model and panics.
use alloc::{format, string::String, vec::Vec};

use super::{
    doc::{self, Key, Node, Span, V},
    spec::{Need, Param, Ty, suggest},
};
use crate::NUM_PINS;

pub struct Params {
    /// The part's kind and name (`aps[0]` for a nested table).
    pub kind: String,
    pub name: String,
    spec: &'static [Param],
    values: Vec<Value>,
    /// GPIOs the values name, for the board's pin check.
    pub(crate) pins: Vec<PinClaim>,
}

struct Value {
    key: &'static str,
    v: Val,
    read: bool,
}

enum Val {
    Int(i64),
    Str(String),
    Tables(Vec<Params>),
}

/// A GPIO named in a part's table.
pub(crate) struct PinClaim {
    pub pin: usize,
    /// `int`, `aps[1].x`: the key that named it.
    pub key: String,
    pub shared: bool,
    pub span: Span,
}

impl Params {
    /// Validate `entries` (a table's keys, minus those the board handles
    /// itself) against `spec`. `at` is where the table is, for errors about
    /// missing keys.
    pub(crate) fn check(
        kind: &str,
        name: &str,
        spec: &'static [Param],
        entries: Vec<(Key, Node)>,
        at: &Span,
    ) -> Result<Self, doc::Error> {
        let mut p = Params {
            kind: kind.into(),
            name: name.into(),
            spec,
            values: Vec::new(),
            pins: Vec::new(),
        };
        for (key, node) in entries {
            let Some(param) = spec.iter().find(|s| s.key == key.name) else {
                let hint = match suggest(&key.name, spec.iter().map(|s| s.key)) {
                    Some(k) => format!(" (did you mean {k}?)"),
                    None if spec.is_empty() => format!(" ({kind} takes no parameters)"),
                    None => format!(
                        " ({kind} takes {})",
                        spec.iter().map(|s| s.key).collect::<Vec<_>>().join(", ")
                    ),
                };
                return Err(doc::err(
                    &key.span,
                    format!("unknown parameter {}{hint}", key.name),
                ));
            };
            let v = p.value(param, node)?;
            p.values.push(Value {
                key: param.key,
                v,
                read: false,
            });
        }
        if let Some(missing) = spec
            .iter()
            .find(|s| s.required() && !p.values.iter().any(|v| v.key == s.key))
        {
            return Err(doc::err(
                at,
                format!("missing {} ({})", missing.key, missing.doc),
            ));
        }
        Ok(p)
    }

    fn value(&mut self, param: &Param, node: Node) -> Result<Val, doc::Error> {
        let key = param.key;
        let what = node.v.what();
        let span = node.span.clone();
        let bad = |want: &str| doc::err(&span, format!("{key} must be {want}, not {what}"));
        match (param.ty, node.v) {
            (Ty::Pin | Ty::SharedPin, V::Int(n)) => {
                if !(0..NUM_PINS as i64).contains(&n) {
                    return Err(doc::err(
                        &node.span,
                        format!("{key} must be a GPIO, 0..{}", NUM_PINS - 1),
                    ));
                }
                let shared = matches!(param.ty, Ty::SharedPin);
                self.pins.push(PinClaim {
                    pin: n as usize,
                    key: key.into(),
                    shared,
                    span: node.span,
                });
                Ok(Val::Int(n))
            }
            (Ty::Pin | Ty::SharedPin, _) => Err(bad("a GPIO number")),
            (Ty::Int { min, max, hex }, V::Int(n)) => {
                if !(min..=max).contains(&n) {
                    let range = if hex {
                        format!("{min:#x}..{max:#x}")
                    } else {
                        format!("{min}..{max}")
                    };
                    return Err(doc::err(&node.span, format!("{key} must be {range}")));
                }
                Ok(Val::Int(n))
            }
            (Ty::Int { .. }, _) => Err(bad("a number")),
            (Ty::Choice(options), V::Str(s)) => {
                if !options.contains(&s.as_str()) {
                    let hint = suggest(&s, options.iter().copied())
                        .map(|o| format!(" (did you mean {o}?)"))
                        .unwrap_or_default();
                    return Err(doc::err(
                        &node.span,
                        format!("{key} must be {}{hint}", one_of(options)),
                    ));
                }
                Ok(Val::Str(s))
            }
            (Ty::Choice(options), _) => Err(bad(&one_of(options))),
            (Ty::Str, V::Str(s)) => Ok(Val::Str(s)),
            (Ty::Str, _) => Err(bad("a string")),
            (Ty::Tables(inner), V::Array(items)) => {
                let mut tables = Vec::new();
                for (i, item) in items.into_iter().enumerate() {
                    let name = format!("{key}[{i}]");
                    let V::Table(entries) = item.v else {
                        return Err(doc::err(
                            &item.span,
                            format!("{name} must be a table, not {}", item.v.what()),
                        ));
                    };
                    let t = Params::check(&self.kind, &name, inner, entries, &item.span).map_err(
                        |e| doc::Error {
                            msg: format!("{name}: {}", e.msg),
                            ..e
                        },
                    )?;
                    self.pins.extend(t.pins.iter().map(|c| PinClaim {
                        key: format!("{name}.{}", c.key),
                        span: c.span.clone(),
                        ..*c
                    }));
                    tables.push(t);
                }
                Ok(Val::Tables(tables))
            }
            (Ty::Tables(_), _) => Err(bad("an array of tables")),
        }
    }

    fn get(&mut self, key: &str) -> (Option<&mut Val>, Need) {
        let Some(param) = self.spec.iter().find(|s| s.key == key) else {
            panic!(
                "{}: asked for {key:?}, which it does not declare",
                self.kind
            );
        };
        let need = param.need;
        let v = self.values.iter_mut().find(|v| v.key == key).map(|v| {
            v.read = true;
            &mut v.v
        });
        (v, need)
    }

    /// A `Pin` or `SharedPin` value, if given.
    pub fn opt_pin(&mut self, key: &str) -> Option<usize> {
        self.opt_int(key).map(|n| n as usize)
    }

    /// A required `Pin` or `SharedPin`.
    pub fn pin(&mut self, key: &str) -> usize {
        self.opt_pin(key)
            .unwrap_or_else(|| panic!("{}: {key} is not required", self.kind))
    }

    /// An integer as given, without its default.
    pub fn opt_int(&mut self, key: &str) -> Option<i64> {
        match self.get(key).0 {
            Some(Val::Int(n)) => Some(*n),
            None => None,
            Some(_) => panic!("{}: {key} is not a number", self.kind),
        }
    }

    /// An integer, or its default.
    pub fn int(&mut self, key: &str) -> i64 {
        let kind = self.kind.clone();
        match self.get(key) {
            (Some(Val::Int(n)), _) => *n,
            (None, Need::DefaultInt(d)) => d,
            _ => panic!("{kind}: {key} is neither a number nor has a default"),
        }
    }

    /// A string or choice as given, without its default.
    pub fn opt_str(&mut self, key: &str) -> Option<&str> {
        match self.get(key).0 {
            Some(Val::Str(s)) => Some(s.as_str()),
            None => None,
            Some(_) => panic!("{key} is not a string"),
        }
    }

    /// A string or choice, or its default.
    pub fn str(&mut self, key: &str) -> &str {
        let kind = self.kind.clone();
        match self.get(key) {
            (Some(Val::Str(s)), _) => s.as_str(),
            (None, Need::DefaultStr(d)) => d,
            _ => panic!("{kind}: {key} is neither a string nor has a default"),
        }
    }

    /// The tables of a `Tables` key (none if not given).
    pub fn tables(&mut self, key: &str) -> Vec<Params> {
        match self.get(key).0 {
            Some(Val::Tables(t)) => core::mem::take(t),
            None => Vec::new(),
            Some(_) => panic!("{key} is not an array of tables"),
        }
    }

    /// Keys that were given but that the device never looked at: a device
    /// model bug (its setting would be silently ignored).
    pub(crate) fn unread(&self) -> Option<&'static str> {
        self.values.iter().find(|v| !v.read).map(|v| v.key)
    }
}

fn one_of(options: &[&str]) -> String {
    match options {
        [a, b] => format!("{a} or {b}"),
        _ => format!("one of {}", options.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::doc;

    const SPEC: &[Param] = &[
        Param::pin("gpio", "its pin"),
        Param::shared_pin("int", "interrupt").optional(),
        Param::hex("addr", 0x20, 0x27, "address").default_int(0x20),
        Param::int("n", 0, 5, "how many").optional(),
        Param::choice("active", &["high", "low"], "level").default_str("high"),
        Param::str("label", "text").optional(),
        Param::tables(
            "items",
            &[
                Param::str("id", "an id"),
                Param::pin("pin", "a pin").optional(),
            ],
            "things",
        )
        .optional(),
    ];

    fn check(text: &str) -> Result<Params, String> {
        let root = doc::parse(text).map_err(|e| e.msg)?;
        let V::Table(entries) = root.v else {
            unreachable!()
        };
        Params::check("k", "n", SPEC, entries, &(0..0)).map_err(|e| e.msg)
    }

    #[test]
    fn typed_getters_and_defaults() {
        let mut p = check("gpio = 5\naddr = 0x21\nactive = \"low\"\nitems = [{ id = \"a\", pin = 7 }, { id = \"b\" }]").unwrap();
        assert_eq!(p.pin("gpio"), 5);
        assert_eq!(p.int("addr"), 0x21);
        assert_eq!(p.opt_int("n"), None);
        assert_eq!(p.str("active"), "low");
        assert_eq!(p.opt_str("label"), None);
        let mut items = p.tables("items");
        assert_eq!(items[0].opt_pin("pin"), Some(7));
        assert_eq!(items[0].str("id"), "a");
        assert_eq!(items[1].opt_pin("pin"), None);
        let claimed: Vec<_> = p
            .pins
            .iter()
            .map(|c| (c.pin, c.key.as_str(), c.shared))
            .collect();
        assert_eq!(claimed, [(5, "gpio", false), (7, "items[0].pin", false)]);
        let mut d = check("gpio = 1\nint = 2").unwrap();
        assert_eq!((d.int("addr"), d.str("active")), (0x20, "high"));
        assert_eq!(d.unread(), Some("gpio"));
        d.pin("gpio");
        assert_eq!(d.unread(), Some("int"));
        assert!(d.pins[1].shared);
    }

    #[test]
    fn errors() {
        let e = |t: &str| check(t).err().unwrap();
        assert_eq!(e("gpio = 30"), "gpio must be a GPIO, 0..29");
        assert_eq!(
            e("gpio = \"x\""),
            "gpio must be a GPIO number, not a string"
        );
        assert_eq!(e("addr = 0x30\ngpio = 1"), "addr must be 0x20..0x27");
        assert_eq!(e("gpio = 1\nn = 9"), "n must be 0..5");
        assert_eq!(
            e("gpio = 1\nactive = \"hig\""),
            "active must be high or low (did you mean high?)"
        );
        assert_eq!(
            e("gpio = 1\ncolour = 1"),
            "unknown parameter colour (k takes gpio, int, addr, n, active, label, items)"
        );
        assert_eq!(e("gpoi = 1"), "unknown parameter gpoi (did you mean gpio?)");
        assert_eq!(e("addr = 0x20"), "missing gpio (its pin)");
        assert_eq!(
            e("gpio = 1\nitems = [{ pin = 2 }]"),
            "items[0]: missing id (an id)"
        );
        assert_eq!(
            e("gpio = 1\nitems = [1]"),
            "items[0] must be a table, not a number"
        );
        assert_eq!(
            e("gpio = 1\nlabel = 2.5"),
            "label must be a string, not a decimal number"
        );
    }

    #[test]
    #[should_panic(expected = "does not declare")]
    fn undeclared_keys_are_a_model_bug() {
        check("gpio = 1").unwrap().opt_int("nope");
    }
}
