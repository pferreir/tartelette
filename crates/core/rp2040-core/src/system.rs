//! The RP2040 system: memory map, peripherals and both cores.
//!
//! The real B2 bootrom image is mapped at 0 so ROM functions and tables work,
//! but only core 1 executes it at reset (it waits there for the launch
//! handshake, exactly as on the chip). Core 0 boot is "HLE": the ELF is loaded
//! straight into flash/SRAM and core 0 starts at the program's vector table, as
//! if boot2 had already run. The XIP SSI answers flash commands (see
//! [`Ssi`]); XIP control is not modelled. The USB controller is a device
//! with [`usbhost::Host`] on the other end of the cable (see [`Usb`]).
use alloc::{string::String, vec::Vec};

use pcb::{Board, InputResult};

use crate::{
    bus::{Bus, Ram},
    cpu::{Cpu, EXC_HARDFAULT, Fault},
    elf::{self, ElfError},
    periph::{
        Peripheral, RegStub,
        adc::{self, Adc},
        clocks::Clocks,
        dma::{self, Dma},
        gpio::{Gpio, IoBank0, PadsBank0},
        i2c::I2c,
        pio::{Pio, PioEnv},
        psm::Psm,
        pwm::Pwm,
        resets::Resets,
        rtc::Rtc,
        sio::Sio,
        spi::{self, Spi},
        ssi::Ssi,
        timer::Timer,
        uart::Uart,
        usb::{self, Usb},
    },
    ppb::Ppb,
    uf2::{self, Uf2Error},
};

/// While the core sleeps with DMA running, time advances in slices of this
/// many cycles (the DMA moves one item per cycle).
const DMA_SLICE: u64 = 32;

/// clk_sys is fixed at the SDK default of 125 MHz.
pub const CYCLES_PER_US: u64 = 125;

pub const BOOTROM_BASE: u32 = 0x0000_0000;
pub const BOOTROM_SIZE: u32 = 0x4000;
/// RP2040 B2 bootrom, see `bootrom/README.md`.
static BOOTROM_B2: &[u8; BOOTROM_SIZE as usize] = include_bytes!("../bootrom/b2.bin");
pub const XIP_BASE: u32 = 0x1000_0000;
pub const XIP_SSI_BASE: u32 = 0x1800_0000;
pub const SRAM_BASE: u32 = 0x2000_0000;
pub const SRAM_SIZE: u32 = 0x42000; // 264 KiB
pub const APB_BASE: u32 = 0x4000_0000;
pub const DMA_BASE: u32 = 0x5000_0000;
pub const USBCTRL_DPRAM_BASE: u32 = 0x5010_0000;
pub const USBCTRL_REGS_BASE: u32 = 0x5011_0000;
pub const PIO0_BASE: u32 = 0x5020_0000;
pub const PIO1_BASE: u32 = 0x5030_0000;
pub const SIO_BASE: u32 = 0xD000_0000;
/// SIO FIFO_RD: reading pops the inter-core FIFO.
const SIO_FIFO_RD: u32 = 0x058;
pub const PPB_BASE: u32 = 0xE000_0000;

const FLASH_DEFAULT: usize = 2 << 20;
/// pico-sdk / rp2040-hal binaries put the 256-byte boot2 stage first.
const BOOT2_SIZE: u32 = 0x100;

// APB blocks are 16 KiB apart: block = (addr - APB_BASE) >> 14.
const APB_SYSINFO: u32 = 0;
const APB_CLOCKS: u32 = 2;
const APB_PSM: u32 = 4;
const APB_RESETS: u32 = 3;
const APB_IO_BANK0: u32 = 5;
const APB_IO_QSPI: u32 = 6;
/// IO_QSPI GPIO_QSPI_SS_CTRL: its OUTOVER field forces the flash chip select.
const QSPI_SS_CTRL: u32 = 0x0C;
const APB_PADS_BANK0: u32 = 7;
const APB_XOSC: u32 = 9;
const APB_BUSCTRL: u32 = 12;
const APB_ROSC: u32 = 24;
const APB_PLL_SYS: u32 = 10;
const APB_PLL_USB: u32 = 11;
const APB_ADC: u32 = 19;
const APB_PWM: u32 = 20;
const APB_TIMER: u32 = 21;
const APB_RTC: u32 = 23;
const APB_UART0: u32 = 13;
const APB_SPI0: u32 = 15;
const APB_SPI1: u32 = 16;
const APB_I2C0: u32 = 17;
const APB_I2C1: u32 = 18;
const APB_UART1: u32 = 14;
pub const IRQ_PWM_WRAP: u32 = 4;
pub const IRQ_USBCTRL: u32 = 5;
pub const IRQ_PIO0_0: u32 = 7;
pub const IRQ_PIO0_1: u32 = 8;
pub const IRQ_PIO1_0: u32 = 9;
pub const IRQ_PIO1_1: u32 = 10;
pub const IRQ_DMA0: u32 = 11;
pub const IRQ_DMA1: u32 = 12;
pub const IRQ_IO_BANK0: u32 = 13;
pub const IRQ_SIO_PROC0: u32 = 15;
pub const IRQ_SIO_PROC1: u32 = 16;
pub const IRQ_SPI0: u32 = 18;
pub const IRQ_SPI1: u32 = 19;
pub const IRQ_UART0: u32 = 20;
pub const IRQ_UART1: u32 = 21;
pub const IRQ_ADC_FIFO: u32 = 22;
pub const IRQ_I2C0: u32 = 23;
pub const IRQ_I2C1: u32 = 24;
const APB_BLOCKS: usize = 32;
/// RESETS bits of the ADC and the DMA controller.
const RESET_ADC: u32 = 0;
const RESET_DMA: u32 = 2;
const RESET_PWM: u32 = 14;
/// RESETS bits of PIO0 and PIO1.
const RESET_PIO: [u32; 2] = [10, 11];
const RESET_USBCTRL: u32 = 24;

/// RESETS bit that holds each APB block in reset (None = always out of reset).
const fn reset_bit(block: u32) -> Option<u32> {
    Some(match block {
        0 => 19,  // SYSINFO
        1 => 18,  // SYSCFG
        5 => 5,   // IO_BANK0
        6 => 6,   // IO_QSPI
        7 => 8,   // PADS_BANK0
        8 => 9,   // PADS_QSPI
        10 => 12, // PLL_SYS
        11 => 13, // PLL_USB
        12 => 1,  // BUSCTRL
        13 => 22, // UART0
        14 => 23, // UART1
        15 => 16, // SPI0
        16 => 17, // SPI1
        17 => 3,  // I2C0
        18 => 4,  // I2C1
        19 => 0,  // ADC
        20 => 14, // PWM
        21 => 21, // TIMER
        23 => 15, // RTC
        27 => 20, // TBMAN
        _ => return None,
    })
}

// Status bits that init code spins on, forced to "ready".
const XOSC_FORCED: &[(u32, u32)] = &[(0x04, 1 << 31)]; // STATUS.STABLE
const PLL_FORCED: &[(u32, u32)] = &[(0x00, 1 << 31)]; // CS.LOCK
// PLL registers.
const PLL_CS: u32 = 0x0;
const PLL_FBDIV_INT: u32 = 0x8;
const PLL_PRIM: u32 = 0xC;
const XOSC_HZ: u64 = 12_000_000;
/// CLOCKS: clk_peri's control (ENABLE, AUXSRC in bits 7:5; no divider).
const CLK_SYS_CTRL: u32 = 0x3C;
const CLK_SYS_DIV: u32 = 0x40;
const CLK_PERI_CTRL: u32 = 0x48;
/// CLOCKS: clk_adc's control (AUXSRC in bits 7:5) and divider (INT in 9:8).
const CLK_ADC_CTRL: u32 = 0x60;
const CLK_ADC_DIV: u32 = 0x64;
// The ring oscillator runs from power-up (pico_rand asserts it does).
const ROSC_FORCED: &[(u32, u32)] = &[(0x18, 1 << 31 | 1 << 12)]; // STATUS.STABLE, ENABLED
// PERFSEL0..3 reset to 0x1F ("none"): pico_rand claims a free counter by
// looking for that value, and hard-asserts if none is left.
const BUSCTRL_RESET: &[(u32, u32)] = &[(0x0C, 0x1F), (0x14, 0x1F), (0x1C, 0x1F), (0x24, 0x1F)];
// CHIP_ID of a B2 RP2040 (revision 2, part 2, Raspberry Pi): drivers skip
// the B0/B1 errata workarounds (TinyUSB's USB enumeration fix, ...).
const SYSINFO_RESET: &[(u32, u32)] = &[(0x00, 0x2000_2927)];
// The PLLs: REFDIV = 1, POSTDIV1 = POSTDIV2 = 7.
const PLL_RESET: &[(u32, u32)] = &[(PLL_CS, 1), (PLL_PRIM, 0x7_7000)];

/// A DMA channel that feeds a timed SPI's TX FIFO from memory, item after
/// item as frames go out, with nothing else watching. Followed one frame at
/// a time, it would end a batch of core instructions at every frame; instead
/// the cores run on, and the stream catches up to the cycle of each access
/// that could see its progress (any peripheral register, or a store into
/// the part of its source not read yet), moving each item at the frame end
/// it would have moved at. It lasts one batch, which ends no later than its
/// last item (so completion, interrupt and chaining go the usual way).
#[derive(Clone, Copy)]
struct Stream {
    spi: usize,
    channel: usize,
    /// The source still to be read, as SRAM offsets `unread.0..unread.1`
    /// (empty for flash or ROM, which the cores cannot store to).
    unread: (u32, u32),
    /// The cycle at which the last item moves.
    last_at: u64,
}

pub struct SystemBus {
    pub bootrom: Ram,
    pub flash: Ram,
    pub sram: Ram,
    pub resets: Resets,
    pub uart: [Uart; 2],
    pub i2c: [I2c; 2],
    /// Boxed: kept inline, the SPI controllers' wire state pushes the hot
    /// fields of this struct apart, and PIO-heavy runs were 10% slower.
    pub spi: alloc::boxed::Box<[Spi; 2]>,
    pub ssi: Ssi,
    pub dma: Dma,
    pub pio: [Pio; 2],
    pub timer: Timer,
    pub gpio: Gpio,
    pub sio: Sio,
    pub psm: Psm,
    pub clocks: Clocks,
    pub rtc: Rtc,
    pub adc: Adc,
    pub pwm: Pwm,
    pub usb: Usb,
    /// Each core has its own NVIC/SysTick/SCB.
    pub ppb: [Ppb; 2],
    stubs: Vec<RegStub>,
    /// PPB VTOR per core; [`System`] copies them into the cores after a step.
    pub vtor: [u32; 2],
    /// The core whose access is being serviced: it picks which PPB and which
    /// view of SIO the access sees. [`System`] sets it around each core step.
    pub core: usize,
    /// Some device state that feeds the NVIC inputs may have changed since
    /// they were last computed.
    pub(crate) lines_dirty: bool,
    /// A peripheral register was accessed since the last full device sync.
    pub(crate) touched: bool,
    /// A DMA channel feeding an SPI on its own during a batch (see
    /// [`Stream`]).
    stream: Option<Stream>,
    /// System cycles the PIO dividers have not been told about yet: quiet
    /// steps add to it, and it is applied before anything that could change
    /// which machines run (a PIO or RESETS access) and before a full sync.
    pio_owed: u64,
    /// clk_sys, the rate the cycles count at; and whether it changed since
    /// [`System`] last looked (the board and the USB host follow it).
    sys_hz: u64,
    clock_changed: bool,
    /// Device time: the cycle up to which peripherals have been brought
    /// current. Each core keeps its own clock; this one follows the earliest
    /// core that is still running.
    pub(crate) time: u64,
    /// While a core runs a batch of instructions without syncing devices, the
    /// cycle its current instruction starts at (otherwise <= `time`).
    now: u64,
}

impl SystemBus {
    pub fn new() -> Self {
        let stubs = (0..APB_BLOCKS as u32)
            .map(|b| {
                let forced = match b {
                    APB_XOSC => XOSC_FORCED,
                    APB_PLL_SYS | APB_PLL_USB => PLL_FORCED,
                    APB_ROSC => ROSC_FORCED,
                    _ => &[],
                };
                let reset = match b {
                    APB_BUSCTRL => BUSCTRL_RESET,
                    APB_SYSINFO => SYSINFO_RESET,
                    APB_PLL_SYS | APB_PLL_USB => PLL_RESET,
                    _ => &[],
                };
                RegStub::with_reset(forced, reset)
            })
            .collect();
        Self {
            bootrom: Ram {
                data: BOOTROM_B2.to_vec(),
            },
            flash: Ram {
                data: alloc::vec![0xFF; FLASH_DEFAULT],
            },
            sram: Ram::new(SRAM_SIZE as usize),
            resets: Resets::new(),
            uart: [Uart::new(), Uart::new()],
            i2c: [I2c::new(), I2c::new()],
            spi: alloc::boxed::Box::new([Spi::new(), Spi::new()]),
            ssi: Ssi::new(),
            dma: Dma::new(),
            pio: [Pio::new(), Pio::new()],
            timer: Timer::new(),
            gpio: Gpio::new(),
            sio: Sio::new(),
            psm: Psm::new(),
            clocks: Clocks::new(),
            rtc: Rtc::new(),
            adc: Adc::new(),
            pwm: Pwm::new(),
            usb: Usb::new(),
            ppb: [Ppb::new(CYCLES_PER_US), Ppb::new(CYCLES_PER_US)],
            stubs,
            vtor: [0; 2],
            core: 0,
            lines_dirty: true,
            touched: true,
            stream: None,
            pio_owed: 0,
            sys_hz: CYCLES_PER_US * 1_000_000,
            clock_changed: false,
            time: 0,
            now: 0,
        }
    }

    /// Bring the devices that keep time on their own (timer, SysTick, UARTs,
    /// PIO dividers) up to the cycle the running instruction starts at. Needed
    /// before an access that can see or react to them, when a batch of quiet
    /// instructions has run ahead of them.
    #[inline]
    fn sync_to_now(&mut self) {
        if self.now > self.time {
            let (from, to) = (self.time, self.now);
            self.time = to;
            self.pio_owed += to - from;
            let timer_lines = self.timer.irq_lines();
            self.timer.update(to);
            if self.timer.irq_lines() != timer_lines {
                self.lines_dirty = true;
            }
            for ppb in &mut self.ppb {
                ppb.advance(to - from);
            }
            self.update_uarts(to);
            self.update_usb(to);
        }
    }

    /// Let the USB frame counter run up to `to` (a SOF interrupt may fire).
    fn update_usb(&mut self, to: u64) {
        if self.usb.held() {
            return;
        }
        let before = self.usb.irq();
        self.usb.update(to);
        if self.usb.irq() != before {
            self.lines_dirty = true;
        }
    }

    /// Run the UART lines and SPI wires up to `to`. A character or frame
    /// that moved can raise an interrupt or a DMA request, so everything
    /// gets a full sync.
    fn update_uarts(&mut self, to: u64) {
        let mut changed = false;
        for uart in &mut self.uart {
            changed |= uart.update(to);
        }
        for n in 0..2 {
            // An idle wire has nothing to run; a streaming SPI's runs with
            // its stream (`stream_catch_up`).
            if self.spi[n].next_frame_end().is_none() || self.stream.is_some_and(|s| s.spi == n) {
                continue;
            }
            let watched = self.spi_watched(n);
            changed |= self.spi[n].update(to, watched);
        }
        if changed {
            self.lines_dirty = true;
            self.touched = true;
        }
    }

    /// The DMA stream into an SPI the cores can run alongside, if there is
    /// one: exactly one running channel paced by a timed SPI's TX request,
    /// reading plain memory and writing that SPI's data register, with its
    /// FIFO full (the system is quiet), the SPI's interrupts off and nothing
    /// waiting on its RX request, and at least two items left.
    #[inline(never)]
    fn find_stream(&self) -> Option<Stream> {
        if self.resets.in_reset(RESET_DMA) || !self.dma.active() {
            return None;
        }
        let needed = self.dma.needed_dreqs();
        (0..2).find_map(|n| {
            let spi = &self.spi[n];
            let tx = 16 + 2 * n as u32;
            if spi.next_frame_end().is_none()
                || spi.interrupts_enabled()
                || needed >> tx & 1 == 0
                || needed >> (tx + 1) & 1 != 0
            {
                return None;
            }
            let mut on_tx = (0..dma::NUM_CHANNELS)
                .filter(|&c| self.dma.view(c).running && self.dma.view(c).treq == tx);
            let channel = on_tx.next()?;
            if on_tx.next().is_some() {
                return None;
            }
            let v = self.dma.view(channel);
            let dr = APB_BASE + ((APB_SPI0 + n as u32) << 14) + 8;
            let plain = matches!(v.read >> 24, 0x00 | 0x10..=0x13 | 0x20 | 0x21);
            if v.write & !3 != dr || v.incr_write || !plain || v.count < 2 {
                return None;
            }
            let unread = match v.read >> 24 {
                0x20 | 0x21 => {
                    let at = v.read & 0xFF_FFFF;
                    match (v.incr_read, v.read_ring_bits) {
                        (false, _) => (at, at + v.bytes),
                        (true, 0) => (at, at.saturating_add(v.count.saturating_mul(v.bytes))),
                        // A ring: the whole of it.
                        (true, bits) => {
                            let base = at & !((1 << bits) - 1);
                            (base, base + (1 << bits))
                        }
                    }
                }
                _ => (0, 0),
            };
            Some(Stream {
                spi: n,
                channel,
                unread,
                last_at: spi.frame_end(v.count as usize)?,
            })
        })
    }

    /// Move the stream's items whose frame ends come by cycle `to`, each at
    /// its own cycle, handing the frames to the board's SPI devices. Ends
    /// the stream if it no longer is one (another channel wants the bus,
    /// the channel was changed, a device moved a pin) or its last item went:
    /// the next sync takes it from there.
    #[inline]
    fn stream_catch_up(&mut self, to: u64, board: &mut Board) {
        if let Some(s) = self.stream {
            self.run_stream(s, to, board);
        }
    }

