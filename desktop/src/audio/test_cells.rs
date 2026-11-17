//! `Cell` and `RefCell` for test fakes the backend holds: the app may move
//! the backend to another thread while streaming starts, so what it holds
//! must be `Send`. Same calls, a mutex inside.

use std::sync::{Mutex, MutexGuard, PoisonError};

#[derive(Default)]
pub struct SyncCell<T>(Mutex<T>);

impl<T: Copy> SyncCell<T> {
    pub fn new(value: T) -> SyncCell<T> {
        SyncCell(Mutex::new(value))
    }

    pub fn get(&self) -> T {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, value: T) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = value;
    }
}

/// Unlike `RefCell`, two borrows at once wait for each other.
#[derive(Default)]
pub struct SyncRefCell<T>(Mutex<T>);

#[allow(
    clippy::should_implement_trait,
    reason = "the names of RefCell's calls, which the fakes' call sites keep"
)]
impl<T> SyncRefCell<T> {
    pub fn new(value: T) -> SyncRefCell<T> {
        SyncRefCell(Mutex::new(value))
    }

    pub fn borrow(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn borrow_mut(&self) -> MutexGuard<'_, T> {
        self.borrow()
    }
}
