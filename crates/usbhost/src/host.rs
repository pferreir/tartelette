//! The host: attach, reset, enumeration, then the class drivers' pipes.
use alloc::{collections::VecDeque, format, string::String, vec::Vec};

use crate::{
    DevicePort, Handshake,
    descriptor::{self, *},
    function::{self, *},
    log::{Event, Kind},
    midi,
};

const EVENTS_KEEP: usize = 4096;
/// From the device pulling D+ up to the reset. The spec allows 100 ms;
/// this is enough for any firmware and wastes less emulated time.
const DEBOUNCE_MS: u64 = 10;
const RESET_MS: u64 = 10;
/// The spec's SET_ADDRESS recovery interval.
const SET_ADDRESS_RECOVERY_MS: u64 = 2;
/// How long a control transfer may stay NAKed (Linux's USB_CTRL_GET_TIMEOUT).
const CONTROL_TIMEOUT_MS: u64 = 5000;
/// A control transfer nobody answered is tried this often, this far apart.
const TRIES: u8 = 3;
const RETRY_MS: u64 = 5;
/// The address the device gets.
const ADDRESS: u8 = 1;
/// Bytes on the wire around a packet's data: sync, PIDs, token, CRC,
/// handshake and the gaps between them.
const OVERHEAD: u64 = 16;
/// Asked for when the size of a report is not known.
const REPORT_MAX: u16 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Detached,
    Debounce,
    Resetting,
    Enumerating,
    Configured,
    /// Enumeration went wrong; nothing happens until the device detaches.
    Failed,
}

/// What a control transfer is for, so its result lands in the right place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    DeviceHead,
    SetAddress,
    Device,
    ConfigHead,
    Config,
    Languages,
    /// 0: manufacturer, 1: product, 2: serial number.
    String(usize),
    SetConfig,
    /// A class request whose answer nobody needs.
    Class,
    ReportDescriptor(usize),
    HidReport(usize, u32),
}

impl Purpose {
    /// Enumeration can't go on without it.
    fn required(self) -> bool {
        matches!(
            self,
            Purpose::DeviceHead
                | Purpose::SetAddress
                | Purpose::Device
                | Purpose::ConfigHead
                | Purpose::Config
                | Purpose::SetConfig
        )
    }
}

struct Request {
    setup: Setup,
    /// Data stage of a host-to-device request.
    data: Vec<u8>,
    purpose: Purpose,
    tries: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Setup,
    In,
    Out,
    Status,
}

struct Control {
    req: Request,
    stage: Stage,
    toggle: bool,
    got: Vec<u8>,
    sent: usize,
    started: u64,
    /// NAKed: waiting for the device to arm a buffer.
    parked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    SerialIn,
    SerialOut,
    SerialNotify,
    HidIn,
    HidOut,
    MidiIn,
    MidiOut,
}

/// A bulk or interrupt endpoint a class driver uses.
struct Pipe {
    func: usize,
    role: Role,
    ep: Endpoint,
    toggle: bool,
    parked: bool,
    halted: bool,
    /// Interrupt endpoints: not polled again before this.
    due: u64,
    /// OUT: the transfer being sent, and how much of it went.
    out: Vec<u8>,
    sent: usize,
    /// A timeout was already logged (endpoints answer once enabled).
    timed_out: bool,
}

/// What enumeration found.
#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub descriptor: DeviceDescriptor,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    pub config: Configuration,
}

impl DeviceInfo {
    /// `vid:pid "product"`, for messages.
    pub fn name(&self) -> String {
        let d = &self.descriptor;
        match &self.product {
            Some(p) => format!("{:04x}:{:04x} \"{p}\"", d.vendor, d.product),
            None => format!("{:04x}:{:04x}", d.vendor, d.product),
        }
    }
}

