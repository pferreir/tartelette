//! The emulator core as a bare WebAssembly module (no wasm-bindgen), for
//! timing it in browsers: `index.html` loads a firmware into it, runs it and
//! times the run with `performance.now()`. `scripts/web-bench.sh` builds it
//! twice, with and without WebAssembly tail calls (threaded dispatch, see
//! crates/armv6m/src/cpu/threaded.rs), and compares the two.
use rp2040_core::system::{StopReason, System};

static mut SYS: Option<System> = None;
static mut OUT: Vec<u8> = Vec::new();

fn sys() -> &'static mut System {
    unsafe { (*&raw mut SYS).as_mut().unwrap() }
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    Vec::<u8>::with_capacity(len).leak().as_mut_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn load(ptr: *const u8, len: usize) -> i32 {
    let fw = unsafe { core::slice::from_raw_parts(ptr, len) };
    let mut s = System::new();
    let ok = s.load_firmware(fw).is_ok();
    unsafe { *&raw mut SYS = Some(s) };
    ok as i32
}

/// Run to the breakpoint (cpu-bench); returns emulated cycles.
#[unsafe(no_mangle)]
pub extern "C" fn run_to_bkpt() -> f64 {
    let s = sys();
    while !matches!(s.run(10_000_000), StopReason::Breakpoint(_)) {}
    s.cycles() as f64
}

/// MicroPython: wait for USB, send a compute loop, wait for its answer.
/// Returns emulated cycles spent on the loop.
#[unsafe(no_mangle)]
pub extern "C" fn micropython() -> f64 {
    let s = sys();
    while s.usb_host().device().is_none() {
        s.run(125_000);
    }
    s.run(125_000_000);
    let start = s.cycles();
    let script = b"exec(\"x=0\\nfor i in range(300000):\\n x+=i*i\\nprint('done',x)\")\r\n";
    s.with_usb_host(|h| h.serial().unwrap().tx.extend(script));
    let mut out = Vec::new();
    loop {
        s.run(1_250_000);
        out.extend(s.with_usb_host(|h| h.serial().unwrap().rx.drain(..).collect::<Vec<_>>()));
        if out.windows(5).any(|w| w == b"done ") && out.ends_with(b">>> ") {
            break;
        }
    }
    unsafe { *&raw mut OUT = out };
    (s.cycles() - start) as f64
}

/// What UART0 (or, after `micropython`, the USB serial port) printed.
#[unsafe(no_mangle)]
pub extern "C" fn output_ptr() -> *const u8 {
    let s = sys();
    unsafe {
        if (*&raw const OUT).is_empty() {
            *&raw mut OUT = s.bus.uart[0].tx.clone();
        }
        (*&raw const OUT).as_ptr()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn output_len() -> usize {
    unsafe { (*&raw const OUT).len() }
}