    #[inline(never)]
    fn run_stream(&mut self, s: Stream, to: u64, board: &mut Board) {
        let n = s.spi;
        while let Some(t) = self.spi[n].next_frame_end().filter(|&t| t <= to) {
            let v = self.dma.view(s.channel);
            let others = self.dma.needed_dreqs() & !(1 << (16 + 2 * n));
            if !v.running
                || v.treq != 16 + 2 * n as u32
                || self.spi[n].interrupts_enabled()
                || (others != 0 && self.dma.can_run(self.dreq_mask(others)))
            {
                self.end_stream();
                return;
            }
            // The frame ends; the next one starts, so there's room.
            self.spi[n].update(t, 0);
            let Some(item) = self.dma.pick(self.dreq_mask(self.dma.needed_dreqs())) else {
                self.end_stream();
                return;
            };
            let mut v = match item.bytes {
                1 => self.read8(item.read) as u32,
                2 => self.read16(item.read) as u32,
                _ => self.read32(item.read),
            };
            if item.bswap {
                v = match item.bytes {
                    2 => (v as u16).swap_bytes() as u32,
                    4 => v.swap_bytes(),
                    _ => v,
                };
            }
            // As a sub-word store to a peripheral: the value in every lane.
            let v = match item.bytes {
                1 => v * 0x0101_0101,
                2 => v * 0x0001_0001,
                _ => v,
            };
            self.spi[n].write(0x008, v);
            self.dma.complete(&item);
            self.lines_dirty = true;
            let mut wire = board.spi_bus(n, &mut self.gpio, t);
            self.spi[n].pump(&mut wire, t);
            if self.gpio.has_changes() || !self.dma.view(s.channel).running {
                self.end_stream();
                return;
            }
            // What's read is no longer the stream's business.
            if let Some(st) = &mut self.stream {
                let next = self.dma.view(s.channel).read & 0xFF_FFFF;
                if (st.unread.0..=st.unread.1).contains(&next) {
                    st.unread.0 = next;
                }
            }
        }
    }

    /// Back to following the SPI frame by frame; the next sync looks at
    /// everything.
    fn end_stream(&mut self) {
        self.stream = None;
        self.touched = true;
        self.lines_dirty = true;
    }

    /// The [`Spi::outputs`] of SPI `n` that something reacts to without
    /// reading its registers: the interrupt line, and the DMA requests a
    /// running channel waits on. (A DMA request enabled once at start-up,
    /// with no transfer under way, changes nothing as it comes and goes.)
    #[inline]
    fn spi_watched(&self, n: usize) -> u8 {
        let needed = match self.resets.in_reset(RESET_DMA) {
            true => 0,
            false => self.dma.needed_dreqs() >> (16 + 2 * n),
        };
        spi::OUT_IRQ
            | ((needed & 1 != 0) as u8 * spi::OUT_DREQ_TX)
            | ((needed & 2 != 0) as u8 * spi::OUT_DREQ_RX)
    }

    /// Let the PIO dividers catch up with the quiet steps taken so far.
    fn flush_pio(&mut self) {
        let cycles = core::mem::take(&mut self.pio_owed);
        if cycles != 0 {
            for block in 0..2 {
                if !self.resets.in_reset(RESET_PIO[block]) {
                    self.pio[block].idle(cycles);
                }
            }
        }
    }

    /// Copy `data` into the memory at `addr` (flash included, which the CPU
    /// itself cannot write). Returns false if the range isn't loadable.
    pub fn load(&mut self, addr: u32, data: &[u8]) -> bool {
        let end = addr as u64 + data.len() as u64;
        if (XIP_BASE as u64..XIP_BASE as u64 + 0x100_0000).contains(&(addr as u64))
            && end <= XIP_BASE as u64 + 0x100_0000
        {
            let off = (addr - XIP_BASE) as usize;
            if self.flash.data.len() < off + data.len() {
                self.flash.data.resize(off + data.len(), 0xFF);
            }
            self.flash.data[off..off + data.len()].copy_from_slice(data);
            true
        } else if addr >= SRAM_BASE && end <= (SRAM_BASE + SRAM_SIZE) as u64 {
            let off = (addr - SRAM_BASE) as usize;
            self.sram.data[off..off + data.len()].copy_from_slice(data);
            true
        } else {
            false
        }
    }

    // ---- 32-bit word access; sub-word accesses are derived from these ----

    fn mmio_read(&mut self, addr: u32) -> u32 {
        self.lines_dirty |= Self::read_moves_lines(addr);
        self.touched |= Self::needs_sync(addr);
        if !(addr >> 28 == 0xD && !Gpio::is_sio_gpio(addr.wrapping_sub(SIO_BASE))) {
            self.sync_to_now();
        }
        self.flush_pio_for(addr);
        match addr >> 28 {
            0xD if Gpio::is_sio_gpio(addr - SIO_BASE) => self.gpio.sio_read(addr - SIO_BASE),
            0xD if Sio::handles(addr - SIO_BASE) => self.sio.read(self.core, addr - SIO_BASE),
            0xE => self.ppb_read(addr),
            0x4 => self.apb_read(addr),
            0x1 if addr & 0xFF00_0000 == XIP_SSI_BASE => {
                self.ssi.read(addr & 0xFF, &mut self.flash.data)
            }
            0x5 if addr - DMA_BASE < 0x4000 && !self.resets.in_reset(RESET_DMA) => {
                self.dma.read(addr & 0xFFF)
            }
            0x5 if addr >> 16 == USBCTRL_REGS_BASE >> 16 => match self.usb.held() {
                true => 0,
                false => self.usb.read(addr & 0xFFF),
            },
            0x5 => match self.pio_at(addr) {
                Some(b) => self.pio[b].read(addr & 0xFFF),
                None => 0,
            },
            _ => 0,
        }
    }

    fn mmio_write(&mut self, addr: u32, val: u32) {
        self.lines_dirty = true;
        self.touched |= Self::needs_sync(addr);
        if !(addr >> 28 == 0xD && !Gpio::is_sio_gpio(addr.wrapping_sub(SIO_BASE))) {
            self.sync_to_now();
        }
        self.flush_pio_for(addr);
        match addr >> 28 {
            0xD if Gpio::is_sio_gpio(addr.wrapping_sub(SIO_BASE)) => {
                self.gpio.sio_write(addr - SIO_BASE, val)
            }
            0xD if Sio::handles(addr.wrapping_sub(SIO_BASE)) => {
                self.sio.write(self.core, addr - SIO_BASE, val)
            }
            0xE => self.ppb_write(addr, val),
            0x4 => self.apb_write(addr, val),
            0x1 if addr & 0xFF00_0000 == XIP_SSI_BASE => {
                self.ssi.write(addr & 0xFF, val, &mut self.flash.data)
            }
            0x5 if addr - DMA_BASE < 0x4000 => {
                if self.resets.in_reset(RESET_DMA) {
                    return;
                }
                // Atomic aliases as for PIO below (embassy enables channel
                // interrupts with a SET write to INTE0).
                let off = addr & 0xFFF;
                let val = match addr >> 12 & 3 {
                    alias if alias != 0 && Dma::alias_applies(off) => {
                        let cur = self.dma.read(off);
                        match alias {
                            1 => cur ^ val,
                            2 => cur | val,
                            _ => cur & !val,
                        }
                    }
                    _ => val,
                };
                self.dma.sync_timers(self.time);
                self.dma.write(off, val)
            }
            0x5 if addr >> 16 == USBCTRL_REGS_BASE >> 16 => {
                if self.usb.held() {
                    return;
                }
                // Atomic aliases as for PIO; in the write-one-to-clear
                // registers every alias clears the bits it names.
                let off = addr & 0xFFF;
                let val = match addr >> 12 & 3 {
                    0 => val,
                    _ if Usb::write_clears(off) => val,
                    alias => {
                        let cur = self.usb.read(off);
                        match alias {
                            1 => cur ^ val,
                            2 => cur | val,
                            _ => cur & !val,
                        }
                    }
                };
                self.usb.write(off, val);
            }
            0x5 => {
                if let Some(b) = self.pio_at(addr) {
                    let off = addr & 0xFFF;
                    // The atomic aliases (+0x1000 XOR, +0x2000 SET, +0x3000 CLR) work on
                    // the plain registers; FIFOs and write-1-to-act registers ignore them.
                    let val = match addr >> 12 & 3 {
                        alias if alias != 0 && Pio::alias_applies(off) => {
                            let cur = self.pio[b].read(off);
                            match alias {
                                1 => cur ^ val,
                                2 => cur | val,
                                _ => cur & !val,
                            }
                        }
                        _ => val,
                    };
                    self.pio[b].write(off, val);
                }
            }
            _ => {}
        }
    }

    /// Can reading `addr` change an interrupt line? Only reads with side
    /// effects can: popping a FIFO the line watches (SIO FIFO_RD, the UART,
    /// SPI, I2C, ADC and PIO RX FIFOs) or a read-to-clear (I2C's CLR_* registers,
    /// USB SOF_RD). Reads of everything else (the hot ones: SIO CPUID, the
    /// timer, GPIO, NVIC) leave the lines alone, so the cores' NVIC inputs
    /// needn't be recomputed after them. Time catching up on a read raises
    /// its own flag (see `sync_to_now`).
    #[inline]
    fn read_moves_lines(addr: u32) -> bool {
        match addr >> 28 {
            0xD => addr.wrapping_sub(SIO_BASE) == SIO_FIFO_RD,
            0x4 => matches!(
                addr.wrapping_sub(APB_BASE) >> 14,
                APB_UART0 | APB_UART1 | APB_SPI0 | APB_SPI1 | APB_I2C0 | APB_I2C1 | APB_ADC
            ),
            0x5 if addr >> 16 == USBCTRL_REGS_BASE >> 16 => addr & 0xFFF == usb::SOF_RD,
            0x5 => addr >> 20 == PIO0_BASE >> 20 || addr >> 20 == PIO1_BASE >> 20,
            _ => false,
        }
    }

    /// Is `addr` in a UART, SPI or I2C controller?
    #[inline]
    fn is_bus_controller(addr: u32) -> bool {
        addr >> 28 == 0x4
            && matches!(
                addr.wrapping_sub(APB_BASE) >> 14,
                APB_UART0 | APB_UART1 | APB_SPI0 | APB_SPI1 | APB_I2C0 | APB_I2C1
            )
    }

    /// Can an access to `addr` start work in a device that the quiet path
    /// doesn't advance (DMA, PIO, buses, GPIO, resets)? Spinlocks, the SIO
    /// FIFOs and divider, NVIC/SysTick and the timer can't: they only move
    /// interrupt lines, which `lines_dirty` already covers.
    #[inline]
    fn needs_sync(addr: u32) -> bool {
        match addr >> 28 {
            0xE => false,
            0xD => Gpio::is_sio_gpio(addr.wrapping_sub(SIO_BASE)),
            0x4 => addr.wrapping_sub(APB_BASE) >> 14 != APB_TIMER,
            _ => true,
        }
    }

    /// Catch the PIO dividers up before an access that can see or change
    /// them: PIO and DMA space, or the RESETS block.
    #[inline]
    fn flush_pio_for(&mut self, addr: u32) {
        if self.pio_owed != 0
            && (addr >> 28 == 5
                || (addr >> 28 == 4 && addr.wrapping_sub(APB_BASE) >> 14 == APB_RESETS))
        {
            self.flush_pio();
        }
    }

    /// Which PIO block (if any, and out of reset) owns `addr`.
    fn pio_at(&self, addr: u32) -> Option<usize> {
        let block = match addr >> 20 {
            0x502 => 0,
            0x503 => 1,
            _ => return None,
        };
        (!self.resets.in_reset(RESET_PIO[block])).then_some(block)
    }

    fn ppb_read(&mut self, addr: u32) -> u32 {
        match addr - PPB_BASE {
            0xED08 => self.vtor[self.core],
            off => self.ppb[self.core].read(off),
        }
    }
    fn ppb_write(&mut self, addr: u32, val: u32) {
        match addr - PPB_BASE {
            0xED08 => self.vtor[self.core] = val & !0xFF,
            off => self.ppb[self.core].write(off, val),
        }
    }

    /// Level of every NVIC input of core `core` (bit n = IRQ n). Most lines
    /// reach both cores; IO_BANK0 has a separate status set per core, and the
    /// two SIO FIFO lines each show one core's FIFO.
    pub fn irq_lines(&self, core: usize) -> u32 {
        self.timer.irq_lines()
            | (self.gpio.irq_for(core) as u32) << IRQ_IO_BANK0
            | (self.sio.irq(0) as u32) << IRQ_SIO_PROC0
            | (self.sio.irq(1) as u32) << IRQ_SIO_PROC1
            | (self.uart[0].irq() as u32) << IRQ_UART0
            | (self.uart[1].irq() as u32) << IRQ_UART1
            | (self.i2c[0].irq() as u32) << IRQ_I2C0
            | (self.i2c[1].irq() as u32) << IRQ_I2C1
            | (self.spi[0].irq() as u32) << IRQ_SPI0
            | (self.spi[1].irq() as u32) << IRQ_SPI1
            | (self.dma.irq(0) as u32) << IRQ_DMA0
            | (self.dma.irq(1) as u32) << IRQ_DMA1
            | (self.pio[0].irq(0) as u32) << IRQ_PIO0_0
            | (self.pio[0].irq(1) as u32) << IRQ_PIO0_1
            | (self.pio[1].irq(0) as u32) << IRQ_PIO1_0
            | (self.pio[1].irq(1) as u32) << IRQ_PIO1_1
            | (self.usb.irq() as u32) << IRQ_USBCTRL
            | (self.adc.irq() as u32) << IRQ_ADC_FIFO
            | (self.pwm.irq() as u32) << IRQ_PWM_WRAP
    }

    /// Current level of every DMA request in `needed` (bit n = TREQ n).
    /// Requests from blocks that aren't modelled stay low.
    fn dreq_mask(&self, needed: u64) -> u64 {
        let level = |n: u32| match n {
            0..=3 => self.pio[0].dreq_tx(n as usize),
            4..=7 => self.pio[0].dreq_rx(n as usize - 4),
            8..=11 => self.pio[1].dreq_tx(n as usize - 8),
            12..=15 => self.pio[1].dreq_rx(n as usize - 12),
            16 => self.spi[0].dreq_tx(),
            17 => self.spi[0].dreq_rx(),
            18 => self.spi[1].dreq_tx(),
            19 => self.spi[1].dreq_rx(),
            20 => self.uart[0].dreq_tx(),
            21 => self.uart[0].dreq_rx(),
            22 => self.uart[1].dreq_tx(),
            23 => self.uart[1].dreq_rx(),
            24..=31 => self.pwm.dreq(n as usize - 24),
            32 => self.i2c[0].dreq_tx(),
            33 => self.i2c[0].dreq_rx(),
            34 => self.i2c[1].dreq_tx(),
            35 => self.i2c[1].dreq_rx(),
            36 => self.adc.dreq(),
            _ => false,
        };
        let mut mask = 0;
        let mut rest = needed;
        while rest != 0 {
            let n = rest.trailing_zeros();
            rest &= rest - 1;
            if level(n) {
                mask |= 1 << n;
            }
        }
        mask
    }

    /// Core cycles until a device will next change state on its own.
    fn cycles_to_event(&self) -> Option<u64> {
        let until = |t: Option<u64>| t.map(|t| t.saturating_sub(self.time));
        let mut e = earliest(self.timer.cycles_to_event(), self.ppb[0].cycles_to_event());
        e = earliest(e, self.ppb[1].cycles_to_event());
        e = earliest(e, until(self.uart[0].next_event()));
        e = earliest(e, until(self.uart[1].next_event()));
        for n in 0..2 {
            let next = match self.stream {
                // Its frames are no events; its last item is.
                Some(s) if s.spi == n => Some(s.last_at),
                _ if self.spi[n].next_frame_end().is_none() => None,
                _ => self.spi[n].next_event(self.spi_watched(n)),
            };
            e = earliest(e, until(next));
        }
        e = earliest(e, until(self.usb.next_event()));
        e = earliest(e, until(self.pwm.next_event()));
        earliest(e, until(self.adc.next_event()))
    }

    /// Run `f` on the peripheral behind `addr` and its register offset;
    /// None if the address is unmapped or the block is held in reset.
    fn with_apb<R>(
        &mut self,
        addr: u32,
        f: impl FnOnce(&mut dyn Peripheral, u32) -> R,
    ) -> Option<R> {
        let rel = addr.wrapping_sub(APB_BASE);
        let block = rel >> 14;
        if block as usize >= APB_BLOCKS {
            return None;
        }
        if reset_bit(block).is_some_and(|b| self.resets.in_reset(b)) {
            return None;
        }
        let off = rel & 0xFFF;
        Some(match block {
            APB_RESETS => f(&mut self.resets, off),
            APB_PSM => f(&mut self.psm, off),
            APB_IO_BANK0 => f(&mut IoBank0(&mut self.gpio), off),
            APB_PADS_BANK0 => f(&mut PadsBank0(&mut self.gpio), off),
            APB_TIMER => f(&mut self.timer, off),
            APB_UART0 => f(&mut self.uart[0], off),
            APB_UART1 => f(&mut self.uart[1], off),
            APB_I2C0 => f(&mut self.i2c[0], off),
            APB_I2C1 => f(&mut self.i2c[1], off),
            APB_SPI0 => f(&mut self.spi[0], off),
            APB_SPI1 => f(&mut self.spi[1], off),
            APB_CLOCKS => f(&mut self.clocks, off),
            APB_RTC => {
                self.rtc.now = self.time;
                f(&mut self.rtc, off)
            }
            APB_ADC => {
                self.adc.now = self.time;
                f(&mut self.adc, off)
            }
            APB_PWM => {
                // Events due by now are the system's to run, with the board
                // seeing their edges; one left over only moves the outputs.
                let time = self.time;
                if self.pwm.update(time, |_, _| {}) {
                    self.lines_dirty = true;
                }
                self.gpio
                    .set_pwm_outputs(self.pwm.pin_levels(), self.pwm.pin_enables());
                self.pwm.now = time;
                f(&mut self.pwm, off)
            }
            b => f(&mut self.stubs[b as usize], off),
        })
    }

