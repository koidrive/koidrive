// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rustix::fs::{Mode, OFlags};
use tracing_subscriber::fmt::MakeWriter;

pub const MAX: u64 = 10 * 1024 * 1024;

// launchd appends to the file the property list names and never rotates it.
pub struct CappedLog {
    path: PathBuf,
    max: u64,
    file: Mutex<File>,
    redirect: fn(&File) -> io::Result<()>,
}

impl CappedLog {
    pub fn of_stderr(path: PathBuf) -> Option<Self> {
        let stderr = rustix::fs::fstat(io::stderr()).ok()?;
        let log = fs::metadata(&path).ok()?;
        if (stderr.st_dev as u64, stderr.st_ino as u64) != (log.dev(), log.ino()) {
            return None;
        }
        let flags = OFlags::WRONLY | OFlags::APPEND;
        let file = koidrive_state_file::open_regular(rustix::fs::CWD, &path, flags).ok()?;
        Some(CappedLog::new(path, MAX, file, redirect))
    }

    fn new(path: PathBuf, max: u64, file: File, redirect: fn(&File) -> io::Result<()>) -> Self {
        CappedLog {
            path,
            max,
            file: Mutex::new(file),
            redirect,
        }
    }

    fn write_line(&self, line: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let size = file.metadata()?.len();
        if size > 0
            && size + line.len() as u64 > self.max
            && let Ok(new) = self.rotate()
        {
            *file = new;
        }
        file.write_all(line)
    }

    fn rotate(&self) -> io::Result<File> {
        fs::rename(&self.path, previous(&self.path))?;
        let flags = OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE;
        let new = koidrive_state_file::create_regular(
            rustix::fs::CWD,
            &self.path,
            flags,
            Mode::from_raw_mode(0o666),
        )?;
        let _ = (self.redirect)(&new);
        Ok(new)
    }
}

pub fn previous(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

fn redirect(file: &File) -> io::Result<()> {
    rustix::stdio::dup2_stdout(file)?;
    rustix::stdio::dup2_stderr(file)?;
    Ok(())
}

pub struct Line<'a>(&'a CappedLog);

impl Write for Line<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_line(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CappedLog {
    type Writer = Line<'a>;

    fn make_writer(&'a self) -> Line<'a> {
        Line(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(dir: &Path, max: u64) -> CappedLog {
        let path = dir.join("dev.koidrive.koi.log");
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        CappedLog::new(path, max, file, |_| Ok(()))
    }

    fn write(log: &CappedLog, line: &str) {
        log.make_writer().write_all(line.as_bytes()).unwrap();
    }

    #[test]
    fn the_log_stays_under_its_cap_and_keeps_the_previous_one_whole() {
        let dir = tempfile::tempdir().unwrap();
        let log = log(dir.path(), 100);
        let lines: Vec<String> = (0..40)
            .map(|n| format!("line {n:02} of the daemon\n"))
            .collect();
        for line in &lines {
            write(&log, line);
            assert!(fs::metadata(&log.path).unwrap().len() <= 100);
        }
        let current = fs::read_to_string(&log.path).unwrap();
        let old = fs::read_to_string(previous(&log.path)).unwrap();
        assert!(old.len() <= 100, "{old}");
        assert!(current.ends_with(lines.last().unwrap()), "{current}");
        let kept = format!("{old}{current}");
        let tail = lines.concat();
        assert!(tail.ends_with(&kept), "{kept}");
        assert!(
            kept.lines().all(|line| line.ends_with("of the daemon")),
            "{kept}"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_log_already_past_the_cap_moves_aside_at_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.koidrive.koi.log");
        fs::write(&path, "x".repeat(500)).unwrap();
        let log = log(dir.path(), 100);
        write(&log, "the daemon is running\n");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "the daemon is running\n"
        );
        assert_eq!(fs::metadata(previous(&path)).unwrap().len(), 500);
    }

    #[test]
    fn a_line_longer_than_the_cap_still_goes_in_a_log_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let log = log(dir.path(), 10);
        write(&log, "short\n");
        write(&log, "a line longer than the cap\n");
        assert_eq!(
            fs::read_to_string(&log.path).unwrap(),
            "a line longer than the cap\n"
        );
        assert_eq!(fs::read_to_string(previous(&log.path)).unwrap(), "short\n");
    }

    #[test]
    fn stderr_that_is_not_the_log_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev.koidrive.koi.log");
        fs::write(&path, "").unwrap();
        assert!(CappedLog::of_stderr(path).is_none());
    }
}
