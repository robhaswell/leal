//! The "user is interacting" signal, and waiting for it to clear (DESIGN
//! §3.10 rule 3).
//!
//! The app reports input; background jobs ask, at each checkpoint, whether
//! to wait. A waiting job sleeps on a condition variable rather than
//! polling, and wakes when input has been idle long enough, when the input
//! state changes, or when it is cancelled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub(super) struct Input {
    state: Mutex<State>,
    /// Signalled when the input state changes or a job is cancelled.
    changed: Condvar,
    idle_after: Duration,
}

struct State {
    /// In the middle of a gesture ([`Input::set_interacting`]).
    interacting: bool,
    /// When input was last reported.
    last_input: Option<Instant>,
}

impl Input {
    pub(super) fn new(idle_after: Duration) -> Self {
        Input {
            state: Mutex::new(State {
                interacting: false,
                last_input: None,
            }),
            changed: Condvar::new(),
            idle_after,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is two plain values, always consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One input event.
    pub(super) fn note(&self) {
        self.lock().last_input = Some(Instant::now());
        self.changed.notify_all();
    }

    /// A gesture began or ended.
    pub(super) fn set_interacting(&self, interacting: bool) {
        {
            let mut state = self.lock();
            state.interacting = interacting;
            state.last_input = Some(Instant::now());
        }
        self.changed.notify_all();
    }

    /// Whether background work should wait now.
    pub(super) fn should_wait(&self) -> bool {
        self.remaining(&self.lock(), Instant::now()).is_some()
    }

    /// Wakes every waiting job, so each checks its cancel flag. Taking the
    /// lock first means a job that has just checked its flag, and is about
    /// to wait, can't miss the wake-up.
    pub(super) fn wake(&self) {
        drop(self.lock());
        self.changed.notify_all();
    }

    /// Waits until background work may run, or `cancel` is set. Returns
    /// straight away if it may run now.
    pub(super) fn wait_until_idle(&self, cancel: &AtomicBool) {
        let mut state = self.lock();
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let Some(wait) = self.remaining(&state, Instant::now()) else {
                return;
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// How much longer background work must wait, or `None` if it may run.
    /// During a gesture that is unknown, so it is the whole idle time (the
    /// waiter is woken when the gesture ends).
    fn remaining(&self, state: &State, now: Instant) -> Option<Duration> {
        if state.interacting {
            return Some(self.idle_after);
        }
        let since = now.saturating_duration_since(state.last_input?);
        self.idle_after
            .checked_sub(since)
            .filter(|left| !left.is_zero())
    }
}
