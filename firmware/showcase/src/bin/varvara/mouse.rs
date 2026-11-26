//! A PS/2 mouse on GPIO20 (CLK) and GPIO21 (DATA), for Varvara's Mouse
//! device. Both lines are open-drain with the pads' pull-ups.
//!
//! A task of its own sets the mouse up (a reset takes the mouse a third of
//! a second, which the menu shouldn't wait for) and then passes its packets
//! to the frame loop through [`PACKETS`].
//!
//! PIO0 SM0 receives: at 1 MHz it waits for CLK to fall and samples DATA,
//! pushing every 11 bits (start, data, parity, stop) to its RX FIFO. The
//! pins stay SIO outputs for sending, and PIO reads them all the same.
//! Sending, only at start, is bit-banged with the receiver stopped: it
//! would take the host's own frames for bits.
//!
//! When something goes wrong on the way in (a bad frame, or the FIFO full
//! while the VM kept the core busy, so bits went by unread), the host holds
//! CLK for a moment: the mouse gives up the byte it was on and sends it
//! again, and the receiver starts over on a frame boundary.
use defmt::info;
use embassy_rp::{
    Peri,
    gpio::{Level, OutputOpenDrain},
    pac,
    peripherals::{PIN_20, PIN_21, PIO0},
    pio::{Common, Config, FifoJoin, Pio, ShiftConfig, ShiftDirection, StateMachine},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, block_for, with_timeout};

const DATA: u8 = 21;

/// Packets a second: one a frame.
const RATE: u8 = 60;

/// A packet from the mouse, in screen terms.
pub struct Packet {
    pub dx: i16,
    /// Down is positive.
    pub dy: i16,
    /// Varvara's order: left, middle, right from bit 0.
    pub buttons: u8,
    /// Towards the user is positive.
    pub scroll: i16,
}

/// Packets not taken yet; more are dropped.
pub static PACKETS: Channel<CriticalSectionRawMutex, Packet, 8> = Channel::new();

struct Mouse {
    clk: OutputOpenDrain<'static>,
    data: OutputOpenDrain<'static>,
    _common: Common<'static, PIO0>,
    sm: StateMachine<'static, PIO0, 0>,
    /// The wheel is on: packets have a fourth byte.
    wheel: bool,
}

/// Sets the mouse up, then reads it for good; ends if no mouse answers.
#[embassy_executor::task]
pub async fn task(
    pio: Pio<'static, PIO0>,
    clk: Peri<'static, PIN_20>,
    data: Peri<'static, PIN_21>,
    sys_hz: u32,
) {
    let mut m = Mouse::new(pio, clk, data, sys_hz);
    let Some(id) = m.setup().await else {
        info!("varvara: no PS/2 mouse");
        return;
    };
    info!("varvara: a PS/2 mouse, ID {=u8}", id);
    loop {
        let p = m.packet().await;
        let _ = PACKETS.try_send(p);
    }
}

impl Mouse {
    fn new(
        pio: Pio<'static, PIO0>,
        clk: Peri<'static, PIN_20>,
        data: Peri<'static, PIN_21>,
        sys_hz: u32,
    ) -> Self {
        let Pio {
            mut common,
            mut sm0,
            ..
        } = pio;
        let program = pio::pio_asm!(
            ".wrap_target",
            "wait 0 gpio 20",
            "in pins, 1",
            "wait 1 gpio 20",
            ".wrap",
        );
        let loaded = common.load_program(&program.program);
        let mut cfg = Config::default();
        cfg.use_program(&loaded, &[]);
        cfg.clock_divider_x256 = (sys_hz / 1_000_000) << 8;
        cfg.fifo_join = FifoJoin::RxOnly;
        cfg.shift_in = ShiftConfig {
            threshold: 11,
            direction: ShiftDirection::Right,
            auto_fill: true,
        };
        sm0.set_config(&cfg);
        // `in pins` reads DATA, which stays an SIO pin: embassy only sets
        // IN_BASE for pins it hands to the PIO.
        pac::PIO0.sm(0).pinctrl().modify(|w| w.set_in_base(DATA));

        let (mut clk, mut data) = (
            OutputOpenDrain::new(clk, Level::High),
            OutputOpenDrain::new(data, Level::High),
        );
        clk.set_pullup(true);
        data.set_pullup(true);
        Mouse {
            clk,
            data,
            _common: common,
            sm: sm0,
            wheel: false,
        }
    }

    /// Reset the mouse, turn its wheel on (sample rates 200, 100, 80) and
    /// reporting; its ID, or None if it doesn't answer.
    async fn setup(&mut self) -> Option<u8> {
        // The pull-ups take a moment to bring the lines up.
        block_for(Duration::from_micros(100));
        self.receive();
        // The self-test's AA 00 after the FA.
        self.command(0xFF, 2, 1_000).await?;
        for rate in [200, 100, 80] {
            self.command(0xF3, 0, 50).await?;
            self.command(rate, 0, 50).await?;
        }
        let id = self.command(0xF2, 1, 50).await?[0];
        self.wheel = id == 3;
        self.command(0xF3, 0, 50).await?;
        self.command(RATE, 0, 50).await?;
        self.command(0xF4, 0, 50).await?;
        Some(id)
    }

