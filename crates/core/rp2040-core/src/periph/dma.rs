//! RP2040 DMA controller: 12 channels, the four register aliases per channel
//! with their trigger semantics, chaining, address rings, byte swapping,
//! pacing timers, and the two interrupt lines.
//!
//! This block holds only the controller's *state*; the data movement is done
//! by the system, which owns the bus: it asks [`Dma::pick`] which channel
//! may move one item, performs the read and write, then calls
//! [`Dma::complete`]. One item takes one cycle; bus contention with the CPU
//! is not modelled. Not modelled: the sniffer (its registers are stored) and
//! the error flags.
//!
//! Requests (DREQ/TREQ) from other peripherals are passed in as a bit mask
//! indexed by TREQ number, see [`Dma::needed_dreqs`].

pub const NUM_CHANNELS: usize = 12;
/// TREQ_SEL values for the four pacing timers, and "always ready".
const TREQ_TIMER0: u32 = 0x3B;
const TREQ_PERMANENT: u32 = 0x3F;

// CHn_CTRL fields.
const CTRL_EN: u32 = 1 << 0;
const CTRL_HIGH_PRIORITY: u32 = 1 << 1;
const CTRL_INCR_READ: u32 = 1 << 4;
const CTRL_INCR_WRITE: u32 = 1 << 5;
const CTRL_RING_SEL: u32 = 1 << 10;
const CTRL_IRQ_QUIET: u32 = 1 << 21;
const CTRL_BSWAP: u32 = 1 << 22;
const CTRL_BUSY: u32 = 1 << 24;
/// Writable bits of CTRL (the error flags and BUSY are not).
const CTRL_WRITABLE: u32 = 0x00FF_FFFF & !CTRL_BUSY;

#[derive(Clone, Copy, Default)]
struct Channel {
    read: u32,
    write: u32,
    /// Value copied into `count` each time the channel is triggered.
    reload: u32,
    /// Items left in the running sequence.
    count: u32,
    ctrl: u32,
    busy: bool,
}

impl Channel {
    fn new(index: usize) -> Self {
        // CHAIN_TO resets to the channel itself, i.e. "no chaining".
        Self {
            ctrl: (index as u32) << 11,
            ..Self::default()
        }
    }

    fn enabled(&self) -> bool {
        self.ctrl & CTRL_EN != 0
    }
    fn treq(&self) -> u32 {
        self.ctrl >> 15 & 0x3F
    }
    fn chain_to(&self) -> usize {
        (self.ctrl >> 11 & 0xF) as usize
    }
    /// Bytes per item.
    fn item_bytes(&self) -> u32 {
        match self.ctrl >> 2 & 3 {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }
}

/// Pacing timer: lets a channel run at `x / y` of the system clock.
#[derive(Clone, Copy, Default)]
struct PaceTimer {
    x: u16,
    y: u16,
    /// Fractional progress towards the next token, in units of 1/y.
    credit: u32,
    /// Transfers currently permitted.
    tokens: u32,
    last_cycle: u64,
}

impl PaceTimer {
    /// Run the timer up to `now`. Its requests only count while a running
    /// channel listens (`used`): a channel triggered later doesn't start
    /// with the requests of the time it was idle.
    fn sync(&mut self, now: u64, used: bool) {
        if now <= self.last_cycle {
            return;
        }
        let dt = now - self.last_cycle;
        self.last_cycle = now;
        if !used {
            self.tokens = 0;
            return;
        }
        if self.y == 0 || self.x == 0 {
            return;
        }
        let total = self.credit as u64 + dt * self.x as u64;
        self.tokens = (self.tokens as u64 + total / self.y as u64).min(u32::MAX as u64) as u32;
        self.credit = (total % self.y as u64) as u32;
    }
}

/// One item the system should move: read `bytes` from `read`, write them to `write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub channel: usize,
    pub read: u32,
    pub write: u32,
    pub bytes: u32,
    pub bswap: bool,
}

