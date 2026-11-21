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
mod midi_bridge;
#[cfg(not(target_arch = "wasm32"))]
mod mkdisk;
mod panel;
#[cfg(target_arch = "wasm32")]
mod preload;
mod sdbrowse;
mod session;
mod sound;
mod synth;
mod uart;
mod usb;
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
        None => cli
            .run
            .check_uart_links()
            .and_then(|()| match cli.run.headless {
                true => headless::run(&cli.run),
                false => window(&cli.run),
            }),
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
    app.set_voices(&args.osc, args.drums);
    for (name, file) in &args.sd {
        if !app.load_sd_card(name, Source::from_file(file)?.bytes) {
            return Err(format!("--sd: no SD card named {name:?} on the board"));
        }
        if args.sd_live {
            app.write_sd_card_to(name, file)
                .map_err(|e| format!("--sd-live: {e}"))?;
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tartelette")
            .with_inner_size([1100.0, 760.0])
            .with_icon(brand::window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "tartelette",
        options,
        Box::new(|cc| {
            app::install_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
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
        let preload = preload::Preload::from_page().await;
        let start = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    app::install_fonts(&cc.egui_ctx);
                    Ok(Box::new(preload.into_app()))
                }),
            )
            .await;
        if let Err(e) = start {
            let msg = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            files::log(&format!("failed to start: {msg}"));
            show_start_error(&msg);
        }
    });
}

/// Put why the app could not start on the page, in place of the canvas,
/// which would otherwise just stay blank.
#[cfg(target_arch = "wasm32")]
fn show_start_error(msg: &str) {
    let Some(body) = eframe::web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.body())
    else {
        return;
    };
    let escaped = msg.replace('&', "&amp;").replace('<', "&lt;");
    let help = if msg.contains("WebGL") {
        "tartelette draws its front panel with WebGL, and this browser didn't \
         provide it. It is usually there but switched off: check that hardware \
         acceleration is on in the browser's settings, and what <code>chrome://gpu</code> \
         or Firefox's <code>about:support</code> say about WebGL (its \"WebGL 1 Driver \
         Renderer\" line gives the reason). LibreWolf turns it off by default: set \
         <code>webgl.disabled</code> to false in <code>about:config</code>. Remote \
         desktops and virtual machines without a GPU often have none."
    } else {
        "Opening the browser's console may tell more."
    };
    body.set_inner_html(&format!(
        "<div style=\"max-width:36em;margin:15vh auto;padding:0 16px;color:#ddd;\
         font:16px/1.5 system-ui,sans-serif\"><h2 style=\"color:#fff\">tartelette \
         could not start</h2><p><code>{escaped}</code></p><p>{help}</p></div>"
    ));
    let _ = body.set_attribute("style", "overflow:auto;height:auto");
}
