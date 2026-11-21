use std::{
    ffi::CStr,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
};

/// Bytes kept for a terminal that isn't reading; past this they are dropped.
const KEEP: usize = 1 << 20;

/// The master side of a pseudo-terminal; programs open the other side.
pub struct Pty {
    master: OwnedFd,
    name: String,
    link: Option<PathBuf>,
    /// Written by the device, not yet taken by the terminal.
    pending: Vec<u8>,
}

fn last_error(what: &str) -> String {
    format!("pseudo-terminal: {what}: {}", io::Error::last_os_error())
}

impl Pty {
    /// Make a pseudo-terminal in raw mode, and a symlink to it at `link`
    /// (replacing an older symlink there).
    pub fn open(link: Option<&Path>) -> Result<Self, String> {
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(last_error("posix_openpt"));
        }
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
            return Err(last_error("unlockpt"));
        }
        let mut buf = [0 as libc::c_char; 128];
        if unsafe { libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
            return Err(last_error("ptsname"));
        }
        let name = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        // Raw mode, so that nothing is echoed back to the device or
        // translated. The setting stays with the terminal while it exists.
        let slave = unsafe { libc::open(buf.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave < 0 {
            return Err(last_error(&name));
        }
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(slave, &mut t);
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(slave, libc::TCSANOW, &t);
            libc::close(slave);
        }
        if let Some(link) = link {
            if std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = std::fs::remove_file(link);
            }
            std::os::unix::fs::symlink(&name, link)
                .map_err(|e| format!("{}: {e}", link.display()))?;
        }
        Ok(Self {
            master,
            name,
            link: link.map(Path::to_path_buf),
            pending: Vec::new(),
        })
    }

    /// `/dev/pts/N`, or the symlink to it.
    pub fn name(&self) -> String {
        match &self.link {
            Some(l) => format!("{} ({})", l.display(), self.name),
            None => self.name.clone(),
        }
    }

    /// Does a program have the terminal open? (The master sees a hang-up
    /// while nobody does.)
    pub fn is_open(&self) -> bool {
        let mut p = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        unsafe { libc::poll(&mut p, 1, 0) };
        p.revents & libc::POLLHUP == 0
    }

    /// What programs wrote to the terminal since the last call.
    pub fn read(&mut self) -> Vec<u8> {
        self.read_at_most(usize::MAX)
    }

    /// Up to `max` bytes of what programs wrote to the terminal. The rest
    /// stays in the terminal, whose buffer filling up holds the writer back:
    /// flow control for a device slower than the program.
    pub fn read_at_most(&mut self, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while out.len() < max {
            let want = buf.len().min(max - out.len());
            let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), want) };
            if n <= 0 {
                break; // EAGAIN, or EIO while nobody has it open
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        out
    }

    /// Pass the device's bytes to whoever has the terminal open; with
    /// nobody there they are dropped, as a closed port would.
    pub fn write(&mut self, data: &[u8]) {
        if !self.is_open() {
            self.pending.clear();
            return;
        }
        self.pending.extend_from_slice(data);
        while !self.pending.is_empty() {
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    self.pending.as_ptr().cast(),
                    self.pending.len(),
                )
            };
            if n <= 0 {
                break;
            }
            self.pending.drain(..n as usize);
        }
        let extra = self.pending.len().saturating_sub(KEEP);
        self.pending.drain(..extra);
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(link) = &self.link {
            let _ = std::fs::remove_file(link);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
    };

    use super::*;

    #[test]
    fn bytes_go_both_ways_and_open_follows_the_terminal() {
        let dir = std::env::temp_dir().join(format!("bridge-usb-{}", std::process::id()));
        let mut pty = Pty::open(Some(&dir)).unwrap();
        assert!(!pty.is_open());
        pty.write(b"dropped");
        let mut tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dir)
            .unwrap();
        assert!(pty.is_open());
        pty.write(b"hello");
        let mut buf = [0u8; 5];
        tty.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        tty.write_all(b"\r\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(pty.read(), b"\r\n", "raw: no translation, no echo");
        tty.write_all(b"abcdef").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(pty.read_at_most(4), b"abcd");
        assert_eq!(pty.read_at_most(4), b"ef");
        drop(tty);
        assert!(!pty.is_open());
        drop(pty);
        assert!(!dir.exists(), "the symlink goes with it");
    }
}
