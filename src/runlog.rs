//! Run log: writes a whole session to disk so it can be replayed or grepped
//! after the TUI exits.
//!
//! Design notes:
//! - One line per event, `[elapsed seconds] tag content`. Relative time says
//!   more about "was the model slow or was ffmpeg slow" than wall-clock does.
//! - The filename carries local time, fetched through a single powershell call
//!   (same trick `get_key` uses) rather than pulling in chrono for a timestamp.
//! - Any IO failure only disables the log itself and never the main flow:
//!   logging must not become a new failure point.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Per-line cap, so one tool cannot dump a whole JSON payload into the log.
const MAX_LINE_CHARS: usize = 2_000;

pub struct RunLog {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    start: Instant,
}

impl RunLog {
    /// Create `agent-<local time>.log` under `dir`. Degrades to a no-op log when
    /// the directory or file cannot be created.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(format!("agent-{}.log", stamp()));
        let file = std::fs::create_dir_all(dir).ok().and_then(|_| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        });
        let log = Self {
            path: file.as_ref().map(|_| path),
            file,
            start: Instant::now(),
        };
        log.line(
            "START",
            &format!("epoch={} pid={}", epoch(), std::process::id()),
        );
        log
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Write one entry (embedded newlines are split; the tag only shows on the
    /// first line).
    pub fn line(&self, tag: &str, msg: &str) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let mut w = file;
        let elapsed = self.start.elapsed().as_secs_f64();
        for (index, text) in msg.lines().enumerate() {
            let text = truncate(text, MAX_LINE_CHARS);
            let tag = if index == 0 { tag } else { "  " };
            // A failed write means the file was deleted or the disk is full.
            // Nothing worth recovering from; drop it.
            let _ = writeln!(w, "[{elapsed:>8.3}] {tag:<6} {text}");
        }
        let _ = w.flush();
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…(truncated)")
}

fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Local time as `yyyyMMdd-HHmmss`. Falls back to epoch seconds so the filename
/// is always usable.
fn stamp() -> String {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-Date -Format 'yyyyMMdd-HHmmss'",
        ])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    epoch().to_string()
}
