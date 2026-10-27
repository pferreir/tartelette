//! Native only: reload the firmware, board or shields when their file changes
//! on disk, so a rebuild shows up without touching the app.
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use crate::session::Source;

const EVERY: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Watch {
    /// (path, modification time) of each watched file.
    files: Vec<(PathBuf, Option<SystemTime>)>,
    last: Option<Instant>,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Watch {
    /// Watch the files these sources came from.
    pub fn track<'a>(&mut self, sources: impl IntoIterator<Item = &'a Source>) {
        self.files = sources
            .into_iter()
            .filter_map(|s| s.path.clone())
            .map(|p| (mtime(&p), p))
            .map(|(t, p)| (p, t))
            .collect();
    }

    /// Re-read any watched file that changed; true if one did.
    pub fn changed<'a>(&mut self, sources: impl IntoIterator<Item = &'a mut Source>) -> bool {
        if self.last.is_some_and(|t| t.elapsed() < EVERY) {
            return false;
        }
        self.last = Some(Instant::now());
        let mut any = false;
        for src in sources {
            let Some(path) = &src.path else { continue };
            let Some((_, seen)) = self.files.iter_mut().find(|(p, _)| p == path) else {
                continue;
            };
            let now = mtime(path);
            if now == *seen || now.is_none() {
                continue;
            }
            // A build may still be writing it: wait for the next check if it
            // doesn't read back.
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            *seen = now;
            src.bytes = bytes;
            any = true;
        }
        any
    }
}