    /// clk_peri's frequency, from its mux and the PLL it may run from. The
    /// sources the emulator doesn't model (the ring oscillator, GPIN pins),
    /// and PLLs not set up yet, count as clk_sys.
    fn clk_peri_hz(&mut self) -> u64 {
        let hz = match self.clocks.read(CLK_PERI_CTRL) >> 5 & 7 {
            1 => self.pll_hz(APB_PLL_SYS),
            2 => self.pll_hz(APB_PLL_USB),
            4 => XOSC_HZ,
            _ => self.sys_hz,
        };
        if hz == 0 { self.sys_hz } else { hz }
    }

    /// clk_sys's frequency: a PLL's output through clk_sys's divider, when
    /// it runs from one. The sources the emulator doesn't model (the ring
    /// oscillator and clk_ref, which start-up code runs from while it sets
    /// the PLLs up, and the GPIN pins), and PLLs not set up yet, count as
    /// 125 MHz, so firmware that doesn't overclock runs as it always has.
    fn clk_sys_hz(&mut self) -> u64 {
        const DEFAULT: u64 = CYCLES_PER_US * 1_000_000;
        let ctrl = self.clocks.read(CLK_SYS_CTRL);
        let pll = match (ctrl & 1, ctrl >> 5 & 7) {
            (1, 0) => self.pll_hz(APB_PLL_SYS),
            (1, 1) => self.pll_hz(APB_PLL_USB),
            _ => 0,
        };
        // DIV is INT.FRAC in 24.8 bits; 0 divides by 1.
        let div = match self.clocks.read(CLK_SYS_DIV) as u64 {
            0 => 256,
            d => d,
        };
        match pll * 256 / div {
            0 => DEFAULT,
            hz => hz,
        }
    }

    /// clk_sys is now `hz`: what turns cycles into time follows, from now
    /// on (each first catches up to now at the old rate).
    fn set_sys_hz(&mut self, hz: u64) {
        let now = self.time;
        self.timer.update(now);
        self.timer.set_hz(hz);
        self.rtc.set_hz(now, hz);
        for ppb in &mut self.ppb {
            ppb.set_cycles_per_us(hz / 1_000_000);
        }
        for u in &mut self.uart {
            u.set_clk_sys(hz);
        }
        for s in self.spi.iter_mut() {
            s.set_clk_sys(hz);
        }
        self.adc.set_clk_sys(hz);
        self.usb.set_hz(now, hz);
        self.sys_hz = hz;
        self.clock_changed = true;
        self.lines_dirty = true;
    }

    /// clk_sys: the rate the cycles count at.
    pub fn sys_hz(&self) -> u64 {
        self.sys_hz
    }

    /// clk_adc's frequency, from its mux, its divider and the PLL it may run
    /// from. Unmodelled sources (the ring oscillator, GPIN pins), and PLLs
    /// not set up yet, count as the SDK's 48 MHz.
    fn clk_adc_hz(&mut self) -> u64 {
        let hz = match self.clocks.read(CLK_ADC_CTRL) >> 5 & 7 {
            0 => self.pll_hz(APB_PLL_USB),
            1 => self.pll_hz(APB_PLL_SYS),
            3 => XOSC_HZ,
            _ => 0,
        };
        // DIV.INT is two bits; 0 divides by 4.
        let div = match self.clocks.read(CLK_ADC_DIV) >> 8 & 3 {
            0 => 4,
            d => d as u64,
        };
        match hz / div {
            0 => adc::DEFAULT_CLK_HZ,
            hz => hz,
        }
    }

    /// A PLL's output frequency from its registers (0 if not set up).
    fn pll_hz(&mut self, block: u32) -> u64 {
        let regs = &mut self.stubs[block as usize];
        let refdiv = (regs.read(PLL_CS) & 0x3F) as u64;
        let fbdiv = (regs.read(PLL_FBDIV_INT) & 0xFFF) as u64;
        let prim = regs.read(PLL_PRIM);
        let post = (prim >> 16 & 7) as u64 * (prim >> 12 & 7) as u64;
        match refdiv * post {
            0 => 0,
            d => XOSC_HZ * fbdiv / d,
        }
    }

    fn apb_read(&mut self, addr: u32) -> u32 {
        // Reads ignore the atomic alias bits.
        self.with_apb(addr, |p, off| p.read(off)).unwrap_or(0)
    }

    fn apb_write(&mut self, addr: u32, val: u32) {
        let alias = addr >> 12 & 3;
        self.with_apb(addr, |p, off| {
            let val = match alias {
                0 => val,
                1 => p.read(off) ^ val,  // XOR
                2 => p.read(off) | val,  // SET
                _ => p.read(off) & !val, // CLR
            };
            p.write(off, val)
        });
        let rel = addr.wrapping_sub(APB_BASE);
        if rel >> 14 == APB_RESETS {
            self.usb.set_held(self.resets.in_reset(RESET_USBCTRL));
            if self.resets.in_reset(RESET_ADC) {
                self.adc.reset();
            }
            if self.resets.in_reset(RESET_PWM) {
                self.pwm.reset();
            }
        }
        if matches!(rel >> 14, APB_RESETS | APB_PWM) {
            self.gpio
                .set_pwm_outputs(self.pwm.pin_levels(), self.pwm.pin_enables());
        }
        if matches!(rel >> 14, APB_CLOCKS | APB_PLL_SYS | APB_PLL_USB) {
            let hz = self.clk_sys_hz();
            if hz != self.sys_hz {
                self.set_sys_hz(hz);
            }
            let hz = self.clk_peri_hz();
            for u in &mut self.uart {
                u.set_clk_peri(hz);
            }
            for s in &mut self.spi {
                s.set_clk_peri(hz);
            }
            let hz = self.clk_adc_hz();
            self.adc.set_clk_adc(hz);
        }
        if rel >> 14 == APB_IO_QSPI && rel & 0xFFF == QSPI_SS_CTRL {
            let outover = self.stubs[APB_IO_QSPI as usize].read(QSPI_SS_CTRL) >> 8 & 3;
            self.ssi.set_cs_override(outover, &mut self.flash.data);
        }
    }

    fn read_word(&mut self, addr: u32) -> u32 {
        let a = addr & !3;
        match addr >> 24 {
            0x00 if a < BOOTROM_SIZE => self.bootrom.read32(a),
            0x10..=0x13 => self.flash.read32(a & 0xFF_FFFF),
            0x20 | 0x21 if a & 0xFF_FFFF < SRAM_SIZE => self.sram.read32(a & 0xFF_FFFF),
            0x50 if Self::is_dpram(a) => {
                let o = (a & 0xFFF) as usize;
                u32::from_le_bytes(self.usb.dpram[o..o + 4].try_into().unwrap())
            }
            _ => self.mmio_read(a),
        }
    }

    /// USB DPRAM: memory, with byte and halfword writes like SRAM.
    #[inline]
    fn is_dpram(addr: u32) -> bool {
        addr & !0xFFF == USBCTRL_DPRAM_BASE
    }

    fn write_dpram(&mut self, addr: u32, bytes: &[u8]) {
        let o = (addr & 0xFFF) as usize;
        self.usb.dpram[o..o + bytes.len()].copy_from_slice(bytes);
        if (o as u32) < usb::DPRAM_CONTROL_END {
            self.usb.dpram_written(o as u32);
            // The host may have an endpoint to try again.
            self.touched = true;
        }
    }
}

impl Default for SystemBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for SystemBus {
    fn read8(&mut self, addr: u32) -> u8 {
        (self.read_word(addr) >> (8 * (addr & 3))) as u8
    }
    fn read16(&mut self, addr: u32) -> u16 {
        (self.read_word(addr) >> (8 * (addr & 2))) as u16
    }
    fn read32(&mut self, addr: u32) -> u32 {
        self.read_word(addr)
    }
    #[inline]
    fn fetch16(&mut self, addr: u32) -> u16 {
        match addr >> 24 {
            0x10..=0x13 => self.flash.read16(addr & 0xFF_FFFF),
            0x20 | 0x21 if addr & 0xFF_FFFF < SRAM_SIZE => self.sram.read16(addr & 0xFF_FFFF),
            _ => (self.read_word(addr) >> (8 * (addr & 2))) as u16,
        }
    }
    // Sub-word stores to peripherals replicate the value across byte lanes,
    // as the real bus fabric does; memories take them natively.
    fn write8(&mut self, addr: u32, val: u8) {
        match addr >> 24 {
            0x20 | 0x21 if addr & 0xFF_FFFF < SRAM_SIZE => self.sram.write8(addr & 0xFF_FFFF, val),
            0x50 if Self::is_dpram(addr) => self.write_dpram(addr, &[val]),
            0x00 | 0x10..=0x13 => {}
            _ => self.mmio_write(addr & !3, val as u32 * 0x0101_0101),
        }
    }
    fn write16(&mut self, addr: u32, val: u16) {
        match addr >> 24 {
            0x20 | 0x21 if addr & 0xFF_FFFF < SRAM_SIZE => self.sram.write16(addr & 0xFF_FFFF, val),
            0x50 if Self::is_dpram(addr & !1) => self.write_dpram(addr & !1, &val.to_le_bytes()),
            0x00 | 0x10..=0x13 => {}
            _ => self.mmio_write(addr & !3, val as u32 * 0x0001_0001),
        }
    }
    fn write32(&mut self, addr: u32, val: u32) {
        match addr >> 24 {
            0x20 | 0x21 if addr & 0xFF_FFFF < SRAM_SIZE => self.sram.write32(addr & 0xFF_FFFF, val),
            0x50 if Self::is_dpram(addr & !3) => self.write_dpram(addr & !3, &val.to_le_bytes()),
            0x00 | 0x10..=0x13 => {}
            _ => self.mmio_write(addr, val),
        }
    }
    #[inline]
    fn instruction_start(&mut self, cycles: u64) {
        self.now = cycles;
    }
    /// The accesses that end a batch (see `System::run_batch`).
    #[inline]
    fn interrupted(&self) -> bool {
        self.touched | self.lines_dirty
    }
}

/// The bus a core runs a batch on while a [`Stream`] goes: the system bus,
/// with the stream caught up before every access that could tell.
struct StreamBus<'a> {
    bus: &'a mut SystemBus,
    board: &'a mut Board,
}

impl StreamBus<'_> {
    /// Before an access to `addr` of `len` bytes: anything but plain memory,
    /// or a store into the stream's unread source.
    #[inline]
    fn before(&mut self, addr: u32, len: u32, store: bool) {
        let Some(s) = &self.bus.stream else { return };
        let catch_up = match addr >> 24 {
            0x00 | 0x10..=0x13 => false,
            0x20 | 0x21 if addr & 0xFF_FFFF < SRAM_SIZE => {
                let at = addr & 0xFF_FFFF;
                store && at < s.unread.1 && at + len > s.unread.0
            }
            _ => true,
        };
        if catch_up {
            let now = self.bus.now;
            self.bus.stream_catch_up(now, self.board);
        }
    }
}

impl Bus for StreamBus<'_> {
    #[inline]
    fn read8(&mut self, addr: u32) -> u8 {
        self.before(addr, 1, false);
        self.bus.read8(addr)
    }
    #[inline]
    fn read16(&mut self, addr: u32) -> u16 {
        self.before(addr, 2, false);
        self.bus.read16(addr)
    }
    #[inline]
    fn read32(&mut self, addr: u32) -> u32 {
        self.before(addr, 4, false);
        self.bus.read32(addr)
    }
    #[inline]
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.before(addr, 2, false);
        self.bus.fetch16(addr)
    }
    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        self.before(addr, 1, true);
        self.bus.write8(addr, val)
    }
    #[inline]
    fn write16(&mut self, addr: u32, val: u16) {
        self.before(addr, 2, true);
        self.bus.write16(addr, val)
    }
    #[inline]
    fn write32(&mut self, addr: u32, val: u32) {
        self.before(addr, 4, true);
        self.bus.write32(addr, val)
    }
    #[inline]
    fn instruction_start(&mut self, cycles: u64) {
        self.bus.instruction_start(cycles)
    }
    #[inline]
    fn interrupted(&self) -> bool {
        self.bus.interrupted()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    Elf(ElfError),
    Uf2(Uf2Error),
    /// A segment targets an address outside flash / SRAM.
    Unmapped(u32),
    /// A UF2 image has no vector table after boot2 or at the start of SRAM.
    NoVectorTable,
}

impl From<ElfError> for LoadError {
    fn from(e: ElfError) -> Self {
        LoadError::Elf(e)
    }
}

impl From<Uf2Error> for LoadError {
    fn from(e: Uf2Error) -> Self {
        LoadError::Uf2(e)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    CycleLimit,
    /// The firmware executed `bkpt #n`.
    Breakpoint(u8),
    /// Core is in WFI/WFE and nothing can wake it yet.
    Sleeping,
    /// An instruction at `pc` faulted, with [`System::stop_on_fault`] set.
    /// The core has taken the HardFault: its handler is about to run.
    Fault {
        core: usize,
        pc: u32,
        cause: Fault,
    },
}

impl core::fmt::Display for StopReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StopReason::CycleLimit => f.write_str("cycle limit"),
            StopReason::Breakpoint(n) => write!(f, "bkpt #{n}"),
            StopReason::Sleeping => f.write_str("asleep, nothing can wake it"),
            StopReason::Fault { core, pc, cause } => {
                write!(f, "HardFault on core {core} at pc {pc:#010x}: {cause}")
            }
        }
    }
}

/// Exception names, for reports.
fn exception_name(n: u8) -> String {
    match n {
        0 => "thread mode".into(),
        2 => "NMI".into(),
        3 => "HardFault".into(),
        11 => "SVCall".into(),
        14 => "PendSV".into(),
        15 => "SysTick".into(),
        16.. => alloc::format!("IRQ {}", n - 16),
        _ => alloc::format!("exception {n}"),
    }
}

/// How many faults are reported as warnings before the rest go unreported.
const FAULT_WARNINGS: u32 = 10;

/// The earlier of two optional times (`None`: never). These are worked out
/// for every segment and batch, so no iterator chains.
#[inline]
fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) | (None, x) => x,
    }
}

/// What a PIO block sees of the rest of the board while it runs: the pads,
/// and the devices that must react to its output edges at the cycle they happen.
struct PioWiring<'a> {
    gpio: &'a mut Gpio,
    board: &'a mut Board,
    block: usize,
}

impl PioEnv for PioWiring<'_> {
    fn pads_in(&self) -> u32 {
        self.gpio.pad_inputs()
    }

    fn pads_out(&mut self, out: u32, oe: u32, cycle: u64) {
        self.gpio.set_pio_outputs(self.block, out, oe);
        // Devices see each edge with its own timestamp; one may drive a pin
        // another watches, so go round until the pins settle.
        for _ in 0..8 {
            let changed = self.gpio.take_changes();
            if changed == 0 {
                break;
            }
            self.board.service(self.gpio, changed, cycle);
        }
    }
}

pub struct System {
    /// The two Cortex-M0+ cores. Core 1 runs the bootrom's launch loop from
    /// power-up until firmware starts it.
    pub cores: [Cpu; 2],
    pub bus: SystemBus,
    pub board: Board,
    /// Cores held in reset (see [`Psm`]).
    held: [bool; 2],
    /// Nothing but the timer and SysTick can change on their own until a
    /// register is touched or a pin changes: no DMA transfer, PIO program,
    /// or bus transaction is pending. Lets a step skip the full device sync.
    quiet: bool,
    /// Let `run` execute quiet stretches in batches (see `run_batch`). Only
    /// off to check that batching changes nothing.
    pub batching: bool,
    /// Problems spotted while running (a bus used with the board's pins not
    /// routed to it), for the host to show. Each is reported once.
    pub warnings: Vec<String>,
    /// Buses whose wiring was checked: I2C0, I2C1, SPI0, SPI1.
    wiring_checked: [bool; 4],
    /// Stop `run` when an instruction faults, before its HardFault handler
    /// runs. Off by default: the handler runs, and the fault is a warning.
    pub stop_on_fault: bool,
    /// The last fault of each core: the faulting instruction and why.
    pub last_fault: [Option<(u32, Fault)>; 2],
    /// Faults reported as warnings so far.
    faults_reported: u32,
    /// Where the `bkpt` that last stopped `run` was: the core, and the
    /// instruction's address (the core's pc is past it).
    pub bkpt_at: Option<(usize, u32)>,
    /// A fault `run` should stop on.
    fault_stop: Option<(u32, Fault)>,
    /// The PC on the other end of the USB cable.
    usb_host: usbhost::Host,
    /// When the host wants to be polled next.
    usb_wake: Option<u64>,
    /// Each clk_sys rate the run has had: the cycle it started at, the
    /// emulated seconds by then, and the rate.
    clock_log: Vec<(u64, f64, u64)>,
}

/// clk_sys from some cycle on: its rate, the cycle it took it at, and the
/// emulated seconds by then. A copy turns cycles into time without the
/// [`System`], for cycles from `at` on (or a little before: the rate is the
/// same back to the change before).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clock {
    pub at: u64,
    pub secs: f64,
    pub hz: u64,
}

impl Clock {
    /// The clock of a run that never changed it.
    pub const DEFAULT: Clock = Clock {
        at: 0,
        secs: 0.0,
        hz: CYCLES_PER_US * 1_000_000,
    };

    /// Emulated seconds at `cycles`.
    pub fn seconds(&self, cycles: u64) -> f64 {
        self.secs + (cycles as f64 - self.at as f64) / self.hz as f64
    }

    /// The cycle at which `secs` emulated seconds will have passed.
    pub fn cycle(&self, secs: f64) -> u64 {
        let c = self.at as f64 + (secs - self.secs) * self.hz as f64;
        (c.max(0.0) + 0.5) as u64
    }
}

