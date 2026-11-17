//! Files a tool keeps in a directory its host granted (`Ports::data_dir`):
//! lock files and atomic writes, over std::fs, so the same code runs
//! natively and in a WASI component.
//!
//! wasi:filesystem has no advisory locks (no flock or fcntl), but it has
//! the two operations this needs: creating a file only if it doesn't exist
//! (exclusive create), and renaming over an existing file. Several
//! processes sharing a directory (two MCP servers, a CLI run beside one, a
//! native build beside a component) coordinate through them:
//!
//! - [`lock`]: hold `NAME.lock` while reading, changing and writing. It
//!   waits by the tool's own timer, so a holder in the same process (a task
//!   awaiting something with the lock held) carries on meanwhile.
//! - [`write_atomic`]: readers never see a half-written file, so reads need
//!   no lock.
//!
//! A lock left by a process that died is broken once it's older than
//! [`LockOptions::stale_ms`]. Locks aren't fair, and breaking a stale lock
//! can race with another breaker; they're for short, well-behaved holders.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct LockOptions {
    /// How long to wait for a lock someone else holds.
    pub wait_ms: u64,
    /// How often to look again.
    pub retry_ms: u64,
    /// A lock older than this is taken to be left by a process that died.
    pub stale_ms: u64,
}

impl Default for LockOptions {
    fn default() -> Self {
        LockOptions { wait_ms: 10_000, retry_ms: 20, stale_ms: 300_000 }
    }
}

#[derive(Debug)]
pub enum LockError {
    /// Someone else held it for the whole wait: who (as they described
    /// themselves) and for how long.
    Busy { holder: String, held_ms: u64 },
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy { holder, held_ms } => write!(f, "locked by {holder} for {:.1} s", *held_ms as f64 / 1000.0),
            LockError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// A held lock; dropping it releases it.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    token: String,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Only if it's still ours (it could have been broken as stale).
        if std::fs::read_to_string(&self.path).is_ok_and(|s| s.lines().next() == Some(self.token.as_str())) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique enough to tell holders and temporary files apart.
fn unique() -> String {
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn age_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(SystemTime::now().duration_since(modified).unwrap_or(Duration::ZERO).as_millis() as u64)
}

/// Takes the lock at `path` (e.g. `drafts/d3.lock`), waiting for it by
/// `sleep` (the tool's timer, so other tasks run meanwhile). `holder`
/// describes the taker, for whoever finds it locked.
pub async fn lock<F, Fut>(path: &Path, holder: &str, opts: &LockOptions, sleep: F) -> Result<Lock, LockError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let token = unique();
    let mut waited = 0;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                f.write_all(format!("{token}\n{holder}\n").as_bytes()).map_err(LockError::Io)?;
                return Ok(Lock { path: path.to_path_buf(), token });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(LockError::Io(e)),
        }
        let age = age_ms(path);
        if age.is_some_and(|a| a > opts.stale_ms) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if waited >= opts.wait_ms {
            let held = std::fs::read_to_string(path).unwrap_or_default();
            return Err(LockError::Busy {
                holder: held.lines().nth(1).unwrap_or("someone").to_string(),
                held_ms: age.unwrap_or(0),
            });
        }
        sleep(opts.retry_ms).await;
        waited += opts.retry_ms;
    }
}

/// Writes `data` to `path` so readers see the old contents or the new,
/// never part: a temporary file beside it, then a rename over it.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", unique()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Reads a file, None if it doesn't exist.
pub fn read_if_exists(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(d) => Ok(Some(d)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tool-files-{name}-{}", unique()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn no_wait(_: u64) {}

    #[test]
    fn excludes_and_releases() {
        let d = dir("lock");
        let p = d.join("x.lock");
        let quick = LockOptions { wait_ms: 60, retry_ms: 20, stale_ms: 60_000 };
        futures::executor::block_on(async {
            let held = lock(&p, "first", &quick, no_wait).await.unwrap();
            match lock(&p, "second", &quick, no_wait).await {
                Err(LockError::Busy { holder, .. }) => assert_eq!(holder, "first"),
                other => panic!("expected busy, got {other:?}"),
            }
            drop(held);
            assert!(!p.exists());
            lock(&p, "second", &quick, no_wait).await.unwrap();
        });
    }

    #[test]
    fn breaks_stale_locks() {
        let d = dir("stale");
        let p = d.join("x.lock");
        std::fs::write(&p, "someone-elses\ndead process\n").unwrap();
        let stale_now = LockOptions { wait_ms: 0, retry_ms: 1, stale_ms: 0 };
        std::thread::sleep(Duration::from_millis(5));
        futures::executor::block_on(async {
            let l = lock(&p, "me", &stale_now, no_wait).await.unwrap();
            assert!(std::fs::read_to_string(&p).unwrap().contains("me"));
            drop(l);
        });
    }

    #[test]
    fn a_broken_lock_isnt_removed_by_its_old_holder() {
        let d = dir("broken");
        let p = d.join("x.lock");
        futures::executor::block_on(async {
            let old = lock(&p, "old", &LockOptions::default(), no_wait).await.unwrap();
            std::fs::remove_file(&p).unwrap(); // broken as stale by someone
            let new = lock(&p, "new", &LockOptions::default(), no_wait).await.unwrap();
            drop(old);
            assert!(p.exists(), "the old holder must leave the new lock alone");
            drop(new);
        });
    }

    #[test]
    fn writes_atomically() {
        let d = dir("write");
        let p = d.join("a.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no temporary files left");
        assert_eq!(read_if_exists(&d.join("none")).unwrap(), None);
    }
}
