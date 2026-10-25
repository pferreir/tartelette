//! Front-panel app: runs a firmware ELF on the emulated board and shows the
//! board's parts (screen, LEDs, buttons, knobs, ...) as an interface.
//!
//! Native: `rp2040-app [firmware.elf] [--board FILE] [--sd NAME=FILE.img]...`;
//! the firmware and board reload when their files change. Web: built with
//! `trunk` (see `index.html`); files are opened or dropped onto the page.
mod app;
mod files;
mod midi;
mod panel;
mod session;
#[cfg(not(target_arch = "wasm32"))]
mod watch;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    use session::Source;
    let mut args = std::env::args().skip(1);
    let (mut firmware, mut board, mut cards) = (None, None, Vec::new());
    let read = |p: &str| -> Source {
        let bytes = std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("{p}: {e}");
            std::process::exit(1)
        });
        let path = std::path::PathBuf::from(p);
        let mut s = Source::new(path.file_name().map_or(p.into(), |n| n.to_string_lossy().into_owned()), bytes);
        s.path = std::fs::canonicalize(&path).ok();
        s
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--board" => board = args.next().map(|p| read(&p)),
            "--sd" => match args.next().as_deref().and_then(|v| v.split_once('=')) {
                Some((name, file)) => cards.push((name.to_string(), read(file).bytes)),
                None => usage(),
            },
            _ if !a.starts_with("--") && firmware.is_none() => firmware = Some(read(&a)),
            _ => usage(),
        }
    }
    let mut app = app::App::new(firmware, board);
    for (name, image) in cards {
        if !app.load_sd_card(&name, image) {
            eprintln!("--sd: no SD card named {name:?} on the board");
            std::process::exit(1);
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("RP2040 emulator").with_inner_size([960.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native("rp2040-app", options, Box::new(|_| Ok(Box::new(app))))
}

#[cfg(not(target_arch = "wasm32"))]
fn usage() -> ! {
    eprintln!("usage: rp2040-app [firmware.elf] [--board FILE] [--sd NAME=FILE.img]...");
    std::process::exit(2)
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;
    wasm_bindgen_futures::spawn_local(async {
        let canvas = eframe::web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("app"))
            .and_then(|e| e.dyn_into::<eframe::web_sys::HtmlCanvasElement>().ok())
            .expect("no <canvas id=\"app\"> on the page");
        let start = eframe::WebRunner::new()
            .start(canvas, eframe::WebOptions::default(), Box::new(|_| Ok(Box::new(app::App::new(None, None)))))
            .await;
        if let Err(e) = start {
            files::log(&format!("failed to start: {e:?}"));
        }
    });
}