impl System {
    pub fn new() -> Self {
        let mut sys = Self {
            cores: [Cpu::new(), Cpu::new()],
            bus: SystemBus::new(),
            board: Board::new(),
            held: [false; 2],
            quiet: false,
            batching: true,
            warnings: Vec::new(),
            wiring_checked: [false; 4],
            stop_on_fault: false,
            last_fault: [None; 2],
            faults_reported: 0,
            fault_stop: None,
            bkpt_at: None,
            usb_host: usbhost::Host::new(CYCLES_PER_US),
            usb_wake: None,
            clock_log: alloc::vec![(0, 0.0, CYCLES_PER_US * 1_000_000)],
        };
        sys.reset_core1();
        sys
    }

    /// Current device time in core clock cycles.
    pub fn cycles(&self) -> u64 {
        self.bus.time
    }

    /// Emulated seconds at cycle `cycles`, through the clk_sys rates the run
    /// has had (cycles before the last change are taken at their rate).
    pub fn seconds_at(&self, cycles: u64) -> f64 {
        let i = self.clock_log.partition_point(|c| c.0 <= cycles).max(1) - 1;
        let (at, secs, hz) = self.clock_log[i];
        secs + cycles.saturating_sub(at) as f64 / hz as f64
    }

    /// Emulated seconds since power-up.
    pub fn seconds(&self) -> f64 {
        self.seconds_at(self.cycles())
    }

    /// clk_sys as it is now, to turn cycles into time and back.
    pub fn clock(&self) -> Clock {
        let &(at, secs, hz) = self.clock_log.last().unwrap();
        Clock { at, secs, hz }
    }

    /// clk_sys: the rate the cycles count at now.
    pub fn sys_hz(&self) -> u64 {
        self.bus.sys_hz
    }

    /// Bring what lives outside the bus (the USB host, the board) to a new
    /// clk_sys, and note when it changed.
    #[cold]
    #[inline(never)]
    fn follow_clock(&mut self) {
        self.bus.clock_changed = false;
        let (now, hz) = (self.bus.time, self.bus.sys_hz);
        let secs = self.seconds_at(now);
        self.clock_log.push((now, secs, hz));
        self.usb_host.set_cycles_per_us(hz / 1_000_000);
        self.board.set_clock(now, hz);
    }

    /// Hold a core in reset (as the PSM does) or release it. Releasing core 1
    /// restarts it in the bootrom; core 0 is only paused.
    pub fn set_core_held(&mut self, core: usize, held: bool) {
        if core == 1 && !held && self.held[1] {
            self.reset_core1();
        }
        self.held[core] = held;
    }

    /// Restart core 1 from the bootrom's reset vector.
    fn reset_core1(&mut self) {
        self.bus.ppb[1] = Ppb::new(self.bus.sys_hz / 1_000_000);
        self.bus.vtor[1] = 0;
        self.cores[1] = Cpu::new();
        self.cores[1].reset_from_vector_table(&mut self.bus, BOOTROM_BASE);
        self.cores[1].cycles = self.bus.time;
    }

    /// Load an ELF or UF2 image, whichever `bytes` is, and reset the core
    /// into it.
    pub fn load_firmware(&mut self, bytes: &[u8]) -> Result<(), LoadError> {
        if uf2::is_uf2(bytes) {
            self.load_uf2(bytes)
        } else {
            self.load_elf(bytes)
        }
    }

    /// Load a UF2 image and reset the core into it. A UF2 has no entry
    /// point: start from the vector table the bootrom would use, after boot2
    /// in flash, or at the start of SRAM for a RAM-only image.
    pub fn load_uf2(&mut self, bytes: &[u8]) -> Result<(), LoadError> {
        let blocks = uf2::parse(bytes)?;
        for b in &blocks {
            if !self.bus.load(b.addr, b.data) {
                return Err(LoadError::Unmapped(b.addr));
            }
        }
        let in_flash = blocks
            .iter()
            .any(|b| b.addr >= XIP_BASE && b.addr < SRAM_BASE);
        let vt = if in_flash {
            XIP_BASE + BOOT2_SIZE
        } else {
            SRAM_BASE
        };
        let ram = SRAM_BASE..=SRAM_BASE + SRAM_SIZE;
        if !ram.contains(&self.bus.read32(vt)) {
            return Err(LoadError::NoVectorTable);
        }
        self.reset_from_vector_table(vt);
        Ok(())
    }

    /// Load an ELF image and reset the core into it.
    pub fn load_elf(&mut self, bytes: &[u8]) -> Result<(), LoadError> {
        let elf = elf::parse(bytes)?;
        for seg in &elf.segments {
            if !self.bus.load(seg.addr, seg.data) {
                return Err(LoadError::Unmapped(seg.addr));
            }
            if seg.zero_fill != 0 {
                let zeros = alloc::vec![0u8; seg.zero_fill as usize];
                if !self.bus.load(seg.addr + seg.data.len() as u32, &zeros) {
                    return Err(LoadError::Unmapped(seg.addr));
                }
            }
        }
        // Find the vector table: after boot2 in flash images, at the start of
        // a RAM image. Accept the candidate whose reset vector is the ELF
        // entry point and whose initial SP is in SRAM.
        let entry = elf.entry & !1;
        let base = entry & 0xFF00_0000;
        let candidates = [base + BOOT2_SIZE, base, SRAM_BASE];
        let ram = SRAM_BASE..=SRAM_BASE + SRAM_SIZE;
        for vt in candidates {
            let (sp, pc) = (self.bus.read32(vt), self.bus.read32(vt + 4));
            if pc & !1 == entry && ram.contains(&sp) {
                self.reset_from_vector_table(vt);
                return Ok(());
            }
        }
        // pico-sdk flash images: the entry is `_entry_point`, which re-enters
        // through the bootrom, while the table after boot2 holds
        // `_reset_handler`. Start from that table, as the bootrom would.
        if base == XIP_BASE {
            let vt = XIP_BASE + BOOT2_SIZE;
            let (sp, pc) = (self.bus.read32(vt), self.bus.read32(vt + 4));
            if pc & 1 == 1 && (XIP_BASE..SRAM_BASE).contains(&pc) && ram.contains(&sp) {
                self.reset_from_vector_table(vt);
                return Ok(());
            }
        }
        // No recognisable table: start at the entry with a full-descending
        // stack from the top of SRAM.
        self.cores[0].reset_to(SRAM_BASE + SRAM_SIZE, entry);
        self.bus.vtor[0] = 0;
        Ok(())
    }

    /// Reset core 0 into the vector table at `vtor`.
    pub fn reset_from_vector_table(&mut self, vtor: u32) {
        self.bus.core = 0;
        self.cores[0].reset_from_vector_table(&mut self.bus, vtor);
        self.bus.vtor[0] = vtor;
    }

    /// Plug a PCB into the pins, replacing any previous one.
    pub fn set_board(&mut self, mut board: Board) {
        board.set_clock(self.bus.time, self.bus.sys_hz);
        board.attach(&mut self.bus.gpio);
        self.bus.gpio.take_changes();
        self.board = board;
        self.wiring_checked = [false; 4];
        self.quiet = false;
    }

    /// Do something to a board device from the host side (see each device's
    /// docs for its actions). Returns false if the part or action is unknown.
    pub fn input(&mut self, name: &str, action: &str, value: i64) -> bool {
        let r = self
            .board
            .input(&mut self.bus.gpio, name, action, value, self.bus.time);
        self.settle_inputs();
        r == InputResult::Done
    }

    /// Host -> firmware: put bytes on UART `n`'s RX line. They arrive at the
    /// baud rate the firmware set (see [`Uart::receive`]).
    pub fn send_uart(&mut self, n: usize, bytes: &[u8]) {
        self.bus.uart[n].receive(bytes);
        self.bus.lines_dirty = true;
        self.quiet = false;
        self.refresh_core_views();
    }

    /// The USB host: what it found on the device, its traffic log.
    pub fn usb_host(&self) -> &usbhost::Host {
        &self.usb_host
    }

    /// Change something on the USB host side (queue data for the device,
    /// take what it sent); the host looks at the bus again right away.
    pub fn with_usb_host<R>(&mut self, f: impl FnOnce(&mut usbhost::Host) -> R) -> R {
        let r = f(&mut self.usb_host);
        self.bus.usb.kicked = true;
        self.bus.touched = true;
        self.quiet = false;
        r
    }

    /// Let the USB host run its transactions due now.
    fn service_usb(&mut self) {
        let now = self.bus.time;
        let kicked = core::mem::take(&mut self.bus.usb.kicked);
        self.bus.usb.update(now);
        self.usb_wake = self
            .usb_host
            .poll(&mut self.bus.usb, now, kicked)
            .map(|t| t.max(now + 1));
        self.bus.lines_dirty = true;
    }

    /// Press or release a board button. False if there is no such button.
    pub fn set_button(&mut self, name: &str, down: bool) -> bool {
        self.input(name, if down { "press" } else { "release" }, 0)
    }

    /// A host action may have moved pins and raised an interrupt while the
    /// cores sleep: let devices react, then refresh the NVIC inputs.
    fn settle_inputs(&mut self) {
        self.bus.lines_dirty = true;
        self.quiet = false;
        self.sync_board();
        self.refresh_core_views();
    }

    /// Let time pass with every core asleep (for hosts that skip ahead to
    /// the next scheduled input). Does nothing if `cycle` is not in the future.
    pub fn idle_until(&mut self, cycle: u64) {
        if cycle > self.bus.time {
            self.bus.lines_dirty = true;
            self.bus.touched = true;
            self.advance_to(cycle);
        }
    }

    /// Let the UARTs send what the firmware left in their TX FIFOs, as they
    /// keep doing on the chip after the cores stop (pico-sdk's `_exit` is a
    /// `bkpt` right after `main` returns, often with the last line still
    /// queued). Device time moves on; the cores stay where they are.
    pub fn drain_uarts(&mut self) {
        // At most a FIFO plus the shift register each.
        for _ in 0..2 * 34 {
            match self.bus.uart.iter().filter_map(Uart::tx_event).min() {
                Some(t) => self.idle_until(t),
                None => break,
            }
        }
    }

    /// Let board devices react to pin changes, until the pins settle (a
    /// device may drive a pin that another one watches).
    fn sync_board(&mut self) {
        for _ in 0..8 {
            if !self.bus.gpio.has_changes() {
                break;
            }
            let changed = self.bus.gpio.take_changes();
            self.bus.lines_dirty = true;
            self.quiet = false;
            self.board
                .service(&mut self.bus.gpio, changed, self.bus.time);
        }
    }

    /// Run bus transactions the firmware has queued in the controllers
    /// against the devices on the board.
    fn service_buses(&mut self) {
        for n in 0..2 {
            if self.bus.i2c[n].has_work() {
                self.check_wiring(n);
                self.bus.lines_dirty = true;
                let mut wire = self.board.i2c_bus(n, &mut self.bus.gpio, self.bus.time);
                self.bus.i2c[n].pump(&mut wire);
            }
            if self.bus.spi[n].has_work() {
                self.check_wiring(2 + n);
                self.bus.lines_dirty = true;
                let mut wire = self.board.spi_bus(n, &mut self.bus.gpio, self.bus.time);
                self.bus.spi[n].pump(&mut wire, self.bus.time);
            }
        }
    }

    /// The first time the firmware uses a bus controller (0, 1: I2C; 2, 3:
    /// SPI), compare the board's wiring for it with the firmware's pin
    /// functions.
    fn check_wiring(&mut self, which: usize) {
        if core::mem::replace(&mut self.wiring_checked[which], true) {
            return;
        }
        let name = ["i2c0", "i2c1", "spi0", "spi1"][which];
        if let Some(decl) = self.board.buses.iter().find(|b| b.name == name) {
            self.warnings
                .extend(crate::pinmux::wiring_warnings(decl, &self.bus.gpio));
        }
    }

    /// Let the DMA controller move up to `budget` items (one per cycle).
    /// Returns how many it moved.
    fn run_dma(&mut self, budget: u64) -> u64 {
        if self.bus.resets.in_reset(RESET_DMA) {
            return 0;
        }
        if !self.bus.dma.active() {
            return 0;
        }
        self.bus.dma.sync_timers(self.bus.time);
        let mut moved = 0;
        while moved < budget {
            let dreq = self.bus.dreq_mask(self.bus.dma.needed_dreqs());
            let Some(t) = self.bus.dma.pick(dreq) else {
                break;
            };
            // A wrap is one request: the transfer it paces serves it.
            if let pwm @ 24..=31 = self.bus.dma.view(t.channel).treq {
                self.bus.pwm.served(pwm as usize - 24);
            }
            let mut v = match t.bytes {
                1 => self.bus.read8(t.read) as u32,
                2 => self.bus.read16(t.read) as u32,
                _ => self.bus.read32(t.read),
            };
            if t.bswap {
                v = match t.bytes {
                    2 => (v as u16).swap_bytes() as u32,
                    4 => v.swap_bytes(),
                    _ => v,
                };
            }
            let touched = self.bus.touched;
            match t.bytes {
                1 => self.bus.write8(t.write, v as u8),
                2 => self.bus.write16(t.write, v as u16),
                _ => self.bus.write32(t.write, v),
            }
            self.bus.dma.complete(&t);
            self.bus.lines_dirty = true;
            // The write may have queued a frame for a board device.
            self.service_buses();
            // That was all a bus controller's FIFO needed: the rest of this
            // sync covers it, so it doesn't cost the cores a sync of their
            // own (a DMA stream into a timed SPI would, once per frame).
            if SystemBus::is_bus_controller(t.write) {
                self.bus.touched = touched;
            }
            moved += 1;
        }
        moved
    }

    /// Could the DMA controller move something right now?
    fn dma_runnable(&self) -> bool {
        if self.bus.resets.in_reset(RESET_DMA) {
            return false;
        }
        // No running channel means no request bits at all.
        let needed = self.bus.dma.needed_dreqs();
        needed != 0 && self.bus.dma.can_run(self.bus.dreq_mask(needed))
    }

    /// Is a PIO machine running a program (not parked waiting on the outside)?
    fn pio_active(&self) -> bool {
        (0..2).any(|b| !self.bus.resets.in_reset(RESET_PIO[b]) && self.bus.pio[b].active())
    }

    /// Give each core's NVIC the current IRQ inputs and its core's IPSR.
    fn refresh_core_views(&mut self) {
        let dirty = core::mem::take(&mut self.bus.lines_dirty);
        for i in 0..2 {
            if dirty {
                self.bus.ppb[i].lines = self.bus.irq_lines(i);
            }
            self.bus.ppb[i].ipsr = self.cores[i].ipsr;
        }
    }

    /// Bring every device up to time `to`, stopping at each cycle a board
    /// device asked to be woken so its action lands at the right time for
    /// everything running (PIO sampling its pins, DMA, the cores).
    fn advance_to(&mut self, to: u64) {
        if self.bus.clock_changed {
            self.follow_clock();
        }
        loop {
            let board = self.board.next_timer().filter(|&t| t <= to);
            let usb = self.usb_wake.filter(|&t| t <= to);
            let Some(due) = earliest(board, usb) else {
                self.advance_segment(to);
                break;
            };
            self.advance_segment(due.max(self.bus.time));
            if board.is_some_and(|t| t <= due) {
                self.board
                    .fire_timers(&mut self.bus.gpio, due.max(self.bus.time));
                self.quiet = false;
                self.sync_board();
            }
            if self.usb_wake.is_some_and(|t| t <= self.bus.time) {
                self.service_usb();
                self.refresh_core_views();
            }
        }
    }

    /// Bring every device up to time `to` (no board timers in between): run
    /// the bus work the cores queued, let DMA move one item per elapsed
    /// cycle, step the PIO blocks, advance timers and SysTick, refresh the
    /// interrupt lines and let board devices react.
    fn advance_segment(&mut self, to: u64) {
        self.bus.stream_catch_up(to, &mut self.board);
        let from = self.bus.time;
        if self.quiet && !self.bus.touched && !self.bus.gpio.has_changes() && !self.pio_active() {
            if to > from {
                self.bus.time = to;
                self.bus.pio_owed += to - from;
                self.advance_clocks(from, to);
            }
            // A device's own event (a frame ending on a timed SPI wire, a
            // UART character) raised a DMA request: the DMA answers it now,
            // as it would on its next cycle, so a DMA stream costs one sync
            // per item, not a sync and then a core step on its own.
            if self.bus.touched && self.dma_runnable() {
                self.bus.touched = false;
                self.run_dma(1);
                self.refresh_core_views();
                self.sync_board();
                self.quiet = self.is_quiet();
                return;
            }
            self.refresh_core_views();
            return;
        }
        self.bus.touched = false;
        self.bus.flush_pio();
        self.service_buses();
        if self.bus.usb.kicked {
            self.service_usb();
        }
        if to > from {
            self.bus.time = to;
            self.run_dma(to - from);
            for block in 0..2 {
                if !self.bus.resets.in_reset(RESET_PIO[block]) {
                    let mut env = PioWiring {
                        gpio: &mut self.bus.gpio,
                        board: &mut self.board,
                        block,
                    };
                    let pio = &mut self.bus.pio[block];
                    let lines = (pio.irq(0), pio.irq(1));
                    pio.advance(from, to - from, &mut env);
                    // Only the block's interrupt outputs reach the NVIC.
                    if core::mem::take(&mut pio.ticked) && (pio.irq(0), pio.irq(1)) != lines {
                        self.bus.lines_dirty = true;
                    }
                }
            }
            self.advance_clocks(from, to);
        }
        self.refresh_core_views();
        self.sync_board();
        self.quiet = self.is_quiet();
    }

