//! The SD card, as Varvara's two File devices (ports 0xa0-0xaf and
//! 0xb0-0xbf) and as the place the menu finds ROMs. See
//! <https://wiki.xxiivv.com/site/varvara.html#file>; the behaviour follows
//! uxn's `file.c`, with the card's first FAT volume as the sandbox.
//!
//! A path is `/`-separated from the card's root. Each part is found by its
//! long name or its 8.3 one, regardless of case. `embedded-sdmmc` can't
//! write long names, so a new file or directory whose name isn't 8.3 gets
//! one made from it (`FILE~3A2.TXT` for `file-output.txt`: up to four
//! characters of the name, a hash of all of it, the extension's first
//! three), and the same name finds it again.

use core::{cell::RefCell, ops::ControlFlow};

use embedded_hal::{delay::DelayNs, spi::SpiDevice};
use embedded_sdmmc::{
    DirEntry, LfnBuffer, Mode, RawDirectory, RawFile, RawVolume, SdCard, ShortFileName, TimeSource,
    VolumeIdx, VolumeManager,
};

/// The longest path a program can name; a longer one names nothing.
const MAX_PATH: usize = 256;

/// What a File device has open, for the reads or writes that follow.
#[derive(Clone, Copy)]
enum State {
    Idle,
    Read(RawFile),
    Write(RawFile),
    /// Listing a directory: how many of its entries were given already.
    List(usize),
}

/// One File device: the name it was given, and what it has open.
struct Port {
    path: [u8; MAX_PATH],
    len: usize,
    state: State,
}

impl Port {
    const fn new() -> Self {
        Self {
            path: [0; MAX_PATH],
            len: 0,
            state: State::Idle,
        }
    }
}

pub struct Disk<S: SpiDevice, D: DelayNs, T: TimeSource> {
    vm: VolumeManager<SdCard<S, D>, T, 4, 4, 1>,
    /// Storage for a long name being read: 255 UTF-16 units, at most 3
    /// bytes each in UTF-8.
    lfn: RefCell<[u8; 768]>,
    /// The volume and its root directory, while mounted.
    root: Option<(RawVolume, RawDirectory)>,
    ports: [Port; 2],
}

/// An entry's name for a program: its long name, or its 8.3 one in lower
/// case (which is how most cards' short names were written).
fn display_name<'a>(e: &DirEntry, lfn: Option<&'a str>, buf: &'a mut [u8; 12]) -> &'a [u8] {
    if let Some(l) = lfn {
        return l.as_bytes();
    }
    let (base, ext) = (e.name.base_name(), e.name.extension());
    let mut n = 0;
    for &b in base {
        buf[n] = b.to_ascii_lowercase();
        n += 1;
    }
    if !ext.is_empty() {
        buf[n] = b'.';
        n += 1;
        for &b in ext {
            buf[n] = b.to_ascii_lowercase();
            n += 1;
        }
    }
    &buf[..n]
}

/// The 8.3 name a new file or directory called `name` gets: the name itself
/// if it is one, or else one made from it (see the module's doc).
fn short_name(name: &[u8]) -> Option<ShortFileName> {
    let s = core::str::from_utf8(name).ok()?;
    if let Ok(sfn) = ShortFileName::create_from_str(s) {
        return Some(sfn);
    }
    alias(name)
}

fn alias(name: &[u8]) -> Option<ShortFileName> {
    let ok = |c: &u8| c.is_ascii_alphanumeric() || b"!#$%&'()-@^_`{}~".contains(c);
    let (base, ext) = match name.iter().rposition(|&c| c == b'.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, &[][..]),
    };
    let hash = name.iter().fold(0x811c_9dc5u32, |h, &c| {
        (h ^ c.to_ascii_lowercase() as u32).wrapping_mul(0x0100_0193)
    });
    let mut s = heapless::String::<12>::new();
    for &c in base.iter().filter(|c| ok(c)).take(4) {
        let _ = s.push(c.to_ascii_uppercase() as char);
    }
    if s.is_empty() {
        let _ = s.push('_');
    }
    let _ = s.push('~');
    for i in (0..3).rev() {
        let d = (hash >> (4 * i)) & 0xf;
        let _ = s.push(char::from_digit(d, 16)?.to_ascii_uppercase());
    }
    let mut ext = ext.iter().filter(|c| ok(c)).take(3).peekable();
    if ext.peek().is_some() {
        let _ = s.push('.');
        for &c in ext {
            let _ = s.push(c.to_ascii_uppercase() as char);
        }
    }
    ShortFileName::create_from_str(&s).ok()
}

