//! Tartelette, an RP2040 emulator: runs a firmware ELF or UF2 on the emulated board, either with a
//! front panel that shows the board's parts (screen, LEDs, buttons, knobs,
//! ...) as an interface, or headless (`--headless`) for scripts and tests.
//! See `--help` for the options.
//!
//! Web: built with `trunk` (see `index.html`); files are opened or dropped
//! onto the page.
mod app;
mod brand;
#[cfg(not(target_arch = "wasm32"))]
mod cli;
mod files;
#[cfg(not(target_arch = "wasm32"))]
mod headless;
mod midi;
#[cfg(not(target_arch = "wasm32"))]
mod mkdisk;
mod panel;
mod session;
#[cfg(not(target_arch = "wasm32"))]
mod watch;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    let cli = cli::Cli::parse();
    let result = match &cli.command {
        Some(cli::Command::Mkdisk(args)) => mkdisk::run(args),
        Some(cli::Command::Parts(args)) => cli::parts(args),
        Some(cli::Command::Schema) => {
            print!("{}", cli::schema());
            Ok(())
        }
        None if cli.run.headless => headless::run(&cli.run),
        None => window(&cli.run),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn window(args: &cli::RunArgs) -> Result<(), String> {
    use session::Source;
    let firmware = args
        .firmware
        .as_deref()
        .map(Source::from_file)
        .transpose()?;
    let board = args.board()?;
    let mut app = app::App::new(firmware, board, args.shields()?, args.network());
    for (name, file) in &args.sd {
        if !app.load_sd_card(name, Source::from_file(file)?.bytes) {
            return Err(format!("--sd: no SD card named {name:?} on the board"));
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tartelette")
            .with_inner_size([1100.0, 760.0])
            .with_icon(brand::window_icon()),
        ..Default::default()
    };
    eframe::run_native("tartelette", options, Box::new(|_| Ok(Box::new(app))))
        .map_err(|e| e.to_string())
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
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|_| {
                    Ok(Box::new(app::App::new(
                        None,
                        None,
                        Vec::new(),
                        Default::default(),
                    )))
                }),
            )
            .await;
        if let Err(e) = start {
            files::log(&format!("failed to start: {e:?}"));
        }
    });
}