    /// The part of a device sync that never goes idle: timer, SysTick, the
    /// UART lines, and the clocks of sleeping cores.
    fn advance_clocks(&mut self, from: u64, to: u64) {
        let timer_lines = self.bus.timer.irq_lines();
        self.bus.timer.update(to);
        if self.bus.timer.irq_lines() != timer_lines {
            self.bus.lines_dirty = true;
        }
        for ppb in &mut self.bus.ppb {
            ppb.advance(to - from);
        }
        self.bus.update_uarts(to);
        self.bus.update_usb(to);
        self.update_adc(to);
        self.update_pwm(to);
        // A sleeping core's clock keeps pace with real time.
        for (cpu, held) in self.cores.iter_mut().zip(self.held) {
            if cpu.sleeping && !held {
                cpu.cycles = cpu.cycles.max(to);
            }
        }
    }

    /// Finish the ADC conversions due by `to`, sampling the board's analog
    /// sources. A pin no device drives reads as its digital level (a pull,
    /// a button), at 0 or 3.3 V. A result can raise the FIFO interrupt or
    /// DMA request, so it gets a full sync.
    fn update_adc(&mut self, to: u64) {
        if self.bus.adc.next_event().is_none_or(|t| t > to) {
            return;
        }
        let (board, gpio) = (&self.board, &self.bus.gpio);
        let sample = |pin: usize, at: u64| {
            board
                .analog(pin, at)
                .unwrap_or(if gpio.pin_level(pin) { adc::VREF } else { 0.0 })
        };
        if self.bus.adc.update(to, sample) {
            self.bus.lines_dirty = true;
            self.bus.touched = true;
        }
    }

    /// Run the PWM slices' edges and wraps due by `to`, each at its own
    /// cycle: the pins change then, and the board hears of it then.
    fn update_pwm(&mut self, to: u64) {
        if self.bus.pwm.next_event().is_none_or(|t| t > to) {
            return;
        }
        let (gpio, board) = (&mut self.bus.gpio, &mut self.board);
        let wrapped = self.bus.pwm.update(to, |pwm, cycle| {
            gpio.set_pwm_outputs(pwm.pin_levels(), pwm.pin_enables());
            for _ in 0..8 {
                let changed = gpio.take_changes();
                if changed == 0 {
                    break;
                }
                board.service(gpio, changed, cycle);
            }
        });
        if wrapped {
            self.bus.lines_dirty = true;
            self.bus.touched = true;
        }
    }

    /// Is every device other than the timer and SysTick at rest, so that
    /// only a register access or a pin change can disturb it?
    fn is_quiet(&self) -> bool {
        !self.bus.gpio.has_changes()
            && !self.dma_runnable()
            && (0..2).all(|b| {
                // A block that runs is quiet until its next tick (an event):
                // once its synchronizer has the pads, only its dividers move
                // before then.
                let (pio, pads) = (&self.bus.pio[b], self.bus.gpio.pad_inputs());
                self.bus.resets.in_reset(RESET_PIO[b])
                    || pio.parked(pads)
                    || pio.settled_inputs(pads)
            })
            && (0..2).all(|n| !self.bus.i2c[n].has_work() && !self.bus.spi[n].has_work())
    }

    /// Apply a PSM reset request for core 1, if there was one.
    fn apply_psm(&mut self) -> bool {
        match self.bus.psm.take_proc1_change() {
            Some(true) => self.held[1] = true,
            Some(false) => self.set_core_held(1, false),
            None => return false,
        }
        true
    }

    /// Wake `core` if an interrupt is pending (WFI or WFE) or, for WFE, an
    /// event arrived. A core that sleeps through time resumes at the
    /// current device time.
    fn try_wake(&mut self, core: usize) {
        let cpu = &mut self.cores[core];
        if !cpu.sleeping || self.held[core] {
            return;
        }
        let by_irq = self.bus.ppb[core].wakes_core();
        let by_event = cpu.wfe && cpu.event;
        if by_irq || by_event {
            cpu.sleeping = false;
            cpu.wfe = false;
            if by_event {
                cpu.event = false;
            }
            cpu.cycles = cpu.cycles.max(self.bus.time);
        }
    }

    /// The core that executes next: of the running cores, the one whose
    /// clock is earliest (core 0 on ties). None if every core is asleep or held.
    fn next_core(&mut self) -> Option<usize> {
        self.try_wake(0);
        self.try_wake(1);
        (0..2)
            .filter(|&i| !self.held[i] && !self.cores[i].sleeping)
            .min_by_key(|&i| (self.cores[i].cycles, i))
    }

    /// Execute one instruction on `core`, or take its pending exception
    /// instead, then bring the devices up to date. Returns the cycles it took.
    fn step_core(&mut self, core: usize) -> u32 {
        self.bus.core = core;
        let cpu = &mut self.cores[core];
        let before = cpu.cycles;
        let cycles = match self.bus.ppb[core].pick(cpu.ipsr, cpu.primask) {
            Some(exc) => {
                self.bus.ppb[core].acknowledge(exc);
                cpu.take_exception(&mut self.bus, exc);
                (cpu.cycles - before) as u32
            }
            None => cpu.step(&mut self.bus),
        };
        cpu.vtor = self.bus.vtor[core];
        if core::mem::take(&mut cpu.sev_sent) {
            // SEV signals every processor in the system.
            self.cores[1 - core].event = true;
        }
        self.note_fault(core);
        self.bus.core = 0;
        self.apply_psm();
        // Devices follow the earliest core that is still running (or, if the
        // step put the last running core to sleep, that core's clock).
        let running = (0..2)
            .filter(|&i| !self.held[i] && !self.cores[i].sleeping)
            .map(|i| self.cores[i].cycles)
            .min();
        self.advance_to(
            running
                .unwrap_or(self.cores[core].cycles)
                .max(self.bus.time),
        );
        cycles
    }

    /// Can `run` execute a batch of instructions without syncing devices
    /// after each one? Only when nothing but the timer, SysTick and PIO
    /// dividers is moving, and no register access is waiting for a sync.
    fn can_batch(&self) -> bool {
        self.batching
            && self.quiet
            && !self.bus.touched
            && !self.bus.lines_dirty
            && !self.bus.gpio.has_changes()
    }

    /// The cycle at which a device next changes state on its own.
    fn batch_limit(&self) -> u64 {
        let mut event = self.bus.cycles_to_event();
        // A PIO block that runs is quiet only until its next tick, which a
        // full sync steps (while the cores sleep, the PIO runs its stretch
        // in one go instead).
        for block in 0..2 {
            if !self.bus.resets.in_reset(RESET_PIO[block]) {
                event = earliest(event, self.bus.pio[block].cycles_to_tick());
            }
        }
        let event = event.map(|c| self.bus.time + c);
        let timer = self.board.next_timer();
        earliest(earliest(event, timer), self.usb_wake).unwrap_or(u64::MAX)
    }

    /// Like `step_core`, but keeps executing `core` while the devices can
    /// be left alone: it stays the earliest core, no device event is due,
    /// and no register access needs a full sync. Timer, SysTick and PIO
    /// dividers are brought up to date by the bus when an access looks at
    /// them, and everything else catches up once at the end.
    fn run_batch(&mut self, core: usize, end: u64) {
        let other = 1 - core;
        self.bus.stream = self.bus.find_stream();
        let mut limit = self.batch_limit();
        self.bus.core = core;
        loop {
            let cpu = &mut self.cores[core];
            self.bus.now = cpu.cycles;
            match self.bus.ppb[core].pick(cpu.ipsr, cpu.primask) {
                Some(exc) => {
                    self.bus.ppb[core].acknowledge(exc);
                    // (Stacking stores to SRAM, maybe the stream's source.)
                    self.bus.stream_catch_up(cpu.cycles, &mut self.board);
                    cpu.take_exception(&mut self.bus, exc);
                }
                None if cpu.sleeping => {
                    self.bus.stream_catch_up(cpu.cycles, &mut self.board);
                    cpu.step(&mut self.bus);
                }
                // Run until one of the checks below could come out
                // differently: the limits (the other core's turn included:
                // it goes first once it is behind, or level and lower
                // numbered), an access that needs the system, or a change
                // to the core's own state. Between those, no exception
                // can become pending, so none is looked for.
                None => {
                    let o = &self.cores[other];
                    let turn = match !self.held[other] && !o.sleeping {
                        true => o.cycles + (other > core) as u64,
                        false => u64::MAX,
                    };
                    let cpu = &mut self.cores[core];
                    let until = limit.min(end).min(turn);
                    if self.bus.stream.is_some() {
                        let mut bus = StreamBus {
                            bus: &mut self.bus,
                            board: &mut self.board,
                        };
                        cpu.run_threaded(&mut bus, until);
                    } else {
                        cpu.run_threaded(&mut self.bus, until);
                    }
                }
            }
            let cpu = &mut self.cores[core];
            self.bus.ppb[core].ipsr = cpu.ipsr;
            let c = cpu.cycles;
            // Everything below the hot path starts from a register access
            // (`lines_dirty` is set by every MMIO access, and pin and PSM
            // changes only come from those) or from the core itself.
            let rare = cpu.sleeping
                | cpu.bkpt.is_some()
                | cpu.fault.is_some()
                | cpu.sev_sent
                | self.bus.touched
                | self.bus.lines_dirty;
            if !rare {
                let o = &self.cores[other];
                let other_first = !self.held[other] && !o.sleeping && (o.cycles, other) < (c, core);
                if c >= limit || c >= end || other_first {
                    break;
                }
                continue;
            }
            cpu.vtor = self.bus.vtor[core];
            let mut stop = cpu.sleeping || cpu.bkpt.is_some() || cpu.fault.is_some();
            self.note_fault(core);
            let cpu = &mut self.cores[core];
            if core::mem::take(&mut cpu.sev_sent) {
                self.cores[other].event = true;
                stop = true;
            }
            stop |= self.apply_psm() || self.bus.touched || self.bus.gpio.has_changes();
            if !stop && self.bus.lines_dirty {
                self.refresh_core_views();
                let o = &self.cores[other];
                stop = o.sleeping
                    && !self.held[other]
                    && (self.bus.ppb[other].wakes_core() || (o.wfe && o.event));
                limit = self.batch_limit();
            }
            let o = &self.cores[other];
            let other_first = !self.held[other] && !o.sleeping && (o.cycles, other) < (c, core);
            if stop || c >= limit || c >= end || other_first {
                break;
            }
        }
        self.bus.core = 0;
        let running = (0..2)
            .filter(|&i| !self.held[i] && !self.cores[i].sleeping)
            .map(|i| self.cores[i].cycles)
            .min();
        self.advance_to(
            running
                .unwrap_or(self.cores[core].cycles)
                .max(self.bus.time),
        );
        // Caught up to the devices' time: from here the SPI is followed
        // frame by frame again.
        self.bus.stream = None;
    }

    /// Advance the system by one instruction of whichever core is next, or
    /// by one idle cycle if every core is asleep. Returns the cycles that
    /// instruction took.
    pub fn step(&mut self) -> u32 {
        self.bus.lines_dirty = true;
        self.bus.touched = true;
        match self.next_core() {
            Some(core) => self.step_core(core),
            None => {
                self.advance_to(self.bus.time + 1);
                1
            }
        }
    }

    /// Run for about `max_cycles` cycles (stops early on `bkpt`, or when
    /// every core sleeps with nothing that could ever wake it).
    pub fn run(&mut self, max_cycles: u64) -> StopReason {
        self.bus.lines_dirty = true;
        self.bus.touched = true;
        let end = self.bus.time.saturating_add(max_cycles);
        self.fault_stop = None;
        while self.bus.time < end {
            let Some(core) = self.next_core() else {
                // Nothing to execute: skip ahead to the next device event,
                // in short slices while the DMA controller or a PIO has work to do.
                let timer = self
                    .board
                    .next_timer()
                    .map(|t| t.saturating_sub(self.bus.time).max(1));
                let usb = match self.bus.usb.kicked {
                    true => Some(1),
                    false => self
                        .usb_wake
                        .map(|t| t.saturating_sub(self.bus.time).max(1)),
                };
                let event = earliest(earliest(self.bus.cycles_to_event(), timer), usb);
                let slice = match (event, self.dma_runnable() || self.pio_active()) {
                    (None, false) => return StopReason::Sleeping,
                    (Some(c), false) => c,
                    (c, true) => c.map_or(DMA_SLICE, |c| c.min(DMA_SLICE)),
                };
                self.advance_to((self.bus.time + slice.max(1)).min(end));
                continue;
            };
            if self.can_batch() {
                self.run_batch(core, end);
            } else {
                self.step_core(core);
            }
            if let Some(n) = self.cores[core].bkpt.take() {
                self.bkpt_at = Some((core, self.cores[core].pc.wrapping_sub(2)));
                return StopReason::Breakpoint(n);
            }
            if let Some((pc, cause)) = self.fault_stop.take() {
                return StopReason::Fault { core, pc, cause };
            }
        }
        StopReason::CycleLimit
    }

    /// Record a fault the core just took (as a warning, up to a point), and
    /// leave it for `run` to stop on if it should.
    fn note_fault(&mut self, core: usize) {
        let Some((pc, cause)) = self.cores[core].fault.take() else {
            return;
        };
        self.last_fault[core] = Some((pc, cause));
        if self.stop_on_fault {
            self.fault_stop = Some((pc, cause));
        }
        self.faults_reported += 1;
        match self.faults_reported {
            ..FAULT_WARNINGS => self.warnings.push(alloc::format!(
                "core {core}: HardFault at pc {pc:#010x}: {cause}"
            )),
            FAULT_WARNINGS => self.warnings.push(alloc::format!(
                "core {core}: HardFault at pc {pc:#010x}: {cause} (further faults are not reported)"
            )),
            _ => {}
        }
    }

    /// A report of `core`'s state for when a run stops: its registers, and
    /// in a fault handler the exception frame it stacked and the last fault.
    /// Reads only RAM, so it changes nothing.
    pub fn core_report(&self, core: usize) -> String {
        use core::fmt::Write;
        let cpu = &self.cores[core];
        let mut out = String::new();
        let mode = exception_name(cpu.ipsr);
        let _ = writeln!(out, "core {core}: pc {:#010x}, in {mode}", cpu.pc);
        let line = |out: &mut String, regs: &[(&str, u32)]| {
            let mut l = String::from(" ");
            for (n, v) in regs {
                let _ = write!(l, " {n:>4} {v:08x}");
            }
            let _ = writeln!(out, "{l}");
        };
        let r = |i: usize| {
            (
                [
                    "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12",
                ][i],
                cpu.r[i],
            )
        };
        line(&mut out, &[r(0), r(1), r(2), r(3), r(4), r(5), r(6)]);
        line(&mut out, &[r(7), r(8), r(9), r(10), r(11), r(12)]);
        let xpsr = (cpu.n as u32) << 31
            | (cpu.z as u32) << 30
            | (cpu.c as u32) << 29
            | (cpu.v as u32) << 28
            | 1 << 24
            | cpu.ipsr as u32;
        line(
            &mut out,
            &[("sp", cpu.sp()), ("lr", cpu.lr), ("xpsr", xpsr)],
        );
        if cpu.ipsr != 0 {
            // EXC_RETURN in lr says which stack holds the frame.
            let sp = match cpu.lr {
                0xFFFF_FFFD => cpu.psp,
                _ => cpu.sp(),
            };
            let frame: Option<Vec<u32>> = (0..8)
                .map(|i| {
                    let a = sp.wrapping_add(4 * i);
                    (a >= SRAM_BASE && a + 4 <= SRAM_BASE + SRAM_SIZE)
                        .then(|| self.bus.sram.read32(a - SRAM_BASE))
                })
                .collect();
            match frame {
                Some(f) => {
                    let _ = writeln!(
                        out,
                        "  {mode} entered from pc {:#010x} (lr {:#010x}, xpsr {:#010x}), stacked at {sp:#010x}:",
                        f[6], f[5], f[7]
                    );
                    line(
                        &mut out,
                        &[
                            ("r0", f[0]),
                            ("r1", f[1]),
                            ("r2", f[2]),
                            ("r3", f[3]),
                            ("r12", f[4]),
                        ],
                    );
                }
                None => {
                    let _ = writeln!(out, "  no exception frame: sp {sp:#010x} is outside SRAM");
                }
            }
            if cpu.ipsr == EXC_HARDFAULT {
                match self.last_fault[core] {
                    Some((pc, cause)) => {
                        let _ = writeln!(out, "  cause: {cause}, by the instruction at {pc:#010x}");
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  cause: not an instruction fault (an escalated exception?)"
                        );
                    }
                }
            }
        }
        out
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::ElfError;

    const RESETS: u32 = APB_BASE + 0xC000;
    const UART0: u32 = APB_BASE + 0x34000;

    #[test]
    fn peripherals_in_reset_ignore_accesses() {
        let mut bus = SystemBus::new();
        bus.write32(UART0 + 0x24, 42);
        assert_eq!(bus.read32(UART0 + 0x24), 0);
        bus.write32(RESETS + 0x3000, 1 << 22); // atomic clear releases UART0
        assert_eq!(bus.read32(RESETS + 8) >> 22 & 1, 1);
        bus.write32(UART0 + 0x24, 42);
        assert_eq!(bus.read32(UART0 + 0x24), 42);
    }

    #[test]
    fn atomic_aliases() {
        let mut bus = SystemBus::new();
        bus.write32(RESETS, 0);
        bus.write32(RESETS + 0x2000, 0b0110); // set
        bus.write32(RESETS + 0x1000, 0b0011); // xor
        bus.write32(RESETS + 0x3000, 0b1000); // clr
        assert_eq!(bus.read32(RESETS), 0b0101);
    }

    #[test]
    fn pio_registers_have_atomic_aliases() {
        let mut bus = SystemBus::new();
        bus.write32(RESETS + 0x3000, 1 << 10); // release PIO0
        let ctrl = PIO0_BASE;
        bus.write32(ctrl, 0b0101);
        bus.write32(ctrl + 0x3000, 0b0001); // CLR: how embassy stops a state machine
        assert_eq!(bus.read32(ctrl), 0b0100);
        bus.write32(ctrl + 0x2000, 0b0010); // SET
        bus.write32(ctrl + 0x1000, 0b0100); // XOR
        assert_eq!(bus.read32(ctrl), 0b0010);
        // A TX FIFO write through an alias is still a plain push.
        bus.write32(PIO0_BASE + 0x10 + 0x3000, 7);
        assert_eq!(
            bus.read32(PIO0_BASE + 0x04) >> 24 & 1,
            0,
            "TXF0 empty after a push"
        );
    }

