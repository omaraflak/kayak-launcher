//! Somewhere for the launcher to say what it did.
//!
//! Everything interesting the launcher does happens off-screen: it drives the
//! Docker CLI, runs an installer, and supervises a native inference server. When
//! any of that failed the user saw a spinner or a bare "error" and had nothing
//! to report, which is how a broken Metal launch went undiagnosed.
//!
//! Records are kept in memory so a dump is cheap, and mirrored to files: one in
//! `~/Library/Logs/Kayak`, where Help › Show Kayak Log finds it and which exists
//! from the first line -- so a launch that fails before Kayak ever runs still
//! leaves a record -- and one in the shared data directory, so Kayak, which runs
//! in a container and cannot see the host, can include them in a support bundle.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::lock;

/// Lines retained in memory. Enough to cover a model load, which is the longest
/// thing that happens, without letting a chatty server grow without bound.
const CAPACITY: usize = 4000;

/// A log file larger than this is cut back when the launcher starts.
///
/// The files are appended to across launches, and a GPU server is chatty, so
/// without a limit they grow for as long as Kayak is installed.
const TRIM_ABOVE: u64 = 5 * 1024 * 1024;

/// How much of a trimmed file is kept: the most recent part, which is the part
/// anyone looking at it wants.
const TRIM_TO: u64 = 1024 * 1024;

struct Log {
    lines: Mutex<VecDeque<String>>,
    files: Mutex<Vec<(PathBuf, File)>>,
}

fn log() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(|| Log {
        lines: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        files: Mutex::new(Vec::new()),
    })
}

/// Where the launcher keeps its own log: `~/Library/Logs/Kayak/launcher.log`,
/// the place macOS apps keep theirs and Console lists.
pub fn launcher_log_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join("Library")
            .join("Logs")
            .join("Kayak")
            .join("launcher.log")
    })
}

/// Starts mirroring records to a file, and replays what is already buffered.
///
/// Called before some records have been made, so the buffer is flushed rather
/// than dropped. Mirroring the same file twice is ignored, so it is safe to
/// call on every successful start.
pub fn mirror_to(path: &Path) {
    let mut files = lock(&log().files);
    if files.iter().any(|(mirrored, _)| mirrored == path) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    trim(path, TRIM_ABOVE, TRIM_TO);

    let opened = OpenOptions::new().create(true).append(true).open(path);
    let Ok(mut file) = opened else {
        return;
    };
    for line in lock(&log().lines).iter() {
        let _ = writeln!(file, "{line}");
    }
    let _ = file.flush();
    files.push((path.to_path_buf(), file));
}

/// Cuts a file down to its last `keep` bytes once it grows past `limit`.
///
/// The cut lands on a line boundary so the file does not begin halfway through
/// a record. Failure leaves the file as it was: a log that is too long is a far
/// smaller problem than one that has been lost.
fn trim(path: &Path, limit: u64, keep: u64) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return;
    };
    if length <= limit {
        return;
    }
    let mut tail = Vec::new();
    if file.seek(SeekFrom::Start(length.saturating_sub(keep))).is_err()
        || file.read_to_end(&mut tail).is_err()
    {
        return;
    }
    let kept = from_line_start(&tail);
    let temporary = path.with_extension("log.tmp");
    if std::fs::write(&temporary, kept).is_ok() {
        let _ = std::fs::rename(&temporary, path);
    }
}

/// The part of `bytes` from the first complete line onwards.
fn from_line_start(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte == b'\n') {
        Some(newline) => &bytes[newline + 1..],
        None => bytes,
    }
}

/// Records one line, tagged with its source.
pub fn record(source: &str, message: &str) {
    // Trimmed because most records come from a child process's output, where a
    // trailing carriage return is common and makes the file awkward to read.
    let line = format!("[{source}] {}", message.trim_end());

    for (_, file) in lock(&log().files).iter_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }

    let mut lines = lock(&log().lines);
    if lines.len() == CAPACITY {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// The most recent records, oldest first.
pub fn tail(count: usize) -> Vec<String> {
    let lines = lock(&log().lines);
    lines
        .iter()
        .skip(lines.len().saturating_sub(count))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_most_recent_lines() {
        for index in 0..10 {
            record("test", &format!("line {index}"));
        }
        let tail = tail(3);

        assert_eq!(tail.len(), 3);
        assert!(tail[2].ends_with("line 9"));
        assert!(tail[2].starts_with("[test]"));
    }

    #[test]
    fn asking_for_more_than_exists_returns_everything() {
        let all = tail(CAPACITY * 2);

        // Never panics or over-skips, which a saturating_sub protects against.
        assert!(all.len() <= CAPACITY);
    }

    #[test]
    fn trailing_whitespace_is_trimmed() {
        record("test", "noisy line\r\n");
        let last = tail(1).pop().unwrap();

        assert_eq!(last, "[test] noisy line");
    }

    #[test]
    fn a_trimmed_log_starts_on_a_whole_line() {
        assert_eq!(from_line_start(b"ial line]\n[boot] next\n"), b"[boot] next\n");
        assert_eq!(from_line_start(b"no newline at all"), b"no newline at all");
    }

    #[test]
    fn an_oversized_log_keeps_only_its_recent_end() {
        let dir = std::env::temp_dir().join("kayak-launcher-trim");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("launcher.log");
        let body: String = (0..1000).map(|index| format!("[test] line {index}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        trim(&path, 1000, 200);

        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.len() <= 200);
        assert!(kept.starts_with("[test] line "));
        assert!(kept.ends_with("[test] line 999\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_small_log_is_left_alone() {
        let dir = std::env::temp_dir().join("kayak-launcher-trim-small");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("launcher.log");
        std::fs::write(&path, "[test] one\n").unwrap();

        trim(&path, 1000, 200);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[test] one\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