    /// Start the receiver afresh, on whatever comes next.
    fn receive(&mut self) {
        self.sm.set_enable(false);
        self.sm.clear_fifos();
        self.sm.restart();
        // SAFETY: `jmp 0`, to the start of the program, loaded at 0.
        unsafe { self.sm.exec_instr(0x0000) };
        self.sm.set_enable(true);
    }

    /// The next byte; Err on a bad frame.
    async fn byte(&mut self) -> Result<u8, ()> {
        let word = self.sm.rx().wait_pull().await;
        // Shifted in from the top: the frame is the word's top 11 bits.
        let frame = word >> 21;
        let byte = (frame >> 1) as u8;
        let parity = frame >> 9 & 1;
        let ok = frame & 1 == 0
            && frame >> 10 & 1 == 1
            && !(byte.count_ones() + parity).is_multiple_of(2);
        if ok { Ok(byte) } else { Err(()) }
    }

    fn wait_for(&self, clk: bool, high: bool) -> bool {
        let line = if clk { &self.clk } else { &self.data };
        let until = Instant::now() + Duration::from_millis(20);
        while line.is_high() != high {
            if Instant::now() > until {
                return false;
            }
        }
        true
    }

    /// Send `byte`; true if the mouse acknowledged it. Blocks for the
    /// byte's millisecond or so.
    fn send(&mut self, byte: u8) -> bool {
        self.sm.set_enable(false);
        // Request to send: inhibit, start bit, let the mouse clock.
        self.clk.set_low();
        block_for(Duration::from_micros(120));
        self.data.set_low();
        self.clk.set_high();
        let parity = byte.count_ones().is_multiple_of(2);
        let frame = byte as u16 | (parity as u16) << 8 | 1 << 9;
        let mut sent = true;
        for i in 0..10 {
            if !self.wait_for(true, false) {
                sent = false;
                break;
            }
            if frame >> i & 1 != 0 {
                self.data.set_high();
            } else {
                self.data.set_low();
            }
            if !self.wait_for(true, true) {
                sent = false;
                break;
            }
        }
        // The acknowledge: DATA low through one more pulse.
        let acked = sent && self.wait_for(true, false) && self.data.is_low();
        self.data.set_high();
        let released = acked && self.wait_for(true, true) && self.wait_for(false, true);
        self.receive();
        released
    }

    /// Send `byte`, take its `FA` and the `n` bytes after it, waiting up to
    /// `ms` for them.
    async fn command(&mut self, byte: u8, n: usize, ms: u64) -> Option<[u8; 2]> {
        if !self.send(byte) {
            return None;
        }
        let mut reply = [0; 2];
        for got in 0..n + 1 {
            let b = with_timeout(Duration::from_millis(ms), self.byte())
                .await
                .ok()?
                .ok()?;
            match got {
                0 if b != 0xFA => return None,
                0 => {}
                _ => reply[got - 1] = b,
            }
        }
        Some(reply)
    }

    /// Hold CLK long enough for the mouse to give up its byte, and start
    /// over on a frame boundary.
    fn resync(&mut self) {
        self.sm.set_enable(false);
        self.clk.set_low();
        block_for(Duration::from_micros(200));
        self.receive();
        self.clk.set_high();
    }

    /// The next whole packet.
    async fn packet(&mut self) -> Packet {
        let len = if self.wheel { 4 } else { 3 };
        let mut p = [0u8; 4];
        let mut have = 0;
        while have < len {
            if self.sm.rx().full() {
                // Bits may have gone by unread.
                self.resync();
                have = 0;
                continue;
            }
            match self.byte().await {
                // The first byte always has bit 3 set: anything else is
                // the rest of a packet whose start was lost.
                Ok(b) if have > 0 || b & 0x08 != 0 => {
                    p[have] = b;
                    have += 1;
                }
                Ok(_) => {}
                Err(()) => {
                    self.resync();
                    have = 0;
                }
            }
        }
        // Nine bits, the sign in the first byte; PS/2's y is up.
        let dx = p[1] as i16 - ((p[0] as i16 & 0x10) << 4);
        let dy = p[2] as i16 - ((p[0] as i16 & 0x20) << 3);
        // PS/2 has left, right, middle from bit 0.
        let buttons = p[0] & 1 | (p[0] >> 1 & 2) | (p[0] << 1 & 4);
        let scroll = if self.wheel { p[3] as i8 as i16 } else { 0 };
        Packet {
            dx,
            dy: -dy,
            buttons,
            scroll,
        }
    }
}