    #[test]
    fn dma_registers_have_atomic_aliases() {
        let mut bus = SystemBus::new();
        bus.write32(RESETS + 0x3000, 1 << 2); // release DMA
        let inte0 = DMA_BASE + 0x404;
        // How embassy enables each channel's interrupt.
        bus.write32(inte0 + 0x2000, 1 << 0);
        bus.write32(inte0 + 0x2000, 1 << 1);
        assert_eq!(bus.read32(inte0), 0b11);
        bus.write32(inte0 + 0x3000, 1 << 0);
        assert_eq!(
            bus.read32(inte0 + 0x1000),
            0b10,
            "aliases read the plain register"
        );
        // Write-1-to-clear through an alias still clears only those bits.
        // Null triggers (enabled, zero count) flag channels 0 and 1 done.
        bus.write32(DMA_BASE + 0x0C, 1);
        bus.write32(DMA_BASE + 0x40 + 0x0C, 1);
        assert_eq!(bus.read32(DMA_BASE + 0x400), 0b11);
        bus.write32(DMA_BASE + 0x40C + 0x2000, 0b10);
        assert_eq!(bus.read32(DMA_BASE + 0x400), 0b01);
    }

    #[test]
    fn uart_needs_enable_and_byte_stores_work() {
        let mut bus = SystemBus::new();
        bus.write32(RESETS, 0);
        bus.write8(UART0, b'x'); // UARTEN clear: dropped
        bus.write32(UART0 + 0x30, 0x301);
        bus.write8(UART0, b'y'); // lane-replicated store
        assert_eq!(bus.uart[0].tx, b"y");
        assert_eq!(bus.read32(UART0 + 0x18) & 0x80, 0x80); // TXFE
    }

    #[test]
    fn uart_rx_roundtrip() {
        let mut bus = SystemBus::new();
        bus.write32(RESETS, 0);
        bus.write32(UART0 + 0x30, 0x301 | 1 << 9);
        assert_eq!(bus.read32(UART0 + 0x18) & 0x10, 0x10); // RXFE
        bus.uart[0].receive(b"a");
        assert_eq!(bus.read32(UART0 + 0x18) & 0x10, 0);
        assert_eq!(bus.read8(UART0), b'a');
        assert_eq!(bus.read32(UART0 + 0x18) & 0x10, 0x10);
    }

    #[test]
    fn flash_is_read_only_to_the_cpu_and_mirrored() {
        let mut bus = SystemBus::new();
        assert!(bus.load(XIP_BASE, &[1, 2, 3, 4]));
        bus.write32(XIP_BASE, 0xFFFF_FFFF);
        assert_eq!(bus.read32(XIP_BASE), 0x0403_0201);
        assert_eq!(bus.read32(0x1100_0000), 0x0403_0201); // cached/uncached alias
    }

    // ---- bus decoding -----------------------------------------------------

    const TIMER: u32 = APB_BASE + 0x54000;
    const NO_RESET_BLOCK: u32 = APB_BASE + 22 * 0x4000; // plain register stub

    #[test]
    fn sub_word_stores_to_peripherals_replicate_lanes() {
        let mut bus = SystemBus::new();
        bus.write8(NO_RESET_BLOCK + 1, 0xAB);
        assert_eq!(bus.read32(NO_RESET_BLOCK), 0xABAB_ABAB);
        assert_eq!(bus.read8(NO_RESET_BLOCK + 2), 0xAB);
        bus.write16(NO_RESET_BLOCK + 4, 0x1234);
        assert_eq!(bus.read32(NO_RESET_BLOCK + 4), 0x1234_1234);
        assert_eq!(bus.read16(NO_RESET_BLOCK + 6), 0x1234);
        bus.write32(NO_RESET_BLOCK + 8, 0x1122_3344);
        assert_eq!(
            [
                bus.read8(NO_RESET_BLOCK + 8),
                bus.read8(NO_RESET_BLOCK + 11)
            ],
            [0x44, 0x11]
        );
        assert_eq!(
            [
                bus.read16(NO_RESET_BLOCK + 8),
                bus.read16(NO_RESET_BLOCK + 10)
            ],
            [0x3344, 0x1122]
        );
    }

    #[test]
    fn sram_sub_word_access_and_bounds() {
        let mut bus = SystemBus::new();
        bus.write32(SRAM_BASE, 0x1122_3344);
        bus.write8(SRAM_BASE + 1, 0xAA);
        bus.write16(SRAM_BASE + 2, 0xBBCC);
        assert_eq!(bus.read32(SRAM_BASE), 0xBBCC_AA44);
        assert_eq!(bus.read32(0x2100_0000), 0xBBCC_AA44); // alias window
        let top = SRAM_BASE + SRAM_SIZE;
        bus.write32(top, 0xFFFF_FFFF); // past the end: dropped
        assert_eq!(bus.read32(top), 0);
        bus.write8(top - 1, 0x5A);
        assert_eq!(bus.read8(top - 1), 0x5A);
    }

    #[test]
    fn unmapped_reads_zero_and_rom_is_read_only() {
        let mut bus = SystemBus::new();
        assert_eq!(bus.read32(0x3000_0000), 0);
        assert_eq!(bus.read32(0x5000_0000), 0); // AHB-lite peripherals: not modelled
        let rom = bus.read32(0x20);
        assert_ne!(rom, 0);
        bus.write32(0x20, 0);
        bus.write16(0x20, 0);
        bus.write8(0x20, 0);
        assert_eq!(bus.read32(0x20), rom);
        assert_eq!(bus.read32(BOOTROM_SIZE), 0); // past the 16 KiB ROM
        bus.write8(XIP_BASE, 0);
        assert_eq!(bus.read32(XIP_BASE), 0xFFFF_FFFF); // erased flash
    }

    #[test]
    fn load_rejects_ranges_outside_flash_and_sram() {
        let mut bus = SystemBus::new();
        assert!(!bus.load(0x4000_0000, &[1]));
        assert!(!bus.load(SRAM_BASE + SRAM_SIZE - 1, &[1, 2]));
        assert!(!bus.load(XIP_BASE + 0xFF_FFFF, &[1, 2]));
        assert!(bus.load(SRAM_BASE + SRAM_SIZE - 2, &[1, 2]));
        assert!(bus.load(XIP_BASE + 0x30_0000, &[7])); // grows the flash image
        assert_eq!(bus.read8(XIP_BASE + 0x30_0000), 7);
        assert_eq!(bus.read8(XIP_BASE + 0x2F_FFFF), 0xFF);
    }

    #[test]
    fn ppb_exposes_vtor_cpuid_and_icsr() {
        let mut bus = SystemBus::new();
        bus.write32(PPB_BASE + 0xED08, 0x2000_01FF);
        assert_eq!(bus.read32(PPB_BASE + 0xED08), 0x2000_0100);
        assert_eq!(bus.vtor[0], 0x2000_0100);
        assert_eq!(bus.read32(PPB_BASE + 0xED00), 0x410C_C601);
        assert_eq!(bus.read32(SIO_BASE), 0); // CPUID of core 0
    }

    #[test]
    fn timer_registers_are_reachable_only_out_of_reset() {
        let mut bus = SystemBus::new();
        bus.write32(TIMER + 0x38, 0xF);
        assert_eq!(bus.read32(TIMER + 0x38), 0);
        bus.resets.reset = 0;
        bus.write32(TIMER + 0x38, 0b0001);
        assert_eq!(bus.read32(TIMER + 0x38), 0b0001);
        bus.write32(TIMER + 0x2038, 0b0100); // atomic set
        assert_eq!(bus.read32(TIMER + 0x38), 0b0101);
        bus.write32(TIMER + 0x3038, 0b0001); // atomic clear
        assert_eq!(bus.read32(TIMER + 0x38), 0b0100);
    }

    #[test]
    fn uart_irq_lines_map_to_nvic_inputs() {
        let mut bus = SystemBus::new();
        bus.resets.reset = 0;
        assert_eq!(bus.irq_lines(0), 0);
        for base in [UART0, UART0 + 0x4000] {
            bus.write32(base + 0x30, 0x301 | 1 << 9);
            bus.write32(base + 0x38, 1 << 4); // RX interrupt unmasked
        }
        assert_eq!(bus.irq_lines(0), 0);
        bus.uart[0].receive(b"a");
        assert_eq!(bus.irq_lines(0), 1 << IRQ_UART0);
        bus.uart[1].receive(b"b");
        assert_eq!(bus.irq_lines(0), 1 << IRQ_UART0 | 1 << IRQ_UART1);
        bus.write32(TIMER + 0x38, 1 << 2);
        bus.write32(TIMER + 0x3C, 1 << 2); // force alarm 2
        assert_eq!(bus.irq_lines(0) & 0xF, 1 << 2);
    }

    // ---- ELF loading ------------------------------------------------------

    use crate::elf::testutil::build;

    #[test]
    fn load_elf_finds_flash_vector_table_after_boot2() {
        let mut image = alloc::vec![0u8; 0x100];
        image.extend_from_slice(&(SRAM_BASE + 0x1000).to_le_bytes());
        image.extend_from_slice(&(XIP_BASE + 0x1C1).to_le_bytes());
        let mut sys = System::new();
        sys.load_elf(&build(
            XIP_BASE + 0x1C1,
            &[(XIP_BASE, &image, image.len() as u32)],
        ))
        .unwrap();
        assert_eq!(
            (
                sys.cores[0].msp,
                sys.cores[0].pc,
                sys.bus.vtor[0],
                sys.cores[0].vtor
            ),
            (
                SRAM_BASE + 0x1000,
                XIP_BASE + 0x1C0,
                XIP_BASE + 0x100,
                XIP_BASE + 0x100
            )
        );
    }

    #[test]
    fn load_elf_ram_image_and_bss_zero_fill() {
        let mut table = alloc::vec::Vec::new();
        table.extend_from_slice(&(SRAM_BASE + 0x800).to_le_bytes());
        table.extend_from_slice(&(SRAM_BASE + 0x41).to_le_bytes());
        let mut sys = System::new();
        sys.bus.sram.data[0x20..0x24].fill(0xEE); // stale data in the bss range
        let elf = build(SRAM_BASE + 0x41, &[(SRAM_BASE, &table, 0x40)]);
        sys.load_elf(&elf).unwrap();
        assert_eq!(
            (sys.cores[0].msp, sys.cores[0].pc, sys.bus.vtor[0]),
            (SRAM_BASE + 0x800, SRAM_BASE + 0x40, SRAM_BASE)
        );
        assert_eq!(sys.bus.read32(SRAM_BASE + 0x20), 0);
    }

    #[test]
    fn load_elf_without_vector_table_starts_at_entry_with_top_of_ram_stack() {
        let mut sys = System::new();
        sys.load_elf(&build(
            XIP_BASE + 0x201,
            &[(XIP_BASE + 0x200, &[0x00, 0xBF], 2)],
        ))
        .unwrap();
        assert_eq!(
            (sys.cores[0].msp, sys.cores[0].pc, sys.bus.vtor[0]),
            (SRAM_BASE + SRAM_SIZE, XIP_BASE + 0x200, 0)
        );
    }

    #[test]
    fn load_elf_reports_errors() {
        let mut sys = System::new();
        assert_eq!(
            sys.load_elf(b"nope"),
            Err(LoadError::Elf(ElfError::TooShort))
        );
        assert_eq!(
            sys.load_elf(&build(0, &[(0x3000_0000, &[1], 1)])),
            Err(LoadError::Unmapped(0x3000_0000))
        );
        // The file part fits but the bss tail runs off the end of SRAM.
        let end = SRAM_BASE + SRAM_SIZE;
        assert_eq!(
            sys.load_elf(&build(0, &[(end - 4, &[1], 16)])),
            Err(LoadError::Unmapped(end - 4))
        );
    }

    // ---- UF2 loading ------------------------------------------------------

    use crate::uf2::testutil::{build as build_uf2, rp2040};

    #[test]
    fn load_uf2_boots_from_vector_table_after_boot2() {
        let mut table = alloc::vec::Vec::new();
        table.extend_from_slice(&(SRAM_BASE + 0x1000).to_le_bytes());
        table.extend_from_slice(&(XIP_BASE + 0x1C1).to_le_bytes());
        let mut sys = System::new();
        sys.load_firmware(&build_uf2(&[
            rp2040(XIP_BASE, &[0xAA; 256]),
            rp2040(XIP_BASE + 0x100, &table),
        ]))
        .unwrap();
        assert_eq!(
            (sys.cores[0].msp, sys.cores[0].pc, sys.bus.vtor[0]),
            (SRAM_BASE + 0x1000, XIP_BASE + 0x1C0, XIP_BASE + 0x100)
        );
        assert_eq!(sys.bus.read32(XIP_BASE), 0xAAAA_AAAA);
    }

    #[test]
    fn load_uf2_ram_image() {
        let mut table = alloc::vec::Vec::new();
        table.extend_from_slice(&(SRAM_BASE + 0x800).to_le_bytes());
        table.extend_from_slice(&(SRAM_BASE + 0x41).to_le_bytes());
        let mut sys = System::new();
        sys.load_firmware(&build_uf2(&[rp2040(SRAM_BASE, &table)]))
            .unwrap();
        assert_eq!(
            (sys.cores[0].msp, sys.cores[0].pc, sys.bus.vtor[0]),
            (SRAM_BASE + 0x800, SRAM_BASE + 0x40, SRAM_BASE)
        );
    }

    #[test]
    fn load_uf2_reports_errors() {
        let mut sys = System::new();
        assert_eq!(
            sys.load_firmware(&build_uf2(&[rp2040(0x3000_0000, &[1])])),
            Err(LoadError::Unmapped(0x3000_0000))
        );
        assert_eq!(
            sys.load_firmware(&build_uf2(&[rp2040(XIP_BASE, &[0; 0x108])])),
            Err(LoadError::NoVectorTable)
        );
        // Not UF2: falls through to the ELF loader.
        assert_eq!(
            sys.load_firmware(b"nope"),
            Err(LoadError::Elf(ElfError::TooShort))
        );
    }

    #[test]
    fn drain_uarts_sends_what_is_left_after_a_breakpoint() {
        const UART0: u32 = APB_BASE + 0x34000;
        let mut sys = System::new();
        sys.bus.resets.reset = 0;
        sys.bus.write32(UART0 + 0x24, 67); // IBRD/FBRD: 115200 baud at 125 MHz
        sys.bus.write32(UART0 + 0x28, 52);
        sys.bus.write32(UART0 + 0x2C, 3 << 5 | 1 << 4); // 8N1, FIFOs on
        sys.bus.write32(UART0 + 0x30, 0x301); // UARTEN | TXE | RXE
        for &b in b"bye\n" {
            sys.bus.write32(UART0, b as u32);
        }
        assert!(sys.bus.uart[0].tx.len() < 4);
        let t = sys.cycles();
        sys.drain_uarts();
        assert_eq!(sys.bus.uart[0].tx, b"bye\n");
        assert!(sys.cycles() > t, "the line takes time");
        assert_eq!(sys.bus.uart[0].tx_event(), None);
    }

    #[test]
    fn uart_timing_follows_clk_peri() {
        const CLOCKS: u32 = APB_BASE + 0x8000;
        const PLL_USB: u32 = APB_BASE + 0x2C000;
        const UART0: u32 = APB_BASE + 0x34000;
        let frame_after_setup = |sys: &mut System| {
            sys.bus.write32(UART0 + 0x24, 26); // 115200 baud at 48 MHz: 26 + 3/64
            sys.bus.write32(UART0 + 0x28, 3);
            sys.bus.write32(UART0 + 0x2C, 3 << 5); // 8N1
            sys.bus.write32(UART0 + 0x30, 0x301);
            sys.bus.write32(UART0, b'a' as u32);
            sys.bus.uart[0].tx_event()
        };
        let mut sys = System::new();
        sys.bus.resets.reset = 0;
        // clk_peri is clk_sys until it is switched: at 125 MHz the 48 MHz
        // divisors run 2.6x too fast (10 x 416.75 cycles).
        assert_eq!(frame_after_setup(&mut sys), Some(4168));

        // MicroPython's clocks: the USB PLL at 12 MHz x 100 / (5 x 5).
        let mut sys = System::new();
        sys.bus.resets.reset = 0;
        sys.bus.write32(PLL_USB + PLL_FBDIV_INT, 100);
        sys.bus.write32(PLL_USB + PLL_PRIM, 5 << 16 | 5 << 12);
        sys.bus.write32(CLOCKS + CLK_PERI_CTRL, 1 << 11 | 2 << 5);
        // 10 bits of 16 x 26.047 / 48 MHz: 86.8 us.
        assert_eq!(frame_after_setup(&mut sys), Some(10853));

        // Back to clk_sys (AUXSRC = 0), through the atomic clear alias.
        sys.bus.write32(CLOCKS + 0x3000 + CLK_PERI_CTRL, 7 << 5);
        sys.bus.uart[0].update(20_000);
        sys.bus.write32(UART0, b'b' as u32);
        assert_eq!(sys.bus.uart[0].tx_event(), Some(20_000 + 4168));
    }

    // ---- stepping, sleep and interrupt delivery ---------------------------

    const WFI: u16 = 0xBF30;
    const CPSID: u16 = 0xB672;
    const CPSIE: u16 = 0xB662;
    const NOP: u16 = 0xBF00;
    const SPIN: u16 = 0xE7FE; // b .
    const MOVS_R5_1: u16 = 0x2501;
    const MOVS_R0_1: u16 = 0x2001;

