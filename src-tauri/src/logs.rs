//! Somewhere for the launcher to say what it did.
//!
//! Everything interesting the launcher does happens off-screen: it drives the
//! Docker CLI, runs an installer, and supervises a native inference server. When
//! any of that failed the user saw a spinner or a bare "error" and had nothing
//! to report, which is how a broken Metal launch went undiagnosed.
//!
//! Records are kept in memory so a dump is cheap, and mirrored to a file in the
//! shared data directory so Kayak -- which runs in a container and cannot see
//! the host -- can include them in a support bundle.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Lines retained in memory. Enough to cover a model load, which is the longest
/// thing that happens, without letting a chatty server grow without bound.
const CAPACITY: usize = 4000;

struct Log {
    lines: Mutex<VecDeque<String>>,
    file: Mutex<Option<File>>,
}

fn log() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(|| Log {
        lines: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        file: Mutex::new(None),
    })
}

/// Starts mirroring records to a file, and replays what is already buffered.
///
/// Called once the data directory exists, which is after the first records have
/// already been made, so the buffer is flushed rather than dropped.
pub fn mirror_to(path: &Path) {
    let opened = OpenOptions::new().create(true).append(true).open(path);
    let Ok(mut file) = opened else {
        return;
    };
    {
        let lines = log().lines.lock().unwrap();
        for line in lines.iter() {
            let _ = writeln!(file, "{line}");
        }
    }
    let _ = file.flush();
    *log().file.lock().unwrap() = Some(file);
}

/// Records one line, tagged with its source.
pub fn record(source: &str, message: &str) {
    // Trimmed because most records come from a child process's output, where a
    // trailing carriage return is common and makes the file awkward to read.
    let line = format!("[{source}] {}", message.trim_end());

    if let Ok(mut file) = log().file.lock() {
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    let mut lines = log().lines.lock().unwrap();
    if lines.len() == CAPACITY {
        lines.pop_front();
    }
    lines.push_back(line);
}

/// The most recent records, oldest first.
pub fn tail(count: usize) -> Vec<String> {
    let lines = log().lines.lock().unwrap();
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
}
