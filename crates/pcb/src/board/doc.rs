//! A parsed TOML document as a plain tree: every value keeps the byte span
//! it came from (for error messages), and tables keep their keys in file
//! order (the order parts are declared in is the order they are attached).
use alloc::{format, string::String, vec::Vec};
use core::ops::Range;

use toml::{
    Spanned,
    de::{DeTable, DeValue},
};

pub type Span = Range<usize>;

pub struct Node {
    pub span: Span,
    pub v: V,
}

pub enum V {
    Int(i64),
    Str(String),
    Bool(bool),
    Float,
    Datetime,
    Array(Vec<Node>),
    Table(Vec<(Key, Node)>),
}

pub struct Key {
    pub name: String,
    pub span: Span,
}

/// An error at a place in the text.
pub struct Error {
    pub span: Span,
    pub msg: String,
}

pub fn err(span: &Span, msg: impl Into<String>) -> Error {
    Error {
        span: span.clone(),
        msg: msg.into(),
    }
}

impl V {
    /// What this is, for "must be a ..." messages.
    pub fn what(&self) -> &'static str {
        match self {
            V::Int(_) => "a number",
            V::Str(_) => "a string",
            V::Bool(_) => "true or false",
            V::Float => "a decimal number",
            V::Datetime => "a date",
            V::Array(_) => "an array",
            V::Table(_) => "a table",
        }
    }
}

/// Parse a whole document; its root is a table.
pub fn parse(text: &str) -> Result<Node, Error> {
    let root = DeTable::parse(text).map_err(|e| Error {
        span: e.span().unwrap_or(0..0),
        msg: e.message().into(),
    })?;
    let span = root.span();
    Ok(Node {
        span,
        v: table(root.get_ref())?,
    })
}

fn table(t: &DeTable) -> Result<V, Error> {
    let mut entries = Vec::new();
    for (k, v) in t.iter() {
        entries.push((
            Key {
                name: k.get_ref().as_ref().into(),
                span: k.span(),
            },
            node(v)?,
        ));
    }
    entries.sort_by_key(|(k, _)| k.span.start);
    Ok(V::Table(entries))
}

fn node(v: &Spanned<DeValue>) -> Result<Node, Error> {
    let span = v.span();
    let v = match v.get_ref() {
        DeValue::Integer(i) => {
            let digits: String = i
                .as_str()
                .chars()
                .filter(|&c| c != '_' && c != '+')
                .collect();
            V::Int(
                i64::from_str_radix(&digits, i.radix())
                    .map_err(|_| err(&span, format!("{i} is out of range")))?,
            )
        }
        DeValue::String(s) => V::Str(s.as_ref().into()),
        DeValue::Boolean(b) => V::Bool(*b),
        DeValue::Float(_) => V::Float,
        DeValue::Datetime(_) => V::Datetime,
        DeValue::Array(a) => V::Array(a.iter().map(node).collect::<Result<_, _>>()?),
        DeValue::Table(t) => table(t)?,
    };
    Ok(Node { span, v })
}

/// 1-based line and column of byte `offset` in `text`.
pub fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}
