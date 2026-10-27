//! The `[panel]` section: where a host UI puts each part's controls.
//!
//! ```toml
//! [panel]
//! screen = { at = [0, 0], span = [4, 3], rotate = 90 }
//! knob = { at = [4, 0], span = [2, 2], keys = ["Left", "Right"] }
//! expander = [{ at = [0, 3], label = "Inputs" }, { at = [0, 4], label = "Outputs" }]
//! ```
//!
//! The panel is a grid of equal cells; `at` is the top-left cell (0-based)
//! and `span` how many cells the part covers (default `[1, 1]`). Items may
//! not overlap. A part may have several items (an array), each with its own
//! options. Any other key is a widget option, kept as text for the host to
//! interpret (`rotate = 90` for a display, arrays joined with commas); the
//! host reports the ones it doesn't know.
//!
//! The board has no opinion on what a part looks like: this is only layout.
use super::doc::{self, Node, V};
use alloc::{format, string::String, vec::Vec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelItem {
    /// Index into [`Board::parts`](super::Board::parts).
    pub part: usize,
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
    pub label: Option<String>,
    /// Widget options, in board-file order.
    pub options: Vec<(String, String)>,
}

impl PanelItem {
    /// Value of widget option `key`.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub(super) fn overlaps(&self, o: &PanelItem) -> bool {
        self.col < o.col + o.cols && o.col < self.col + self.cols && self.row < o.row + o.rows && o.row < self.row + self.rows
    }
}

/// Grid positions and sizes are capped so a typo can't ask for a huge grid.
const MAX_CELLS: u16 = 64;

/// `[N, M]`, both within `min..MAX_CELLS + min` (positions from 0, sizes from 1).
fn pair(key: &str, node: &Node, min: u16) -> Result<(u16, u16), doc::Error> {
    let max = MAX_CELLS - 1 + min;
    let err = || doc::err(&node.span, format!("{key} must be [N, M], each {min}..{max}"));
    let V::Array(items) = &node.v else { return Err(err()) };
    match items.as_slice() {
        [a, b] => {
            let get = |n: &Node| match n.v {
                V::Int(v) if (min as i64..=max as i64).contains(&v) => Ok(v as u16),
                _ => Err(err()),
            };
            Ok((get(a)?, get(b)?))
        }
        _ => Err(err()),
    }
}

/// An option value as text: scalars as written, arrays of them joined with commas.
fn text(key: &str, node: &Node) -> Result<String, doc::Error> {
    match &node.v {
        V::Int(n) => Ok(format!("{n}")),
        V::Str(s) => Ok(s.clone()),
        V::Bool(b) => Ok(format!("{b}")),
        V::Array(items) => Ok(items.iter().map(|i| text(key, i)).collect::<Result<Vec<_>, _>>()?.join(",")),
        v => Err(doc::err(&node.span, format!("{key} must be a number, string, true/false or an array of them, not {}", v.what()))),
    }
}

/// The items for part index `part` from its `[panel]` value: a table or an
/// array of tables.
pub(super) fn parse_items(part: usize, node: Node) -> Result<Vec<PanelItem>, doc::Error> {
    let tables = match node.v {
        V::Table(_) => alloc::vec![node],
        V::Array(items) => items,
        v => return Err(doc::err(&node.span, format!("a panel item must be a table or an array of tables, not {}", v.what()))),
    };
    tables.into_iter().map(|t| parse_item(part, t)).collect()
}

fn parse_item(part: usize, node: Node) -> Result<PanelItem, doc::Error> {
    let V::Table(entries) = node.v else {
        return Err(doc::err(&node.span, format!("a panel item must be a table, not {}", node.v.what())));
    };
    let mut item = PanelItem { part, col: 0, row: 0, cols: 1, rows: 1, label: None, options: Vec::new() };
    let mut at = false;
    for (key, value) in entries {
        match key.name.as_str() {
            "at" => {
                (item.col, item.row) = pair("at", &value, 0)?;
                at = true;
            }
            "span" => (item.cols, item.rows) = pair("span", &value, 1)?,
            "label" => match value.v {
                V::Str(s) => item.label = Some(s),
                v => return Err(doc::err(&value.span, format!("label must be a string, not {}", v.what()))),
            },
            _ => {
                let t = text(&key.name, &value)?;
                item.options.push((key.name, t));
            }
        }
    }
    if !at {
        return Err(doc::err(&node.span, "missing at = [COLUMN, ROW]"));
    }
    Ok(item)
}
