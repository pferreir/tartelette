//! Files named in the page's URL, fetched before the app starts, so that a
//! page can open straight onto a firmware:
//!
//! ```text
//! index.html?firmware=fw.elf&board=my-board.toml&sd=card.img
//! ```
//!
//! `firmware` and `sd` are URLs, relative to the page. `board` and `shield`
//! (which may be repeated) are a built-in name or else a URL. `sd` goes to
//! the board's first SD card. The files a board from a URL names (a `wav`
//! part's `file`) are fetched relative to it. `osc` (repeatable) and `drums` patch the test
//! synth as `--osc` and `--drums` do (`drums` takes no value), e.g.
//! `&drums&osc=saw:dac.A:expander.4`.
use eframe::{
    wasm_bindgen::{JsCast as _, JsValue},
    web_sys,
};
use pcb_devices::wav::Wav;
use tartelette_runtime::{Source, boards, synth::Patch};
use wasm_bindgen_futures::JsFuture;

use crate::{app::App, files::Slot};

#[derive(Default)]
pub struct Preload {
    pub firmware: Option<Source>,
    pub board: Option<Source>,
    pub shields: Vec<Source>,
    pub sd: Option<Source>,
    /// WAV files for the board's players, by part name.
    pub wavs: Vec<(String, Source)>,
    pub osc: Vec<Patch>,
    pub drums: bool,
    pub errors: Vec<String>,
}

impl Preload {
    /// Fetch what the page's query string names.
    pub async fn from_page() -> Self {
        let mut p = Self::default();
        let Some(query) = web_sys::window().and_then(|w| w.location().search().ok()) else {
            return p;
        };
        let Ok(params) = web_sys::UrlSearchParams::new_with_str(&query) else {
            return p;
        };
        let get = |key: &str| params.get(key).filter(|v| !v.is_empty());
        if let Some(url) = get("firmware") {
            p.firmware = p.take(fetch(&url).await);
        }
        if let Some(name) = get("board") {
            p.board = p.take(named(boards::BOARDS, &name).await);
            if boards::builtin(boards::BOARDS, &name).is_none()
                && let Some(board) = p.board.clone()
            {
                p.board_files(&name, &board).await;
            }
        }
        for name in params
            .get_all("shield")
            .iter()
            .filter_map(|v| v.as_string())
        {
            if let Some(s) = p.take(named(boards::SHIELDS, &name).await) {
                p.shields.push(s);
            }
        }
        if let Some(url) = get("sd") {
            p.sd = p.take(fetch(&url).await);
        }
        for v in params.get_all("osc").iter().filter_map(|v| v.as_string()) {
            match v.parse() {
                Ok(patch) => p.osc.push(patch),
                Err(e) => p.errors.push(format!("osc={v}: {e}")),
            }
        }
        p.drums = params.has("drums");
        p
    }

    /// Fetch the WAV files the board at `url` names, relative to it.
    async fn board_files(&mut self, url: &str, board: &Source) {
        let Some(b) = self.take_err(boards::parse_board(Some(board), &[])) else {
            return;
        };
        let dir = url.split(['?', '#']).next().unwrap_or(url);
        let dir = dir.rfind('/').map_or("", |i| &dir[..=i]);
        for part in &b.parts {
            let any: &dyn std::any::Any = &*part.device;
            let Some(file) = any.downcast_ref::<Wav>().and_then(|w| w.file.as_ref()) else {
                continue;
            };
            if let Some(src) = self.take(fetch(&format!("{dir}{file}")).await) {
                self.wavs.push((part.name.clone(), src));
            }
        }
    }

    fn take_err<T>(&mut self, r: Result<T, String>) -> Option<T> {
        r.map_err(|e| self.errors.push(e)).ok()
    }

    fn take(&mut self, r: Result<Source, String>) -> Option<Source> {
        r.map_err(|e| self.errors.push(e)).ok()
    }

    pub fn into_app(self) -> App {
        let mut app = App::new(self.firmware, self.board, self.shields, Default::default());
        app.set_voices(&self.osc, self.drums);
        if let Some(sd) = self.sd {
            app.open(Slot::Sd(String::new()), sd);
        }
        for (part, src) in self.wavs {
            app.open(Slot::Wav(part), src);
        }
        for e in self.errors {
            crate::files::log(&e);
            app.report(e);
        }
        app
    }
}

/// A built-in board or shield, or else the file at that URL.
async fn named(list: &[(&str, &str)], name: &str) -> Result<Source, String> {
    match boards::builtin(list, name) {
        Some(src) => Ok(src),
        None => fetch(name).await,
    }
}

async fn fetch(url: &str) -> Result<Source, String> {
    let fail = |e: JsValue| format!("{url}: {}", js_error(&e));
    let window = web_sys::window().ok_or("no window")?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(fail)?
        .unchecked_into();
    if !resp.ok() {
        return Err(format!(
            "{url}: HTTP {} {}",
            resp.status(),
            resp.status_text()
        ));
    }
    let buf = JsFuture::from(resp.array_buffer().map_err(fail)?)
        .await
        .map_err(fail)?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    Ok(Source::new(file_name(url), bytes))
}

/// The last path segment of a URL: `a/b/os.elf?v=3` is `os.elf`.
fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.is_empty() {
        url.into()
    } else {
        name.into()
    }
}

fn js_error(e: &JsValue) -> String {
    e.dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
