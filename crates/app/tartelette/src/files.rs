//! Getting files in and out, natively (dialogs, paths) and in the browser
//! (async pickers, downloads). Loaded files land in an [`Inbox`] that the app
//! drains each frame, whichever way they arrived.
use std::sync::{Arc, Mutex};

use tartelette_runtime::Source;

/// What a loaded file is for.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    Firmware,
    Board,
    Sd(String),
    /// The `wav` player with this name (empty: the first one).
    Wav(String),
    /// Dropped onto the window: decided by its name.
    Guess,
}

#[derive(Clone, Default)]
pub struct Inbox(Arc<Mutex<Vec<(Slot, Source)>>>);

impl Inbox {
    pub fn push(&self, slot: Slot, src: Source) {
        self.0.lock().unwrap().push((slot, src));
    }
    pub fn take(&self) -> Vec<(Slot, Source)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// What a dropped file probably is, from its name.
pub fn guess(name: &str) -> Slot {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".toml") {
        Slot::Board
    } else if lower.ends_with(".img") {
        Slot::Sd(String::new()) // the first card on the board
    } else if lower.ends_with(".wav") {
        Slot::Wav(String::new())
    } else {
        Slot::Firmware
    }
}

fn dialog(slot: &Slot) -> rfd::AsyncFileDialog {
    let d = rfd::AsyncFileDialog::new();
    // On the web rfd merges every filter into one `accept` list; browsers drop
    // the "" and "*" entries and keep ".elf", which hides extensionless ELFs.
    if cfg!(target_arch = "wasm32") {
        return d;
    }
    match slot {
        Slot::Firmware => d
            .set_title("Open firmware")
            .add_filter("Firmware", &["elf", "uf2", ""])
            .add_filter("Any", &["*"]),
        Slot::Board => d
            .set_title("Open board file")
            .add_filter("Board", &["toml"])
            .add_filter("Any", &["*"]),
        Slot::Sd(_) => d
            .set_title("Open SD card image")
            .add_filter("Disk image", &["img", "bin"])
            .add_filter("Any", &["*"]),
        Slot::Wav(_) => d
            .set_title("Open WAV file")
            .add_filter("WAV", &["wav"])
            .add_filter("Any", &["*"]),
        Slot::Guess => d,
    }
}

/// Ask the user for a file; it arrives in `inbox` (later, on the web).
pub fn pick(slot: Slot, inbox: &Inbox, ctx: &egui::Context) {
    let fut = dialog(&slot).pick_file();
    let (inbox, ctx) = (inbox.clone(), ctx.clone());
    spawn(async move {
        if let Some(f) = fut.await {
            #[cfg_attr(target_arch = "wasm32", expect(unused_mut))]
            let mut src = Source::new(f.file_name(), f.read().await);
            #[cfg(not(target_arch = "wasm32"))]
            {
                src.path = Some(f.path().to_path_buf());
            }
            inbox.push(slot, src);
            ctx.request_repaint();
        }
    });
}

/// Files dropped onto the window.
pub fn take_dropped(ctx: &egui::Context, inbox: &Inbox) {
    for f in ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files)) {
        let name = f.path().file_name().map_or_else(
            || "dropped file".into(),
            |n| n.to_string_lossy().into_owned(),
        );
        #[cfg(not(target_arch = "wasm32"))]
        match f.bytes() {
            Ok(bytes) => {
                let mut src = Source::new(name.clone(), bytes);
                src.path = Some(f.path().to_path_buf()).filter(|p| p.is_absolute());
                inbox.push(guess(&name), src);
            }
            Err(e) => log(&format!("{name}: {e}")),
        }
        #[cfg(target_arch = "wasm32")]
        {
            let (inbox, ctx) = (inbox.clone(), ctx.clone());
            spawn(async move {
                match f.bytes_async().await {
                    Ok(bytes) => inbox.push(guess(&name), Source::new(name.clone(), bytes)),
                    Err(e) => log(&format!("{name}: {e}")),
                }
                ctx.request_repaint();
            });
        }
    }
}

/// Hand `bytes` to the user as a file called `name`.
pub fn save(name: &str, bytes: Vec<u8>) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let name = name.to_string();
        spawn(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .set_file_name(&name)
                .save_file()
                .await
                && let Err(e) = f.write(&bytes).await
            {
                log(&format!("{}: {e}", f.file_name()));
            }
        });
    }
    #[cfg(target_arch = "wasm32")]
    download(name, &bytes);
}

#[cfg(target_arch = "wasm32")]
fn download(name: &str, bytes: &[u8]) {
    use eframe::{wasm_bindgen::JsCast, web_sys};
    let run = || -> Option<()> {
        let array = js_sys::Uint8Array::from(bytes);
        let parts = js_sys::Array::of1(&array);
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).ok()?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
        let a: web_sys::HtmlAnchorElement = web_sys::window()?
            .document()?
            .create_element("a")
            .ok()?
            .dyn_into()
            .ok()?;
        a.set_href(&url);
        a.set_download(name);
        a.click();
        web_sys::Url::revoke_object_url(&url).ok()
    };
    if run().is_none() {
        log(&format!("could not download {name}"));
    }
}

/// Run a future to completion in the background.
#[cfg(not(target_arch = "wasm32"))]
fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    std::thread::spawn(move || pollster::block_on(fut));
}

#[cfg(target_arch = "wasm32")]
fn spawn(fut: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}

pub fn log(msg: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{msg}");
    #[cfg(target_arch = "wasm32")]
    eframe::web_sys::console::warn_1(&msg.into());
}