pub struct Host {
    cycles_per_ms: u64,
    cycles_per_us: u64,
    state: State,
    /// The current state's timer, or a pause before the next control transfer.
    hold: u64,
    /// The bus is taken by a transaction until then.
    busy: u64,
    now: u64,
    address: u8,
    max_packet0: u16,
    language: u16,
    queue: VecDeque<Request>,
    control: Option<Control>,
    pipes: Vec<Pipe>,
    next_pipe: usize,
    info: DeviceInfo,
    pub functions: Vec<Function>,
    /// DTR and RTS of a serial port when it is found (default: up, as when
    /// a terminal has it open).
    pub serial_lines: bool,
    generation: u32,
    events: VecDeque<Event>,
}

impl Host {
    /// A host whose clock runs at `cycles_per_us`.
    pub fn new(cycles_per_us: u64) -> Self {
        Self {
            cycles_per_ms: cycles_per_us * 1000,
            cycles_per_us,
            state: State::Detached,
            hold: 0,
            busy: 0,
            now: 0,
            address: 0,
            max_packet0: 64,
            language: 0x0409,
            queue: VecDeque::new(),
            control: None,
            pipes: Vec::new(),
            next_pipe: 0,
            info: DeviceInfo::default(),
            functions: Vec::new(),
            serial_lines: true,
            generation: 0,
            events: VecDeque::new(),
        }
    }

    /// The device, once it is configured.
    pub fn device(&self) -> Option<&DeviceInfo> {
        (self.state == State::Configured).then_some(&self.info)
    }

    /// Changes whenever the set of functions does (a new configuration, a
    /// detach), so frontends know to rebuild their bridges.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The log since the last call.
    pub fn take_events(&mut self) -> VecDeque<Event> {
        core::mem::take(&mut self.events)
    }

    /// The first serial port, if the device has one.
    pub fn serial(&mut self) -> Option<&mut Serial> {
        self.functions.iter_mut().find_map(|f| match f {
            Function::Serial(s) => Some(s),
            _ => None,
        })
    }

    /// Do whatever the bus allows at `now`. `kicked` says the device may have
    /// armed a buffer (or a frontend queued data) since the last call, so
    /// endpoints that NAKed are worth trying again. Returns when to call
    /// again at the latest, if there is anything to wait for.
    pub fn poll(&mut self, port: &mut dyn DevicePort, now: u64, kicked: bool) -> Option<u64> {
        self.now = now;
        if kicked {
            if let Some(c) = &mut self.control {
                c.parked = false;
            }
            for p in &mut self.pipes {
                p.parked = false;
            }
        }
        self.track_attachment(port);
        self.advance_state(port);
        while self.busy <= self.now {
            self.queue_class_requests();
            if !self.transact(port) {
                break;
            }
        }
        self.next_wake()
    }

    fn log(&mut self, kind: Kind) {
        if self.events.len() >= EVENTS_KEEP {
            self.events.pop_front();
        }
        self.events.push_back(Event { at: self.now, kind });
    }

    fn note(&mut self, text: String) {
        self.log(Kind::Note(text));
    }

    /// Take the bus for a packet of `bytes` data bytes (12 Mbit/s).
    fn spend(&mut self, bytes: usize) {
        let bits = (bytes as u64 + OVERHEAD) * 8;
        self.busy = self.busy.max(self.now) + (bits * self.cycles_per_us).div_ceil(12);
    }

    fn track_attachment(&mut self, port: &mut dyn DevicePort) {
        let attached = port.attached();
        if self.state == State::Detached && attached {
            self.log(Kind::Attached);
            self.state = State::Debounce;
            self.hold = self.now + DEBOUNCE_MS * self.cycles_per_ms;
        } else if self.state != State::Detached && !attached {
            self.log(Kind::Detached);
            self.state = State::Detached;
            self.forget();
        }
    }

    fn forget(&mut self) {
        if !self.functions.is_empty() {
            self.generation += 1;
        }
        self.functions.clear();
        self.pipes.clear();
        self.queue.clear();
        self.control = None;
        self.info = DeviceInfo::default();
        self.address = 0;
    }