/// Whether entry `e` (with long name `lfn`) is the one called `name`.
fn is_called(e: &DirEntry, lfn: Option<&str>, name: &[u8]) -> bool {
    if lfn.is_some_and(|l| l.as_bytes().eq_ignore_ascii_case(name)) {
        return true;
    }
    let mut buf = [0; 12];
    display_name(e, None, &mut buf).eq_ignore_ascii_case(name) || alias(name) == Some(e.name)
}

/// The parts of a path: its names, without empty ones and `.`.
fn parts(path: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    path.split(|&c| c == b'/')
        .filter(|p| !p.is_empty() && *p != b".")
}

/// Stat and listing sizes: the low `len` hex digits of `size`, or `?`s if
/// it has more.
fn hex(out: &mut [u8], size: u32) {
    let digit = |i: usize| size.checked_shr(4 * i as u32).unwrap_or(0);
    let fits = digit(out.len()) == 0;
    for (i, o) in out.iter_mut().rev().enumerate() {
        *o = if fits {
            b"0123456789abcdef"[(digit(i) & 0xf) as usize]
        } else {
            b'?'
        };
    }
}

impl<S: SpiDevice, D: DelayNs, T: TimeSource> Disk<S, D, T> {
    pub fn new(card: SdCard<S, D>, time: T) -> Self {
        Self {
            vm: VolumeManager::new_with_limits(card, time, 0),
            lfn: RefCell::new([0; 768]),
            root: None,
            ports: [Port::new(), Port::new()],
        }
    }

