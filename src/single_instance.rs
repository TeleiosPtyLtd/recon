//! Process-wide lock so only one recon TUI dashboard runs at a time.
//!
//! Uses an advisory `flock(2)` on a file under the cache dir. The kernel
//! releases the lock automatically when the process exits, so crashes don't
//! leave the file "stuck" — no stale-PID cleanup needed.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

/// Hold this for the lifetime of the dashboard. Drop releases the lock; so
/// does process exit (the kernel cleans up).
pub struct DashboardLock {
    _file: File,
}

/// Try to acquire the dashboard lock. `Ok(_)` means we hold it; `Err(())`
/// means another dashboard is already running.
pub fn acquire() -> Result<DashboardLock, ()> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| ())?;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        Ok(DashboardLock { _file: file })
    } else {
        Err(())
    }
}

fn lock_path() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    base.join("recon").join("dashboard.lock")
}
