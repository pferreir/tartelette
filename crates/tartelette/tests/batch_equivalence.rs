//! `System::run` executes quiet stretches in batches, leaving the timer,
//! SysTick and PIO dividers to be caught up lazily. With `batching` off it
//! syncs every device after every instruction. Both must agree exactly: same
//! core state and clocks, same UART output, same timer and SysTick values,
//! at every checkpoint, whatever the checkpoint spacing.
use rp2040_core::system::{StopReason, System};
use std::path::PathBuf;
use std::process::Command;

fn build(name: &str) -> Vec<u8> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target_dir = root.join("target/firmware").join(name);
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "thumbv6m-none-eabi", "--manifest-path"])
        .arg(root.join(format!("firmware/tests/{name}/Cargo.toml")))
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo");
    assert!(status.success(), "firmware build failed");
    std::fs::read(target_dir.join(format!("thumbv6m-none-eabi/release/{name}"))).unwrap()
}

fn load(elf: &[u8]) -> System {
    let mut sys = System::new();
    sys.load_elf(elf).unwrap();
    sys
}

/// Everything a program can observe, as text so a mismatch shows what differed.
fn snapshot(s: &System) -> String {
    format!(
        "time={} cores={:?} timer={:?} ppb={:?} uart0={:?} uart1={:?}",
        s.cycles(),
        s.cores,
        s.bus.timer,
        s.bus.ppb,
        String::from_utf8_lossy(&s.bus.uart[0].tx),
        String::from_utf8_lossy(&s.bus.uart[1].tx),
    )
}

/// Run one system with batching and one without, in the same chunks, and
/// compare them after every chunk.
fn compare(name: &str, limit: u64, chunks: &[u64]) {
    let elf = build(name);
    let (mut fast, mut slow) = (load(&elf), load(&elf));
    slow.batching = false;
    let mut i = 0;
    while fast.cycles() < limit {
        let chunk = chunks[i % chunks.len()];
        i += 1;
        let (a_stop, b_stop) = (fast.run(chunk), slow.run(chunk));
        assert_eq!(a_stop, b_stop, "{name}: different stop at chunk {i}");
        let (a, b) = (snapshot(&fast), snapshot(&slow));
        if a != b {
            let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(0);
            let from = at.saturating_sub(120);
            panic!(
                "{name}: diverged at chunk {i} (size {chunk}), time {}\n batch: ...{}\n  full: ...{}",
                fast.cycles(),
                &a[from..(at + 120).min(a.len())],
                &b[from..(at + 120).min(b.len())]
            );
        }
        if a_stop == StopReason::Sleeping {
            // Nothing can wake the cores: let time pass the same way.
            fast.idle_until(fast.cycles() + chunk);
            slow.idle_until(slow.cycles() + chunk);
        } else if matches!(a_stop, StopReason::Breakpoint(_)) {
            break;
        }
    }
}

const SPACING: &[u64] = &[1, 3, 50, 977, 4001, 20_011];

#[test]
fn timer_systick_and_pendsv_interrupts_matches_unbatched() {
    compare("irq-timer", 400_000, SPACING);
}

#[test]
fn two_cores_with_spinlocks_fifo_and_sev_matches_unbatched() {
    compare("multicore-test", 3_000_000, SPACING);
}

#[test]
fn pio_firmware_matches_unbatched() {
    compare("pio-test", 1_000_000, SPACING);
}

#[test]
fn gpio_firmware_matches_unbatched() {
    compare("io-test", 1_000_000, SPACING);
}

#[test]
fn dma_and_spi_firmware_matches_unbatched() {
    compare("spi-dma-test", 1_000_000, SPACING);
}

#[test]
fn uart_firmware_matches_unbatched_with_large_chunks() {
    compare("uart-hello", 2_000_000, &[250_000]);
}

/// The core never sleeps; alarms are re-armed from handlers with changing
/// delays and SysTick fires while it spins. Catches a stale event horizon.
#[test]
fn alarms_rearmed_while_spinning_match_unbatched() {
    compare("batch-timer", 200_000, SPACING);
    compare("batch-timer", 200_000, &[1_000_000]);
}
