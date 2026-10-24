//! Native only: reload the firmware or board when its file changes on disk,
//! so a rebuild shows up without touching the app.
use crate::session::Source;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const EVERY: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Watch {
    /// (path, modification time) of the firmware and the board.
    files: [Option<(PathBuf, Option<SystemTime>)>; 2],
    last: Option<Instant>,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Watch {
    /// Watch the files these sources came from.
    pub fn track(&mut self, firmware: Option<&Source>, board: Option<&Source>) {
        for (slot, src) in self.files.iter_mut().zip([firmware, board]) {
            *slot = src.and_then(|s| s.path.clone()).map(|p| {
                let t = mtime(&p);
                (p, t)
            });
        }
    }

    /// Re-read any watched file that changed; true if one did.
    pub fn changed(&mut self, firmware: &mut Option<Source>, board: &mut Option<Source>) -> bool {
        if self.last.is_some_and(|t| t.elapsed() < EVERY) {
            return false;
        }
        self.last = Some(Instant::now());
        let mut any = false;
        for (slot, src) in self.files.iter_mut().zip([firmware, board]) {
            let Some((path, seen)) = slot else { continue };
            let now = mtime(path);
            if now == *seen || now.is_none() {
                continue;
            }
            // A build may still be writing it: wait for the next check if it
            // doesn't read back.
            let Ok(bytes) = std::fs::read(&*path) else { continue };
            *seen = now;
            if let Some(s) = src {
                s.bytes = bytes;
                any = true;
            }
        }
        any
    }
}
