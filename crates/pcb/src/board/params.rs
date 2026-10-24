//! `key=value` parameters of one board-file line, with typed getters.
//!
//! Every getter marks its key as used; [`Params::finish`] then rejects any
//! key nobody asked for, so typos in a board file are errors, not silently
//! ignored settings.
use crate::NUM_PINS;
use alloc::{format, string::String, vec::Vec};

pub struct Params {
    /// The part's kind (`led`, `mcp23008`, ...) and its unique name.
    pub kind: String,
    pub name: String,
    items: Vec<Item>,
}

struct Item {
    key: String,
    value: String,
    used: bool,
}

impl Params {
    pub fn new(kind: &str, name: &str) -> Self {
        Self { kind: kind.into(), name: name.into(), items: Vec::new() }
    }

    /// Parse `key=value` words.
    pub fn with_words<'a>(mut self, words: impl Iterator<Item = &'a str>) -> Result<Self, String> {
        for w in words {
            let (key, value) = w.split_once('=').ok_or_else(|| format!("expected key=value, got {w:?}"))?;
            if self.items.iter().any(|i| i.key == key) {
                return Err(format!("duplicate parameter {key}"));
            }
            self.items.push(Item { key: key.into(), value: value.into(), used: false });
        }
        Ok(self)
    }

    /// Raw value of `key`, if given.
    pub fn opt_str(&mut self, key: &str) -> Option<&str> {
        let item = self.items.iter_mut().find(|i| i.key == key)?;
        item.used = true;
        Some(&item.value)
    }

    pub fn str(&mut self, key: &str) -> Result<&str, String> {
        let msg = format!("missing {key}=...");
        self.opt_str(key).ok_or(msg)
    }

    /// A GPIO number.
    pub fn opt_pin(&mut self, key: &str) -> Result<Option<usize>, String> {
        let Some(v) = self.opt_str(key) else { return Ok(None) };
        let n: usize = v.parse().map_err(|_| format!("{key} must be a number"))?;
        if n >= NUM_PINS {
            return Err(format!("{key} must be 0..{}", NUM_PINS - 1));
        }
        Ok(Some(n))
    }

    pub fn pin(&mut self, key: &str) -> Result<usize, String> {
        self.opt_pin(key)?.ok_or_else(|| format!("missing {key}=N"))
    }

    /// An integer, decimal or `0x` hex, within `min..=max`.
    pub fn opt_int(&mut self, key: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
        let Some(v) = self.opt_str(key) else { return Ok(None) };
        let parsed = match v.strip_prefix("0x") {
            Some(h) => i64::from_str_radix(h, 16),
            None => v.parse(),
        };
        let n = parsed.map_err(|_| format!("{key} must be a number"))?;
        if !(min..=max).contains(&n) {
            return Err(format!("{key} must be {min}..{max}"));
        }
        Ok(Some(n))
    }

    /// `key=high` / `key=low` as a bool (true = high).
    pub fn opt_level(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.opt_str(key) {
            None => Ok(None),
            Some("high") => Ok(Some(true)),
            Some("low") => Ok(Some(false)),
            Some(_) => Err(format!("{key} must be high or low")),
        }
    }

    /// The parameters no getter looked at, for callers that pass them on
    /// instead of rejecting them.
    pub fn into_unused(self) -> Vec<(String, String)> {
        self.items.into_iter().filter(|i| !i.used).map(|i| (i.key, i.value)).collect()
    }

    /// Error out on any parameter no getter looked at.
    pub fn finish(self) -> Result<(), String> {
        match self.items.iter().find(|i| !i.used) {
            Some(i) => Err(format!("unknown parameter {}", i.key)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(words: &str) -> Params {
        Params::new("k", "n").with_words(words.split_whitespace()).unwrap()
    }

    #[test]
    fn typed_getters() {
        let mut q = p("gpio=5 addr=0x20 n=7 active=low name=x");
        assert_eq!(q.pin("gpio"), Ok(5));
        assert_eq!(q.opt_int("addr", 0, 127), Ok(Some(0x20)));
        assert_eq!(q.opt_int("n", 0, 5), Err("n must be 0..5".into()));
        assert_eq!(q.opt_level("active"), Ok(Some(false)));
        assert_eq!(q.opt_pin("int"), Ok(None));
        assert_eq!(q.str("name"), Ok("x"));
        assert_eq!(q.str("nope"), Err("missing nope=...".into()));
    }

    #[test]
    fn pin_errors() {
        assert_eq!(p("gpio=x").pin("gpio"), Err("gpio must be a number".into()));
        assert_eq!(p("gpio=30").pin("gpio"), Err("gpio must be 0..29".into()));
        assert_eq!(p("").pin("gpio"), Err("missing gpio=N".into()));
    }

    #[test]
    fn unknown_and_malformed_keys() {
        let mut q = p("gpio=1 colour=red");
        q.pin("gpio").unwrap();
        assert_eq!(q.finish(), Err("unknown parameter colour".into()));
        assert!(Params::new("k", "n").with_words(["bare"].into_iter()).is_err());
        assert!(Params::new("k", "n").with_words(["a=1", "a=2"].into_iter()).is_err());
        let mut ok = p("a=1");
        ok.opt_str("a");
        assert_eq!(ok.finish(), Ok(()));
    }
}
