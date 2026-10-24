//! `panel` lines: where a host UI puts each part's controls.
//!
//! ```text
//! panel <part> at=COL,ROW [span=W,H] [label=Some_text] [option=value...]
//! ```
//!
//! The panel is a grid of equal cells; `at` is the top-left cell (0-based)
//! and `span` how many cells the part covers (default 1,1). Underscores in a
//! `label` become spaces. Items may not overlap. A part may have several
//! items (e.g. a port expander's inputs and outputs shown apart), each with
//! its own options. Any other `key=value` is a widget option, kept as given for the
//! host to interpret (e.g. `rotate=90` for a display); the host reports the
//! ones it doesn't know, using [`PanelItem::line`].
//!
//! The board has no opinion on what a part looks like: this is only layout.
use super::Params;
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
    /// 1-based board-file line, for host-side error messages.
    pub line: usize,
}

impl PanelItem {
    /// Value of widget option `key`.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn overlaps(&self, o: &PanelItem) -> bool {
        self.col < o.col + o.cols && o.col < self.col + self.cols && self.row < o.row + o.rows && o.row < self.row + self.rows
    }
}

/// Grid positions and sizes are capped so a typo can't ask for a huge grid.
const MAX_CELLS: u16 = 64;

/// `key=N,M`, both within `min..MAX_CELLS + min` (positions from 0, sizes from 1).
fn pair(p: &mut Params, key: &str, min: u16) -> Result<Option<(u16, u16)>, String> {
    let Some(v) = p.opt_str(key) else { return Ok(None) };
    let err = || format!("{key} must be N,M");
    let (a, b) = v.split_once(',').ok_or_else(err)?;
    let (a, b): (u16, u16) = (a.parse().map_err(|_| err())?, b.parse().map_err(|_| err())?);
    let max = MAX_CELLS - 1 + min;
    if !(min..=max).contains(&a) || !(min..=max).contains(&b) {
        return Err(format!("{key} values must be {min}..{max}"));
    }
    Ok(Some((a, b)))
}

/// Parse the words after `panel` into an item for part index `part`.
pub(super) fn parse_item(mut p: Params, part: usize, line: usize, existing: &[PanelItem]) -> Result<PanelItem, String> {
    let (col, row) = pair(&mut p, "at", 0)?.ok_or("missing at=COL,ROW")?;
    let (cols, rows) = pair(&mut p, "span", 1)?.unwrap_or((1, 1));
    let label = p.opt_str("label").map(|l| l.replace('_', " "));
    let item = PanelItem { part, col, row, cols, rows, label, options: p.into_unused(), line };
    if let Some(o) = existing.iter().find(|o| o.overlaps(&item)) {
        return Err(format!("overlaps the panel item on line {}", o.line));
    }
    Ok(item)
}
