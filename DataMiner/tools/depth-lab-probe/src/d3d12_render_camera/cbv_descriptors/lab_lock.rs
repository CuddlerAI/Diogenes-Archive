//! Lab-only lock substitution. Production without local-perf-lab keeps std::sync::Mutex.
//! Shared store algorithms are unchanged; no raw locking or relaxed failure semantics.
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LockKind {
    Std,
    Eventual,
    Fair,
    ReadWrite,
}

impl LockKind {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "std" => Ok(Self::Std),
            "eventual" => Ok(Self::Eventual),
            "fair" => Ok(Self::Fair),
            "rw" => Ok(Self::ReadWrite),
            _ => Err("descriptor-lock must be std, eventual, fair, or rw".into()),
        }
    }
}

enum Inner<T> {
    Std(std::sync::Mutex<T>),
    Eventual(parking_lot::Mutex<T>),
    Fair(parking_lot::FairMutex<T>),
    ReadWrite(parking_lot::RwLock<T>),
}

pub(crate) struct LabMutex<T> {
    inner: Inner<T>,
    poisoned: AtomicBool,
}
impl<T> LabMutex<T> {
    #[cfg(feature = "local-perf-lab-source-cache")]
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
            || matches!(&self.inner, Inner::Std(lock) if lock.is_poisoned())
    }
    pub(crate) fn new(value: T) -> Self {
        Self::with_kind(value, LockKind::Std)
    }

    pub(crate) fn with_kind(value: T, kind: LockKind) -> Self {
        Self {
            inner: match kind {
                LockKind::Std => Inner::Std(std::sync::Mutex::new(value)),
                LockKind::Eventual => Inner::Eventual(parking_lot::Mutex::new(value)),
                LockKind::Fair => Inner::Fair(parking_lot::FairMutex::new(value)),
                LockKind::ReadWrite => Inner::ReadWrite(parking_lot::RwLock::new(value)),
            },
            poisoned: AtomicBool::new(false),
        }
    }

    pub(crate) fn lock(&self) -> Result<LabGuard<'_, T>, ()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(());
        }
        let inner = match &self.inner {
            Inner::Std(lock) => GuardInner::Std(lock.lock().map_err(|_| ())?),
            Inner::Eventual(lock) => GuardInner::Eventual(lock.lock()),
            Inner::Fair(lock) => GuardInner::Fair(lock.lock()),
            Inner::ReadWrite(lock) => GuardInner::ReadWrite(lock.write()),
        };
        if self.poisoned.load(Ordering::Acquire) {
            return Err(());
        }
        Ok(LabGuard {
            inner,
            poisoned: &self.poisoned,
        })
    }

    pub(crate) fn read(&self) -> Result<LabReadGuard<'_, T>, ()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(());
        }
        let inner = match &self.inner {
            Inner::ReadWrite(lock) => ReadInner::Shared(lock.read()),
            _ => ReadInner::Exclusive(self.lock()?),
        };
        if self.poisoned.load(Ordering::Acquire) {
            return Err(());
        }
        Ok(LabReadGuard {
            inner,
            poisoned: &self.poisoned,
        })
    }
}

enum GuardInner<'a, T> {
    Std(std::sync::MutexGuard<'a, T>),
    Eventual(parking_lot::MutexGuard<'a, T>),
    Fair(parking_lot::FairMutexGuard<'a, T>),
    ReadWrite(parking_lot::RwLockWriteGuard<'a, T>),
}
pub(crate) struct LabGuard<'a, T> {
    inner: GuardInner<'a, T>,
    poisoned: &'a AtomicBool,
}
impl<T> Deref for LabGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            GuardInner::Std(g) => g,
            GuardInner::Eventual(g) => g,
            GuardInner::Fair(g) => g,
            GuardInner::ReadWrite(g) => g,
        }
    }
}
impl<T> DerefMut for LabGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.inner {
            GuardInner::Std(g) => g,
            GuardInner::Eventual(g) => g,
            GuardInner::Fair(g) => g,
            GuardInner::ReadWrite(g) => g,
        }
    }
}

enum ReadInner<'a, T> {
    Exclusive(LabGuard<'a, T>),
    Shared(parking_lot::RwLockReadGuard<'a, T>),
}
pub(crate) struct LabReadGuard<'a, T> {
    inner: ReadInner<'a, T>,
    poisoned: &'a AtomicBool,
}
impl<T> Deref for LabReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            ReadInner::Exclusive(g) => g,
            ReadInner::Shared(g) => g,
        }
    }
}
impl<T> Drop for LabReadGuard<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}
impl<T> Drop for LabGuard<'_, T> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Set before the underlying guard unlocks, so every waiter rejects the data.
            self.poisoned.store(true, Ordering::Release);
        }
    }
}
