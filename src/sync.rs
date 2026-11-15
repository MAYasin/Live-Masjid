//! Lock and thread helpers, so the rest of the code never calls `unwrap`/`expect`.
//!
//! A lock is only poisoned when a thread panicked while holding it. Release builds abort on
//! panic (`panic = "abort"`), so that can't happen there; in tests the data behind our locks
//! is still consistent (every critical section is a plain read or replace), so take it.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult};
use std::thread::JoinHandle;
use std::time::Duration;

pub trait LockExt<T> {
    /// Lock, recovering the data if another thread panicked while holding it.
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub trait RwLockExt<T> {
    /// Read-lock, recovering the data if a writer panicked.
    fn read_lock(&self) -> RwLockReadGuard<'_, T>;
    /// Write-lock, recovering the data if a writer panicked.
    fn write_lock(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_lock(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_lock(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// [`Condvar::wait_timeout`], recovering the guard like [`LockExt::locked`].
pub fn wait_timeout<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>, dur: Duration) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
    cv.wait_timeout(guard, dur).unwrap_or_else(PoisonError::into_inner)
}

/// Start a named thread (the name shows in `top -H` and in panics).
///
/// # Panics
/// Only if the OS refuses to create a thread (out of memory or thread limit). The app can't
/// play anything in that state, so it stops and systemd (or Docker) restarts it.
pub fn spawn_named<F, T>(name: &str, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[expect(clippy::expect_used, reason = "no thread means no playback; a restart is the only recovery")]
    std::thread::Builder::new().name(name.into()).spawn(f).expect("the OS refused to start a thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_mutex_still_gives_its_data() {
        let m = Arc::new(Mutex::new(7));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        assert_eq!(*m.locked(), 7);
    }

    #[test]
    fn a_poisoned_rwlock_still_gives_its_data() {
        let l = Arc::new(RwLock::new(String::from("a")));
        let l2 = l.clone();
        let _ = std::thread::spawn(move || {
            let _g = l2.write().unwrap();
            panic!("poison it");
        })
        .join();
        l.write_lock().push('b');
        assert_eq!(*l.read_lock(), "ab");
    }

    #[test]
    fn waits_time_out() {
        let (m, cv) = (Mutex::new(()), Condvar::new());
        let (_g, r) = wait_timeout(&cv, m.locked(), Duration::from_millis(5));
        assert!(r.timed_out());
    }

    #[test]
    fn named_threads_run() {
        let h = spawn_named("lm-test", || std::thread::current().name().map(str::to_owned));
        assert_eq!(h.join().unwrap().as_deref(), Some("lm-test"));
    }
}