    fn advance_state(&mut self, port: &mut dyn DevicePort) {
        match self.state {
            State::Debounce if self.now >= self.hold => {
                port.bus_reset();
                self.log(Kind::Reset);
                self.state = State::Resetting;
                self.hold = self.now + RESET_MS * self.cycles_per_ms;
            }
            State::Resetting if self.now >= self.hold => {
                self.state = State::Enumerating;
                self.address = 0;
                self.max_packet0 = 64;
                // As Linux does: ask for 64 bytes and take the first packet.
                self.push(
                    Setup::get_descriptor(DT_DEVICE, 0, 0, 64),
                    Purpose::DeviceHead,
                );
            }
            _ => {}
        }
        let timeout = CONTROL_TIMEOUT_MS * self.cycles_per_ms;
        if self
            .control
            .as_ref()
            .is_some_and(|c| c.parked && self.now >= c.started + timeout)
        {
            let c = self.control.take().unwrap();
            self.finish(c, Err(Handshake::Timeout));
        }
    }

    fn push(&mut self, setup: Setup, purpose: Purpose) {
        self.push_data(setup, Vec::new(), purpose);
    }

    fn push_data(&mut self, setup: Setup, data: Vec<u8>, purpose: Purpose) {
        self.queue.push_back(Request {
            setup,
            data,
            purpose,
            tries: 1,
        });
    }

    fn active(&self) -> bool {
        matches!(self.state, State::Enumerating | State::Configured)
    }

    /// One transaction, if there is one to do: the control pipe first, then
    /// the class pipes in turn.
    fn transact(&mut self, port: &mut dyn DevicePort) -> bool {
        if !self.active() {
            return false;
        }
        if self.now >= self.hold {
            if self.control.is_none()
                && let Some(req) = self.queue.pop_front()
            {
                self.control = Some(Control {
                    stage: Stage::Setup,
                    toggle: false,
                    got: Vec::new(),
                    sent: 0,
                    started: self.now,
                    parked: false,
                    req,
                });
            }
            if self.control.as_ref().is_some_and(|c| !c.parked) {
                self.control_step(port);
                return true;
            }
        }
        if self.state != State::Configured {
            return false;
        }
        let n = self.pipes.len();
        for k in 0..n {
            let i = (self.next_pipe + k) % n;
            if self.pipe_ready(i) && self.now >= self.pipes[i].due {
                self.next_pipe = (i + 1) % n;
                self.pipe_step(port, i);
                return true;
            }
        }
        false
    }