    /// Vector table at the start of SRAM (SP, reset, ..., PendSV, SysTick and
    /// IRQ 0 all pointing at one handler), main code at +0x100, handler at +0x200.
    /// The handler sets r5 = 1 and spins. All peripherals are out of reset.
    fn boot(main: &[u16]) -> System {
        let (main_at, handler_at) = (SRAM_BASE + 0x100, SRAM_BASE + 0x200);
        let mut table = [0u32; 17];
        table[0] = SRAM_BASE + SRAM_SIZE;
        table[1] = main_at | 1;
        table[14..].fill(handler_at | 1);
        let bytes = |h: &[u16]| {
            h.iter()
                .flat_map(|x| x.to_le_bytes())
                .collect::<alloc::vec::Vec<_>>()
        };
        let mut sys = System::new();
        sys.bus.load(
            SRAM_BASE,
            &table
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<alloc::vec::Vec<_>>(),
        );
        sys.bus.load(main_at, &bytes(main));
        sys.bus.load(handler_at, &bytes(&[MOVS_R5_1, SPIN]));
        sys.reset_from_vector_table(SRAM_BASE);
        sys.bus.resets.reset = 0;
        // Single-core tests: core 1 would otherwise run the bootrom's launch loop alongside.
        sys.set_core_held(1, true);
        sys
    }

    const NVIC_ISER: u32 = PPB_BASE + 0xE100;
    const NVIC_ISPR: u32 = PPB_BASE + 0xE200;

    #[test]
    fn wfi_with_nothing_to_wake_it_reports_sleeping() {
        let mut sys = boot(&[WFI, SPIN]);
        assert_eq!(sys.run(1_000_000), StopReason::Sleeping);
        assert!(sys.cores[0].cycles < 10); // no busy-waiting through the budget
    }

    #[test]
    fn wfi_skips_time_to_a_timer_alarm_and_runs_the_handler() {
        let mut sys = boot(&[WFI, SPIN]);
        sys.bus.write32(TIMER + 0x38, 1); // INTE alarm 0
        sys.bus.write32(NVIC_ISER, 1);
        sys.bus.write32(TIMER + 0x10, 10); // 10 us = 1250 cycles
        assert_eq!(sys.run(1000), StopReason::CycleLimit);
        assert!(sys.cores[0].sleeping && sys.cores[0].ipsr == 0);
        assert_eq!(sys.cores[0].cycles, 1000); // fast-forward clamps to the budget
        assert_eq!(sys.run(1000), StopReason::CycleLimit);
        assert_eq!(sys.cores[0].ipsr, 16);
        assert_eq!(sys.cores[0].r[5], 1);
        assert!(!sys.cores[0].sleeping);
        assert!(sys.cores[0].cycles >= 1250);
    }

    #[test]
    fn pending_irq_is_taken_before_the_next_instruction_and_acknowledged() {
        let mut sys = boot(&[MOVS_R0_1, SPIN]);
        sys.bus.write32(NVIC_ISER, 1);
        sys.bus.write32(NVIC_ISPR, 1);
        assert_eq!(sys.step(), 15); // exception entry, not an instruction
        assert_eq!(
            (sys.cores[0].ipsr, sys.cores[0].r[0], sys.cores[0].pc),
            (16, 0, SRAM_BASE + 0x200)
        );
        assert_eq!(sys.bus.read32(PPB_BASE + 0xE200), 0); // pending bit cleared on entry
        // The stacked return address is the instruction that was about to run.
        assert_eq!(sys.bus.read32(sys.cores[0].sp() + 24), SRAM_BASE + 0x100);
        sys.run(100);
        assert_eq!(sys.cores[0].r[5], 1);
    }

    #[test]
    fn primask_defers_an_irq_until_cpsie() {
        let mut sys = boot(&[CPSID, NOP, NOP, CPSIE, NOP, SPIN]);
        sys.step(); // cpsid
        sys.bus.write32(NVIC_ISER, 1);
        sys.bus.write32(NVIC_ISPR, 1);
        sys.step();
        sys.step();
        assert_eq!(sys.cores[0].ipsr, 0);
        sys.step(); // cpsie
        assert_eq!(sys.cores[0].ipsr, 0);
        sys.step(); // IRQ taken now
        assert_eq!(sys.cores[0].ipsr, 16);
        assert_eq!(
            sys.bus.read32(sys.cores[0].sp() + 24),
            SRAM_BASE + 0x100 + 8
        ); // resumes at the nop after cpsie
    }

    #[test]
    fn wfi_wakes_for_a_masked_interrupt_without_entering_it() {
        let mut sys = boot(&[CPSID, WFI, MOVS_R0_1, SPIN]);
        sys.bus.write32(NVIC_ISER, 1);
        sys.step(); // cpsid
        sys.step(); // wfi
        assert!(sys.cores[0].sleeping);
        sys.bus.write32(NVIC_ISPR, 1);
        sys.run(100);
        assert_eq!(
            (sys.cores[0].ipsr, sys.cores[0].r[0], sys.cores[0].r[5]),
            (0, 1, 0)
        );
    }

    #[test]
    fn higher_priority_irq_preempts_a_running_handler() {
        let mut sys = boot(&[SPIN]);
        sys.bus.write32(NVIC_ISER, 0b11);
        sys.bus.write32(PPB_BASE + 0xE400, 1 << 6); // IRQ0 prio 1, IRQ1 prio 0
        sys.bus.write32(NVIC_ISPR, 1);
        sys.step();
        assert_eq!(sys.cores[0].ipsr, 16);
        sys.bus.write32(NVIC_ISPR, 1); // same IRQ again: can't preempt itself
        sys.step();
        assert_eq!(sys.cores[0].ipsr, 16);
        sys.bus.write32(NVIC_ISPR, 2);
        sys.step();
        assert_eq!(sys.cores[0].ipsr, 17);
    }

    #[test]
    fn systick_wakes_wfi_through_the_vector_table() {
        let mut sys = boot(&[WFI, SPIN]);
        sys.bus.write32(PPB_BASE + 0xE014, 999);
        sys.bus.write32(PPB_BASE + 0xE010, 0b111);
        assert_eq!(sys.run(900), StopReason::CycleLimit);
        assert!(sys.cores[0].sleeping && sys.cores[0].ipsr == 0);
        assert_eq!(sys.run(200), StopReason::CycleLimit); // wrap at 1000 cycles
        assert_eq!((sys.cores[0].ipsr, sys.cores[0].r[5]), (15, 1));
        assert_eq!(sys.cores[0].vtor, SRAM_BASE);
    }

    // ---- GPIO and the board ------------------------------------------------

    const SIO_GPIO_IN: u32 = SIO_BASE + 0x04;
    const IO_BANK0: u32 = APB_BASE + 0x14000;
    const PADS_BANK0: u32 = APB_BASE + 0x1C000;

    #[test]
    fn gpio_registers_are_reachable_through_the_bus() {
        let mut bus = SystemBus::new();
        // SIO needs no reset release; IO_BANK0 and PADS_BANK0 do.
        bus.write32(IO_BANK0 + 8 * 7 + 4, 5);
        assert_eq!(bus.read32(IO_BANK0 + 8 * 7 + 4), 0); // still in reset
        bus.resets.reset = 0;
        bus.write32(IO_BANK0 + 8 * 7 + 4, 5);
        assert_eq!(bus.read32(IO_BANK0 + 8 * 7 + 4), 5);
        bus.write32(SIO_BASE + 0x24, 1 << 7); // OE_SET
        bus.write32(SIO_BASE + 0x14, 1 << 7); // OUT_SET
        assert_eq!(bus.read32(SIO_GPIO_IN), 1 << 7);
        assert_eq!(bus.read32(SIO_BASE + 0x10), 1 << 7);
        assert_eq!(bus.read32(PADS_BANK0 + 4 + 4 * 7), 0x56);
        // Atomic aliases reach the pad registers like any other APB block.
        bus.write32(PADS_BANK0 + 0x2000 + 4 + 4 * 7, 1 << 3); // PUE via SET
        assert_eq!(bus.read32(PADS_BANK0 + 4 + 4 * 7), 0x5E);
        bus.write32(PADS_BANK0 + 0x3000 + 4 + 4 * 7, 1 << 4); // clear a drive bit
        assert_eq!(bus.read32(PADS_BANK0 + 4 + 4 * 7), 0x4E);
        // The interpolators are not modelled: their registers read zero.
        bus.write32(SIO_BASE + 0x80, 0xFFFF_FFFF);
        assert_eq!(bus.read32(SIO_BASE + 0x80), 0);
    }

    #[test]
    fn gpio_edge_irq_line_follows_io_bank0() {
        let mut bus = SystemBus::new();
        bus.resets.reset = 0;
        bus.write32(IO_BANK0 + 8 * 3 + 4, 5);
        bus.write32(SIO_BASE + 0x24, 1 << 3);
        bus.write32(IO_BANK0 + 0x100, 0b1000 << 12); // PROC0_INTE0: pin 3 EDGE_HIGH
        assert_eq!(bus.irq_lines(0) >> IRQ_IO_BANK0 & 1, 0);
        bus.write32(SIO_BASE + 0x14, 1 << 3);
        assert_eq!(bus.irq_lines(0) >> IRQ_IO_BANK0 & 1, 1);
        bus.write32(IO_BANK0 + 0xF0, 0b1000 << 12);
        assert_eq!(bus.irq_lines(0) >> IRQ_IO_BANK0 & 1, 0);
    }

    #[test]
    fn button_press_wakes_wfi_via_gpio_interrupt_and_led_logs_it() {
        use pcb::Board;
        // IRQ 13 vector -> handler. boot() points IRQ 0 only, so retarget the table entry.
        let mut sys = boot(&[WFI, SPIN]);
        let handler = (SRAM_BASE + 0x200) | 1;
        sys.bus
            .load(SRAM_BASE + 4 * (16 + IRQ_IO_BANK0), &handler.to_le_bytes());
        sys.set_board(
            Board::parse_with("parts.l = { kind = \"led\", gpio = 25 }\nparts.b = { kind = \"button\", gpio = 2 }", &pcb_devices::registry()).unwrap(),
        );
        sys.bus.write32(IO_BANK0 + 8 * 2 + 4, 5); // SIO function
        sys.bus.write32(PADS_BANK0 + 4 + 4 * 2, 1 << 6 | 1 << 3); // pull-up
        sys.bus.write32(IO_BANK0 + 0x100, 0b0100 << 8); // pin 2 EDGE_LOW
        sys.bus.write32(NVIC_ISER, 1 << IRQ_IO_BANK0);
        assert_eq!(sys.run(1000), StopReason::Sleeping);
        assert!(sys.cores[0].sleeping && sys.cores[0].r[5] == 0);

        assert!(sys.set_button("b", true));
        sys.run(100);
        assert_eq!(
            (sys.cores[0].ipsr, sys.cores[0].r[5]),
            ((16 + IRQ_IO_BANK0) as u8, 1)
        );
        assert_eq!(sys.board.events.len(), 1);
        assert!(sys.board.events[0].value == 1 && sys.board.events[0].part == 1);
    }

    #[test]
    fn idle_until_advances_time_only_forwards() {
        let mut sys = boot(&[WFI, SPIN]);
        sys.step();
        let now = sys.cores[0].cycles;
        sys.bus.write32(TIMER + 0x38, 1);
        sys.bus.write32(TIMER + 0x10, 10);
        sys.idle_until(now + 5000);
        assert_eq!(sys.cores[0].cycles, now + 5000);
        assert_eq!(sys.bus.irq_lines(0) & 1, 1); // the 10 us alarm fired while idle
        sys.idle_until(now); // not in the future: ignored
        assert_eq!(sys.cores[0].cycles, now + 5000);
    }

    #[test]
    fn an_overclocked_clk_sys_gets_its_cycles() {
        const CLOCKS: u32 = APB_BASE + 0x8000;
        const PLL_SYS: u32 = APB_BASE + 0x28000;
        const TIMERAWL: u32 = APB_BASE + 0x54000 + 0x28;
        let mut sys = boot(&[SPIN]);
        sys.run(125_000);
        assert_eq!(sys.bus.read32(TIMERAWL), 1000, "125 MHz to start with");
        // As pico-sdk's set_sys_clock_khz(250000) leaves it: 12 MHz x 125
        // / (6 x 1) from PLL_SYS, clk_sys on its aux source.
        sys.bus.write32(PLL_SYS, 1); // CS: REFDIV 1
        sys.bus.write32(PLL_SYS + 0x8, 125); // FBDIV_INT
        sys.bus.write32(PLL_SYS + 0xC, 6 << 16 | 1 << 12); // PRIM: POSTDIV1, POSTDIV2
        sys.bus.write32(CLOCKS + 0x3C, 1); // CLK_SYS_CTRL: SRC aux, AUXSRC PLL_SYS
        assert_eq!(sys.sys_hz(), 250_000_000);
        sys.run(250_000);
        assert_eq!(
            sys.bus.read32(TIMERAWL),
            2000,
            "a millisecond is 250000 cycles now"
        );
        assert!((sys.seconds() - 0.002).abs() < 1e-9, "{}", sys.seconds());
        assert!((sys.seconds_at(125_000) - 0.001).abs() < 1e-9);
        // Back on clk_ref (as start-up code does while it sets PLLs up): the
        // emulator takes the unmodelled source as 125 MHz.
        sys.bus.write32(CLOCKS + 0x3C, 0);
        assert_eq!(sys.sys_hz(), 125_000_000);
    }

    #[test]
    fn bkpt_stops_run() {
        let mut sys = boot(&[0xBE07, SPIN]);
        assert_eq!(sys.run(1000), StopReason::Breakpoint(7));
        assert_eq!(sys.bkpt_at, Some((0, SRAM_BASE + 0x100)));
    }

    /// `boot`, with a HardFault handler at `handler_at` too.
    fn boot_faulting(main: &[u16]) -> System {
        let mut sys = boot(main);
        sys.bus
            .load(SRAM_BASE + 12, &(SRAM_BASE + 0x201).to_le_bytes());
        sys
    }

    // movs r0, #0x20; lsls r0, r0, #24; adds r0, #2; ldr r1, [r0]: a word
    // read at 0x20000002.
    const UNALIGNED_LOAD: [u16; 4] = [0x2020, 0x0600, 0x3002, 0x6801];

    #[test]
    fn a_fault_is_a_warning_and_its_handler_runs() {
        let mut sys = boot_faulting(&UNALIGNED_LOAD);
        assert_eq!(sys.run(1000), StopReason::CycleLimit);
        let pc = SRAM_BASE + 0x106;
        assert_eq!(
            sys.warnings,
            [alloc::format!(
                "core 0: HardFault at pc {pc:#010x}: unaligned 32-bit read at 0x20000002"
            )]
        );
        assert_eq!(sys.cores[0].r[5], 1, "the handler ran");
        let report = sys.core_report(0);
        assert!(report.contains("in HardFault"), "{report}");
        assert!(
            report.contains(&alloc::format!("entered from pc {pc:#010x}")),
            "{report}"
        );
        assert!(
            report.contains(&alloc::format!(
                "cause: unaligned 32-bit read at 0x20000002, by the instruction at {pc:#010x}"
            )),
            "{report}"
        );
    }

    #[test]
    fn stop_on_fault_stops_before_the_handler_runs() {
        for batching in [true, false] {
            let mut sys = boot_faulting(&UNALIGNED_LOAD);
            sys.batching = batching;
            sys.stop_on_fault = true;
            let cause = Fault::Unaligned {
                addr: SRAM_BASE + 2,
                size: 4,
                write: false,
            };
            let stop = StopReason::Fault {
                core: 0,
                pc: SRAM_BASE + 0x106,
                cause,
            };
            assert_eq!(sys.run(1000), stop);
            assert_eq!(sys.cores[0].pc, SRAM_BASE + 0x200);
            assert_eq!(sys.cores[0].r[5], 0, "the handler has not run");
        }
    }

    #[test]
    fn only_the_first_faults_are_warnings() {
        // The handler returns to the faulting instruction (lr is
        // EXC_RETURN), so it faults over and over: bx lr.
        let mut sys = boot_faulting(&UNALIGNED_LOAD);
        sys.bus.load(SRAM_BASE + 0x200, &0x4770u16.to_le_bytes());
        sys.run(100_000);
        assert_eq!(sys.warnings.len(), FAULT_WARNINGS as usize);
        assert!(
            sys.warnings
                .last()
                .unwrap()
                .ends_with("(further faults are not reported)")
        );
    }

    #[test]
    fn reset_bits_are_unique_and_within_the_resets_register() {
        let bits: alloc::vec::Vec<u32> = (0..APB_BLOCKS as u32).filter_map(reset_bit).collect();
        for (i, b) in bits.iter().enumerate() {
            assert!(*b < 25, "bit {b} outside RESETS");
            assert!(!bits[..i].contains(b), "bit {b} used twice");
        }
        // Spot checks against the datasheet numbering used by the SDK.
        assert_eq!(
            [reset_bit(5), reset_bit(13), reset_bit(21)],
            [Some(5), Some(22), Some(21)]
        );
        assert_eq!(reset_bit(APB_RESETS), None); // RESETS itself can't be held in reset
    }

    #[test]
    fn apb_addresses_beyond_the_last_block_are_unmapped() {
        let mut bus = SystemBus::new();
        bus.resets.reset = 0;
        let beyond = APB_BASE + APB_BLOCKS as u32 * 0x4000;
        bus.write32(beyond, 0xFFFF_FFFF);
        assert_eq!(bus.read32(beyond), 0);
        // Blocks that nothing models still behave as plain registers once out of reset.
        bus.write32(NO_RESET_BLOCK + 0x10, 0x1234);
        assert_eq!(bus.read32(NO_RESET_BLOCK + 0x10), 0x1234);
    }

    #[test]
    fn default_system_matches_new() {
        let (a, b) = (System::default(), System::new());
        assert_eq!(
            (a.cores[0].cycles, a.bus.vtor[0], a.bus.resets.reset),
            (b.cores[0].cycles, b.bus.vtor[0], b.bus.resets.reset)
        );
        assert_eq!(SystemBus::default().irq_lines(0), 0);
    }

