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
#[cfg(target_arch = "wasm32")]
mod preload;
mod sdbrowse;
mod sound;
mod usb_midi;
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
        None if cli.run.headless => cli
            .run
            .check_uart_links()
            .and_then(|()| headless::run(&cli.run)),
        None => {
            #[cfg(windows)]
            let detached = windows_console::detach_if_own();
            let result = cli.run.check_uart_links().and_then(|()| window(&cli.run));
            #[cfg(windows)]
            if let (true, Err(e)) = (detached, &result) {
                windows_console::show_error(e);
            }
            result
        }
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
    use tartelette_runtime::Source;
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
    // On Windows, wgpu first (Direct3D 12 or Vulkan): plenty of Windows
    // machines have no OpenGL 2.0 (VMs, Remote Desktop, no GPU driver), and
    // the system's own OpenGL is 1.1. OpenGL if wgpu finds no adapter.
    let app = std::rc::Rc::new(std::cell::RefCell::new(Some(app)));
    #[cfg(windows)]
    match run_panel(&app, eframe::Renderer::Wgpu) {
        Err(e) if app.borrow().is_some() => eprintln!("wgpu: {e}; trying OpenGL"),
        result => return result,
    }
    run_panel(&app, eframe::Renderer::Glow)
}

/// Run the front panel with `renderer`. The app stays in `slot` if the
/// renderer fails to start, so another one can be tried.
#[cfg(not(target_arch = "wasm32"))]
fn run_panel(
    slot: &std::rc::Rc<std::cell::RefCell<Option<app::App>>>,
    renderer: eframe::Renderer,
) -> Result<(), String> {
    let options = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("Tartelette")
            .with_inner_size([1100.0, 760.0])
            .with_icon(brand::window_icon()),
        ..Default::default()
    };
    let slot = slot.clone();
    eframe::run_native(
        "tartelette",
        options,
        Box::new(move |cc| {
            app::install_fonts(&cc.egui_ctx);
            let app = slot.borrow_mut().take().expect("the app is only created once");
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| e.to_string())
}

/// tartelette is a console program, so that it works from a terminal and in
/// scripts like any other: the shell waits for it, and its output and exit
/// status go where they should. Started from Explorer, though, Windows gives
/// it a console window of its own, which the front panel has no use for.
#[cfg(windows)]
mod windows_console {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }

    /// Close the console if no other process shares it, i.e. it was made for
    /// this one rather than inherited from a terminal. True if it was.
    pub fn detach_if_own() -> bool {
        let mut pids = [0u32; 2];
        // SAFETY: the buffer holds the 2 entries the call is told about.
        let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 2) };
        // SAFETY: no arguments; stdout and stderr just go nowhere after it.
        attached == 1 && unsafe { FreeConsole() } != 0
    }

    /// With no console to print it on, say why the panel didn't start.
    pub fn show_error(msg: &str) {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Tartelette")
            .set_description(msg)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
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
