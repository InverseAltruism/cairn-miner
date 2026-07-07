//! Owns the `cairn-miner` child process: locate the binary, spawn it against the
//! launcher-written config with a loopback stats port, stop it cleanly, and tail
//! its log file for the UI.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// A running miner process plus the loopback port its stats are served on.
pub struct MinerHandle {
    child: Child,
    pub stats_port: u16,
    pub log_path: PathBuf,
}

impl MinerHandle {
    /// Still running? (Reaps the child if it has exited.)
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The child's exit code if it has already exited.
    pub fn exit_code(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Ask the miner to stop and reap it. Best-effort.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for MinerHandle {
    fn drop(&mut self) {
        // Never leave an orphaned miner behind when the launcher closes.
        if self.is_running() {
            self.stop();
        }
    }
}

/// Locate the `cairn-miner` binary that ships next to the launcher, falling back
/// to one found on `PATH` (via the bare name).
pub fn miner_exe_path() -> PathBuf {
    let name = format!("cairn-miner{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(&name);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

/// Directory the miner writes its rotating log into.
pub fn log_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("logs")
}

/// Pick a free loopback TCP port by binding `:0` and reading the assignment.
pub fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(41787)
}

/// Spawn the miner against `config_path`, serving stats on a fresh loopback
/// port and logging into `log_dir`. Assumes the caller has already written the
/// config file (settings come from it via `--config`).
pub fn spawn(exe: &Path, config_path: &Path, log_dir: &Path) -> std::io::Result<MinerHandle> {
    let port = free_port();
    let log_path = log_dir.join("cairn-miner.current.log");

    let mut cmd = Command::new(exe);
    cmd.arg("--config")
        .arg(config_path)
        .arg("--stats-port")
        .arg(port.to_string())
        .arg("--log-dir")
        .arg(log_dir);

    // On Windows, don't flash a console window behind the launcher.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn()?;
    Ok(MinerHandle {
        child,
        stats_port: port,
        log_path,
    })
}

/// Return the last `max_lines` lines of the miner log (oldest first), or an
/// empty vec if the file doesn't exist yet.
///
/// Only the final chunk of the file is read (not the whole thing), so this stays
/// cheap even when the log has grown large over a long mining session — it's
/// polled once a second on the UI thread.
pub fn tail_log(path: &Path, max_lines: usize) -> Vec<String> {
    const MAX_BYTES: u64 = 64 * 1024;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(MAX_BYTES);
    let seeked = start > 0;
    if seeked {
        let _ = file.seek(SeekFrom::Start(start));
    }

    let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    // If we started mid-file, the first line read is a partial line — drop it.
    let mut skip_partial = seeked;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if skip_partial {
            skip_partial = false;
            continue;
        }
        if lines.len() == max_lines {
            lines.pop_front();
        }
        lines.push_back(strip_ansi(&line));
    }
    lines.into()
}

/// Drop ANSI SGR escape sequences (the miner colorizes stderr; its file log is
/// already plain, but be defensive) so the log panel shows clean text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip until the terminating 'm' of a CSI ... m sequence.
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_is_nonzero() {
        assert_ne!(free_port(), 0);
    }

    #[test]
    fn strips_ansi_color_codes() {
        let s = "\u{1b}[2m2026-07-07T00:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m hello";
        assert_eq!(strip_ansi(s), "2026-07-07T00:00:00Z  INFO hello");
    }

    #[test]
    fn tail_log_reads_only_last_lines_of_a_large_file() {
        use std::io::Write;
        // >64KB so the tail seek path is exercised (and the partial first line
        // after the seek offset is dropped).
        let path = std::env::temp_dir().join(format!("cairn-tail-{}.log", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            for i in 0..10_000 {
                writeln!(f, "line {i:05}").unwrap();
            }
        }
        let lines = tail_log(&path, 10);
        let _ = std::fs::remove_file(&path);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines.first().unwrap(), "line 09990");
        assert_eq!(lines.last().unwrap(), "line 09999");
    }
}
