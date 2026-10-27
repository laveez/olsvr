//! The PID file. The running instance holds an flock on it for its whole life, so the lock, not
//! the PID inside, says whether an instance runs: nothing signals a PID that an instance which
//! did not exit cleanly left behind.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use crate::backend::macos::process_name;

#[cfg(target_os = "linux")]
pub(crate) fn path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    runtime_dir.join("olsvr.pid")
}

#[cfg(target_os = "macos")]
pub(crate) fn path() -> PathBuf {
    // Not $TMPDIR: macOS deletes files there that have not been accessed for 3 days.
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Application Support/olsvr/olsvr.pid")
}

/// The running instance's lock on the PID file. The kernel releases it with the fd, however
/// the process ends.
pub(crate) struct PidLock {
    path: PathBuf,
    _file: File,
}

impl PidLock {
    /// Takes the lock and writes this process's PID. `Ok(None)` means another instance holds
    /// the lock. An error of kind `WouldBlock` means other olsvr commands kept the file locked
    /// for 5 s.
    pub(crate) fn acquire(path: &Path) -> std::io::Result<Option<PidLock>> {
        #[cfg(target_os = "macos")]
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut file = loop {
            // Truncated only once the lock is ours: until then it may be the running instance's.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            match file.try_lock() {
                // An exiting instance, or a `stop` that found the file stale, unlinks it while
                // holding its lock. A lock taken on the unlinked file would guard nothing.
                Ok(()) if file.metadata()?.nlink() > 0 => break file,
                Ok(()) => continue,
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e),
            }
            // An instance holds the lock exclusively. `stop`, `--activate` and `--now` hold it
            // shared, only while they check the file, so shared holders alone are waited out.
            match file.try_lock_shared() {
                Ok(()) => drop(file),
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => return Err(e),
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    ErrorKind::WouldBlock,
                    "other olsvr commands keep it locked",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        Ok(Some(PidLock {
            path: path.to_owned(),
            _file: file,
        }))
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        remove_if_ours(&self.path);
    }
}

/// The daemon's lock. Exits with `EXIT_ALREADY_RUNNING` when another instance holds it, and
/// with 1 when other olsvr commands keep the file locked. Without a PID file the daemon still
/// runs, but `stop` and `--activate` cannot find it.
pub(crate) fn lock_or_exit() -> Option<PidLock> {
    let path = path();
    match PidLock::acquire(&path) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            match running_pid(&path) {
                Ok(Some(pid)) => eprintln!("olsvr is already running (PID {pid})"),
                _ => eprintln!("olsvr is already running"),
            }
            std::process::exit(crate::EXIT_ALREADY_RUNNING);
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            eprintln!("Cannot lock the PID file {}: {e}", path.display());
            std::process::exit(1);
        }
        Err(e) => {
            log::warn!(
                "Cannot lock the PID file {}, running without one: {e}",
                path.display()
            );
            None
        }
    }
}

/// The PID of the instance that holds the lock. `Ok(None)` when none does; a file left behind
/// by an instance that did not exit cleanly is removed then.
pub(crate) fn running_pid(path: &Path) -> std::io::Result<Option<libc::pid_t>> {
    // A starting instance holds the lock a moment before it has replaced the previous
    // instance's PID with its own, so a file that names no olsvr process is read again.
    let mut retries = 10;
    loop {
        let problem = match holder(path)? {
            Holder::Nobody => return Ok(None),
            Holder::Pid(pid) if is_olsvr(pid) => return Ok(Some(pid)),
            Holder::Pid(pid) => format!("{} names PID {pid}, which is not olsvr", path.display()),
            Holder::Unreadable => format!("{} holds no valid PID", path.display()),
        };
        if retries == 0 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, problem));
        }
        retries -= 1;
        std::thread::sleep(Duration::from_millis(20));
    }
}

enum Holder {
    Nobody,
    Pid(libc::pid_t),
    Unreadable,
}

/// One look at the lock on the PID file at `path`. A file that nobody holds is removed: only
/// an instance that did not exit cleanly leaves one.
fn holder(path: &Path) -> std::io::Result<Holder> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Holder::Nobody),
        Err(e) => return Err(e),
    };
    match file.try_lock_shared() {
        Ok(()) => {
            if std::fs::remove_file(path).is_ok() {
                log::info!("Removed the leftover PID file {}", path.display());
            }
            Ok(Holder::Nobody)
        }
        Err(TryLockError::WouldBlock) => {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            Ok(parse_pid(&contents).map_or(Holder::Unreadable, Holder::Pid))
        }
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Whether process `pid` runs olsvr, under the installed binary's name or under the name this
/// copy runs as.
fn is_olsvr(pid: libc::pid_t) -> bool {
    let name = process_name(pid);
    !name.is_empty() && (name == "olsvr" || name == process_name(std::process::id() as libc::pid_t))
}

/// The kernel's name for process `pid`, the first 15 bytes of its executable's file name, or an
/// empty string if it cannot be read, e.g. because the process has exited.
#[cfg(target_os = "linux")]
fn process_name(pid: libc::pid_t) -> String {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    comm.strip_suffix('\n').unwrap_or(&comm).to_owned()
}

/// Sends `signal` to the running instance and returns its PID. `Ok(None)` when no instance
/// runs.
pub(crate) fn signal_running(signal: libc::c_int) -> std::io::Result<Option<libc::pid_t>> {
    let Some(pid) = running_pid(&path())? else {
        return Ok(None);
    };
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(Some(pid));
    }
    let error = std::io::Error::last_os_error();
    // ESRCH: the instance exited after the lock check.
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(error)
    }
}

pub(crate) enum StopOutcome {
    NotRunning,
    Stopped(libc::pid_t),
    /// SIGTERM was delivered, but the instance still held the lock when the wait ran out.
    StillRunning(libc::pid_t),
}

/// Sends SIGTERM to the running instance, then polls the lock every 50 ms for up to 2 s, so
/// an instance started right after is not refused while this one shuts down.
pub(crate) fn stop_running() -> std::io::Result<StopOutcome> {
    let Some(pid) = signal_running(libc::SIGTERM)? else {
        return Ok(StopOutcome::NotRunning);
    };
    let path = path();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if matches!(running_pid(&path), Ok(None)) {
            return Ok(StopOutcome::Stopped(pid));
        }
    }
    Ok(StopOutcome::StillRunning(pid))
}

/// Removes the PID file at `path` if it holds this process's PID.
pub(crate) fn remove_if_ours(path: &Path) {
    if read_pid(path) == Some(std::process::id() as libc::pid_t) {
        let _ = std::fs::remove_file(path);
    }
}

fn read_pid(path: &Path) -> Option<libc::pid_t> {
    parse_pid(&std::fs::read_to_string(path).ok()?)
}

/// Positive PIDs only: `kill` reads 0 and values below -1 as process groups, and -1 as every
/// process the caller may signal.
fn parse_pid(contents: &str) -> Option<libc::pid_t> {
    contents.trim().parse().ok().filter(|&pid| pid > 0)
}