/// What the system needs to know of a channel to let it stream on its own
/// (see `System`'s DMA streams).
#[derive(Clone, Copy, Debug)]
pub struct ChannelView {
    pub running: bool,
    pub treq: u32,
    pub read: u32,
    pub write: u32,
    pub count: u32,
    pub bytes: u32,
    pub incr_read: bool,
    pub incr_write: bool,
    /// Address bits a ring keeps the read address within (0: no ring).
    pub read_ring_bits: u32,
}

pub struct Dma {
    ch: [Channel; NUM_CHANNELS],
    /// Raw channel interrupts.
    intr: u32,
    inte: [u32; 2],
    intf: [u32; 2],
    timers: [PaceTimer; 4],
    sniff_ctrl: u32,
    sniff_data: u32,
    /// Channel served last, for round-robin arbitration.
    last: usize,
    /// Channels mid-sequence (bit n = channel n) and the TREQs they wait
    /// on: kept up to date wherever a channel starts, stops or is
    /// reconfigured, since the system asks for them for every item.
    running: u16,
    needed: u64,
}

impl Dma {
    pub fn new() -> Self {
        Self {
            ch: core::array::from_fn(Channel::new),
            intr: 0,
            inte: [0; 2],
            intf: [0; 2],
            timers: [PaceTimer::default(); 4],
            sniff_ctrl: 0,
            sniff_data: 0,
            last: NUM_CHANNELS - 1,
            running: 0,
            needed: 0,
        }
    }

    /// Work out [`Dma::running`] and [`Dma::needed`] again.
    fn recount(&mut self) {
        (self.running, self.needed) = (0, 0);
        for (n, c) in self.ch.iter().enumerate() {
            if self.running(c) {
                self.running |= 1 << n;
                self.needed |= 1 << c.treq();
            }
        }
    }

    /// Level of DMA_IRQ_0 / DMA_IRQ_1.
    pub fn irq(&self, line: usize) -> bool {
        (self.intr | self.intf[line]) & self.inte[line] != 0
    }

    /// Bring the pacing timers up to core cycle `now`. The system does this
    /// before each register access too, so a channel starts counting its
    /// timer's requests when it is triggered.
    pub fn sync_timers(&mut self, now: u64) {
        let used = self.needed >> TREQ_TIMER0;
        for (i, t) in self.timers.iter_mut().enumerate() {
            t.sync(now, used >> i & 1 != 0);
        }
    }

    fn running(&self, c: &Channel) -> bool {
        c.busy && c.enabled() && c.count > 0
    }

    pub fn view(&self, n: usize) -> ChannelView {
        let c = &self.ch[n];
        let ring_bits = c.ctrl >> 6 & 0xF;
        ChannelView {
            running: self.running >> n & 1 != 0,
            treq: c.treq(),
            read: c.read,
            write: c.write,
            count: c.count,
            bytes: c.item_bytes(),
            incr_read: c.ctrl & CTRL_INCR_READ != 0,
            incr_write: c.ctrl & CTRL_INCR_WRITE != 0,
            read_ring_bits: if c.ctrl & CTRL_RING_SEL == 0 {
                ring_bits
            } else {
                0
            },
        }
    }

    /// Some channel is mid-sequence (it may still be waiting for a request).
    pub fn active(&self) -> bool {
        self.running != 0
    }

    /// Which TREQ numbers the running channels are waiting on (bit n = TREQ
    /// n). The system answers with the same layout for [`pick`](Self::pick).
    pub fn needed_dreqs(&self) -> u64 {
        self.needed
    }

    fn treq_ok(&self, c: &Channel, dreq: u64) -> bool {
        match c.treq() {
            TREQ_PERMANENT => true,
            t @ TREQ_TIMER0..=0x3E => self.timers[(t - TREQ_TIMER0) as usize].tokens > 0,
            t => dreq >> t & 1 != 0,
        }
    }