    // ---- DMA and SPI at system level ----------------------------------------

    const DMA_CH0: u32 = DMA_BASE;
    const SPI0: u32 = APB_BASE + 0x3C000;

    /// Program DMA channel `ch` and trigger it.
    fn dma_start(sys: &mut System, ch: u32, read: u32, write: u32, count: u32, ctrl: u32) {
        let base = DMA_CH0 + ch * 0x40;
        sys.bus.write32(base, read);
        sys.bus.write32(base + 4, write);
        sys.bus.write32(base + 8, count);
        sys.bus.write32(base + 0xC, ctrl);
    }

    /// EN, word size, both addresses incrementing, `treq`, chained to itself.
    fn mem_ctrl(treq: u32) -> u32 {
        1 | 2 << 2 | 1 << 4 | 1 << 5 | treq << 15
    }

    #[test]
    fn dma_moves_one_item_per_cycle_alongside_the_core() {
        let mut sys = boot(&[SPIN]);
        let (src, dst) = (SRAM_BASE + 0x1000, SRAM_BASE + 0x2000);
        let words: alloc::vec::Vec<u8> = (0..1000u32).flat_map(|w| w.to_le_bytes()).collect();
        sys.bus.load(src, &words);
        dma_start(&mut sys, 0, src, dst, 1000, mem_ctrl(0x3F));
        assert_eq!(sys.run(300), StopReason::CycleLimit);
        let left = sys.bus.read32(DMA_CH0 + 8);
        assert_eq!(
            left as u64,
            1000 - sys.cores[0].cycles,
            "one item per elapsed cycle"
        );
        assert_eq!(sys.bus.read32(dst + 4 * (1000 - left - 1)), 1000 - left - 1);
        sys.run(1000);
        assert_eq!(sys.bus.read32(dst + 4 * 999), 999);
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 1, "completion flagged");
    }

    #[test]
    fn dma_waiting_on_a_request_lets_the_core_report_sleeping() {
        let mut sys = boot(&[WFI, SPIN]);
        let (rx, dst) = (SPI0 + 0x08, SRAM_BASE + 0x2000);
        dma_start(&mut sys, 0, rx, dst, 4, 1 << 5 | 1 | 17 << 15); // byte items from SPI0 RX
        assert_eq!(
            sys.run(10_000),
            StopReason::Sleeping,
            "nothing can ever wake it"
        );
        assert_eq!(sys.bus.read32(DMA_CH0 + 8), 4, "still all 4 items to go");
        assert_eq!(
            sys.bus.read32(DMA_CH0 + 0xC) >> 24 & 1,
            1,
            "BUSY, waiting for the request"
        );
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 0);
        // Loop four frames back into the RX FIFO and let the request through.
        sys.bus.write32(SPI0, 7);
        sys.bus.write32(SPI0 + 0x24, 1); // RXDMAE
        sys.bus.write32(SPI0 + 0x04, 3); // SSE | LBM
        for b in [0x11u32, 0x22, 0x33, 0x44] {
            sys.bus.write32(SPI0 + 0x08, b);
            sys.service_buses();
        }
        assert_eq!(sys.run(10_000), StopReason::Sleeping);
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 1, "sequence finished");
        assert_eq!(sys.bus.read32(DMA_CH0 + 0xC) >> 24 & 1, 0, "no longer BUSY");
        let got: alloc::vec::Vec<u8> = (0..4).map(|i| sys.bus.read8(dst + i)).collect();
        assert_eq!(
            got,
            [0x11, 0x22, 0x33, 0x44],
            "RX FIFO contents landed in order"
        );
        assert_eq!(sys.bus.read32(SPI0 + 0x0C) & 4, 0, "RX FIFO drained");
    }

    #[test]
    fn dma_interrupt_wakes_a_sleeping_core() {
        let mut sys = boot(&[WFI, SPIN]);
        let handler = SRAM_BASE + 0x200;
        sys.bus.load(
            SRAM_BASE + 4 * (16 + IRQ_DMA0),
            &(handler | 1).to_le_bytes(),
        );
        sys.bus.write32(DMA_BASE + 0x404, 1); // INTE0: channel 0
        sys.bus.write32(PPB_BASE + 0xE100, 1 << IRQ_DMA0); // NVIC enable
        dma_start(
            &mut sys,
            0,
            SRAM_BASE + 0x1000,
            SRAM_BASE + 0x2000,
            8,
            mem_ctrl(0x3F),
        );
        assert_eq!(sys.run(1_000), StopReason::CycleLimit, "the handler spins");
        assert_eq!(
            (sys.cores[0].r[5], sys.cores[0].ipsr),
            (1, 16 + IRQ_DMA0 as u8),
            "woken into the DMA handler"
        );
    }

    #[test]
    fn timer_paced_dma_takes_the_time_it_should() {
        let mut sys = boot(&[WFI, SPIN]);
        sys.bus.write32(DMA_BASE + 0x420, 1 << 16 | 50); // TIMER0: one item per 50 cycles
        dma_start(
            &mut sys,
            0,
            SRAM_BASE + 0x1000,
            SRAM_BASE + 0x2000,
            10,
            mem_ctrl(0x3B),
        );
        assert_eq!(sys.run(100_000), StopReason::Sleeping);
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 1, "all ten items moved");
        // 10 items x 50 cycles, give or take the sleep-skip granularity.
        assert!(
            (500..=500 + DMA_SLICE + 50).contains(&sys.cores[0].cycles),
            "{}",
            sys.cores[0].cycles
        );
    }

    #[test]
    fn dma_is_inert_while_held_in_reset() {
        let mut sys = boot(&[SPIN]);
        sys.bus.write32(RESETS + 0x2000, 1 << RESET_DMA); // SET: hold DMA in reset
        dma_start(
            &mut sys,
            0,
            SRAM_BASE + 0x1000,
            SRAM_BASE + 0x2000,
            8,
            mem_ctrl(0x3F),
        );
        assert_eq!(sys.bus.read32(DMA_CH0 + 8), 0, "register writes ignored");
        sys.run(100);
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 0);
    }

    #[test]
    fn spi_frames_written_by_the_cpu_reach_board_devices() {
        use pcb::Board;
        let mut sys = boot(&[SPIN]);
        sys.set_board(
            Board::parse_with("buses.spi0 = { sck = 18, mosi = 19 }\nparts.s = { kind = \"st7789\", bus = \"spi0\", cs = 9, dc = 13 }", &pcb_devices::registry()).unwrap(),
        );
        sys.bus.write32(SPI0, 7);
        sys.bus.write32(SPI0 + 4, 2);
        // CS (GPIO9) low, D/C (13) low: a command byte (RDDID), then data clocked back.
        sys.bus.write32(IO_BANK0 + 8 * 9 + 4, 5); // FUNCSEL = SIO
        sys.bus.write32(IO_BANK0 + 8 * 13 + 4, 5);
        // MOSI where the board has it, SCK on GPIO2 instead of the board's 18.
        sys.bus.write32(IO_BANK0 + 8 * 19 + 4, 1);
        sys.bus.write32(IO_BANK0 + 8 * 2 + 4, 1);
        let sio_oe = SIO_BASE + 0x24;
        sys.bus.write32(sio_oe, 1 << 9 | 1 << 13);
        sys.bus.write32(SIO_BASE + 0x18, 1 << 9 | 1 << 13); // OUT_CLR
        sys.sync_board();
        sys.bus.write32(SPI0 + 8, 0x04);
        sys.service_buses();
        assert_eq!(
            sys.warnings,
            [
                "SPI0 is in use, but GPIO 18 (the board's SPI0 SCK) is set to no function, not SPI; the firmware put SPI0 SCK on GPIO 2"
            ]
        );
        sys.bus.write32(SIO_BASE + 0x14, 1 << 13); // D/C high
        sys.bus.write32(SPI0 + 8, 0);
        sys.service_buses();
        assert_eq!(
            (sys.bus.read32(SPI0 + 8), sys.bus.read32(SPI0 + 8)),
            (0x00, 0x85),
            "command echo, then ID byte 1"
        );
        // CS high: the device is deselected and MISO floats high.
        sys.bus.write32(SIO_BASE + 0x14, 1 << 9);
        sys.sync_board();
        sys.bus.write32(SPI0 + 8, 0);
        sys.service_buses();
        assert_eq!(sys.bus.read32(SPI0 + 8), 0xFF);
        assert_eq!(sys.warnings.len(), 1, "each bus is checked once");
    }

    // ---- two cores ------------------------------------------------------------

    const ADDS_R0_1: u16 = 0x3001;
    const ADDS_R1_1: u16 = 0x3101;
    const LOOP_BACK: u16 = 0xE7FC; // b .-4: to the instruction before it
    const WFE: u16 = 0xBF20;
    const SEV: u16 = 0xBF40;

    /// Two cores with independent code in SRAM, core 1 not held. Core 0 runs
    /// `main0` at +0x100, core 1 `main1` at +0x300 with its own stack.
    fn boot_two(main0: &[u16], main1: &[u16]) -> System {
        let mut sys = boot(main0);
        let bytes: alloc::vec::Vec<u8> = main1.iter().flat_map(|x| x.to_le_bytes()).collect();
        sys.bus.load(SRAM_BASE + 0x300, &bytes);
        sys.set_core_held(1, false);
        sys.cores[1].reset_to(SRAM_BASE + 0x8000, (SRAM_BASE + 0x300) | 1);
        sys
    }

    #[test]
    fn running_cores_interleave_by_clock() {
        let mut sys = boot_two(&[ADDS_R0_1, LOOP_BACK], &[ADDS_R1_1, LOOP_BACK]);
        assert_eq!(sys.run(2_000), StopReason::CycleLimit);
        let (a, b) = (sys.cores[0].r[0], sys.cores[1].r[1]);
        assert!(a > 100 && b > 100, "both made progress: {a} {b}");
        assert!(a.abs_diff(b) <= 2, "instruction-level lockstep: {a} vs {b}");
        assert!(sys.cores[0].cycles.abs_diff(sys.cores[1].cycles) <= 4);
    }

    #[test]
    fn step_runs_whichever_core_is_earliest() {
        let mut sys = boot_two(&[ADDS_R0_1, LOOP_BACK], &[ADDS_R1_1, LOOP_BACK]);
        sys.step(); // tie: core 0
        assert_eq!((sys.cores[0].r[0], sys.cores[1].r[1]), (1, 0));
        sys.step(); // core 1 is now behind
        assert_eq!((sys.cores[0].r[0], sys.cores[1].r[1]), (1, 1));
    }

    #[test]
    fn held_core_does_not_run_and_core_zero_is_unaffected() {
        let mut sys = boot_two(&[ADDS_R0_1, LOOP_BACK], &[ADDS_R1_1, LOOP_BACK]);
        sys.set_core_held(1, true);
        sys.run(500);
        assert_eq!(sys.cores[1].r[1], 0);
        assert!(sys.cores[0].r[0] > 50);
    }

    #[test]
    fn sev_wakes_a_core_in_wfe_but_not_in_wfi() {
        // Core 1 is already asleep in WFE when core 0 signals.
        let mut sys = boot_two(&[NOP, NOP, NOP, NOP, SEV, SPIN], &[WFE, MOVS_R5_1, SPIN]);
        sys.step();
        sys.step();
        assert!(sys.cores[1].sleeping && sys.cores[1].wfe);
        assert_eq!(sys.run(100), StopReason::CycleLimit);
        assert_eq!(sys.cores[1].r[5], 1, "event delivered across cores");

        // An event sent before the WFE is remembered: the WFE doesn't sleep.
        let mut sys = boot_two(&[SEV, SPIN], &[NOP, NOP, NOP, WFE, MOVS_R5_1, SPIN]);
        sys.run(100);
        assert_eq!(sys.cores[1].r[5], 1);

        // WFI is not woken by events.
        let mut sys = boot_two(&[SEV, SPIN], &[NOP, NOP, NOP, WFI, MOVS_R5_1, SPIN]);
        sys.run(100);
        assert!(sys.cores[1].sleeping && sys.cores[1].r[5] == 0);
    }

    #[test]
    fn each_core_has_its_own_nvic_vtor_and_sio_view() {
        let mut bus = SystemBus::new();
        bus.core = 1;
        assert_eq!(bus.read32(SIO_BASE), 1, "CPUID");
        bus.write32(PPB_BASE + 0xED08, SRAM_BASE + 0x400);
        bus.write32(PPB_BASE + 0xE100, 1 << 5); // ISER
        bus.core = 0;
        assert_eq!(bus.read32(SIO_BASE), 0);
        assert_eq!((bus.vtor[0], bus.vtor[1]), (0, SRAM_BASE + 0x400));
        assert_eq!(
            bus.read32(PPB_BASE + 0xE100),
            0,
            "core 0's NVIC is untouched"
        );
        bus.core = 1;
        assert_eq!(bus.read32(PPB_BASE + 0xE100), 1 << 5);
    }

    #[test]
    fn fifo_irq_and_io_bank0_irq_are_per_core() {
        let mut bus = SystemBus::new();
        bus.resets.reset = 0;
        // Core 0 writes the FIFO: core 1's FIFO line (SIO_IRQ_PROC1) rises, visible to both NVICs.
        bus.write32(SIO_BASE + 0x54, 7);
        assert_eq!(bus.irq_lines(0) >> IRQ_SIO_PROC1 & 1, 1);
        assert_eq!(bus.irq_lines(1) >> IRQ_SIO_PROC1 & 1, 1);
        assert_eq!(bus.irq_lines(0) >> IRQ_SIO_PROC0 & 1, 0);
        // IO_BANK0: the level interrupt enabled for proc1 only reaches core 1.
        bus.write32(IO_BANK0 + 0x130, 0b0001); // PROC1_INTE0: LEVEL_LOW on pin 0 (idle low)
        assert_eq!(bus.irq_lines(0) >> IRQ_IO_BANK0 & 1, 0);
        assert_eq!(bus.irq_lines(1) >> IRQ_IO_BANK0 & 1, 1);
    }

    #[test]
    fn bootrom_parks_core_one_and_psm_resets_it() {
        let mut sys = boot_two(&[SPIN], &[SPIN]);
        sys.reset_core1(); // as power-up: the real bootrom
        assert_eq!(sys.run(5_000), StopReason::CycleLimit, "core 0 spins");
        let c1 = &sys.cores[1];
        assert!(c1.sleeping && c1.wfe, "waiting for the launch handshake");
        assert!(c1.pc < BOOTROM_SIZE, "pc = {:#x}", c1.pc);
        assert_eq!(sys.bus.ppb[1].read(0xED10), 4, "bootrom set SLEEPDEEP");

        // PSM: force core 1 off. It stays where it is and does not run.
        let psm = APB_BASE + 0x10000;
        sys.bus.write32(psm + 4, 1 << 16);
        sys.step();
        assert!(sys.held[1]);
        let parked = sys.cores[1].pc;
        sys.run(100);
        assert_eq!(sys.cores[1].pc, parked);
        // Release: back to the bootrom's reset vector, running again.
        sys.bus.write32(psm + 4, 0);
        sys.step();
        assert!(!sys.held[1] && !sys.cores[1].sleeping, "restarted");
        assert_eq!(
            sys.cores[1].pc & !1,
            0xEA & !1 | (sys.cores[1].pc & 1),
            "at the reset handler"
        );
        sys.run(5_000);
        assert!(
            sys.cores[1].sleeping && sys.cores[1].wfe,
            "and parked again"
        );
    }

    #[test]
    fn pwm_drives_its_pin_and_wrap_paced_dma_sets_each_period() {
        const PWM: u32 = 0x4005_0000;
        let mut sys = boot(&[WFI, SPIN]);
        sys.bus.write32(APB_BASE + 0x1_4000 + 16 * 8 + 4, 4); // GPIO16: PWM (slice 0 A)
        let src = SRAM_BASE + 0x1000;
        let levels: alloc::vec::Vec<u8> = [10u32, 20, 30]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        sys.bus.load(src, &levels);
        sys.bus.write32(PWM + 0x10, 99); // TOP: 100 cycles a period
        sys.bus.write32(PWM + 0xA8, 1); // INTE: slice 0
        // Word items from memory into CC, one per wrap (DREQ_PWM_WRAP0).
        dma_start(
            &mut sys,
            0,
            src,
            PWM + 0x0C,
            3,
            1 | 2 << 2 | 1 << 4 | 24 << 15,
        );
        sys.bus.write32(PWM, 1); // CSR: EN
        let level_at = |sys: &mut System, t: u64| {
            sys.run(t - sys.bus.time);
            sys.bus.gpio.pin_level(16)
        };
        // CC is 0 for the first period; the DMA writes 10 at its wrap, which
        // the next wrap takes: high for 10 cycles from 200, 20 from 300.
        assert!(!level_at(&mut sys, 150));
        assert!(level_at(&mut sys, 205));
        assert!(!level_at(&mut sys, 212));
        assert!(level_at(&mut sys, 318));
        assert!(!level_at(&mut sys, 322));
        assert!(level_at(&mut sys, 428));
        assert!(!level_at(&mut sys, 432));
        assert_eq!(sys.bus.read32(PWM + 0x0C), 30, "the last item");
        assert_eq!(sys.bus.read32(DMA_BASE + 0x400), 1, "the DMA finished");
        assert_eq!(sys.bus.read32(PWM + 0xB0), 1, "INTS: wrapped");
        assert_eq!(
            sys.bus.read32(PWM + 0x08),
            32,
            "CTR: 32 cycles after the wrap at 400"
        );
    }

    #[test]
    fn time_follows_a_running_core_while_the_other_sleeps() {
        let mut sys = boot_two(&[ADDS_R0_1, LOOP_BACK], &[WFI]);
        sys.run(1_000);
        assert!(sys.cores[1].sleeping);
        assert!(
            sys.cycles() >= 1_000 && sys.cores[1].cycles >= sys.cycles() - 1,
            "sleeper's clock keeps up"
        );
        assert!(sys.cores[0].cycles.abs_diff(sys.cycles()) <= 4);
    }
}