    /// The SPI device under the card, to change its clock.
    pub fn with_spi<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        self.vm.device(|card| card.spi(f))
    }

    /// Closes everything and opens the card's first volume afresh (a card
    /// may have been put in, or swapped, since). Whether there is one.
    pub fn mount(&mut self) -> bool {
        self.unmount();
        self.vm.device(|card| card.mark_card_uninit());
        let Ok(volume) = self.vm.open_raw_volume(VolumeIdx(0)) else {
            return false;
        };
        match self.vm.open_root_dir(volume) {
            Ok(root) => {
                self.root = Some((volume, root));
                true
            }
            Err(_) => {
                let _ = self.vm.close_volume(volume);
                false
            }
        }
    }

    pub fn mounted(&self) -> bool {
        self.root.is_some()
    }

    /// Closes what the File devices have open, and the volume.
    pub fn unmount(&mut self) {
        for i in 0..2 {
            self.close(i);
            self.ports[i].len = 0;
        }
        if let Some((volume, root)) = self.root.take() {
            let _ = self.vm.close_dir(root);
            let _ = self.vm.close_volume(volume);
        }
    }

    /// Closes what File device `i` has open (flushing a file written to).
    fn close(&mut self, i: usize) {
        if let State::Read(f) | State::Write(f) = self.ports[i].state {
            let _ = self.vm.close_file(f);
        }
        self.ports[i].state = State::Idle;
    }

    /// The entry called `name` in `dir`.
    fn find(&self, dir: RawDirectory, name: &[u8]) -> Option<DirEntry> {
        let mut found = None;
        let mut storage = self.lfn.borrow_mut();
        let mut lfn = LfnBuffer::new(&mut storage[..]);
        let _ = self.vm.iterate_dir_lfn(dir, &mut lfn, |e, l| {
            if is_called(e, l, name) {
                found = Some(e.clone());
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    }

    /// Opens the directories along `dirs` from the root, making missing ones
    /// if `make`: the last (the root if there are none), to give back to
    /// `release`.
    fn walk<'a>(&self, dirs: impl Iterator<Item = &'a [u8]>, make: bool) -> Option<RawDirectory> {
        let (_, root) = self.root?;
        let mut dir = root;
        for name in dirs {
            let sfn = match self.find(dir, name) {
                Some(e) if e.attributes.is_directory() => Some(e.name),
                Some(_) => None,
                None if make => {
                    short_name(name).filter(|sfn| self.vm.make_dir_in_dir(dir, sfn).is_ok())
                }
                None => None,
            };
            let next = sfn.and_then(|sfn| self.vm.open_dir(dir, sfn).ok());
            self.release(dir);
            dir = next?;
        }
        Some(dir)
    }

    fn release(&self, dir: RawDirectory) {
        if self.root.is_some_and(|(_, root)| root != dir) {
            let _ = self.vm.close_dir(dir);
        }
    }

    /// The directory a path's last part is in, and that part (empty for the
    /// root itself).
    fn parent<'p>(&self, path: &'p [u8], make: bool) -> Option<(RawDirectory, &'p [u8])> {
        let mut it = parts(path);
        let last = it.next_back().unwrap_or(&[]);
        Some((self.walk(it, make)?, last))
    }

    /// What `path` is: `Some(None)` for a directory, `Some(Some(size))` for a
    /// file, `None` if there's nothing there.
    fn stat_path(&self, path: &[u8]) -> Option<Option<u32>> {
        let (dir, last) = self.parent(path, false)?;
        let r = if last.is_empty() {
            Some(None)
        } else {
            self.find(dir, last)
                .map(|e| (!e.attributes.is_directory()).then_some(e.size))
        };
        self.release(dir);
        r
    }

    /// Calls `f` with each entry of directory `path` but `.` (and the
    /// volume label), and its name, until it breaks. False if there is no
    /// such directory.
    pub fn list(
        &self,
        path: &[u8],
        mut f: impl FnMut(&DirEntry, &[u8]) -> ControlFlow<()>,
    ) -> bool {
        let Some(dir) = self.walk(parts(path), false) else {
            return false;
        };
        {
            let mut storage = self.lfn.borrow_mut();
            let mut lfn = LfnBuffer::new(&mut storage[..]);
            let _ = self.vm.iterate_dir_lfn(dir, &mut lfn, |e, l| {
                let mut buf = [0; 12];
                let name = display_name(e, l, &mut buf);
                if e.attributes.is_volume() || name == b"." {
                    return ControlFlow::Continue(());
                }
                f(e, name)
            });
        }
        self.release(dir);
        true
    }

    /// Reads the file at `path` into `out`: how many bytes, or None if
    /// there is no such file.
    pub fn load(&self, path: &[u8], out: &mut [u8]) -> Option<usize> {
        let (dir, last) = self.parent(path, false)?;
        let file = self
            .find(dir, last)
            .filter(|e| !e.attributes.is_directory())
            .and_then(|e| self.vm.open_file_in_dir(dir, e.name, Mode::ReadOnly).ok());
        self.release(dir);
        let file = file?;
        let mut n = 0;
        while n < out.len() {
            match self.vm.read(file, &mut out[n..]) {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k,
            }
        }
        let _ = self.vm.close_file(file);
        Some(n)
    }

    // ------------------------------------------------------- the devices ----

    /// A File device's port `port` (0..16) was written; `d` is its 16
    /// ports. Sets File/success as uxn does.
    pub fn deo(&mut self, i: usize, ram: &mut [u8; 0x10000], d: &mut [u8], port: u8) {
        let peek = |p: usize| (d[p] as usize) << 8 | d[p + 1] as usize;
        // A length from File/length, cut short at the end of memory.
        let span = |a: usize| a..a + peek(0xa).min(0x10000 - a);
        let res = match port {
            0x5 => self.stat(i, &mut ram[span(peek(0x4))]),
            0x6 => self.delete(i),
            0x9 => {
                self.close(i);
                let a = peek(0x8);
                let name = &ram[a..];
                let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                let p = &mut self.ports[i];
                // Too long, or not ended within memory: no name.
                p.len = if len < MAX_PATH && a + len < 0x10000 {
                    p.path[..len].copy_from_slice(&name[..len]);
                    len
                } else {
                    0
                };
                0
            }
            0xd => self.read(i, &mut ram[span(peek(0xc))]),
            0xf => {
                let append = d[0x7] & 1 != 0;
                self.write(i, &ram[span(peek(0xe))], append)
            }
            _ => return,
        };
        d[0x2..0x4].copy_from_slice(&res.to_be_bytes());
    }

    /// The port's path, copied out so that the port can change meanwhile.
    fn path(&self, i: usize) -> Option<([u8; MAX_PATH], usize)> {
        let p = &self.ports[i];
        (p.len > 0 && self.root.is_some()).then_some((p.path, p.len))
    }

    fn stat(&mut self, i: usize, out: &mut [u8]) -> u16 {
        let Some((path, len)) = self.path(i) else {
            return 0;
        };
        match self.stat_path(&path[..len]) {
            None => out.fill(b'!'),
            Some(None) => out.fill(b'-'),
            Some(Some(size)) => hex(out, size),
        }
        out.len() as u16
    }

    fn delete(&mut self, i: usize) -> u16 {
        self.close(i);
        let Some((path, len)) = self.path(i) else {
            return 0;
        };
        // An open file can't be deleted here (it can with unlink): the
        // other device lets go of it first.
        let j = 1 - i;
        if self.ports[j].path[..self.ports[j].len].eq_ignore_ascii_case(&path[..len]) {
            self.close(j);
        }
        let Some((dir, last)) = self.parent(&path[..len], false) else {
            return 0xffff;
        };
        let ok = self
            .find(dir, last)
            .is_some_and(|e| self.vm.delete_entry_in_dir(dir, e.name).is_ok());
        self.release(dir);
        // unlink()'s result, as uxn gives it.
        if ok { 0 } else { 0xffff }
    }

    fn read(&mut self, i: usize, out: &mut [u8]) -> u16 {
        let Some((path, len)) = self.path(i) else {
            return 0;
        };
        let path = &path[..len];
        if let State::Idle | State::Write(_) = self.ports[i].state {
            self.close(i);
            self.ports[i].state = match self.stat_path(path) {
                Some(None) => State::List(0),
                Some(Some(_)) => {
                    let file = self.parent(path, false).and_then(|(dir, last)| {
                        let f = self.find(dir, last).and_then(|e| {
                            self.vm.open_file_in_dir(dir, e.name, Mode::ReadOnly).ok()
                        });
                        self.release(dir);
                        f
                    });
                    file.map_or(State::Idle, State::Read)
                }
                None => State::Idle,
            };
        }
        match self.ports[i].state {
            State::Read(f) => self.vm.read(f, out).unwrap_or(0) as u16,
            State::List(given) => {
                let (n, more) = self.list_into(path, given, out);
                self.ports[i].state = State::List(given + more);
                n as u16
            }
            _ => 0,
        }
    }

    /// Lines of a directory listing, from entry `skip` on, as many as fit
    /// whole in `out`: `0012 name` for a file (its size in hex, `????` from
    /// 64 KiB up), `---- name/` for a directory. How many bytes, and how
    /// many entries.
    fn list_into(&self, path: &[u8], skip: usize, out: &mut [u8]) -> (usize, usize) {
        let (mut seen, mut n, mut given) = (0, 0, 0);
        self.list(path, |e, name| {
            seen += 1;
            if seen <= skip {
                return ControlFlow::Continue(());
            }
            let dir = e.attributes.is_directory();
            let line = 5 + name.len() + dir as usize + 1;
            if n + line > out.len() {
                return ControlFlow::Break(());
            }
            let o = &mut out[n..n + line];
            if dir {
                o[..4].fill(b'-');
            } else {
                hex(&mut o[..4], e.size);
            }
            o[4] = b' ';
            o[5..5 + name.len()].copy_from_slice(name);
            if dir {
                o[line - 2] = b'/';
            }
            o[line - 1] = b'\n';
            n += line;
            given += 1;
            ControlFlow::Continue(())
        });
        (n, given)
    }

    fn write(&mut self, i: usize, data: &[u8], append: bool) -> u16 {
        let Some((path, len)) = self.path(i) else {
            return 0;
        };
        let path = &path[..len];
        // A path ending in `/` is a directory, made along with its parents.
        if path.ends_with(b"/") {
            self.close(i);
            return self.walk(parts(path), true).map_or(0, |d| {
                self.release(d);
                1
            });
        }
        if !matches!(self.ports[i].state, State::Write(_)) {
            self.close(i);
            let mode = if append {
                Mode::ReadWriteCreateOrAppend
            } else {
                Mode::ReadWriteCreateOrTruncate
            };
            let file = self.parent(path, true).and_then(|(dir, last)| {
                let sfn = match self.find(dir, last) {
                    Some(e) if !e.attributes.is_directory() => Some(e.name),
                    Some(_) => None,
                    None => short_name(last),
                };
                let f = sfn.and_then(|sfn| self.vm.open_file_in_dir(dir, sfn, mode).ok());
                self.release(dir);
                f
            });
            match file {
                Some(f) => self.ports[i].state = State::Write(f),
                None => return 0,
            }
        }
        let State::Write(f) = self.ports[i].state else {
            return 0;
        };
        // Flushed each time, so that the card holds what was written.
        match self.vm.write(f, data).and_then(|_| self.vm.flush_file(f)) {
            Ok(()) => data.len() as u16,
            Err(_) => 0,
        }
    }
}