    fn control_step(&mut self, port: &mut dyn DevicePort) {
        let mut c = self.control.take().unwrap();
        let (addr, mps) = (self.address, self.max_packet0);
        let setup = c.req.setup;
        let result = match c.stage {
            Stage::Setup => {
                self.spend(8);
                port.setup(addr, setup.bytes()).map(|()| {
                    c.toggle = true;
                    c.stage = match (setup.length, setup.is_in()) {
                        (0, _) => Stage::Status,
                        (_, true) => Stage::In,
                        (_, false) => Stage::Out,
                    };
                })
            }
            Stage::In => match port.token_in(addr, 0, mps) {
                Ok((data1, data)) => {
                    self.spend(data.len());
                    if data1 != c.toggle {
                        self.log(Kind::Toggle { ep: 0x80 });
                    } else {
                        c.toggle = !c.toggle;
                        let short = data.len() < mps as usize;
                        c.got.extend_from_slice(&data);
                        if short || c.got.len() >= setup.length as usize {
                            c.got.truncate(setup.length as usize);
                            c.stage = Stage::Status;
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    self.spend(0);
                    Err(e)
                }
            },
            Stage::Out => {
                let end = (c.sent + mps as usize).min(c.req.data.len());
                let chunk = &c.req.data[c.sent..end];
                self.spend(chunk.len());
                port.token_out(addr, 0, c.toggle, chunk).map(|()| {
                    c.toggle = !c.toggle;
                    c.sent = end;
                    if c.sent >= c.req.data.len() {
                        c.stage = Stage::Status;
                    }
                })
            }
            Stage::Status => {
                self.spend(0);
                let r = if setup.is_in() && setup.length > 0 {
                    port.token_out(addr, 0, true, &[])
                } else {
                    port.token_in(addr, 0, mps).map(|_| ())
                };
                if r.is_ok() {
                    return self.finish(c, Ok(()));
                }
                r
            }
        };
        match result {
            Ok(()) => self.control = Some(c),
            Err(Handshake::Nak) => {
                c.parked = true;
                self.control = Some(c);
            }
            Err(e) => self.finish(c, Err(e)),
        }
    }

    /// A control transfer ended: log it, retry it, or act on its answer.
    fn finish(&mut self, c: Control, result: Result<(), Handshake>) {
        let Control { req, got, .. } = c;
        let data = if req.setup.is_in() {
            got.clone()
        } else {
            req.data.clone()
        };
        self.log(Kind::Control {
            setup: req.setup,
            data,
            result,
        });
        if result == Err(Handshake::Timeout) && req.tries < TRIES {
            self.hold = self.now + RETRY_MS * self.cycles_per_ms;
            self.queue.push_front(Request {
                tries: req.tries + 1,
                ..req
            });
            return;
        }
        if let Err(e) = result {
            if req.purpose.required() {
                self.state = State::Failed;
                self.note(format!(
                    "enumeration failed: {} {}",
                    descriptor::request_name(&req.setup).unwrap_or("request"),
                    e.name()
                ));
                return;
            }
            match req.purpose {
                Purpose::Languages => self.ask_strings(),
                Purpose::HidReport(f, id) => {
                    if let Some(Function::Hid(h)) = self.functions.get_mut(f) {
                        h.replies.push_back(HidReply { id, data: None });
                    }
                }
                _ => {}
            }
            return;
        }
        self.on_answer(req.purpose, got);
    }

    fn on_answer(&mut self, purpose: Purpose, got: Vec<u8>) {
        match purpose {
            Purpose::DeviceHead => {
                if got.len() < 8 {
                    self.state = State::Failed;
                    return self.note("enumeration failed: short device descriptor".into());
                }
                self.max_packet0 = match got[7] {
                    n @ (8 | 16 | 32 | 64) => n as u16,
                    _ => 64,
                };
                self.push(
                    Setup::new(0x00, SET_ADDRESS, ADDRESS as u16, 0, 0),
                    Purpose::SetAddress,
                );
            }
            Purpose::SetAddress => {
                self.address = ADDRESS;
                self.hold = self.now + SET_ADDRESS_RECOVERY_MS * self.cycles_per_ms;
                self.push(Setup::get_descriptor(DT_DEVICE, 0, 0, 18), Purpose::Device);
            }
            Purpose::Device => match DeviceDescriptor::parse(&got) {
                Some(d) => {
                    self.info.descriptor = d;
                    self.push(
                        Setup::get_descriptor(DT_CONFIG, 0, 0, 9),
                        Purpose::ConfigHead,
                    );
                }
                None => {
                    self.state = State::Failed;
                    self.note("enumeration failed: bad device descriptor".into());
                }
            },
            Purpose::ConfigHead if got.len() >= 4 => {
                let total = u16::from_le_bytes([got[2], got[3]]);
                self.push(
                    Setup::get_descriptor(DT_CONFIG, 0, 0, total),
                    Purpose::Config,
                );
            }
            Purpose::Config => match Configuration::parse(&got) {
                Some(c) => {
                    self.info.config = c;
                    let d = &self.info.descriptor;
                    if d.manufacturer | d.product_string | d.serial != 0 {
                        self.push(
                            Setup::get_descriptor(DT_STRING, 0, 0, 255),
                            Purpose::Languages,
                        );
                    } else {
                        self.ask_configuration();
                    }
                }
                None => {
                    self.state = State::Failed;
                    self.note("enumeration failed: bad configuration descriptor".into());
                }
            },
            Purpose::ConfigHead => {
                self.state = State::Failed;
                self.note("enumeration failed: short configuration descriptor".into());
            }
            Purpose::Languages => {
                if got.len() >= 4 {
                    self.language = u16::from_le_bytes([got[2], got[3]]);
                }
                self.ask_strings();
            }
            Purpose::String(k) => {
                let s = descriptor::parse_string(&got);
                match k {
                    0 => self.info.manufacturer = s,
                    1 => self.info.product = s,
                    _ => self.info.serial = s,
                }
            }
            Purpose::SetConfig => {
                self.state = State::Configured;
                self.set_up_functions();
            }
            Purpose::Class => {}
            Purpose::ReportDescriptor(f) => {
                if let Some(Function::Hid(h)) = self.functions.get_mut(f) {
                    h.report_descriptor = got;
                    h.ready = true;
                }
            }
            Purpose::HidReport(f, id) => {
                if let Some(Function::Hid(h)) = self.functions.get_mut(f) {
                    h.replies.push_back(HidReply {
                        id,
                        data: Some(got),
                    });
                }
            }
        }
    }

    /// Read the manufacturer, product and serial strings, then configure.
    fn ask_strings(&mut self) {
        let d = &self.info.descriptor;
        let indices = [d.manufacturer, d.product_string, d.serial];
        for (k, i) in indices.into_iter().enumerate() {
            if i != 0 {
                self.push(
                    Setup::get_descriptor(DT_STRING, i, self.language, 255),
                    Purpose::String(k),
                );
            }
        }
        self.ask_configuration();
    }

    fn ask_configuration(&mut self) {
        let value = self.info.config.value as u16;
        self.push(
            Setup::new(0x00, SET_CONFIGURATION, value, 0, 0),
            Purpose::SetConfig,
        );
    }

    /// Bind a class driver to each interface of the configuration.
    fn set_up_functions(&mut self) {
        let cfg = self.info.config.clone();
        let mut claimed: Vec<u8> = Vec::new();
        for itf in &cfg.interfaces {
            if claimed.contains(&itf.number) {
                continue;
            }
            let f = self.functions.len();
            let data_itf = itf
                .cdc_slaves
                .first()
                .copied()
                .or(Some(itf.number + 1))
                .and_then(|n| cfg.interface(n))
                .filter(|d| d.class == CLASS_CDC_DATA);
            let function = match (itf.class, itf.subclass, data_itf) {
                (CLASS_CDC, SUBCLASS_ACM, Some(data)) => {
                    claimed.push(data.number);
                    for ep in &data.endpoints {
                        match (ep.kind, ep.is_in()) {
                            (TransferType::Bulk, true) => self.add_pipe(f, Role::SerialIn, ep),
                            (TransferType::Bulk, false) => self.add_pipe(f, Role::SerialOut, ep),
                            _ => {}
                        }
                    }
                    for ep in itf.endpoints.iter().filter(|e| e.is_in()) {
                        self.add_pipe(f, Role::SerialNotify, ep);
                    }
                    Function::Serial(Serial::new(itf.number, data.number, self.serial_lines))
                }
                (CLASS_HID, ..) => {
                    let mut has_out = false;
                    for ep in itf
                        .endpoints
                        .iter()
                        .filter(|e| e.kind == TransferType::Interrupt)
                    {
                        if ep.is_in() {
                            self.add_pipe(f, Role::HidIn, ep);
                        } else {
                            has_out = true;
                            self.add_pipe(f, Role::HidOut, ep);
                        }
                    }
                    let i = itf.number as u16;
                    // SET_IDLE(0): report only on change. Many devices stall it.
                    self.push(Setup::new(0x21, 0x0A, 0, i, 0), Purpose::Class);
                    if let Some(len) = itf.hid_report_len {
                        self.push(
                            Setup::new(0x81, GET_DESCRIPTOR, (DT_HID_REPORT as u16) << 8, i, len),
                            Purpose::ReportDescriptor(f),
                        );
                    }
                    Function::Hid(Hid {
                        interface: itf.number,
                        subclass: itf.subclass,
                        protocol: itf.protocol,
                        country: itf.hid_country,
                        report_descriptor: Vec::new(),
                        ready: false,
                        input: VecDeque::new(),
                        output: VecDeque::new(),
                        requests: VecDeque::new(),
                        replies: VecDeque::new(),
                        report_len: itf.hid_report_len.unwrap_or(0),
                        has_out,
                    })
                }
                (CLASS_AUDIO, SUBCLASS_MIDI, _) => {
                    let (mut cables_in, mut cables_out) = (0, 0);
                    for ep in &itf.endpoints {
                        if !matches!(ep.kind, TransferType::Bulk | TransferType::Interrupt) {
                            continue;
                        }
                        if ep.is_in() {
                            cables_in += ep.midi_jacks.max(1);
                            self.add_pipe(f, Role::MidiIn, ep);
                        } else {
                            cables_out += ep.midi_jacks.max(1);
                            self.add_pipe(f, Role::MidiOut, ep);
                        }
                    }
                    Function::Midi(Midi {
                        interface: itf.number,
                        cables_in,
                        cables_out,
                        rx: VecDeque::new(),
                        tx: VecDeque::new(),
                        decoder: midi::Decoder::default(),
                    })
                }
                _ => Function::Other(Other {
                    interface: itf.number,
                    class: itf.class,
                    subclass: itf.subclass,
                    protocol: itf.protocol,
                }),
            };
            self.functions.push(function);
        }
        self.generation += 1;
        let mut summary = format!("configured {}:", self.info.name());
        for f in &self.functions {
            summary.push_str(&match f {
                Function::Serial(s) => format!(" serial (if {}+{})", s.interface, s.data_interface),
                Function::Hid(h) => format!(" HID (if {})", h.interface),
                Function::Midi(m) => format!(" MIDI (if {})", m.interface),
                Function::Other(o) => format!(
                    " {} (if {}, unused)",
                    function::class_name(o.class, o.subclass),
                    o.interface
                ),
            });
        }
        self.note(summary);
    }

    fn add_pipe(&mut self, func: usize, role: Role, ep: &Endpoint) {
        self.pipes.push(Pipe {
            func,
            role,
            ep: *ep,
            toggle: false,
            parked: false,
            halted: false,
            due: self.now,
            out: Vec::new(),
            sent: 0,
            timed_out: false,
        });
    }

    /// Turn what frontends asked of the functions into control transfers.
    fn queue_class_requests(&mut self) {
        if self.state != State::Configured {
            return;
        }
        let mut new = Vec::new();
        for (f, func) in self.functions.iter_mut().enumerate() {
            match func {
                Function::Serial(s) => {
                    let i = s.interface as u16;
                    if s.coding_sent != Some(s.line_coding) {
                        s.coding_sent = Some(s.line_coding);
                        new.push((
                            Setup::new(0x21, 0x20, 0, i, 7),
                            s.coding_bytes(),
                            Purpose::Class,
                        ));
                    }
                    if s.lines_sent != Some((s.dtr, s.rts)) {
                        s.lines_sent = Some((s.dtr, s.rts));
                        let value = s.dtr as u16 | (s.rts as u16) << 1;
                        new.push((
                            Setup::new(0x21, 0x22, value, i, 0),
                            Vec::new(),
                            Purpose::Class,
                        ));
                    }
                }
                Function::Hid(h) => {
                    let i = h.interface as u16;
                    if !h.has_out {
                        let ids = uses_report_ids(&h.report_descriptor);
                        while let Some(r) = h.output.pop_front() {
                            let id = if ids {
                                r.first().copied().unwrap_or(0)
                            } else {
                                0
                            };
                            let value = (ReportType::Output as u16) << 8 | id as u16;
                            let len = r.len() as u16;
                            new.push((Setup::new(0x21, 0x09, value, i, len), r, Purpose::Class));
                        }
                    }
                    while let Some(r) = h.requests.pop_front() {
                        let value = (r.kind as u16) << 8 | r.report_id as u16;
                        let purpose = Purpose::HidReport(f, r.id);
                        new.push(match r.set {
                            Some(data) => {
                                let len = data.len() as u16;
                                (Setup::new(0x21, 0x09, value, i, len), data, purpose)
                            }
                            None => {
                                let len = if h.report_len > 0 { REPORT_MAX } else { 64 };
                                (Setup::new(0xA1, 0x01, value, i, len), Vec::new(), purpose)
                            }
                        });
                    }
                }
                _ => {}
            }
        }
        for (setup, data, purpose) in new {
            self.push_data(setup, data, purpose);
        }
    }

    /// Does the pipe have something to do (data to send, room to receive)?
    fn pipe_ready(&self, i: usize) -> bool {
        let p = &self.pipes[i];
        if p.halted || p.parked {
            return false;
        }
        let pending = p.sent < p.out.len();
        match (p.role, &self.functions[p.func]) {
            (Role::SerialIn, Function::Serial(s)) => s.rx.len() < function::SERIAL_KEEP,
            (Role::SerialOut, Function::Serial(s)) => pending || !s.tx.is_empty(),
            (Role::SerialNotify, _) => true,
            (Role::HidIn, Function::Hid(h)) => h.input.len() < function::MESSAGES_KEEP,
            (Role::HidOut, Function::Hid(h)) => pending || !h.output.is_empty(),
            (Role::MidiIn, Function::Midi(m)) => m.rx.len() < function::MESSAGES_KEEP,
            (Role::MidiOut, Function::Midi(m)) => pending || !m.tx.is_empty(),
            _ => false,
        }
    }

    fn pipe_step(&mut self, port: &mut dyn DevicePort, i: usize) {
        let addr = self.address;
        let p = &self.pipes[i];
        let (ep, mps, interrupt) = (
            p.ep.address,
            p.ep.max_packet,
            p.ep.kind == TransferType::Interrupt,
        );
        let interval = p.ep.interval.max(1) as u64 * self.cycles_per_ms;
        let result = if ep & 0x80 != 0 {
            match port.token_in(addr, ep & 0xF, mps) {
                Ok((data1, data)) => {
                    self.spend(data.len());
                    if data1 != self.pipes[i].toggle {
                        self.log(Kind::Toggle { ep });
                    } else {
                        self.pipes[i].toggle ^= true;
                        self.deliver(i, &data);
                        self.log(Kind::Data { ep, data });
                    }
                    Ok(())
                }
                Err(e) => {
                    self.spend(0);
                    Err(e)
                }
            }
        } else {
            if self.pipes[i].sent >= self.pipes[i].out.len() {
                let out = self.fill(i);
                let p = &mut self.pipes[i];
                (p.out, p.sent) = (out, 0);
            }
            let p = &self.pipes[i];
            let end = (p.sent + mps as usize).min(p.out.len());
            let chunk = p.out[p.sent..end].to_vec();
            let toggle = p.toggle;
            self.spend(chunk.len());
            port.token_out(addr, ep, toggle, &chunk).map(|()| {
                let p = &mut self.pipes[i];
                p.toggle ^= true;
                p.sent = end;
                self.log(Kind::Data { ep, data: chunk });
            })
        };
        if interrupt {
            self.pipes[i].due = self.now + interval;
        }
        match result {
            Ok(()) | Err(Handshake::Nak) => self.pipes[i].parked = result.is_err(),
            Err(Handshake::Stall) => {
                self.pipes[i].halted = true;
                self.log(Kind::Halted {
                    ep,
                    why: Handshake::Stall,
                });
            }
            Err(Handshake::Timeout) => {
                // Usually an endpoint the firmware has not enabled yet.
                self.pipes[i].parked = true;
                if !core::mem::replace(&mut self.pipes[i].timed_out, true) {
                    self.note(format!("ep{} does not answer (not enabled?)", ep & 0xF));
                }
            }
        }
    }

    /// Hand a packet from an IN pipe to its function.
    fn deliver(&mut self, i: usize, data: &[u8]) {
        let p = &self.pipes[i];
        match (p.role, &mut self.functions[p.func]) {
            (Role::SerialIn, Function::Serial(s)) => s.rx.extend(data),
            // SERIAL_STATE: a notification header then the 16-bit state.
            (Role::SerialNotify, Function::Serial(s)) if data.len() >= 10 && data[1] == 0x20 => {
                s.serial_state = u16::from_le_bytes([data[8], data[9]]);
            }
            (Role::HidIn, Function::Hid(h)) => h.input.push_back(data.to_vec()),
            (Role::MidiIn, Function::Midi(m)) => {
                for p in data.as_chunks::<4>().0 {
                    if let Some(msg) = m.decoder.packet(*p) {
                        m.rx.push_back(msg);
                    }
                }
            }
            _ => {}
        }
    }

    /// The next transfer for an OUT pipe, from its function's queue.
    fn fill(&mut self, i: usize) -> Vec<u8> {
        let p = &self.pipes[i];
        let mps = p.ep.max_packet.max(4) as usize;
        match (p.role, &mut self.functions[p.func]) {
            (Role::SerialOut, Function::Serial(s)) => {
                let n = s.tx.len().min(mps);
                s.tx.drain(..n).collect()
            }
            (Role::HidOut, Function::Hid(h)) => h.output.pop_front().unwrap_or_default(),
            (Role::MidiOut, Function::Midi(m)) => {
                let mut out = Vec::new();
                while let Some((cable, msg)) = m.tx.front() {
                    let packets = midi::encode(*cable, msg);
                    if !out.is_empty() && out.len() + 4 * packets.len() > mps {
                        break;
                    }
                    out.extend(packets.iter().flatten());
                    m.tx.pop_front();
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn next_wake(&self) -> Option<u64> {
        let mut wake: Option<u64> = None;
        let mut at = |t: u64| wake = Some(wake.map_or(t, |w| w.min(t)));
        if matches!(self.state, State::Debounce | State::Resetting) {
            at(self.hold);
        }
        let bus = self.busy.max(self.now);
        if self.active() {
            match &self.control {
                Some(c) if c.parked => at(c.started + CONTROL_TIMEOUT_MS * self.cycles_per_ms),
                Some(_) => at(bus.max(self.hold)),
                None if !self.queue.is_empty() => at(bus.max(self.hold)),
                None => {}
            }
        }
        if self.state == State::Configured {
            for (i, p) in self.pipes.iter().enumerate() {
                if self.pipe_ready(i) {
                    at(bus.max(p.due));
                }
            }
        }
        wake
    }
}

/// Does a HID report descriptor give its reports IDs (a Report ID item)?
pub fn uses_report_ids(desc: &[u8]) -> bool {
    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xFE {
            // Long item: size in the next byte.
            i += 3 + desc.get(i + 1).copied().unwrap_or(0) as usize;
            continue;
        }
        if b & 0xFC == 0x84 {
            return true;
        }
        i += 1 + [0, 1, 2, 4][(b & 3) as usize];
    }
    false
}