    /// Could any channel move an item now, or (timer-paced ones) soon
    /// without anything outside the controller changing?
    pub fn can_run(&self, dreq: u64) -> bool {
        self.ch.iter().enumerate().any(|(n, c)| {
            self.running >> n & 1 != 0
                && match c.treq() {
                    t @ TREQ_TIMER0..=0x3E => {
                        let timer = &self.timers[(t - TREQ_TIMER0) as usize];
                        timer.tokens > 0 || (timer.x != 0 && timer.y != 0)
                    }
                    _ => self.treq_ok(c, dreq),
                }
        })
    }

    /// Choose the channel that moves the next item: high priority first,
    /// round-robin within a priority. Spends a pacing token if the channel
    /// uses one.
    pub fn pick(&mut self, dreq: u64) -> Option<Transfer> {
        // One round-robin pass from the channel after `last`: remember the
        // first ready channel and stop early at a high-priority one.
        let mut n = NUM_CHANNELS;
        let mut idx = self.last;
        for _ in 0..NUM_CHANNELS {
            idx = if idx + 1 == NUM_CHANNELS { 0 } else { idx + 1 };
            let c = &self.ch[idx];
            if !(self.running >> idx & 1 != 0 && self.treq_ok(c, dreq)) {
                continue;
            }
            if c.ctrl & CTRL_HIGH_PRIORITY != 0 {
                n = idx;
                break;
            }
            if n == NUM_CHANNELS {
                n = idx;
            }
        }
        if n == NUM_CHANNELS {
            return None;
        }
        self.last = n;
        let c = &self.ch[n];
        if let t @ TREQ_TIMER0..=0x3E = c.treq() {
            self.timers[(t - TREQ_TIMER0) as usize].tokens -= 1;
        }
        Some(Transfer {
            channel: n,
            read: c.read,
            write: c.write,
            bytes: c.item_bytes(),
            bswap: c.ctrl & CTRL_BSWAP != 0,
        })
    }

    /// The system finished moving the item from [`pick`](Self::pick):
    /// advance the addresses and count, and finish/chain the sequence if it
    /// was the last one.
    pub fn complete(&mut self, t: &Transfer) {
        let c = &mut self.ch[t.channel];
        let ring_bits = c.ctrl >> 6 & 0xF;
        let ring_on_write = c.ctrl & CTRL_RING_SEL != 0;
        let advance = |addr: u32, ring: bool| {
            let next = addr.wrapping_add(t.bytes);
            if ring && ring_bits != 0 {
                let mask = (1u32 << ring_bits) - 1;
                addr & !mask | next & mask
            } else {
                next
            }
        };
        if c.ctrl & CTRL_INCR_READ != 0 {
            c.read = advance(c.read, !ring_on_write);
        }
        if c.ctrl & CTRL_INCR_WRITE != 0 {
            c.write = advance(c.write, ring_on_write);
        }
        c.count -= 1;
        if c.count == 0 {
            c.busy = false;
            if c.ctrl & CTRL_IRQ_QUIET == 0 {
                self.intr |= 1 << t.channel;
            }
            let chain = c.chain_to();
            if chain != t.channel && chain < NUM_CHANNELS {
                self.trigger(chain);
            }
            self.recount();
        }
    }

    /// Start channel `n`'s sequence. Ignored unless the channel is enabled.
    /// A trigger with a zero count is a "null trigger": it raises the
    /// channel interrupt immediately, even in quiet mode.
    fn trigger(&mut self, n: usize) {
        let c = &mut self.ch[n];
        if !c.enabled() {
            return;
        }
        if c.reload == 0 {
            c.busy = false;
            c.count = 0;
            self.intr |= 1 << n;
        } else {
            c.count = c.reload;
            c.busy = true;
        }
    }

    fn abort(&mut self, n: usize) {
        self.ch[n].busy = false;
        self.ch[n].count = 0;
    }

    // ---- registers ----

    /// Which channel register a word offset within a channel's 0x40 window
    /// is, and whether writing it triggers the channel. The four aliases
    /// permute the same registers, each with a different one that triggers.
    fn alias(sub: u32) -> (Reg, bool) {
        use Reg::*;
        const ALIASES: [[Reg; 4]; 4] = [
            [Read, Write, Count, Ctrl],
            [Ctrl, Read, Write, Count],
            [Ctrl, Count, Read, Write],
            [Ctrl, Write, Count, Read],
        ];
        let slot = (sub >> 2 & 3) as usize;
        (ALIASES[(sub >> 4 & 3) as usize][slot], slot == 3)
    }

    /// Whether the atomic register aliases (XOR/SET/CLR) work on the
    /// register at `off` as a read-modify-write. The write-1-to-act
    /// registers (interrupt clears, multi-trigger, abort) take an alias
    /// write as a plain one.
    pub fn alias_applies(off: u32) -> bool {
        !matches!(off, 0x400 | 0x40C | 0x41C | 0x430 | 0x444)
    }

    pub fn read(&mut self, off: u32) -> u32 {
        if off < 0x400 {
            let c = &self.ch[(off >> 6) as usize];
            return match Self::alias(off & 0x3F).0 {
                Reg::Read => c.read,
                Reg::Write => c.write,
                Reg::Count => {
                    if c.busy {
                        c.count
                    } else {
                        c.reload
                    }
                }
                Reg::Ctrl => c.ctrl | (c.busy as u32) << 24,
            };
        }
        match off {
            0x400 => self.intr,
            0x404 => self.inte[0],
            0x408 => self.intf[0],
            0x40C => (self.intr | self.intf[0]) & self.inte[0],
            0x414 => self.inte[1],
            0x418 => self.intf[1],
            0x41C => (self.intr | self.intf[1]) & self.inte[1],
            0x420 | 0x424 | 0x428 | 0x42C => {
                let t = &self.timers[((off - 0x420) >> 2) as usize];
                (t.x as u32) << 16 | t.y as u32
            }
            0x434 => self.sniff_ctrl,
            0x438 => self.sniff_data,
            0x448 => NUM_CHANNELS as u32,
            // Per-channel debug: CTDREQ (request counter, not modelled) and TCR (transfers remaining).
            0x800..=0xAFF if off & 0x3F == 4 => self.ch[((off - 0x800) >> 6) as usize].count,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u32, val: u32) {
        let mask = (1 << NUM_CHANNELS) - 1;
        if off < 0x400 {
            let n = (off >> 6) as usize;
            let (reg, trigger) = Self::alias(off & 0x3F);
            let c = &mut self.ch[n];
            match reg {
                Reg::Read => c.read = val,
                Reg::Write => c.write = val,
                Reg::Count => c.reload = val,
                Reg::Ctrl => c.ctrl = val & CTRL_WRITABLE,
            }
            if trigger {
                self.trigger(n);
            }
            self.recount();
            return;
        }
        match off {
            0x400 | 0x40C | 0x41C => self.intr &= !val, // write 1 to clear
            0x404 => self.inte[0] = val & mask,
            0x408 => self.intf[0] = val & mask,
            0x414 => self.inte[1] = val & mask,
            0x418 => self.intf[1] = val & mask,
            0x420 | 0x424 | 0x428 | 0x42C => {
                let t = &mut self.timers[((off - 0x420) >> 2) as usize];
                (t.x, t.y, t.credit, t.tokens) = ((val >> 16) as u16, val as u16, 0, 0);
            }
            0x430 => {
                (0..NUM_CHANNELS)
                    .filter(|n| val >> n & 1 != 0)
                    .for_each(|n| self.trigger(n));
                self.recount();
            }
            0x434 => self.sniff_ctrl = val,
            0x438 => self.sniff_data = val,
            0x444 => {
                (0..NUM_CHANNELS)
                    .filter(|n| val >> n & 1 != 0)
                    .for_each(|n| self.abort(n));
                self.recount();
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum Reg {
    Read,
    Write,
    Count,
    Ctrl,
}

impl Default for Dma {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    // Register offsets within channel 0's window.
    const READ: u32 = 0x00;
    const WRITE: u32 = 0x04;
    const COUNT: u32 = 0x08;
    const CTRL_TRIG: u32 = 0x0C;
    const CTRL: u32 = 0x10;
    const COUNT_TRIG: u32 = 0x1C;
    const WRITE_TRIG: u32 = 0x2C;
    const READ_TRIG: u32 = 0x3C;
    const INTR: u32 = 0x400;
    const INTE0: u32 = 0x404;
    const INTS0: u32 = 0x40C;

    /// ctrl word: enabled, `size` log2 bytes, increments, treq, chain_to.
    fn ctrl(size: u32, incr_r: bool, incr_w: bool, treq: u32, chain: u32) -> u32 {
        CTRL_EN | size << 2 | (incr_r as u32) << 4 | (incr_w as u32) << 5 | chain << 11 | treq << 15
    }

    /// Run the channel(s) to completion against a plain memory, the way the
    /// system does; returns the number of items moved.
    fn run(d: &mut Dma, mem: &mut [u8], dreq: u64) -> usize {
        let mut n = 0;
        while let Some(t) = d.pick(dreq) {
            let mut v = 0u32;
            for i in 0..t.bytes {
                v |= (mem[(t.read + i) as usize] as u32) << (8 * i);
            }
            if t.bswap {
                v = if t.bytes == 4 {
                    v.swap_bytes()
                } else if t.bytes == 2 {
                    (v as u16).swap_bytes() as u32
                } else {
                    v
                };
            }
            for i in 0..t.bytes {
                mem[(t.write + i) as usize] = (v >> (8 * i)) as u8;
            }
            d.complete(&t);
            n += 1;
        }
        n
    }

    fn start(d: &mut Dma, ch: u32, read: u32, write: u32, count: u32, ctrl: u32) {
        let base = ch * 0x40;
        d.write(base + READ, read);
        d.write(base + WRITE, write);
        d.write(base + COUNT, count);
        d.write(base + CTRL_TRIG, ctrl);
    }

    #[test]
    fn aliases_permute_registers_and_only_one_triggers() {
        let mut d = Dma::new();
        d.write(CTRL, ctrl(2, true, true, TREQ_PERMANENT, 0));
        d.write(0x14, 0x100); // alias 1: READ at +4
        d.write(0x18, 0x200); // alias 1: WRITE at +8
        d.write(0x24, 4); // alias 2: COUNT at +4
        assert_eq!(
            (d.read(READ), d.read(WRITE), d.read(COUNT)),
            (0x100, 0x200, 4)
        );
        assert!(!d.active(), "none of those writes trigger");
        d.write(0x34, 0x204); // alias 3: WRITE at +4, no trigger
        assert_eq!(d.read(WRITE), 0x204);
        d.write(READ_TRIG, 0x110);
        assert!(d.active() && d.read(0x0C) >> 24 & 1 == 1, "BUSY");
        assert_eq!(d.read(READ), 0x110);
        // Other triggers: alias1 count, alias2 write.
        let mut e = Dma::new();
        e.write(CTRL, ctrl(0, true, true, TREQ_PERMANENT, 0));
        e.write(COUNT_TRIG, 3);
        assert!(e.active());
        let mut f = Dma::new();
        f.write(CTRL, ctrl(0, true, true, TREQ_PERMANENT, 0));
        f.write(COUNT, 3);
        f.write(WRITE_TRIG, 0x50);
        assert!(f.active() && f.read(WRITE) == 0x50);
    }

    #[test]
    fn memory_copy_by_item_size_and_increments() {
        for size in 0..3u32 {
            let mut d = Dma::new();
            let mut mem = [0u8; 64];
            for (i, b) in mem[..16].iter_mut().enumerate() {
                *b = i as u8 + 1;
            }
            let bytes = 1 << size;
            start(
                &mut d,
                0,
                0,
                32,
                16 / bytes,
                ctrl(size, true, true, TREQ_PERMANENT, 0),
            );
            assert_eq!(run(&mut d, &mut mem, 0), (16 / bytes) as usize);
            assert_eq!(mem[32..48], mem[0..16], "size {size}");
            assert_eq!((d.read(READ), d.read(WRITE)), (16, 48));
            assert!(!d.active());
        }
        // Fixed write address (peripheral FIFO): only the last value remains.
        let mut d = Dma::new();
        let mut mem = [1, 2, 3, 4, 0, 0, 0, 0];
        start(&mut d, 0, 0, 7, 4, ctrl(0, true, false, TREQ_PERMANENT, 0));
        run(&mut d, &mut mem, 0);
        assert_eq!((mem[7], d.read(WRITE)), (4, 7));
    }

    #[test]
    fn completion_raises_and_clears_interrupts() {
        let mut d = Dma::new();
        let mut mem = [0u8; 8];
        d.write(INTE0, 0b1);
        start(&mut d, 0, 0, 4, 2, ctrl(0, true, true, TREQ_PERMANENT, 0));
        assert!(!d.irq(0));
        run(&mut d, &mut mem, 0);
        assert_eq!((d.read(INTR), d.read(INTS0)), (1, 1));
        assert!(d.irq(0) && !d.irq(1));
        d.write(INTS0, 1); // write 1 to clear
        assert!(!d.irq(0) && d.read(INTR) == 0);
        // Forced interrupts, and the second line.
        d.write(0x418, 0b10);
        d.write(0x414, 0b10);
        assert!(d.irq(1));
        d.write(0x418, 0);
        assert!(!d.irq(1));
        // Quiet mode suppresses the end-of-sequence interrupt.
        start(
            &mut d,
            0,
            0,
            4,
            2,
            ctrl(0, true, true, TREQ_PERMANENT, 0) | CTRL_IRQ_QUIET,
        );
        run(&mut d, &mut mem, 0);
        assert_eq!(d.read(INTR), 0);
    }

    #[test]
    fn null_trigger_raises_interrupt_even_when_quiet() {
        let mut d = Dma::new();
        d.write(COUNT, 0);
        d.write(
            CTRL_TRIG,
            ctrl(0, true, true, TREQ_PERMANENT, 0) | CTRL_IRQ_QUIET,
        );
        assert!(!d.active());
        assert_eq!(d.read(INTR), 1);
    }

    #[test]
    fn disabled_channels_ignore_triggers() {
        let mut d = Dma::new();
        d.write(COUNT, 4);
        d.write(CTRL_TRIG, ctrl(0, true, true, TREQ_PERMANENT, 0) & !CTRL_EN);
        assert!(!d.active());
        // Clearing EN mid-sequence pauses it; setting it resumes.
        let mut mem = [0u8; 16];
        start(&mut d, 0, 0, 8, 4, ctrl(0, true, true, TREQ_PERMANENT, 0));
        let t = d.pick(0).unwrap();
        d.complete(&t);
        d.write(CTRL, ctrl(0, true, true, TREQ_PERMANENT, 0) & !CTRL_EN);
        assert!(
            d.pick(0).is_none() && d.read(0x10) >> 24 & 1 == 1,
            "still BUSY, but paused"
        );
        d.write(CTRL, ctrl(0, true, true, TREQ_PERMANENT, 0));
        assert_eq!(run(&mut d, &mut mem, 0), 3);
    }

    #[test]
    fn chaining_triggers_the_next_channel() {
        let mut d = Dma::new();
        let mut mem = [0u8; 32];
        mem[..4].copy_from_slice(&[9, 8, 7, 6]);
        // ch1 copies 8..12 -> 16..20 and is idle until chained to.
        d.write(0x40 + READ, 8);
        d.write(0x40 + WRITE, 16);
        d.write(0x40 + COUNT, 4);
        d.write(0x40 + CTRL, ctrl(0, true, true, TREQ_PERMANENT, 1));
        // ch0 copies 0..4 -> 8..12, then chains to ch1.
        start(&mut d, 0, 0, 8, 4, ctrl(0, true, true, TREQ_PERMANENT, 1));
        assert_eq!(run(&mut d, &mut mem, 0), 8);
        assert_eq!(mem[16..20], [9, 8, 7, 6]);
        assert_eq!(d.read(INTR), 0b11);
    }

    #[test]
    fn retriggering_reloads_the_count() {
        let mut d = Dma::new();
        let mut mem = [0u8; 16];
        start(&mut d, 0, 0, 8, 3, ctrl(0, true, true, TREQ_PERMANENT, 0));
        run(&mut d, &mut mem, 0);
        assert_eq!(d.read(COUNT), 3, "idle channel reports the reload value");
        d.write(READ_TRIG, 0);
        if let Some(t) = d.pick(0) {
            d.complete(&t);
        }
        assert_eq!(d.read(COUNT), 2, "busy channel reports items left");
        d.write(0x800 + 4, 0); // debug registers are read-only
        assert_eq!(d.read(0x800 + 4), 2);
    }

    #[test]
    fn requests_gate_transfers() {
        let mut d = Dma::new();
        let mut mem = [0u8; 8];
        start(&mut d, 0, 0, 4, 2, ctrl(0, true, true, 16, 0)); // SPI0 TX
        assert_eq!(d.needed_dreqs(), 1 << 16);
        assert!(!d.can_run(0) && d.pick(0).is_none());
        assert!(d.can_run(1 << 16));
        assert_eq!(run(&mut d, &mut mem, 1 << 16), 2);
        assert_eq!(d.needed_dreqs(), 0);
    }

    #[test]
    fn pacing_timer_meters_transfers() {
        let mut d = Dma::new();
        let mut mem = [0u8; 16];
        d.write(0x420, 1 << 16 | 10); // one transfer per 10 cycles
        start(&mut d, 0, 0, 8, 4, ctrl(0, true, true, TREQ_TIMER0, 0));
        d.sync_timers(0);
        assert_eq!(run(&mut d, &mut mem, 0), 0);
        d.sync_timers(25);
        assert_eq!(run(&mut d, &mut mem, 0), 2, "two tokens so far");
        d.sync_timers(100);
        assert_eq!(run(&mut d, &mut mem, 0), 2, "the rest");
        // x/y = 3/2: 1.5 transfers per cycle on average.
        d.write(0x420, 3 << 16 | 2);
        start(&mut d, 0, 0, 8, 8, ctrl(0, true, true, TREQ_TIMER0, 0));
        d.sync_timers(100);
        d.sync_timers(104);
        assert_eq!(d.timers[0].tokens, 6);
        // A running timer-paced channel counts as runnable even with no token yet;
        // a stopped timer (x or y zero) never will.
        let mut e = Dma::new();
        e.write(0x420, 1 << 16 | 1000);
        start(&mut e, 0, 0, 8, 4, ctrl(0, true, true, TREQ_TIMER0, 0));
        assert!(e.can_run(0));
        e.write(0x420, 0);
        assert!(!e.can_run(0));
    }

    #[test]
    fn an_idle_pacing_timer_saves_no_requests() {
        let mut d = Dma::new();
        let mut mem = [0u8; 16];
        d.write(0x420, 1 << 16 | 10);
        d.sync_timers(0);
        d.sync_timers(10_000); // no channel listening
        start(&mut d, 0, 0, 8, 4, ctrl(0, true, true, TREQ_TIMER0, 0));
        d.sync_timers(10_000);
        assert_eq!(
            run(&mut d, &mut mem, 0),
            0,
            "nothing from before the trigger"
        );
        d.sync_timers(10_020);
        assert_eq!(run(&mut d, &mut mem, 0), 2);
        // Syncing to an earlier time (the bus can lag a core) changes nothing.
        d.sync_timers(10_000);
        d.sync_timers(10_020);
        assert_eq!(run(&mut d, &mut mem, 0), 0);
    }

    #[test]
    fn priority_and_round_robin() {
        let mut d = Dma::new();
        for ch in 0..3 {
            start(
                &mut d,
                ch,
                0,
                0,
                4,
                ctrl(0, false, false, TREQ_PERMANENT, ch),
            );
        }
        let order: Vec<usize> = (0..6)
            .map(|_| {
                d.pick(0)
                    .map(|t| {
                        d.complete(&t);
                        t.channel
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(order, [0, 1, 2, 0, 1, 2], "round robin among equals");
        // A high-priority channel jumps the queue.
        let mut d = Dma::new();
        for ch in 0..3 {
            let hp = if ch == 2 { CTRL_HIGH_PRIORITY } else { 0 };
            start(
                &mut d,
                ch,
                0,
                0,
                4,
                ctrl(0, false, false, TREQ_PERMANENT, ch) | hp,
            );
        }
        let order: Vec<usize> = (0..6)
            .map(|_| {
                d.pick(0)
                    .map(|t| {
                        d.complete(&t);
                        t.channel
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(order, [2, 2, 2, 2, 0, 1]);
    }

    #[test]
    fn rings_wrap_the_selected_address() {
        let mut d = Dma::new();
        let mut mem = [0u8; 64];
        for (i, b) in mem[..8].iter_mut().enumerate() {
            *b = 0x10 + i as u8;
        }
        // Read ring of 4 bytes (RING_SIZE=2) starting at 2: 2,3,0,1,2,3,0,1.
        start(
            &mut d,
            0,
            2,
            32,
            8,
            ctrl(0, true, true, TREQ_PERMANENT, 0) | 2 << 6,
        );
        run(&mut d, &mut mem, 0);
        assert_eq!(
            mem[32..40],
            [0x12, 0x13, 0x10, 0x11, 0x12, 0x13, 0x10, 0x11]
        );
        // Write ring (RING_SEL=1): the destination wraps instead.
        let mut d = Dma::new();
        let mut mem = [0u8; 64];
        mem[..4].copy_from_slice(&[1, 2, 3, 4]);
        start(
            &mut d,
            0,
            0,
            34,
            4,
            ctrl(0, true, true, TREQ_PERMANENT, 0) | 2 << 6 | CTRL_RING_SEL,
        );
        run(&mut d, &mut mem, 0);
        assert_eq!(mem[32..36], [3, 4, 1, 2]);
    }

    #[test]
    fn byte_swap() {
        let mut d = Dma::new();
        let mut mem = [0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0, 0, 0];
        start(
            &mut d,
            0,
            0,
            4,
            1,
            ctrl(2, true, true, TREQ_PERMANENT, 0) | CTRL_BSWAP,
        );
        run(&mut d, &mut mem, 0);
        assert_eq!(mem[4..8], [0x44, 0x33, 0x22, 0x11]);
        start(
            &mut d,
            0,
            0,
            8,
            1,
            ctrl(1, true, true, TREQ_PERMANENT, 0) | CTRL_BSWAP,
        );
        run(&mut d, &mut mem, 0);
        assert_eq!(mem[8..10], [0x22, 0x11]);
    }

    #[test]
    fn multi_trigger_and_abort() {
        let mut d = Dma::new();
        for ch in 0..3 {
            let base = ch * 0x40;
            d.write(base + COUNT, 4);
            d.write(base + CTRL, ctrl(0, false, false, TREQ_PERMANENT, ch));
        }
        d.write(0x430, 0b101);
        assert_eq!(d.needed_dreqs(), 1 << 0x3F);
        assert!(d.ch[0].busy && !d.ch[1].busy && d.ch[2].busy);
        d.write(0x444, 0b001);
        assert!(!d.ch[0].busy && d.ch[2].busy);
        assert_eq!(d.read(0x448), 12);
    }

    #[test]
    fn reset_state() {
        let mut d = Dma::new();
        assert_eq!(d.read(0x10 + 0x40 * 3) >> 11 & 0xF, 3, "CHAIN_TO = self");
        assert!(!d.active() && d.read(INTR) == 0);
        d.write(0x434, 0xAB);
        assert_eq!(d.read(0x434), 0xAB);
    }
}
